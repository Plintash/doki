use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
#[cfg(debug_assertions)]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use parking_lot::Mutex;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};
use uuid::Uuid;

use waku_protocol::{
    ClientMessage, Command, DiagnosticCursor, DiagnosticEvent, DiagnosticExpiry, DiagnosticRecord,
    PROTOCOL_VERSION, ReplayCursor, Request, ResponseOutcome, ResponsePayload, RpcError,
    SequencedEvent, ServerMessage,
};
use waku_protocol::{
    DiagnosticCounterReport, DiagnosticCounters, DiagnosticLevel, DiagnosticLogs, DiagnosticOrigin,
    DiagnosticQuery, DiagnosticTimeline, DiagnosticTimelineQuery, DiagnosticVerbosity,
    MAX_WIRE_MESSAGE_BYTES,
};

use waku_protocol::{
    AutomationCall, AutomationCallResult, AutomationCommand, AutomationCursor, AutomationError,
    AutomationEvent, AutomationExecutorEvent, AutomationExecutorOutcome, AutomationExecutorReply,
    AutomationExecutorRequest, AutomationExpiry, AutomationInstance, AutomationResult,
    AutomationRun, AutomationSession, AutomationStream, AutomationWait, MAX_AUTOMATION_EVENTS,
};

const READ_POLL_INTERVAL: Duration = Duration::from_millis(25);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_BUFFERED_EVENTS_PER_RUNTIME: usize = 4096;
// A complete retained tail plus its preceding server/local expiry marker.
const MAX_BUFFERED_AUTOMATION_MESSAGES: usize = MAX_AUTOMATION_EVENTS + 1;
// A resumed backlog may contain one retained tail and the bounded live tail
// that raced it, each with its own expiry marker.
const MAX_BUFFERED_AUTOMATION_REPLAY: usize = MAX_BUFFERED_AUTOMATION_MESSAGES * 2;
const MAX_AUTOMATION_OUTGOING: usize = 8;

enum Outgoing {
    Message(ClientMessage),
    Automation(ClientMessage, AutomationPermit),
    Shutdown,
    Disconnect,
}

/// Automation snapshots can be much larger than ordinary notifications. Keep
/// their slots until socket serialization/write finishes, without changing the
/// existing outgoing queue's FIFO order or unrelated driver message semantics.
struct AutomationBackpressure {
    available: Receiver<()>,
    released: Sender<()>,
}

struct AutomationPermit(Sender<()>);

impl Drop for AutomationPermit {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

impl AutomationBackpressure {
    fn new() -> Self {
        let (released, available) = bounded(MAX_AUTOMATION_OUTGOING);
        for _ in 0..MAX_AUTOMATION_OUTGOING {
            released.try_send(()).unwrap();
        }
        Self {
            available,
            released,
        }
    }

    fn acquire(&self, timeout: Duration) -> anyhow::Result<AutomationPermit> {
        self.available
            .recv_timeout(timeout)
            .map_err(|_| anyhow!("timed out waiting for the automation transport queue"))?;
        Ok(AutomationPermit(self.released.clone()))
    }
}

struct ClientInner {
    outgoing: Sender<Outgoing>,
    automation_outgoing: AutomationBackpressure,
    pending: Mutex<HashMap<Uuid, Sender<Result<ResponsePayload, RpcError>>>>,
    sessions: Mutex<HashMap<(Uuid, Uuid), Sender<SequencedEvent>>>,
    pending_events: Mutex<HashMap<(Uuid, Uuid), VecDeque<SequencedEvent>>>,
    task_state_subscribers: Mutex<Vec<Sender<u64>>>,
    diagnostic_subscribers: Mutex<Vec<Sender<DiagnosticMessage>>>,
    pending_diagnostics: Mutex<VecDeque<DiagnosticMessage>>,
    last_sequences: Mutex<HashMap<(Uuid, Uuid), LastSequence>>,
    last_diagnostic: Mutex<Option<DiagnosticCursor>>,
    automation: Mutex<AutomationState>,
    automation_executor: Mutex<Option<Sender<AutomationExecutorMessage>>>,
    disconnected: AtomicBool,
    #[cfg(debug_assertions)]
    diagnostic_verbosity: Mutex<DiagnosticVerbosity>,
    #[cfg(debug_assertions)]
    request_round_trip_us: AtomicU64,
}

/// One message from the instance's diagnostic stream.
///
/// A reconnect that outran retention is named as [`Self::Expired`] before
/// the retained [`Self::Event`]s, so a consumer knows a gap exists instead of
/// inferring one from a sequence jump.
#[derive(Clone, Debug)]
pub enum DiagnosticMessage {
    Event(DiagnosticEvent),
    Expired(DiagnosticExpiry),
}

/// Incoming UI work is queued, never executed on the socket reader. Subscribe
/// before registering the executor and dispatch these messages on its own task.
#[derive(Clone, Debug)]
pub enum AutomationExecutorMessage {
    Request(AutomationExecutorRequest),
    Interest {
        instance_id: Uuid,
        attached: bool,
        streams: Vec<AutomationStream>,
        model_sections: Vec<String>,
    },
}

#[derive(Clone, Debug)]
pub enum AutomationMessage {
    Event(AutomationEvent),
    Expired(AutomationExpiry),
    WaitResolved(AutomationWait),
    RunChanged(AutomationRun),
}

/// The original automation failure, including its cross-process evidence.
#[derive(Clone, Debug)]
pub struct DaemonAutomationError {
    pub error: AutomationError,
}

impl std::fmt::Display for DaemonAutomationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.error.code, self.error.message)
    }
}

impl std::error::Error for DaemonAutomationError {}

#[derive(Default)]
struct AutomationState {
    subscribers: HashMap<Uuid, Vec<AutomationSubscriber>>,
    pending: HashMap<Uuid, AutomationBuffer>,
    cursors: HashMap<Uuid, AutomationCursor>,
    resuming: HashSet<Uuid>,
    resume_messages: HashMap<Uuid, VecDeque<AutomationMessage>>,
    resume_dropped: HashMap<Uuid, AutomationResumeGap>,
    resume_subscribers: HashMap<Uuid, AutomationSubscriber>,
}

struct AutomationResumeGap {
    cursor: AutomationCursor,
    notification: bool,
}

struct AutomationBuffer {
    messages: VecDeque<AutomationMessage>,
    limit: usize,
}

impl Default for AutomationBuffer {
    fn default() -> Self {
        Self {
            messages: VecDeque::new(),
            limit: MAX_BUFFERED_AUTOMATION_MESSAGES,
        }
    }
}

/// One slot is reserved for an expiry. A stalled subscriber receives that
/// marker and then closes, rather than blocking the socket or losing events
/// silently. It can resubscribe and resume from its last consumed event.
struct AutomationSubscriber {
    sender: Sender<AutomationMessage>,
    last_cursor: Option<AutomationCursor>,
    limit: usize,
}

impl AutomationMessage {
    fn cursor(&self) -> AutomationCursor {
        match self {
            Self::Event(event) => event.cursor,
            Self::Expired(expiry) => expiry.cursor,
            Self::WaitResolved(wait) => wait.cursor,
            Self::RunChanged(run) => run.cursor,
        }
    }

    fn gap_before(&self, newest: AutomationCursor) -> Self {
        let requested = match self {
            Self::Expired(expiry) => expiry.requested,
            _ => {
                let mut cursor = self.cursor();
                cursor.sequence = cursor.sequence.saturating_sub(1);
                cursor
            }
        };
        Self::Expired(AutomationExpiry {
            requested,
            oldest_sequence: newest.sequence,
            cursor: newest,
        })
    }
}

impl AutomationSubscriber {
    fn new(limit: usize) -> (Self, Receiver<AutomationMessage>) {
        let (sender, receiver) = bounded(limit + 1);
        (
            Self {
                sender,
                last_cursor: None,
                limit,
            },
            receiver,
        )
    }

    fn deliver(&mut self, message: AutomationMessage) -> bool {
        let cursor = message.cursor();
        if self.sender.len() >= self.limit {
            let _ = self
                .sender
                .try_send(AutomationMessage::Expired(AutomationExpiry {
                    requested: self.last_cursor.unwrap_or(cursor),
                    oldest_sequence: cursor.sequence.saturating_add(1),
                    cursor,
                }));
            return false;
        }
        self.last_cursor = Some(cursor);
        self.sender.try_send(message).is_ok()
    }
}

impl AutomationState {
    fn deliver(&mut self, session_id: Uuid, message: AutomationMessage) {
        let subscribers = self.subscribers.entry(session_id).or_default();
        subscribers.retain_mut(|subscriber| subscriber.deliver(message.clone()));
        if subscribers.is_empty() {
            let pending = self.pending.entry(session_id).or_default();
            if pending.messages.len() >= pending.limit {
                let gap = pending
                    .messages
                    .front()
                    .unwrap()
                    .gap_before(message.cursor());
                pending.messages.clear();
                pending.messages.push_back(gap);
            }
            pending.messages.push_back(message);
        }
    }

    fn prepare_replay(&mut self, session_id: Uuid) -> VecDeque<AutomationMessage> {
        // A successful explicit resume replaces unread stream history. Keep
        // notifications that are not represented by the event replay. Do this
        // only at commit, so a failed resume leaves the old queue usable.
        let mut notifications = self
            .pending
            .remove(&session_id)
            .unwrap_or_default()
            .messages;
        notifications.retain(|message| {
            matches!(
                message,
                AutomationMessage::WaitResolved(_) | AutomationMessage::RunChanged(_)
            )
        });
        self.pending.insert(
            session_id,
            AutomationBuffer {
                messages: VecDeque::new(),
                limit: MAX_BUFFERED_AUTOMATION_REPLAY,
            },
        );
        if let Some(subscriber) = self.resume_subscribers.remove(&session_id) {
            self.subscribers
                .entry(session_id)
                .or_default()
                .push(subscriber);
        }
        notifications
    }

    fn event(&mut self, event: AutomationEvent) {
        let session_id = event.session_id;
        if self.resuming.contains(&session_id) {
            self.buffer_resume(session_id, AutomationMessage::Event(event));
            return;
        }
        if self.cursors.get(&session_id).is_some_and(|cursor| {
            cursor.epoch == event.cursor.epoch && cursor.sequence >= event.cursor.sequence
        }) {
            return;
        }
        self.deliver_event(event);
    }

    fn notification(&mut self, session_id: Uuid, message: AutomationMessage) {
        if self.resuming.contains(&session_id) {
            self.buffer_resume(session_id, message);
        } else {
            self.deliver(session_id, message);
        }
    }

    fn buffer_resume(&mut self, session_id: Uuid, message: AutomationMessage) {
        let messages = self.resume_messages.entry(session_id).or_default();
        if messages.len() >= MAX_AUTOMATION_EVENTS {
            if let Some(dropped) = messages.pop_front() {
                let gap = self
                    .resume_dropped
                    .entry(session_id)
                    .or_insert(AutomationResumeGap {
                        cursor: dropped.cursor(),
                        notification: false,
                    });
                gap.cursor = dropped.cursor();
                gap.notification |= !matches!(dropped, AutomationMessage::Event(_));
            }
        }
        messages.push_back(message);
    }

    fn deliver_event(&mut self, event: AutomationEvent) {
        let cursor = self.cursors.entry(event.session_id).or_insert(event.cursor);
        if cursor.epoch != event.cursor.epoch || cursor.sequence < event.cursor.sequence {
            *cursor = event.cursor;
        }
        self.deliver(event.session_id, AutomationMessage::Event(event));
    }

    fn finish_resume(&mut self, session_id: Uuid) {
        self.resuming.remove(&session_id);
        self.resume_subscribers.remove(&session_id);
        let dropped = self.resume_dropped.remove(&session_id);
        if let Some(messages) = self.resume_messages.remove(&session_id) {
            if let Some(dropped) = dropped {
                let requested =
                    self.cursors
                        .get(&session_id)
                        .copied()
                        .unwrap_or(AutomationCursor {
                            epoch: dropped.cursor.epoch,
                            sequence: 0,
                        });
                if dropped.notification
                    || requested.epoch != dropped.cursor.epoch
                    || requested.sequence < dropped.cursor.sequence
                {
                    self.deliver(
                        session_id,
                        AutomationMessage::Expired(AutomationExpiry {
                            requested,
                            oldest_sequence: messages
                                .front()
                                .map_or(dropped.cursor.sequence + 1, |message| {
                                    message.cursor().sequence
                                }),
                            cursor: messages
                                .back()
                                .map_or(dropped.cursor, AutomationMessage::cursor),
                        }),
                    );
                }
            }
            for message in messages {
                match message {
                    AutomationMessage::Event(event) => self.event(event),
                    notification => self.deliver(session_id, notification),
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
struct LastSequence {
    epoch: Uuid,
    sequence: u64,
}

/// A daemon failure with the complete wire error retained for inspection.
#[derive(Clone, Debug)]
pub struct DaemonRpcError {
    pub error: RpcError,
}

impl std::fmt::Display for DaemonRpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.error.message)
    }
}

impl std::error::Error for DaemonRpcError {}

#[derive(Clone)]
pub struct DaemonClient {
    inner: Arc<ClientInner>,
}

impl DaemonClient {
    pub fn connect(address: &str, token: String) -> anyhow::Result<Self> {
        Self::connect_with_cursors(address, token, Vec::new(), None)
    }

    /// Connect to a literal daemon socket address under one absolute deadline.
    /// Intended for the native REPL's process-local endpoint. This deliberately
    /// avoids hostname resolution, redirects and independent handshake budgets.
    pub fn connect_with_deadline(
        address: &str,
        token: String,
        deadline: Instant,
    ) -> anyhow::Result<Self> {
        let address: SocketAddr = address
            .parse()
            .context("expected a daemon socket address")?;
        let stream = TcpStream::connect_timeout(&address, connect_remaining(deadline)?)?;
        stream.set_nodelay(true)?;
        stream.set_nonblocking(true)?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_WIRE_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_WIRE_MESSAGE_BYTES));
        let url = daemon_url(&address.to_string())?;
        let mut handshake = tungstenite::client::client_with_config(
            url.as_str(),
            MaybeTlsStream::Plain(stream),
            Some(config),
        );
        let mut socket = loop {
            connect_remaining(deadline)?;
            match handshake {
                Ok((socket, _)) => break socket,
                Err(tungstenite::HandshakeError::Interrupted(pending)) => {
                    connect_retry(deadline)?;
                    handshake = pending.handshake();
                }
                Err(tungstenite::HandshakeError::Failure(error)) => return Err(error.into()),
            }
        };
        if let Err(error) = write_json(
            &mut socket,
            &ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                token,
                client_id: Uuid::new_v4(),
                resume_from: Vec::new(),
                diagnostic_from: None,
            },
        ) {
            if !retryable_error(error.as_ref()) {
                return Err(error);
            }
            // send() already queued Hello; flush it without sending it twice.
            loop {
                connect_retry(deadline)?;
                match socket.flush() {
                    Ok(()) => break,
                    Err(error) if retryable_error(&error) => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        let hello = loop {
            connect_remaining(deadline)?;
            match socket.read() {
                Ok(Message::Text(text)) => break serde_json::from_str(text.as_ref())?,
                Ok(Message::Close(_)) => bail!("Waku daemon closed during handshake"),
                Ok(_) => {}
                Err(error) if retryable_error(&error) => connect_retry(deadline)?,
                Err(error) => return Err(error.into()),
            }
        };
        validate_hello(hello)?;
        connect_remaining(deadline)?;
        if let MaybeTlsStream::Plain(stream) = socket.get_mut() {
            stream.set_nonblocking(false)?;
        }
        Self::from_authenticated_socket(socket, HashMap::new(), None)
    }

    /// Connect and resume both sequenced streams: the runtime events keyed by
    /// task/runtime, and the instance's diagnostic stream. A `None`
    /// diagnostic cursor asks for the retained tail, which is what a client
    /// that has never read the stream wants.
    pub fn connect_with_cursors(
        address: &str,
        token: String,
        resume_from: Vec<ReplayCursor>,
        diagnostic_from: Option<DiagnosticCursor>,
    ) -> anyhow::Result<Self> {
        let last_sequences = resume_from
            .iter()
            .map(|cursor| {
                (
                    (cursor.session_id, cursor.runtime_id),
                    LastSequence {
                        epoch: cursor.epoch,
                        sequence: cursor.sequence,
                    },
                )
            })
            .collect();
        let url = daemon_url(address)?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_WIRE_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_WIRE_MESSAGE_BYTES));
        let (mut socket, _) =
            tungstenite::client::connect_with_config(url.as_str(), Some(config), 3)
                .context("could not connect to Waku daemon")?;
        set_client_read_timeout(&mut socket, Some(Duration::from_secs(5)))?;
        write_json(
            &mut socket,
            &ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                token,
                client_id: Uuid::new_v4(),
                resume_from,
                diagnostic_from,
            },
        )?;
        let hello = read_server_message(&mut socket)?;
        validate_hello(hello)?;
        Self::from_authenticated_socket(socket, last_sequences, diagnostic_from)
    }

    fn from_authenticated_socket(
        mut socket: WebSocket<MaybeTlsStream<TcpStream>>,
        last_sequences: HashMap<(Uuid, Uuid), LastSequence>,
        diagnostic_from: Option<DiagnosticCursor>,
    ) -> anyhow::Result<Self> {
        set_client_read_timeout(&mut socket, Some(READ_POLL_INTERVAL))?;

        let (outgoing, outgoing_rx) = unbounded();
        let inner = Arc::new(ClientInner {
            outgoing,
            automation_outgoing: AutomationBackpressure::new(),
            pending: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            pending_events: Mutex::new(HashMap::new()),
            task_state_subscribers: Mutex::new(Vec::new()),
            diagnostic_subscribers: Mutex::new(Vec::new()),
            pending_diagnostics: Mutex::new(VecDeque::new()),
            last_sequences: Mutex::new(last_sequences),
            last_diagnostic: Mutex::new(diagnostic_from),
            automation: Mutex::new(AutomationState::default()),
            automation_executor: Mutex::new(None),
            disconnected: AtomicBool::new(false),
            #[cfg(debug_assertions)]
            diagnostic_verbosity: Mutex::new(DiagnosticVerbosity::default()),
            #[cfg(debug_assertions)]
            request_round_trip_us: AtomicU64::new(0),
        });
        let thread_inner = inner.clone();
        std::thread::Builder::new()
            .name("waku-daemon-client".into())
            .spawn(move || run_client(socket, outgoing_rx, thread_inner))
            .context("could not start Waku daemon client thread")?;
        Ok(Self { inner })
    }

    pub fn subscribe(&self, session_id: Uuid, runtime_id: Uuid) -> Receiver<SequencedEvent> {
        let (events, receiver) = unbounded();
        let key = (session_id, runtime_id);
        let mut sessions = self.inner.sessions.lock();
        sessions.insert(key, events.clone());
        // Keep the subscription lock while draining the pre-subscription
        // replay queue. The socket thread takes these locks in the same order,
        // so a new live event cannot overtake older replayed events here.
        if let Some(buffered) = self.inner.pending_events.lock().remove(&key) {
            for event in buffered {
                let _ = events.send(event);
            }
        }
        receiver
    }

    /// Whether two handles send through the same WebSocket connection.
    ///
    /// The daemon supervisor publishes replacement clients after a managed
    /// restart. Runtime adapters use this identity check to ignore the
    /// subscription's initial snapshot and wait for an actual replacement.
    pub fn same_connection(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub fn is_disconnected(&self) -> bool {
        self.inner.disconnected.load(Ordering::Acquire)
    }

    pub fn unsubscribe(&self, session_id: Uuid, runtime_id: Uuid) {
        self.inner.sessions.lock().remove(&(session_id, runtime_id));
    }

    pub fn subscribe_task_state(&self) -> Receiver<u64> {
        let (events, receiver) = unbounded();
        self.inner.task_state_subscribers.lock().push(events);
        receiver
    }

    /// Read the instance's diagnostic stream from the next record onward.
    ///
    /// A fresh client asks for the retained tail in the handshake; a client
    /// that reconnects passes [`Self::last_diagnostic_cursor`] into
    /// [`Self::connect_with_cursors`] instead. Records that arrive before the
    /// first subscriber are buffered, so a subscription sees the whole
    /// handshake replay.
    pub fn subscribe_diagnostics(&self) -> Receiver<DiagnosticMessage> {
        let (events, receiver) = unbounded();
        // Keep the subscriber lock while draining: the socket thread locks
        // the same mutex before delivering, so no live record can overtake
        // the buffered ones.
        let mut subscribers = self.inner.diagnostic_subscribers.lock();
        let pending = std::mem::take(&mut *self.inner.pending_diagnostics.lock());
        for message in pending {
            let _ = events.send(message);
        }
        subscribers.push(events);
        receiver
    }

    /// The last diagnostic record this client observed. A reconnect passes
    /// this cursor so the daemon resumes exactly where delivery stopped.
    pub fn last_diagnostic_cursor(&self) -> Option<DiagnosticCursor> {
        *self.inner.last_diagnostic.lock()
    }

    /// Send one structured record to the daemon, which sequences it into the
    /// instance's stream. Fire-and-forget: the daemon does not answer.
    pub fn record_diagnostic(&self, record: DiagnosticRecord) -> anyhow::Result<()> {
        if self.inner.disconnected.load(Ordering::Acquire) {
            bail!("Waku daemon is disconnected");
        }
        self.inner
            .outgoing
            .send(Outgoing::Message(ClientMessage::Diagnostic(record)))
            .map_err(|_| anyhow!("Waku daemon connection is closed"))
    }

    /// Read the daemon's current recording policy without an RPC. It is
    /// refreshed on every connection and runtime policy update. Optional
    /// detail is skipped on contention rather than blocking the UI pump.
    pub fn diagnostic_enabled(
        &self,
        level: DiagnosticLevel,
        origin: &DiagnosticOrigin,
        source: &str,
    ) -> bool {
        #[cfg(debug_assertions)]
        {
            self.inner
                .diagnostic_verbosity
                .try_lock()
                .is_some_and(|policy| policy.enabled(level, origin, source))
        }
        #[cfg(not(debug_assertions))]
        {
            let _ = (level, origin, source);
            false
        }
    }

    /// Transport round-trip of the latest completed request, including socket
    /// scheduling and the response. Zero means no request has completed yet.
    pub fn last_request_round_trip_ms(&self) -> f64 {
        #[cfg(debug_assertions)]
        {
            self.inner.request_round_trip_us.load(Ordering::Relaxed) as f64 / 1000.0
        }
        #[cfg(not(debug_assertions))]
        {
            0.0
        }
    }

    /// Queue an absolute sample without waiting on the daemon or its writer.
    pub fn report_diagnostic_counters(
        &self,
        report: DiagnosticCounterReport,
    ) -> anyhow::Result<()> {
        if !cfg!(debug_assertions) {
            return Ok(());
        }
        if self.is_disconnected() {
            bail!("Waku daemon is disconnected");
        }
        self.inner
            .outgoing
            .send(Outgoing::Message(ClientMessage::DiagnosticCounters(report)))
            .map_err(|_| anyhow!("Waku daemon connection is closed"))
    }

    pub fn diagnostic_logs(&self, query: DiagnosticQuery) -> anyhow::Result<DiagnosticLogs> {
        match self.request(Uuid::nil(), Uuid::nil(), Command::DiagnosticLogs { query })? {
            ResponsePayload::DiagnosticLogs { result } => Ok(result),
            other => bail!("unexpected diagnostic logs response: {other:?}"),
        }
    }

    pub fn diagnostic_timeline(
        &self,
        query: DiagnosticTimelineQuery,
    ) -> anyhow::Result<DiagnosticTimeline> {
        match self.request(
            Uuid::nil(),
            Uuid::nil(),
            Command::DiagnosticTimeline { query },
        )? {
            ResponsePayload::DiagnosticTimeline { result } => Ok(result),
            other => bail!("unexpected diagnostic timeline response: {other:?}"),
        }
    }

    pub fn diagnostic_counters(&self) -> anyhow::Result<DiagnosticCounters> {
        match self.request(Uuid::nil(), Uuid::nil(), Command::DiagnosticCounters)? {
            ResponsePayload::DiagnosticCounters { result } => Ok(result),
            other => bail!("unexpected diagnostic counters response: {other:?}"),
        }
    }

    pub fn diagnostic_verbosity(
        &self,
        update: Option<DiagnosticVerbosity>,
    ) -> anyhow::Result<DiagnosticVerbosity> {
        match self.request(
            Uuid::nil(),
            Uuid::nil(),
            Command::DiagnosticVerbosity { update },
        )? {
            // Only ordered socket broadcasts update the local filter. A
            // concurrent later update may already have arrived by the time
            // this waiting caller resumes; copying this response back would
            // overwrite that newer policy.
            ResponsePayload::DiagnosticVerbosity { verbosity } => Ok(verbosity),
            other => bail!("unexpected diagnostic verbosity response: {other:?}"),
        }
    }

    /// Execute a daemon-owned automation operation. A returned failure keeps
    /// its full wire shape in [`DaemonAutomationError`], downcastable from anyhow.
    pub fn automation(&self, command: AutomationCommand) -> anyhow::Result<AutomationResult> {
        self.automation_with_timeout(command, REQUEST_TIMEOUT)
    }

    /// Use a caller deadline without bypassing replay ordering or session cleanup.
    pub fn automation_with_timeout(
        &self,
        command: AutomationCommand,
        timeout: Duration,
    ) -> anyhow::Result<AutomationResult> {
        self.automation_with_subscription(command, timeout, None)
    }

    fn automation_with_subscription(
        &self,
        command: AutomationCommand,
        timeout: Duration,
        subscriber: Option<AutomationSubscriber>,
    ) -> anyhow::Result<AutomationResult> {
        let resuming = match &command {
            AutomationCommand::Resume { session_id, .. } => {
                let mut state = self.inner.automation.lock();
                if !state.resuming.insert(*session_id) {
                    bail!("automation session {session_id} is already resuming");
                }
                if let Some(subscriber) = subscriber {
                    state.resume_subscribers.insert(*session_id, subscriber);
                }
                Some(*session_id)
            }
            _ => None,
        };
        let detaching = match &command {
            AutomationCommand::Detach { session_id } => Some(*session_id),
            _ => None,
        };
        let response = self.request_with_timeout(
            Uuid::nil(),
            Uuid::nil(),
            Command::Automation { command },
            timeout,
        );
        if let Some(session_id) = resuming {
            self.inner.automation.lock().finish_resume(session_id);
        }
        match response? {
            ResponsePayload::Automation {
                result: AutomationResult::Error { error },
            } => Err(anyhow!(DaemonAutomationError { error })),
            ResponsePayload::Automation { result } => {
                if let Some(session_id) = detaching {
                    let mut state = self.inner.automation.lock();
                    state.cursors.remove(&session_id);
                    state.pending.remove(&session_id);
                    state.subscribers.remove(&session_id);
                }
                Ok(result)
            }
            other => bail!("unexpected automation response: {other:?}"),
        }
    }

    pub fn automation_instances(&self) -> anyhow::Result<Vec<AutomationInstance>> {
        match self.automation(AutomationCommand::Instances)? {
            AutomationResult::Instances { instances } => Ok(instances),
            other => bail!("unexpected automation instances response: {other:?}"),
        }
    }

    pub fn attach_automation(&self, instance_id: Uuid) -> anyhow::Result<AutomationSession> {
        match self.automation(AutomationCommand::Attach { instance_id })? {
            AutomationResult::Attached { session, .. } => Ok(session),
            other => bail!("unexpected automation attach response: {other:?}"),
        }
    }

    pub fn resume_automation(
        &self,
        session_id: Uuid,
        after: Option<AutomationCursor>,
    ) -> anyhow::Result<AutomationSession> {
        match self.automation(AutomationCommand::Resume { session_id, after })? {
            AutomationResult::Attached { session, .. } => Ok(session),
            other => bail!("unexpected automation resume response: {other:?}"),
        }
    }

    /// Resume into a fresh receiver without first copying stale pending events
    /// into it. Replay and racing live messages are committed before return;
    /// the existing subscription remains usable if the request fails.
    pub fn resume_automation_with_subscription(
        &self,
        session_id: Uuid,
        after: Option<AutomationCursor>,
        timeout: Duration,
    ) -> anyhow::Result<(AutomationResult, Receiver<AutomationMessage>)> {
        let (subscriber, receiver) = AutomationSubscriber::new(MAX_BUFFERED_AUTOMATION_REPLAY);
        let result = self.automation_with_subscription(
            AutomationCommand::Resume { session_id, after },
            timeout,
            Some(subscriber),
        )?;
        match result {
            AutomationResult::Attached { .. } => Ok((result, receiver)),
            other => bail!("unexpected automation resume response: {other:?}"),
        }
    }

    pub fn automation_call(
        &self,
        session_id: Uuid,
        call: AutomationCall,
    ) -> anyhow::Result<AutomationCallResult> {
        match self.automation(AutomationCommand::Call { session_id, call })? {
            AutomationResult::Called { result } => Ok(result),
            other => bail!("unexpected automation call response: {other:?}"),
        }
    }

    /// Local delivery subscription; use `AutomationCommand::Subscribe` to pick
    /// which streams the daemon pushes. Wait and run notifications use it too.
    pub fn subscribe_automation(&self, session_id: Uuid) -> Receiver<AutomationMessage> {
        let mut state = self.inner.automation.lock();
        let pending = state.pending.remove(&session_id).unwrap_or_default();
        let (mut subscriber, receiver) = AutomationSubscriber::new(pending.limit);
        for message in pending.messages {
            subscriber.deliver(message);
        }
        state
            .subscribers
            .entry(session_id)
            .or_default()
            .push(subscriber);
        receiver
    }

    pub fn last_automation_cursor(&self, session_id: Uuid) -> Option<AutomationCursor> {
        self.inner
            .automation
            .lock()
            .cursors
            .get(&session_id)
            .copied()
    }

    pub fn subscribe_automation_executor(&self) -> Receiver<AutomationExecutorMessage> {
        let (sender, receiver) = bounded(MAX_AUTOMATION_EVENTS);
        *self.inner.automation_executor.lock() = Some(sender);
        receiver
    }

    pub fn register_automation_executor(
        &self,
        instance: AutomationInstance,
    ) -> anyhow::Result<AutomationInstance> {
        match self.automation(AutomationCommand::RegisterExecutor { instance })? {
            AutomationResult::Registered { instance } => Ok(instance),
            other => bail!("unexpected automation registration response: {other:?}"),
        }
    }

    /// May wait for bounded transport capacity. Call from the executor's
    /// background writer, never while holding a UI/entity lease.
    pub fn reply_automation_executor(&self, reply: AutomationExecutorReply) -> anyhow::Result<()> {
        self.send_automation_message(ClientMessage::AutomationExecutorReply { reply })
    }

    /// May wait for bounded transport capacity; use a background writer.
    pub fn publish_automation_event(&self, event: AutomationExecutorEvent) -> anyhow::Result<()> {
        self.send_automation_message(ClientMessage::AutomationExecutorEvent { event })
    }

    fn send_automation_message(&self, message: ClientMessage) -> anyhow::Result<()> {
        if self.is_disconnected() {
            bail!("Waku daemon is disconnected");
        }
        let permit = self.inner.automation_outgoing.acquire(REQUEST_TIMEOUT)?;
        if self.is_disconnected() {
            bail!("Waku daemon is disconnected");
        }
        self.inner
            .outgoing
            .send(Outgoing::Automation(message, permit))
            .map_err(|_| anyhow!("Waku daemon connection is closed"))
    }

    pub fn request(
        &self,
        session_id: Uuid,
        runtime_id: Uuid,
        command: Command,
    ) -> anyhow::Result<ResponsePayload> {
        self.request_with_timeout(session_id, runtime_id, command, REQUEST_TIMEOUT)
    }

    /// Override the ordinary RPC deadline without spawning a waiting thread.
    /// Timing out removes its pending response slot; it does not replay or
    /// cancel a command whose effect may already have reached the executor.
    pub fn request_with_timeout(
        &self,
        session_id: Uuid,
        runtime_id: Uuid,
        command: Command,
        timeout: Duration,
    ) -> anyhow::Result<ResponsePayload> {
        if self.inner.disconnected.load(Ordering::Acquire) {
            bail!("Waku daemon is disconnected");
        }
        #[cfg(debug_assertions)]
        let started_at = std::time::Instant::now();
        let request_id = Uuid::new_v4();
        let (response, response_rx) = bounded(1);
        self.inner.pending.lock().insert(request_id, response);
        let message = ClientMessage::Request(Request {
            request_id,
            session_id,
            runtime_id,
            command,
        });
        if self
            .inner
            .outgoing
            .send(Outgoing::Message(message))
            .is_err()
        {
            self.inner.pending.lock().remove(&request_id);
            bail!("Waku daemon connection is closed");
        }
        let result = match response_rx.recv_timeout(timeout) {
            Ok(Ok(payload)) => Ok(payload),
            Ok(Err(error)) => Err(anyhow!(DaemonRpcError { error })),
            Err(error) => {
                self.inner.pending.lock().remove(&request_id);
                Err(anyhow!("timed out waiting for Waku daemon: {error}"))
            }
        };
        #[cfg(debug_assertions)]
        self.inner.request_round_trip_us.store(
            started_at.elapsed().as_micros().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        result
    }

    pub fn notify(
        &self,
        session_id: Uuid,
        runtime_id: Uuid,
        command: Command,
    ) -> anyhow::Result<()> {
        if self.inner.disconnected.load(Ordering::Acquire) {
            bail!("Waku daemon is disconnected");
        }
        self.inner
            .outgoing
            .send(Outgoing::Message(ClientMessage::Request(Request {
                // The nil request id is reserved for fire-and-forget controls;
                // the daemon executes them in the runtime mailbox but does
                // not allocate or send a response.
                request_id: Uuid::nil(),
                session_id,
                runtime_id,
                command,
            })))
            .map_err(|_| anyhow!("Waku daemon connection is closed"))
    }

    pub fn last_sequences(&self) -> Vec<ReplayCursor> {
        self.inner
            .last_sequences
            .lock()
            .iter()
            .map(|(&(session_id, runtime_id), cursor)| ReplayCursor {
                session_id,
                runtime_id,
                epoch: cursor.epoch,
                sequence: cursor.sequence,
            })
            .collect()
    }

    /// Close only this client transport. The daemon and its other clients keep
    /// running; unlike `shutdown`, this sends no protocol shutdown message.
    pub fn disconnect(&self) {
        self.inner.disconnected.store(true, Ordering::Release);
        let _ = self.inner.outgoing.send(Outgoing::Disconnect);
    }

    pub fn shutdown(&self) {
        let _ = self.inner.outgoing.send(Outgoing::Shutdown);
    }
}

fn connect_remaining(deadline: Instant) -> anyhow::Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        bail!("Waku daemon connection deadline elapsed");
    }
    Ok(remaining)
}

fn connect_retry(deadline: Instant) -> anyhow::Result<()> {
    std::thread::sleep(connect_remaining(deadline)?.min(Duration::from_millis(5)));
    connect_remaining(deadline).map(|_| ())
}

fn validate_hello(hello: ServerMessage) -> anyhow::Result<()> {
    match hello {
        ServerMessage::Hello {
            protocol_version, ..
        } if protocol_version == PROTOCOL_VERSION => Ok(()),
        ServerMessage::Hello {
            protocol_version, ..
        } => bail!(
            "daemon protocol {protocol_version} does not match desktop protocol {PROTOCOL_VERSION}"
        ),
        ServerMessage::Rejected { message } => bail!("daemon rejected connection: {message}"),
        other => bail!("daemon sent an invalid handshake response: {other:?}"),
    }
}

fn daemon_url(address: &str) -> anyhow::Result<String> {
    let normalized = if address.starts_with("ws://") || address.starts_with("wss://") {
        address.to_owned()
    } else {
        format!("ws://{address}")
    };
    let mut url = url::Url::parse(&normalized).context("Waku daemon address is invalid")?;
    url.set_path("/v1");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.into())
}

fn run_client(
    mut socket: WebSocket<MaybeTlsStream<TcpStream>>,
    outgoing: Receiver<Outgoing>,
    inner: Arc<ClientInner>,
) {
    'connection: loop {
        while let Ok(message) = outgoing.try_recv() {
            match message {
                Outgoing::Message(message) => {
                    if write_json(&mut socket, &message).is_err() {
                        break 'connection;
                    }
                }
                Outgoing::Automation(message, _permit) => {
                    if write_json(&mut socket, &message).is_err() {
                        break 'connection;
                    }
                }
                Outgoing::Disconnect => {
                    let _ = socket.close(None);
                    break 'connection;
                }
                Outgoing::Shutdown => {
                    let _ = write_json(&mut socket, &ClientMessage::Shutdown);
                    let _ = socket.flush();
                    break 'connection;
                }
            }
        }

        match socket.read() {
            Ok(Message::Text(text)) => {
                let Ok(message) = serde_json::from_str::<ServerMessage>(text.as_ref()) else {
                    continue;
                };
                match message {
                    ServerMessage::AutomationExecutorRequest { request } => {
                        let request_id = request.request_id;
                        let sent =
                            inner
                                .automation_executor
                                .lock()
                                .as_ref()
                                .is_some_and(|sender| {
                                    sender
                                        .try_send(AutomationExecutorMessage::Request(request))
                                        .is_ok()
                                });
                        if !sent {
                            let reply = AutomationExecutorReply {
                                request_id,
                                outcome: AutomationExecutorOutcome::Error {
                                    error: AutomationError::new(
                                        "executor_unavailable",
                                        "automation executor is unavailable or its work queue is full",
                                    ),
                                },
                            };
                            let _ = inner.outgoing.send(Outgoing::Message(
                                ClientMessage::AutomationExecutorReply { reply },
                            ));
                        }
                    }
                    ServerMessage::AutomationInterest {
                        instance_id,
                        attached,
                        streams,
                        model_sections,
                    } => {
                        if let Some(sender) = inner.automation_executor.lock().as_ref() {
                            if sender
                                .try_send(AutomationExecutorMessage::Interest {
                                    instance_id,
                                    attached,
                                    streams,
                                    model_sections,
                                })
                                .is_err()
                            {
                                // Interest cannot be dropped: that could leave a
                                // detached executor sending frames indefinitely.
                                break 'connection;
                            }
                        }
                    }
                    ServerMessage::AutomationEvent { event } => {
                        inner.automation.lock().event(event)
                    }
                    ServerMessage::AutomationWaitResolved { session_id, wait } => inner
                        .automation
                        .lock()
                        .notification(session_id, AutomationMessage::WaitResolved(wait)),
                    ServerMessage::AutomationRunChanged { session_id, run } => inner
                        .automation
                        .lock()
                        .notification(session_id, AutomationMessage::RunChanged(run)),
                    ServerMessage::Response {
                        request_id,
                        outcome,
                    } => {
                        // A timed-out request no longer owns delivery state.
                        // Ignore its late response before touching replay queues
                        // or a receiver staged by a newer resume of that session.
                        let Some(pending) = inner.pending.lock().remove(&request_id) else {
                            continue;
                        };
                        // Commit replay on the reader before waking the caller.
                        // A following close cannot disconnect its receiver first.
                        if let ResponseOutcome::Ok {
                            payload:
                                ResponsePayload::Automation {
                                    result: AutomationResult::Attached { session, replay },
                                },
                        } = &outcome
                        {
                            let mut state = inner.automation.lock();
                            let notifications = if state.resuming.remove(&session.session_id) {
                                state.prepare_replay(session.session_id)
                            } else {
                                VecDeque::new()
                            };
                            if let Some(expiry) = &replay.expiry {
                                state.deliver(
                                    session.session_id,
                                    AutomationMessage::Expired(expiry.clone()),
                                );
                            }
                            // Replay follows the caller's consumption cursor, not
                            // the receive cursor: a local buffer may have dropped
                            // events that this connection has already received.
                            for event in &replay.events {
                                state.deliver_event(event.clone());
                            }
                            let cursor = state
                                .cursors
                                .entry(session.session_id)
                                .or_insert(replay.cursor);
                            if cursor.epoch != replay.cursor.epoch
                                || cursor.sequence < replay.cursor.sequence
                            {
                                *cursor = replay.cursor;
                            }
                            for notification in notifications {
                                state.deliver(session.session_id, notification);
                            }
                            state.finish_resume(session.session_id);
                        }
                        let result = match outcome {
                            ResponseOutcome::Ok { payload } => Ok(payload),
                            ResponseOutcome::Error { error } => Err(error),
                        };
                        let _ = pending.send(result);
                    }
                    ServerMessage::Event(event) => {
                        let should_deliver = {
                            let mut sequences = inner.last_sequences.lock();
                            let previous = sequences
                                .entry((event.session_id, event.runtime_id))
                                .or_insert(LastSequence {
                                    epoch: event.epoch,
                                    sequence: 0,
                                });
                            if previous.epoch == event.epoch && event.sequence <= previous.sequence
                            {
                                false
                            } else {
                                previous.epoch = event.epoch;
                                previous.sequence = event.sequence;
                                true
                            }
                        };
                        if should_deliver {
                            let key = (event.session_id, event.runtime_id);
                            let sessions = inner.sessions.lock();
                            if let Some(events) = sessions.get(&key) {
                                let _ = events.send(event);
                            } else {
                                let mut pending = inner.pending_events.lock();
                                let buffered = pending.entry(key).or_default();
                                buffered.push_back(event);
                                while buffered.len() > MAX_BUFFERED_EVENTS_PER_RUNTIME {
                                    buffered.pop_front();
                                }
                            }
                        }
                    }
                    ServerMessage::TaskStateChanged { revision } => {
                        inner
                            .task_state_subscribers
                            .lock()
                            .retain(|subscriber| subscriber.send(revision).is_ok());
                    }
                    ServerMessage::Diagnostic(event) => {
                        let fresh = {
                            let mut last = inner.last_diagnostic.lock();
                            let stale = last.is_some_and(|cursor| {
                                cursor.epoch == event.epoch && event.sequence <= cursor.sequence
                            });
                            if !stale {
                                *last = Some(DiagnosticCursor {
                                    epoch: event.epoch,
                                    sequence: event.sequence,
                                });
                            }
                            !stale
                        };
                        if fresh {
                            deliver_diagnostic(&inner, DiagnosticMessage::Event(event));
                        }
                    }
                    ServerMessage::DiagnosticVerbosity { verbosity } => {
                        #[cfg(debug_assertions)]
                        {
                            *inner.diagnostic_verbosity.lock() = verbosity;
                        }
                        #[cfg(not(debug_assertions))]
                        let _ = verbosity;
                    }
                    ServerMessage::DiagnosticCursorExpired { expiry } => {
                        deliver_diagnostic(&inner, DiagnosticMessage::Expired(expiry));
                    }
                    ServerMessage::ShuttingDown => break,
                    ServerMessage::Hello { .. } | ServerMessage::Rejected { .. } => {}
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(_)) => {
                let _ = socket.flush();
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(error)) if retryable_io(&error) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => break,
            Err(_) => break,
        }
    }

    inner.disconnected.store(true, Ordering::Release);
    let pending = std::mem::take(&mut *inner.pending.lock());
    for (_, response) in pending {
        let _ = response.send(Err(RpcError {
            message: "Waku daemon disconnected".into(),
        }));
    }
    // Closing the desktop transport is not evidence that a daemon-owned
    // provider exited. Drop the subscription senders so runtime adapters can
    // hand off to a replacement client and ask the daemon whether the same
    // runtime still exists. Real provider exits arrive through the replayable
    // `processExited` event emitted by the daemon.
    drop(std::mem::take(&mut *inner.sessions.lock()));
    inner.task_state_subscribers.lock().clear();
    inner.diagnostic_subscribers.lock().clear();
    inner.automation.lock().subscribers.clear();
    inner.automation_executor.lock().take();
}

fn deliver_diagnostic(inner: &ClientInner, message: DiagnosticMessage) {
    let mut subscribers = inner.diagnostic_subscribers.lock();
    if subscribers.is_empty() {
        let mut pending = inner.pending_diagnostics.lock();
        pending.push_back(message);
        // The daemon's own retention bound, so a handshake replay is never
        // truncated by a narrower client buffer.
        while pending.len() > waku_protocol::MAX_DIAGNOSTIC_EVENTS {
            pending.pop_front();
        }
        return;
    }
    subscribers.retain(|subscriber| subscriber.send(message.clone()).is_ok());
}

fn set_client_read_timeout(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    timeout: Option<Duration>,
) -> io::Result<()> {
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => stream.set_read_timeout(timeout),
        MaybeTlsStream::Rustls(stream) => stream.sock.set_read_timeout(timeout),
        #[allow(unreachable_patterns)]
        _ => Ok(()),
    }
}

fn retryable_io(error: &io::Error) -> bool {
    retryable_error(error)
}

fn retryable_error(error: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(error) = error.downcast_ref::<io::Error>() {
        if matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
        ) {
            return true;
        }
        #[cfg(unix)]
        if error.raw_os_error() == Some(libc::EAGAIN)
            || error.raw_os_error() == Some(libc::EWOULDBLOCK)
        {
            return true;
        }
    }
    error.source().is_some_and(retryable_error)
}

fn write_json<S: io::Read + io::Write, T: serde::Serialize>(
    socket: &mut WebSocket<S>,
    value: &T,
) -> anyhow::Result<()> {
    let payload = serde_json::to_string(value)?;
    socket.send(Message::Text(payload.into()))?;
    Ok(())
}

fn read_server_message(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
) -> anyhow::Result<ServerMessage> {
    loop {
        match socket.read()? {
            Message::Text(text) => return Ok(serde_json::from_str(text.as_ref())?),
            Message::Ping(_) => socket.flush()?,
            Message::Close(_) => bail!("Waku daemon closed during handshake"),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(debug_assertions)]
    fn diagnostic_policy_check_skips_contended_optional_detail() {
        let client = DaemonClient {
            inner: Arc::new(ClientInner {
                outgoing: unbounded().0,
                automation_outgoing: AutomationBackpressure::new(),
                pending: Mutex::new(HashMap::new()),
                sessions: Mutex::new(HashMap::new()),
                pending_events: Mutex::new(HashMap::new()),
                task_state_subscribers: Mutex::new(Vec::new()),
                diagnostic_subscribers: Mutex::new(Vec::new()),
                pending_diagnostics: Mutex::new(VecDeque::new()),
                last_sequences: Mutex::new(HashMap::new()),
                last_diagnostic: Mutex::new(None),
                automation: Mutex::new(AutomationState::default()),
                automation_executor: Mutex::new(None),
                disconnected: AtomicBool::new(false),
                diagnostic_verbosity: Mutex::new(DiagnosticVerbosity::default()),
                request_round_trip_us: AtomicU64::new(0),
            }),
        };
        let origin = DiagnosticOrigin::App {
            pid: 1,
            version: "test".into(),
        };
        assert!(client.diagnostic_enabled(DiagnosticLevel::Info, &origin, "pump"));
        let policy = client.inner.diagnostic_verbosity.lock();
        assert!(!client.diagnostic_enabled(DiagnosticLevel::Info, &origin, "pump"));
        drop(policy);
        assert!(client.diagnostic_enabled(DiagnosticLevel::Info, &origin, "pump"));
    }

    #[test]
    fn daemon_endpoint_accepts_addresses_and_secure_urls() {
        assert_eq!(
            daemon_url("127.0.0.1:4312").unwrap(),
            "ws://127.0.0.1:4312/v1"
        );
        assert_eq!(
            daemon_url("wss://waku.example.test/old?ignored=1").unwrap(),
            "wss://waku.example.test/v1"
        );
    }
}

#[cfg(test)]
#[path = "automation_tests.rs"]
mod automation_tests;
