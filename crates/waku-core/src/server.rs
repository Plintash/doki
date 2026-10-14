use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::{Context as _, bail};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use parking_lot::Mutex;
use subtle::ConstantTimeEq as _;
use tungstenite::handshake::server::{
    ErrorResponse, Request as HandshakeRequest, Response as HandshakeResponse,
};
use tungstenite::http::{StatusCode, header::ORIGIN};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket, accept_hdr_with_config};
use uuid::Uuid;

#[path = "automation.rs"]
mod automation;

use crate::diagnostics::DiagnosticLog;
use crate::model::{AgentSession, Project, ProviderKind, SessionStatus, SessionWorkspace, Task};
use crate::protocol::MAX_WIRE_MESSAGE_BYTES;
use crate::protocol::{
    ClientMessage, Command, PROTOCOL_VERSION, ReplayCursor, Request, ResponseOutcome,
    ResponsePayload, RpcError, SequencedEvent, ServerMessage, WireDriverEvent,
};
use crate::protocol::{
    DiagnosticCounterReport, DiagnosticCounters, DiagnosticExpiry, DiagnosticLevel,
    DiagnosticOrigin, DiagnosticVerbosity,
};
use crate::protocol::{DiagnosticCursor, DiagnosticEvent, DiagnosticRecord, MAX_DIAGNOSTIC_EVENTS};

const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(25);
/// A socket write that makes no progress for this long means the client is
/// not reading. Keeping the connection would let its event queue grow without
/// bound, so it is dropped; clients reconnect and resume from their cursors.
const SOCKET_WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HANDSHAKE_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_CONNECTIONS: usize = 64;
const MAX_REPLAY_EVENTS_PER_SESSION: usize = 2048;
/// Per-subscriber cap on queued daemon messages. A subscriber that falls this
/// far behind live events is dropped instead of buffered for, so a stalled
/// client cannot make the daemon's heap grow at the stream rate forever.
/// (Max 8 replaying runtimes at `MAX_REPLAY_EVENTS_PER_SESSION` fits; an
/// overflow during replay or live emission closes the connection and the
/// client reconnects from its last replayed cursor.)
const MAX_QUEUED_MESSAGES_PER_SUBSCRIBER: usize = 16384;
const MAX_CACHED_RESPONSES: usize = 2048;
/// Responses are cached by request id so a client that missed a reply can
/// fetch it after reconnecting. Caching is bounded by bytes as well as count:
/// outcomes such as a hydrated session can be megabytes, and a count-only cap
/// would let a handful of them pin hundreds of megabytes in the daemon.
const MAX_CACHED_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const NATIVE_CLIENT_HEADER: &str = "x-waku-client";
const NATIVE_CLIENT_HEADER_VALUE: &str = "native";

#[derive(Clone, Debug, Default)]
pub struct ServerOptions {
    /// Browser WebSocket handshakes carry an Origin header. Most native clients
    /// do not; React Native does and identifies itself with `x-waku-client`.
    /// An empty set therefore still permits native clients only.
    pub allowed_origins: HashSet<String>,
    /// Only a daemon owned by the desktop process should accept the global
    /// shutdown control message. Service-managed daemons keep running when an
    /// authenticated client disconnects.
    pub allow_shutdown: bool,
    /// The instance data directory whose diagnostic files the daemon writes.
    /// `None` is a release build (or a test): the stream still exists in
    /// memory, but no records reach disk.
    pub diagnostics_directory: Option<PathBuf>,
}

struct ConnectionPermit(Arc<AtomicUsize>);

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub trait Backend: Send + Sync + 'static {
    fn handle(&self, request: Request, events: EventSink) -> anyhow::Result<ResponsePayload>;

    fn handle_automation_scratch(
        &self,
        _command: &waku_protocol::AutomationCommand,
    ) -> anyhow::Result<waku_protocol::AutomationResult> {
        bail!("scratch instances are unavailable on this backend")
    }

    fn handle_automation_jobs(
        &self,
        _command: &waku_protocol::AutomationCommand,
    ) -> waku_protocol::AutomationResult {
        waku_protocol::AutomationResult::Error {
            error: waku_protocol::AutomationError::new(
                "unavailable",
                "jobs are unavailable on this backend",
            ),
        }
    }

    fn forward_automation(
        &self,
        _command: &waku_protocol::AutomationCommand,
        _outgoing: Sender<ServerMessage>,
        _subscriber_id: u64,
    ) -> Option<anyhow::Result<waku_protocol::AutomationResult>> {
        None
    }

    fn automation_subscriber_disconnected(&self, _subscriber_id: u64) {}

    fn shutdown(&self) {}
}

#[derive(Clone)]
pub struct EventSink {
    session_id: Uuid,
    runtime_id: Uuid,
    hub: Arc<Hub>,
}

impl EventSink {
    /// A daemon-originated task mutation must refresh the submitting client
    /// as well as observers; no client owns this change optimistically.
    pub(crate) fn task_state_changed(&self) {
        self.hub.task_state_changed(u64::MAX);
    }

    pub fn send(&self, event: WireDriverEvent) -> anyhow::Result<()> {
        self.hub.emit(self.session_id, self.runtime_id, event, true);
        Ok(())
    }

    /// Broadcast a live-only event without retaining it in the replay journal.
    /// High-volume PTY output is meaningful only to a terminal emulator that
    /// is currently attached; replaying raw chunks into a fresh emulator would
    /// also retain an unbounded terminal transcript in daemon memory.
    pub fn send_ephemeral(&self, event: WireDriverEvent) -> anyhow::Result<()> {
        self.hub
            .emit(self.session_id, self.runtime_id, event, false);
        Ok(())
    }

    /// Build and place one structured record in the instance's diagnostic
    /// stream. The build runs only in debug builds, so a release daemon does
    /// no diagnostic work.
    pub fn record(&self, build: impl FnOnce() -> Option<DiagnosticRecord>) {
        if !cfg!(debug_assertions) {
            return;
        }
        if let Some(record) = build() {
            self.hub.record_diagnostic(record);
        }
    }
}

/// One connected client's delivery state.
struct Subscriber {
    messages: Sender<ServerMessage>,
    /// Signalled when the subscriber falls too far behind and is dropped from
    /// the hub. The connection polls it and closes, letting the client
    /// reconnect and resume from its last replayed cursor.
    kicked: Sender<()>,
}

impl Subscriber {
    fn new(messages: Sender<ServerMessage>) -> (Self, Receiver<()>) {
        let (kicked, kicked_rx) = unbounded();
        (Self { messages, kicked }, kicked_rx)
    }
}

#[derive(Default)]
struct HubState {
    next_subscriber_id: u64,
    task_state_revision: u64,
    subscribers: HashMap<u64, Subscriber>,
    active_runtimes: HashMap<Uuid, Uuid>,
    next_sequences: HashMap<(Uuid, Uuid), u64>,
    journal: HashMap<(Uuid, Uuid), VecDeque<SequencedEvent>>,
    next_diagnostic_sequence: u64,
    diagnostics: VecDeque<DiagnosticEvent>,
    diagnostic_verbosity: DiagnosticVerbosity,
    diagnostic_reports: VecDeque<DiagnosticCounterReport>,
    diagnostic_filtered: u64,
    driver_events: u64,
    requests_completed: u64,
    request_latency_ms: f64,
    request_latency_max_ms: f64,
    responses: VecDeque<(Uuid, ResponseOutcome, usize)>,
    cached_response_bytes: usize,
    catalog_projects: HashMap<Uuid, ProjectCatalogEntry>,
    catalog_sessions: HashMap<Uuid, SessionCatalogEntry>,
    catalog_tasks: HashMap<Uuid, Task>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProjectCatalogEntry {
    name: String,
    path: std::path::PathBuf,
    created_at: u64,
}

impl From<&Project> for ProjectCatalogEntry {
    fn from(project: &Project) -> Self {
        Self {
            name: project.name.clone(),
            path: project.path.clone(),
            created_at: project.created_at,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SessionCatalogEntry {
    title: String,
    auto_title: Option<String>,
    project_id: Uuid,
    task_id: Option<Uuid>,
    workspace: SessionWorkspace,
    is_draft: bool,
    provider: ProviderKind,
    model: Option<String>,
    status: SessionStatus,
    created_at: u64,
    last_reply_at: Option<u64>,
}

impl From<&AgentSession> for SessionCatalogEntry {
    fn from(session: &AgentSession) -> Self {
        Self {
            title: session.title.clone(),
            auto_title: session.auto_title.clone(),
            project_id: session.project_id,
            task_id: session.task_id,
            workspace: session.workspace.clone(),
            is_draft: !session.has_started(),
            provider: session.provider,
            model: session.model.clone(),
            status: session.status,
            created_at: session.created_at,
            last_reply_at: session.last_reply_at,
        }
    }
}

struct Hub {
    // Keep automation delivery in the same order as the source sequence.
    // Acquire before HubState; release HubState before entering the runtime,
    // whose delivery path reacquires HubState. No runtime path takes this lock.
    #[cfg(debug_assertions)]
    automation_emission_order: Mutex<()>,
    automation: Arc<automation::Runtime>,
    epoch: Uuid,
    state: Mutex<HubState>,
    diagnostic_log: Option<DiagnosticLog>,
}

impl Default for Hub {
    fn default() -> Self {
        Self::new(None)
    }
}

impl Hub {
    fn new(diagnostic_log: Option<DiagnosticLog>) -> Self {
        Self {
            #[cfg(debug_assertions)]
            automation_emission_order: Mutex::new(()),
            automation: Arc::new(automation::Runtime::default()),
            epoch: Uuid::new_v4(),
            state: Mutex::new(HubState::default()),
            diagnostic_log,
        }
    }
}

struct DispatchedRequest {
    request: Request,
    outgoing: Sender<ServerMessage>,
    source_subscriber_id: u64,
}

struct RuntimeMailbox {
    id: Uuid,
    sender: Sender<DispatchedRequest>,
}

struct RequestDispatcher {
    backend: Arc<dyn Backend>,
    hub: Arc<Hub>,
    /// A live provider runtime is an actor owned by the daemon, not by any
    /// particular WebSocket connection. One mailbox per session preserves
    /// lifecycle order across desktop and web clients without serializing
    /// unrelated sessions or read-only requests.
    runtime_mailboxes: Arc<Mutex<HashMap<Uuid, RuntimeMailbox>>>,
}

/// Push one replayed message into a subscriber's bounded queue. `false` means
/// the queue is full; replay stops exactly as live delivery stops, and the
/// client reconnects from its last cursor.
fn try_replay(messages: &Sender<ServerMessage>, message: ServerMessage) -> bool {
    messages.len() < MAX_QUEUED_MESSAGES_PER_SUBSCRIBER && messages.try_send(message).is_ok()
}

impl Hub {
    fn event_sink(self: &Arc<Self>, session_id: Uuid, runtime_id: Uuid) -> EventSink {
        EventSink {
            session_id,
            runtime_id,
            hub: self.clone(),
        }
    }

    fn begin_runtime(&self, session_id: Uuid, runtime_id: Uuid) {
        let mut state = self.state.lock();
        state.active_runtimes.insert(session_id, runtime_id);
        state
            .next_sequences
            .retain(|(candidate, _), _| *candidate != session_id);
        state
            .journal
            .retain(|(candidate, _), _| *candidate != session_id);
    }

    fn end_runtime(&self, session_id: Uuid, runtime_id: Option<Uuid>) {
        let mut state = self.state.lock();
        let matches_active = runtime_id
            .is_none_or(|runtime_id| state.active_runtimes.get(&session_id) == Some(&runtime_id));
        if !matches_active {
            return;
        }
        state.active_runtimes.remove(&session_id);
        state
            .next_sequences
            .retain(|(candidate, _), _| *candidate != session_id);
        state
            .journal
            .retain(|(candidate, _), _| *candidate != session_id);
    }

    fn emit(&self, session_id: Uuid, runtime_id: Uuid, event: WireDriverEvent, replayable: bool) {
        #[cfg(debug_assertions)]
        let _ordered = self.automation_emission_order.lock();
        let mut state = self.state.lock();
        if state.active_runtimes.get(&session_id) != Some(&runtime_id) {
            return;
        }
        if cfg!(debug_assertions) {
            state.driver_events = state.driver_events.saturating_add(1);
        }
        let sequence = state
            .next_sequences
            .entry((session_id, runtime_id))
            .or_default();
        *sequence = sequence.saturating_add(1);
        let event = SequencedEvent {
            session_id,
            runtime_id,
            epoch: self.epoch,
            sequence: *sequence,
            event,
        };
        if replayable {
            let journal = state.journal.entry((session_id, runtime_id)).or_default();
            journal.push_back(event.clone());
            while journal.len() > MAX_REPLAY_EVENTS_PER_SESSION {
                journal.pop_front();
            }
        }
        let message = ServerMessage::Event(event);
        Self::broadcast(&mut state, &message, None);
        drop(state);
        #[cfg(debug_assertions)]
        if let ServerMessage::Event(event) = &message {
            self.automation.model_event(self, event);
        }
    }

    fn record_diagnostic(&self, record: DiagnosticRecord) {
        // Diagnostics are debug-build behavior: a release daemon neither
        // sequences nor broadcasts records, whatever a client sends.
        if !cfg!(debug_assertions) {
            return;
        }
        #[cfg(debug_assertions)]
        let _ordered = self.automation_emission_order.lock();
        let mut state = self.state.lock();
        if !state
            .diagnostic_verbosity
            .enabled(record.level, &record.origin, &record.source)
        {
            state.diagnostic_filtered = state.diagnostic_filtered.saturating_add(1);
            return;
        }
        state.next_diagnostic_sequence = state.next_diagnostic_sequence.saturating_add(1);
        let event = DiagnosticEvent {
            epoch: self.epoch,
            sequence: state.next_diagnostic_sequence,
            record,
        };
        state.diagnostics.push_back(event.clone());
        while state.diagnostics.len() > MAX_DIAGNOSTIC_EVENTS {
            state.diagnostics.pop_front();
        }
        // append only queues: keeping it under the sequencing lock prevents
        // concurrent emitters from reversing the NDJSON order. Disk I/O stays
        // exclusively on the background writer.
        if let Some(log) = &self.diagnostic_log {
            log.append(&event);
        }
        let message = ServerMessage::Diagnostic(event);
        Self::broadcast(&mut state, &message, None);
        drop(state);
        #[cfg(debug_assertions)]
        if let ServerMessage::Diagnostic(event) = &message {
            self.automation.diagnostic(self, event);
        }
    }

    fn report_diagnostic_counters(&self, mut report: DiagnosticCounterReport) {
        if !cfg!(debug_assertions) {
            return;
        }
        // Samples are absolute and replace the last report from this process.
        // A stopped emitter keeps its original sample time; both dimensions
        // are bounded even if authenticated clients report arbitrary origins.
        report
            .values
            .retain(|name, value| name.len() <= 128 && value.is_finite());
        report.values = report.values.into_iter().take(128).collect();
        let mut state = self.state.lock();
        state
            .diagnostic_reports
            .retain(|previous| previous.origin != report.origin);
        state.diagnostic_reports.push_back(report);
        while state.diagnostic_reports.len() > 128 {
            state.diagnostic_reports.pop_front();
        }
    }

    fn sync_diagnostic_verbosity(&self, subscriber_id: u64) {
        if !cfg!(debug_assertions) {
            return;
        }
        let mut state = self.state.lock();
        let failed = state
            .subscribers
            .get(&subscriber_id)
            .is_some_and(|subscriber| {
                subscriber
                    .messages
                    .try_send(ServerMessage::DiagnosticVerbosity {
                        verbosity: state.diagnostic_verbosity.clone(),
                    })
                    .is_err()
            });
        // A replay-saturated connection must resume rather than silently use
        // a stale recording policy for the rest of its lifetime.
        if failed && let Some(subscriber) = state.subscribers.remove(&subscriber_id) {
            let _ = subscriber.kicked.send(());
        }
    }

    fn diagnostic_command(&self, command: &Command) -> Option<anyhow::Result<ResponsePayload>> {
        use crate::diagnostic_queries;
        if !matches!(
            command,
            Command::DiagnosticLogs { .. }
                | Command::DiagnosticTimeline { .. }
                | Command::DiagnosticCounters
                | Command::DiagnosticVerbosity { .. }
        ) {
            return None;
        }
        if !cfg!(debug_assertions) {
            return Some(Err(anyhow::anyhow!(
                "diagnostic queries are unavailable in release builds"
            )));
        }
        Some((|| {
            let mut state = self.state.lock();
            Ok(match command {
                Command::DiagnosticLogs { query } => ResponsePayload::DiagnosticLogs {
                    result: diagnostic_queries::logs(
                        self.epoch,
                        state.next_diagnostic_sequence,
                        &state.diagnostics,
                        query,
                    ),
                },
                Command::DiagnosticTimeline { query } => ResponsePayload::DiagnosticTimeline {
                    result: diagnostic_queries::timeline(
                        &state.diagnostics,
                        query,
                        crate::model::unix_time_millis(),
                    )?,
                },
                Command::DiagnosticCounters => {
                    let observed_at_ms = crate::model::unix_time_millis();
                    let (log_queue, log_dropped, log_failed) = self
                        .diagnostic_log
                        .as_ref()
                        .map_or((0, 0, false), DiagnosticLog::counters);
                    let mut processes: Vec<_> = state.diagnostic_reports.iter().cloned().collect();
                    processes.push(DiagnosticCounterReport {
                        timestamp_ms: observed_at_ms,
                        origin: DiagnosticOrigin::Daemon {
                            pid: std::process::id(),
                        },
                        values: [
                            ("diagnosticRecords", state.next_diagnostic_sequence as f64),
                            ("diagnosticRetained", state.diagnostics.len() as f64),
                            (
                                "retentionDropped",
                                state
                                    .next_diagnostic_sequence
                                    .saturating_sub(state.diagnostics.len() as u64)
                                    as f64,
                            ),
                            ("diagnosticFiltered", state.diagnostic_filtered as f64),
                            ("driverEvents", state.driver_events as f64),
                            (
                                "replayRetained",
                                state.journal.values().map(VecDeque::len).sum::<usize>() as f64,
                            ),
                            (
                                "eventBacklog",
                                state
                                    .subscribers
                                    .values()
                                    .map(|subscriber| subscriber.messages.len())
                                    .sum::<usize>() as f64,
                            ),
                            (
                                "outgoingQueueDepth",
                                state
                                    .subscribers
                                    .values()
                                    .map(|subscriber| subscriber.messages.len())
                                    .sum::<usize>() as f64,
                            ),
                            ("logQueueDepth", log_queue as f64),
                            ("logDropped", log_dropped as f64),
                            ("logFailed", u8::from(log_failed) as f64),
                            ("connectedClients", state.subscribers.len() as f64),
                            ("activeRuntimes", state.active_runtimes.len() as f64),
                            ("requestsCompleted", state.requests_completed as f64),
                            ("requestServiceMs", state.request_latency_ms),
                            ("requestServiceMaxMs", state.request_latency_max_ms),
                        ]
                        .into_iter()
                        .map(|(key, value)| (key.to_owned(), value))
                        .collect(),
                    });
                    ResponsePayload::DiagnosticCounters {
                        result: DiagnosticCounters {
                            observed_at_ms,
                            processes,
                        },
                    }
                }
                Command::DiagnosticVerbosity { update } => {
                    if let Some(verbosity) = update {
                        anyhow::ensure!(
                            verbosity.overrides.len() <= 128
                                && verbosity.overrides.iter().all(|rule| rule
                                    .source
                                    .as_ref()
                                    .is_none_or(|source| source.len() <= 128)),
                            "diagnostic verbosity exceeds the override bound"
                        );
                        state.diagnostic_verbosity = verbosity.clone();
                        Self::broadcast(
                            &mut state,
                            &ServerMessage::DiagnosticVerbosity {
                                verbosity: verbosity.clone(),
                            },
                            None,
                        );
                    }
                    ResponsePayload::DiagnosticVerbosity {
                        verbosity: state.diagnostic_verbosity.clone(),
                    }
                }
                _ => unreachable!(),
            })
        })())
    }

    /// Wait until every queued record is on disk. Tests read the files through
    /// this; the daemon's live paths never wait for the writer.
    #[cfg(test)]
    fn flush_diagnostics(&self) {
        if let Some(log) = &self.diagnostic_log {
            log.flush();
        }
    }

    fn subscribe(
        &self,
        resume_from: &[ReplayCursor],
        diagnostic_from: Option<DiagnosticCursor>,
        subscriber: Subscriber,
    ) -> u64 {
        let mut state = self.state.lock();
        let id = state.next_subscriber_id;
        state.next_subscriber_id = state.next_subscriber_id.saturating_add(1);
        // The subscriber is registered before replay so an oversized journal
        // can only truncate the replay, never leave a live connection without
        // events. Replay runs under the hub lock so no live event can
        // interleave with the journal catch-up, and pushes into the bounded
        // queue are non-blocking. Dropping the oldest replayed events matches
        // the journal's own rollover semantics; clients reconcile any gap
        // from session state.
        let messages = subscriber.messages.clone();
        state.subscribers.insert(id, subscriber);
        // A cursor older than retention is named before the retained tail is
        // replayed: the client then knows the gap exists instead of inferring
        // one from a sequence jump.
        if let Some(cursor) = diagnostic_from {
            let next_sequence = state.next_diagnostic_sequence.saturating_add(1);
            let oldest_sequence = state
                .diagnostics
                .front()
                .map(|event| event.sequence)
                .unwrap_or(next_sequence);
            let expired =
                cursor.epoch != self.epoch || cursor.sequence.saturating_add(1) < oldest_sequence;
            if expired {
                let notice = ServerMessage::DiagnosticCursorExpired {
                    expiry: DiagnosticExpiry {
                        epoch: self.epoch,
                        requested_sequence: cursor.sequence,
                        oldest_sequence,
                    },
                };
                if !try_replay(&messages, notice) {
                    return id;
                }
            }
        }
        for (&(session_id, runtime_id), events) in &state.journal {
            let sequence = resume_from
                .iter()
                .find(|cursor| {
                    cursor.session_id == session_id
                        && cursor.runtime_id == runtime_id
                        && cursor.epoch == self.epoch
                })
                .map(|cursor| cursor.sequence)
                .unwrap_or_default();
            for event in events.iter().filter(|event| event.sequence > sequence) {
                if !try_replay(&messages, ServerMessage::Event(event.clone())) {
                    return id;
                }
            }
        }
        let sequence = diagnostic_from
            .filter(|cursor| cursor.epoch == self.epoch)
            .map(|cursor| cursor.sequence)
            .unwrap_or_default();
        for event in state
            .diagnostics
            .iter()
            .filter(|event| event.sequence > sequence)
        {
            if !try_replay(&messages, ServerMessage::Diagnostic(event.clone())) {
                return id;
            }
        }
        id
    }

    fn unsubscribe(&self, subscriber_id: u64) {
        self.state.lock().subscribers.remove(&subscriber_id);
        #[cfg(debug_assertions)]
        self.automation.disconnected(self, subscriber_id);
    }

    fn send_to(&self, subscriber_id: u64, message: ServerMessage) -> bool {
        let mut state = self.state.lock();
        let sent = state
            .subscribers
            .get(&subscriber_id)
            .is_some_and(|s| try_replay(&s.messages, message));
        if !sent && let Some(subscriber) = state.subscribers.remove(&subscriber_id) {
            let _ = subscriber.kicked.send(());
        }
        sent
    }

    fn kick(&self, subscriber_id: u64) {
        if let Some(subscriber) = self.state.lock().subscribers.remove(&subscriber_id) {
            let _ = subscriber.kicked.send(());
        }
    }

    /// Sends `message` to every subscriber except `skip`, dropping any whose
    /// bounded queue is full. Buffering a stalled client at the daemon's
    /// event rate is how resident memory grew without bound; a dropped
    /// subscriber's connection observes the kick, closes, and reconnects with
    /// replay cursors, so no event stream is lost permanently.
    fn broadcast(state: &mut HubState, message: &ServerMessage, skip: Option<u64>) {
        let mut overwhelmed = Vec::new();
        for (&subscriber_id, subscriber) in &state.subscribers {
            if Some(subscriber_id) == skip {
                continue;
            }
            if !try_replay(&subscriber.messages, message.clone()) {
                overwhelmed.push((subscriber_id, subscriber.kicked.clone()));
            }
        }
        for (subscriber_id, kicked) in overwhelmed {
            state.subscribers.remove(&subscriber_id);
            let _ = kicked.send(());
        }
    }

    fn task_state_changed(&self, source_subscriber_id: u64) {
        let mut state = self.state.lock();
        Self::broadcast_task_state_changed(&mut state, source_subscriber_id);
    }

    fn replace_task_catalog(
        &self,
        projects: &[Project],
        sessions: &[AgentSession],
        tasks: &[Task],
    ) {
        let mut state = self.state.lock();
        state.catalog_projects = projects
            .iter()
            .map(|project| (project.id, ProjectCatalogEntry::from(project)))
            .collect();
        state.catalog_sessions = sessions
            .iter()
            .map(|session| (session.id, SessionCatalogEntry::from(session)))
            .collect();
        state.catalog_tasks = tasks.iter().map(|task| (task.id, task.clone())).collect();
    }

    fn task_state_saved(
        &self,
        source_subscriber_id: u64,
        projects: &[Project],
        sessions: &[AgentSession],
        tasks: &[Task],
    ) {
        let mut state = self.state.lock();
        let mut changed = false;
        for project in projects {
            let next = ProjectCatalogEntry::from(project);
            changed |= state
                .catalog_projects
                .insert(project.id, next.clone())
                .is_none_or(|previous| previous != next);
        }
        for session in sessions {
            let next = SessionCatalogEntry::from(session);
            changed |= state
                .catalog_sessions
                .insert(session.id, next.clone())
                .is_none_or(|previous| previous != next);
        }
        for task in tasks {
            if state
                .catalog_tasks
                .get(&task.id)
                .is_some_and(|previous| previous.updated_at >= task.updated_at)
            {
                continue;
            }
            changed |= state
                .catalog_tasks
                .insert(task.id, task.clone())
                .is_none_or(|previous| previous != *task);
        }
        if changed {
            Self::broadcast_task_state_changed(&mut state, source_subscriber_id);
        }
    }

    fn broadcast_task_state_changed(state: &mut HubState, source_subscriber_id: u64) {
        state.task_state_revision = state.task_state_revision.saturating_add(1);
        let message = ServerMessage::TaskStateChanged {
            revision: state.task_state_revision,
        };
        Self::broadcast(state, &message, Some(source_subscriber_id));
    }

    fn cached_response(&self, request_id: Uuid) -> Option<ResponseOutcome> {
        self.state
            .lock()
            .responses
            .iter()
            .rev()
            .find_map(|(cached_id, outcome, _)| (*cached_id == request_id).then(|| outcome.clone()))
    }

    fn cache_response(&self, request_id: Uuid, outcome: ResponseOutcome) {
        // File bytes are bulk data that clients read once; caching a copy per
        // request would duplicate every open file in daemon memory, and the
        // request is cheap to re-run if it is ever retried.
        if matches!(
            &outcome,
            ResponseOutcome::Ok {
                payload: ResponsePayload::BlobData { .. }
            }
        ) {
            return;
        }
        // Outcomes carry serde payloads whose in-memory size tracks their
        // serialized form closely enough for a cache budget.
        let bytes = serde_json::to_string(&outcome)
            .map(|serialized| serialized.len())
            .unwrap_or(0);
        let mut state = self.state.lock();
        state.cached_response_bytes = state.cached_response_bytes.saturating_add(bytes);
        state.responses.push_back((request_id, outcome, bytes));
        while state.responses.len() > MAX_CACHED_RESPONSES
            || state.cached_response_bytes > MAX_CACHED_RESPONSE_BYTES
        {
            if let Some((_, _, evicted_bytes)) = state.responses.pop_front() {
                state.cached_response_bytes =
                    state.cached_response_bytes.saturating_sub(evicted_bytes);
            }
        }
    }
}

impl RequestDispatcher {
    fn new(backend: Arc<dyn Backend>, hub: Arc<Hub>) -> Self {
        Self {
            backend,
            hub,
            runtime_mailboxes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn dispatch(
        &self,
        request: Request,
        outgoing: Sender<ServerMessage>,
        source_subscriber_id: u64,
    ) {
        if command_targets_runtime(&request.command) {
            self.dispatch_runtime(request, outgoing, source_subscriber_id);
        } else {
            self.dispatch_independent(request, outgoing, source_subscriber_id);
        }
    }

    fn dispatch_independent(
        &self,
        request: Request,
        outgoing: Sender<ServerMessage>,
        source_subscriber_id: u64,
    ) {
        let backend = self.backend.clone();
        let hub = self.hub.clone();
        let failed_request_id = request.request_id;
        let failed_outgoing = outgoing.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("waku-daemon-request".into())
            .spawn(move || {
                handle_request(request, outgoing, source_subscriber_id, backend, hub);
            })
        {
            send_dispatch_error(
                failed_request_id,
                failed_outgoing,
                &self.hub,
                format!("could not start daemon request worker: {error}"),
            );
        }
    }

    fn dispatch_runtime(
        &self,
        request: Request,
        outgoing: Sender<ServerMessage>,
        source_subscriber_id: u64,
    ) {
        let session_id = request.session_id;
        let failed_request_id = request.request_id;
        let failed_outgoing = outgoing.clone();
        let mut dispatched = DispatchedRequest {
            request,
            outgoing,
            source_subscriber_id,
        };
        loop {
            let mut mailboxes = self.runtime_mailboxes.lock();
            if let Some(mailbox) = mailboxes.get(&session_id) {
                match mailbox.sender.send(dispatched) {
                    Ok(()) => return,
                    Err(error) => {
                        dispatched = error.0;
                        mailboxes.remove(&session_id);
                        continue;
                    }
                }
            }

            let mailbox_id = Uuid::new_v4();
            let (sender, requests) = unbounded();
            sender
                .send(dispatched)
                .expect("a new runtime mailbox still has its receiver");
            mailboxes.insert(
                session_id,
                RuntimeMailbox {
                    id: mailbox_id,
                    sender,
                },
            );

            let backend = self.backend.clone();
            let hub = self.hub.clone();
            let mailbox_registry = Arc::downgrade(&self.runtime_mailboxes);
            let worker = std::thread::Builder::new()
                .name(format!("waku-daemon-runtime-{session_id}"))
                .spawn(move || {
                    run_runtime_mailbox(
                        session_id,
                        mailbox_id,
                        requests,
                        mailbox_registry,
                        backend,
                        hub,
                    );
                });
            if let Err(error) = worker {
                if mailboxes
                    .get(&session_id)
                    .is_some_and(|mailbox| mailbox.id == mailbox_id)
                {
                    mailboxes.remove(&session_id);
                }
                drop(mailboxes);
                send_dispatch_error(
                    failed_request_id,
                    failed_outgoing,
                    &self.hub,
                    format!("could not start runtime worker: {error}"),
                );
            }
            return;
        }
    }
}

pub fn serve(
    listener: TcpListener,
    token: String,
    backend: Arc<dyn Backend>,
    shutdown: Arc<AtomicBool>,
    options: ServerOptions,
) -> anyhow::Result<()> {
    listener
        .set_nonblocking(true)
        .context("could not configure Waku daemon listener")?;
    let mut files_unavailable = None;
    let diagnostic_log = match options
        .diagnostics_directory
        .as_ref()
        .filter(|_| cfg!(debug_assertions))
    {
        Some(directory) => match DiagnosticLog::open(directory) {
            Ok(log) => Some(log),
            Err(error) => {
                // Diagnostics are development tooling; a directory that cannot
                // hold them must not keep the daemon from starting. The loss
                // is still recorded in the stream below.
                eprintln!("waku-daemon: diagnostic files are unavailable: {error:#}");
                files_unavailable = Some(format!("{error:#}"));
                None
            }
        },
        None => None,
    };
    let hub = Arc::new(Hub::new(diagnostic_log));
    if let Some(reason) = files_unavailable {
        hub.record_diagnostic(DiagnosticRecord::new(
            DiagnosticLevel::Warn,
            DiagnosticOrigin::Daemon {
                pid: std::process::id(),
            },
            "diagnostics",
            format!("diagnostic files are unavailable: {reason}"),
        ));
    }
    #[cfg(debug_assertions)]
    let _automation_repl_endpoint = crate::driver::set_automation_repl_endpoint(
        listener.local_addr()?.to_string(),
        token.clone(),
    );
    let dispatcher = Arc::new(RequestDispatcher::new(backend.clone(), hub.clone()));
    let options = Arc::new(options);
    let active_connections = Arc::new(AtomicUsize::new(0));
    while !shutdown.load(Ordering::Acquire) {
        #[cfg(debug_assertions)]
        hub.automation.tick(&hub);
        match listener.accept() {
            Ok((stream, _)) => {
                if active_connections
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                        (active < MAX_CONNECTIONS).then_some(active + 1)
                    })
                    .is_err()
                {
                    continue;
                }
                let connection_permit = ConnectionPermit(active_connections.clone());
                let token = token.clone();
                let dispatcher = dispatcher.clone();
                let hub = hub.clone();
                let shutdown = shutdown.clone();
                let options = options.clone();
                std::thread::Builder::new()
                    .name("waku-daemon-connection".into())
                    .spawn(move || {
                        let _connection_permit = connection_permit;
                        if let Err(error) =
                            handle_connection(stream, &token, dispatcher, hub, shutdown, &options)
                        {
                            eprintln!("waku-daemon connection ended: {error:#}");
                        }
                    })
                    .context("could not start Waku daemon connection thread")?;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("Waku daemon listener failed"),
        }
    }
    backend.shutdown();
    Ok(())
}

fn handle_connection(
    stream: TcpStream,
    expected_token: &str,
    dispatcher: Arc<RequestDispatcher>,
    hub: Arc<Hub>,
    shutdown: Arc<AtomicBool>,
    options: &ServerOptions,
) -> anyhow::Result<()> {
    // Accepted sockets can inherit the listener's nonblocking flag on some
    // platforms. The handshake is deliberately blocking; steady-state reads
    // get their bounded polling behavior from SO_RCVTIMEO below.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_HANDSHAKE_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_HANDSHAKE_MESSAGE_BYTES));
    let allowed_origins = options.allowed_origins.clone();
    let mut socket = accept_hdr_with_config(
        stream,
        move |request: &HandshakeRequest, response: HandshakeResponse| {
            validate_handshake(request, response, &allowed_origins)
        },
        Some(config),
    )
    .context("WebSocket handshake failed")?;
    let hello = read_client_message(&mut socket)?;
    let (resume_from, diagnostic_from) = match hello {
        ClientMessage::Hello {
            protocol_version,
            token,
            resume_from,
            diagnostic_from,
            ..
        } if protocol_version == PROTOCOL_VERSION && token_matches(expected_token, &token) => {
            (resume_from, diagnostic_from)
        }
        ClientMessage::Hello {
            protocol_version, ..
        } if protocol_version != PROTOCOL_VERSION => {
            write_json(
                &mut socket,
                &ServerMessage::Rejected {
                    message: format!(
                        "protocol {protocol_version} is unsupported; expected {PROTOCOL_VERSION}"
                    ),
                },
            )?;
            return Ok(());
        }
        ClientMessage::Hello { .. } => {
            write_json(
                &mut socket,
                &ServerMessage::Rejected {
                    message: "authentication failed".into(),
                },
            )?;
            return Ok(());
        }
        _ => bail!("first daemon message was not a hello"),
    };
    write_json(
        &mut socket,
        &ServerMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: env!("CARGO_PKG_VERSION").into(),
        },
    )?;
    socket.set_config(|config| {
        config.max_message_size = Some(MAX_WIRE_MESSAGE_BYTES);
        config.max_frame_size = Some(MAX_WIRE_MESSAGE_BYTES);
    });
    socket
        .get_mut()
        .set_read_timeout(Some(SOCKET_POLL_INTERVAL))?;
    // A client that stops reading must not let its outgoing queue grow at the
    // daemon's event rate; a write stalled past this timeout drops it.
    socket
        .get_mut()
        .set_write_timeout(Some(SOCKET_WRITE_STALL_TIMEOUT))?;

    let (outgoing, outgoing_rx) = bounded(MAX_QUEUED_MESSAGES_PER_SUBSCRIBER);
    let (subscriber, kicked) = Subscriber::new(outgoing.clone());
    let subscriber_id = hub.subscribe(&resume_from, diagnostic_from, subscriber);
    hub.sync_diagnostic_verbosity(subscriber_id);
    // Also unregister executors on socket errors, not only a clean close.
    struct SubscriptionGuard {
        hub: Arc<Hub>,
        backend: Arc<dyn Backend>,
        subscriber: u64,
    }
    impl Drop for SubscriptionGuard {
        fn drop(&mut self) {
            self.hub.unsubscribe(self.subscriber);
            self.backend
                .automation_subscriber_disconnected(self.subscriber);
        }
    }
    let _subscription = SubscriptionGuard {
        hub: hub.clone(),
        backend: dispatcher.backend.clone(),
        subscriber: subscriber_id,
    };

    'connection: while !shutdown.load(Ordering::Acquire) {
        if kicked.try_recv().is_ok() {
            break;
        }
        while let Ok(message) = outgoing_rx.try_recv() {
            if write_json(&mut socket, &message).is_err() {
                break 'connection;
            }
        }
        match socket.read() {
            Ok(Message::Text(text)) => match serde_json::from_str(text.as_ref()) {
                Ok(ClientMessage::Request(request)) => {
                    dispatcher.dispatch(request, outgoing.clone(), subscriber_id);
                }
                Ok(ClientMessage::AutomationExecutorReply { reply }) => {
                    hub.automation.reply(&hub, subscriber_id, reply);
                }
                Ok(ClientMessage::AutomationExecutorEvent { event }) => {
                    hub.automation.event(&hub, subscriber_id, event);
                }
                Ok(ClientMessage::Diagnostic(record)) => {
                    hub.record_diagnostic(record);
                }
                Ok(ClientMessage::DiagnosticCounters(report)) => {
                    hub.report_diagnostic_counters(report);
                }
                Ok(ClientMessage::Shutdown) => {
                    if options.allow_shutdown {
                        // The requester can close immediately after sending.
                        // Accept shutdown before replying so a broken socket
                        // cannot leave the daemon running without its owner.
                        shutdown.store(true, Ordering::Release);
                        write_json(&mut socket, &ServerMessage::ShuttingDown)?;
                        break;
                    }
                    write_json(
                        &mut socket,
                        &ServerMessage::Rejected {
                            message: "daemon shutdown is managed by its service owner".into(),
                        },
                    )?;
                }
                Ok(ClientMessage::Hello { .. }) => {}
                Err(error) => {
                    eprintln!("waku-daemon ignored invalid message: {error}");
                }
            },
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(_)) => {
                let _ = socket.flush();
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(error)) if retryable_io(&error) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => break,
            Err(error) => return Err(error).context("Waku daemon WebSocket failed"),
        }
    }
    Ok(())
}

fn validate_handshake(
    request: &HandshakeRequest,
    response: HandshakeResponse,
    allowed_origins: &HashSet<String>,
) -> Result<HandshakeResponse, ErrorResponse> {
    if request.uri().path() != "/v1" {
        return Err(handshake_error(
            StatusCode::NOT_FOUND,
            "unknown daemon endpoint",
        ));
    }
    let is_native_client = request
        .headers()
        .get(NATIVE_CLIENT_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == NATIVE_CLIENT_HEADER_VALUE);
    if let Some(origin) = request.headers().get(ORIGIN) {
        let allowed = origin
            .to_str()
            .ok()
            .is_some_and(|origin| allowed_origins.contains(origin));
        if !allowed && !is_native_client {
            return Err(handshake_error(
                StatusCode::FORBIDDEN,
                "WebSocket origin is not allowed",
            ));
        }
    }
    Ok(response)
}

fn handshake_error(status: StatusCode, message: &str) -> ErrorResponse {
    tungstenite::http::Response::builder()
        .status(status)
        .body(Some(message.to_owned()))
        .expect("static WebSocket rejection is valid")
}

fn token_matches(expected: &str, candidate: &str) -> bool {
    expected.as_bytes().ct_eq(candidate.as_bytes()).into()
}

fn command_targets_runtime(command: &Command) -> bool {
    matches!(
        command,
        Command::AttachSession
            | Command::Start { .. }
            | Command::Prompt { .. }
            | Command::Steer { .. }
            | Command::Cancel
            | Command::CancelComputerUse
            | Command::RefreshBackgroundWork
            | Command::StopBackgroundWork { .. }
            | Command::Respond { .. }
            | Command::RespondUserInput { .. }
            | Command::RunComputerTool { .. }
            | Command::RejectComputerTool { .. }
            | Command::ApplyOptions { .. }
            | Command::Rollback { .. }
            | Command::Fork { .. }
            | Command::ForkSessionFromResponse { .. }
            | Command::RewindSessionToMessage { .. }
            | Command::OpenTerminal { .. }
            | Command::AttachTerminal { .. }
            | Command::WriteTerminal { .. }
            | Command::ResizeTerminal { .. }
            | Command::CloseTerminal
            | Command::CloseSession
            | Command::CloseSessionAndWait
            | Command::RemoveSession
    )
}

fn run_runtime_mailbox(
    session_id: Uuid,
    mailbox_id: Uuid,
    requests: Receiver<DispatchedRequest>,
    mailbox_registry: Weak<Mutex<HashMap<Uuid, RuntimeMailbox>>>,
    backend: Arc<dyn Backend>,
    hub: Arc<Hub>,
) {
    let mut active_runtime_id = None;
    let mut pending = None;
    loop {
        let dispatched = match pending.take() {
            Some(request) => request,
            None => match requests.recv() {
                Ok(request) => request,
                Err(_) => return,
            },
        };
        let runtime_id = dispatched.request.runtime_id;
        let starts_runtime = matches!(
            &dispatched.request.command,
            Command::Start { .. } | Command::OpenTerminal { .. }
        );
        let closes_runtime = matches!(
            &dispatched.request.command,
            Command::CloseSession
                | Command::CloseSessionAndWait
                | Command::CloseTerminal
                | Command::RemoveSession
        );
        let removes_session = matches!(&dispatched.request.command, Command::RemoveSession);
        let handled = handle_request(
            dispatched.request,
            dispatched.outgoing,
            dispatched.source_subscriber_id,
            backend.clone(),
            hub.clone(),
        );

        if handled.executed {
            if starts_runtime {
                active_runtime_id =
                    matches!(&handled.outcome, ResponseOutcome::Ok { .. }).then_some(runtime_id);
            } else if let ResponseOutcome::Ok {
                payload:
                    ResponsePayload::SessionRuntime {
                        runtime_id: Some(attached_runtime_id),
                        ..
                    },
            } = &handled.outcome
            {
                // A replacement mailbox can rediscover a provider runtime
                // that survived its previous actor worker.
                active_runtime_id = Some(*attached_runtime_id);
            } else if closes_runtime {
                if (removes_session || active_runtime_id == Some(runtime_id))
                    && matches!(&handled.outcome, ResponseOutcome::Ok { .. })
                {
                    hub.end_runtime(session_id, (!removes_session).then_some(runtime_id));
                    active_runtime_id = None;
                }
            } else if active_runtime_id.is_none()
                && !matches!(
                    &handled.outcome,
                    ResponseOutcome::Ok {
                        payload: ResponsePayload::SessionRuntime { .. }
                    }
                )
                && matches!(&handled.outcome, ResponseOutcome::Ok { .. })
            {
                // Recover the supervisor state if a previous mailbox worker
                // exited unexpectedly while the backend runtime stayed alive.
                active_runtime_id = Some(runtime_id);
            }
        }

        if active_runtime_id.is_none() {
            pending =
                take_queued_request_or_retire(session_id, mailbox_id, &requests, &mailbox_registry);
            if pending.is_none() {
                return;
            }
        }
    }
}

fn take_queued_request_or_retire(
    session_id: Uuid,
    mailbox_id: Uuid,
    requests: &Receiver<DispatchedRequest>,
    mailbox_registry: &Weak<Mutex<HashMap<Uuid, RuntimeMailbox>>>,
) -> Option<DispatchedRequest> {
    let Some(mailbox_registry) = mailbox_registry.upgrade() else {
        return requests.try_recv().ok();
    };
    // Dispatchers send while holding this same lock. Therefore an empty
    // receiver followed by removal is atomic with respect to a new command:
    // it either joins this actor before retirement or creates its successor.
    let mut mailboxes = mailbox_registry.lock();
    match requests.try_recv() {
        Ok(request) => Some(request),
        Err(crossbeam_channel::TryRecvError::Empty) => {
            if mailboxes
                .get(&session_id)
                .is_some_and(|mailbox| mailbox.id == mailbox_id)
            {
                mailboxes.remove(&session_id);
            }
            None
        }
        Err(crossbeam_channel::TryRecvError::Disconnected) => None,
    }
}

struct HandledRequest {
    outcome: ResponseOutcome,
    executed: bool,
}

enum TaskCatalogAction {
    None,
    Load,
    Save {
        projects: Vec<Project>,
        tasks: Vec<Task>,
    },
    Changed,
}

fn handle_request(
    request: Request,
    outgoing: Sender<ServerMessage>,
    source_subscriber_id: u64,
    backend: Arc<dyn Backend>,
    hub: Arc<Hub>,
) -> HandledRequest {
    #[cfg(debug_assertions)]
    let started_at = std::time::Instant::now();
    let request_id = request.request_id;
    let notification = request_id.is_nil();
    let session_id = request.session_id;
    let runtime_id = request.runtime_id;
    let task_catalog_action = task_catalog_action(&request.command);
    let starts_runtime = matches!(
        &request.command,
        Command::Start { .. } | Command::OpenTerminal { .. }
    );
    let (outcome, executed) = if !notification && let Some(cached) = hub.cached_response(request_id)
    {
        (cached, false)
    } else {
        if starts_runtime {
            hub.begin_runtime(session_id, runtime_id);
        }
        let handled = if let Command::Automation { command } = &request.command {
            Ok(ResponsePayload::Automation {
                result: hub.automation.command(
                    &hub,
                    &backend,
                    command.clone(),
                    source_subscriber_id,
                ),
            })
        } else {
            hub.diagnostic_command(&request.command)
                .unwrap_or_else(|| backend.handle(request, hub.event_sink(session_id, runtime_id)))
        };
        let outcome = match handled {
            Ok(payload) => ResponseOutcome::Ok { payload },
            Err(error) => {
                let error = RpcError::from(error);
                // A failed request is the daemon's own record of the failure:
                // it names what the daemon could not do for this session, next to
                // the app's and the provider's records in the same stream.
                hub.event_sink(session_id, runtime_id).record(|| {
                    Some(
                        DiagnosticRecord::new(
                            DiagnosticLevel::Error,
                            DiagnosticOrigin::Daemon {
                                pid: std::process::id(),
                            },
                            "request",
                            error.message.clone(),
                        )
                        .with_correlation(
                            crate::protocol::DiagnosticCorrelation {
                                session_id: (!session_id.is_nil()).then_some(session_id),
                                ..Default::default()
                            },
                        ),
                    )
                });
                ResponseOutcome::Error { error }
            }
        };
        if !notification {
            hub.cache_response(request_id, outcome.clone());
        }
        (outcome, true)
    };
    if executed && starts_runtime && matches!(&outcome, ResponseOutcome::Error { .. }) {
        hub.end_runtime(session_id, Some(runtime_id));
    }
    if executed {
        match (&task_catalog_action, &outcome) {
            (
                TaskCatalogAction::Load,
                ResponseOutcome::Ok {
                    payload:
                        ResponsePayload::TaskState {
                            projects,
                            sessions,
                            tasks,
                            ..
                        },
                },
            ) => hub.replace_task_catalog(projects, sessions, tasks),
            (
                TaskCatalogAction::Save { projects, tasks },
                ResponseOutcome::Ok {
                    payload: ResponsePayload::TaskStateSaved { sessions },
                },
            ) => hub.task_state_saved(source_subscriber_id, projects, sessions, tasks),
            (TaskCatalogAction::Changed, ResponseOutcome::Ok { .. }) => {
                hub.task_state_changed(source_subscriber_id);
            }
            _ => {}
        }
    }
    if !notification {
        let _ = outgoing.send(ServerMessage::Response {
            request_id,
            outcome: outcome.clone(),
        });
    }
    #[cfg(debug_assertions)]
    {
        let elapsed = started_at.elapsed().as_secs_f64() * 1000.0;
        let mut state = hub.state.lock();
        state.requests_completed = state.requests_completed.saturating_add(1);
        state.request_latency_ms = elapsed;
        state.request_latency_max_ms = state.request_latency_max_ms.max(elapsed);
    }
    HandledRequest { outcome, executed }
}

fn task_catalog_action(command: &Command) -> TaskCatalogAction {
    match command {
        Command::LoadTaskState => TaskCatalogAction::Load,
        Command::SaveTaskState {
            projects, tasks, ..
        } => TaskCatalogAction::Save {
            projects: projects.clone(),
            tasks: tasks.clone(),
        },
        Command::RemoveSession
        | Command::ForkSessionFromResponse { .. }
        | Command::RewindSessionToMessage { .. } => TaskCatalogAction::Changed,
        _ => TaskCatalogAction::None,
    }
}

fn send_dispatch_error(
    request_id: Uuid,
    outgoing: Sender<ServerMessage>,
    hub: &Arc<Hub>,
    message: String,
) {
    if request_id.is_nil() {
        return;
    }
    let outcome = hub
        .cached_response(request_id)
        .unwrap_or_else(|| ResponseOutcome::Error {
            error: RpcError { message },
        });
    hub.cache_response(request_id, outcome.clone());
    let _ = outgoing.send(ServerMessage::Response {
        request_id,
        outcome,
    });
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

fn read_client_message(socket: &mut WebSocket<TcpStream>) -> anyhow::Result<ClientMessage> {
    loop {
        match socket.read()? {
            Message::Text(text) => return Ok(serde_json::from_str(text.as_ref())?),
            Message::Ping(_) => socket.flush()?,
            Message::Close(_) => bail!("client closed during daemon handshake"),
            _ => {}
        }
    }
}

fn write_json<S: io::Read + io::Write, T: serde::Serialize>(
    socket: &mut WebSocket<S>,
    value: &T,
) -> anyhow::Result<()> {
    socket.send(Message::Text(serde_json::to_string(value)?.into()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::daemon::WakuBackend;
    #[cfg(unix)]
    use crate::model::Project;
    use crate::model::{AgentSession, ProviderKind, RuntimeMode};
    use crate::model::{Task, TaskState, TaskWorkspace};
    #[cfg(unix)]
    use crate::persistence::StateStore;
    #[cfg(unix)]
    use crate::settings::DaemonSettingsStore;
    use crate::{DaemonSettings, WireDriverStartOptions};
    #[cfg(unix)]
    use base64::Engine as _;
    use crossbeam_channel::{RecvTimeoutError, bounded};
    use serde_json::json;
    use std::path::PathBuf;
    use waku_client::{DaemonClient, DaemonSupervisor, DiagnosticMessage};

    /// Opt-in real transport coverage. Supply an official pinned Pi executable;
    /// no credentials, user state, prompt, or model request reaches the child.
    #[cfg(unix)]
    #[test]
    #[ignore = "requires WAKU_TEST_PI_BINARY pointing to the official pinned Pi executable"]
    fn confirmed_shutdown_real_pi_exits_without_a_model_prompt() {
        use std::os::unix::fs::PermissionsExt;
        let binary = std::env::var("WAKU_TEST_PI_BINARY").expect("set WAKU_TEST_PI_BINARY");
        let root = std::env::temp_dir().join(format!("waku-confirmed-pi-{}", Uuid::new_v4()));
        let home = root.join("home");
        let cwd = root.join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let quote = |value: &str| format!("'{}'", value.replace('\'', "'\"'\"'"));
        let wrapper = root.join("pi-isolated");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nexec env -i HOME={} PATH={} PI_SKIP_VERSION_CHECK=1 {} \"$@\"\n",
                quote(&home.to_string_lossy()),
                quote(&std::env::var("PATH").unwrap()),
                quote(&binary),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("app.db")),
        )
        .unwrap();
        let hub = Arc::new(Hub::default());
        let session = Uuid::new_v4();
        let runtime = Uuid::new_v4();
        hub.begin_runtime(session, runtime);
        let request = |command| Request {
            request_id: Uuid::new_v4(),
            session_id: session,
            runtime_id: runtime,
            command,
        };
        let mut options = test_start_options();
        options.provider = "pi".into();
        options.binary = wrapper;
        options.cwd = cwd;
        assert!(matches!(
            backend
                .handle(
                    request(Command::Start { options }),
                    hub.event_sink(session, runtime)
                )
                .unwrap(),
            ResponsePayload::Started { .. }
        ));
        assert!(matches!(
            backend
                .handle(
                    request(Command::CloseSessionAndWait),
                    hub.event_sink(session, runtime)
                )
                .unwrap(),
            ResponsePayload::Ack
        ));
        assert!(matches!(
            backend
                .handle(
                    request(Command::AttachSession),
                    hub.event_sink(session, runtime)
                )
                .unwrap(),
            ResponsePayload::SessionRuntime {
                runtime_id: None,
                ..
            }
        ));
        backend.shutdown();
        drop(backend);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[derive(Default)]
    struct TestBackend {
        runtimes: Mutex<HashMap<Uuid, Uuid>>,
    }

    impl Backend for TestBackend {
        fn handle(&self, request: Request, events: EventSink) -> anyhow::Result<ResponsePayload> {
            let session_id = request.session_id;
            let runtime_id = request.runtime_id;
            match request.command {
                Command::Start { .. } => {
                    self.runtimes.lock().insert(session_id, runtime_id);
                    events.send(WireDriverEvent::new("connected", json!({})))?;
                    Ok(ResponsePayload::Started {
                        supports_steer: true,
                    })
                }
                Command::AttachSession => Ok(ResponsePayload::SessionRuntime {
                    runtime_id: self.runtimes.lock().get(&session_id).copied(),
                    supports_steer: true,
                }),
                Command::GetSettings => Ok(ResponsePayload::Settings {
                    settings: DaemonSettings::default(),
                }),
                Command::Prompt { prompt, .. } => {
                    events.send(WireDriverEvent::new("textDelta", json!(prompt)))?;
                    Ok(ResponsePayload::Ack)
                }
                Command::CloseSession => {
                    self.runtimes.lock().remove(&session_id);
                    Ok(ResponsePayload::Ack)
                }
                _ => Ok(ResponsePayload::Ack),
            }
        }
    }

    #[derive(Default)]
    struct TaskStateBackend {
        sessions: Mutex<Vec<AgentSession>>,
    }
    impl Backend for TaskStateBackend {
        fn handle(&self, request: Request, _events: EventSink) -> anyhow::Result<ResponsePayload> {
            match request.command {
                Command::SaveTaskState { sessions, .. } => {
                    let mut stored = self.sessions.lock();
                    for session in sessions {
                        if let Some(existing) =
                            stored.iter_mut().find(|existing| existing.id == session.id)
                        {
                            *existing = session;
                        } else {
                            stored.push(session);
                        }
                    }
                    Ok(ResponsePayload::TaskStateSaved {
                        sessions: stored.clone(),
                    })
                }
                Command::LoadTaskState => Ok(ResponsePayload::TaskState {
                    projects: Vec::new(),
                    tasks: Vec::new(),
                    sessions: self.sessions.lock().clone(),
                    default_cwd: PathBuf::from("/tmp"),
                    projectless_root: Some(PathBuf::from("/tmp/.waku/projects")),
                }),
                _ => Ok(ResponsePayload::Ack),
            }
        }
    }

    #[test]
    fn authorized_shutdown_survives_a_failed_acknowledgment() {
        shutdown_with_failed_acknowledgment(true);
    }

    #[test]
    fn denied_shutdown_stays_denied_when_rejection_write_fails() {
        shutdown_with_failed_acknowledgment(false);
    }

    fn shutdown_with_failed_acknowledgment(allow_shutdown: bool) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let (streams, server_stream) = bounded(1);
        let (finished, completion) = bounded(1);
        let connection = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            streams.send(stream.try_clone().unwrap()).unwrap();
            let hub = Arc::new(Hub::default());
            let dispatcher = Arc::new(RequestDispatcher::new(
                Arc::new(TestBackend::default()),
                hub.clone(),
            ));
            let result = handle_connection(
                stream,
                "secret",
                dispatcher,
                hub,
                server_shutdown,
                &ServerOptions {
                    allow_shutdown,
                    ..Default::default()
                },
            );
            finished.send(result).unwrap();
        });
        let stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let (mut socket, _) = tungstenite::client(format!("ws://{address}/v1"), stream).unwrap();
        write_json(
            &mut socket,
            &ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                client_id: Uuid::new_v4(),
                token: "secret".into(),
                resume_from: vec![],
                diagnostic_from: None,
            },
        )
        .unwrap();
        let hello: ServerMessage =
            serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
        assert!(matches!(hello, ServerMessage::Hello { .. }));
        // Subscription setup queues diagnostic controls after Hello. A Pong
        // proves those writes were drained before we break only the reply.
        socket.send(Message::Ping(Vec::new().into())).unwrap();
        while !matches!(socket.read().unwrap(), Message::Pong(_)) {}

        // Keep client -> server reads working, but deterministically fail the
        // reply. Closing the client itself could discard its shutdown request.
        let server_stream = server_stream.recv_timeout(Duration::from_secs(3)).unwrap();
        server_stream.shutdown(std::net::Shutdown::Write).unwrap();
        write_json(&mut socket, &ClientMessage::Shutdown).unwrap();
        let result = completion
            .recv_timeout(Duration::from_secs(3))
            .expect("shutdown handler did not finish");
        connection.join().unwrap();
        assert!(
            result.is_err(),
            "fixture must fail the acknowledgment write"
        );
        assert_eq!(
            shutdown.load(Ordering::Acquire),
            allow_shutdown,
            "an accepted shutdown must not depend on delivering its reply"
        );
    }

    #[test]
    fn diagnostic_request_timing_respects_build_mode() {
        let hub = Arc::new(Hub::default());
        let (outgoing, _responses) = unbounded();
        handle_request(
            Request {
                request_id: Uuid::new_v4(),
                session_id: Uuid::nil(),
                runtime_id: Uuid::nil(),
                command: Command::GetSettings,
            },
            outgoing,
            0,
            Arc::new(TestBackend::default()),
            hub.clone(),
        );
        {
            let state = hub.state.lock();
            assert_eq!(state.requests_completed, u64::from(cfg!(debug_assertions)));
            if !cfg!(debug_assertions) {
                assert_eq!(state.request_latency_ms, 0.0);
                assert_eq!(state.request_latency_max_ms, 0.0);
            }
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(TestBackend::default()),
                Arc::new(AtomicBool::new(false)),
                ServerOptions {
                    allow_shutdown: true,
                    ..Default::default()
                },
            )
            .unwrap()
        });
        let client = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        assert_eq!(client.last_request_round_trip_ms(), 0.0);
        client
            .request(Uuid::nil(), Uuid::nil(), Command::GetSettings)
            .unwrap();
        if cfg!(debug_assertions) {
            assert!(client.last_request_round_trip_ms() > 0.0);
        } else {
            assert_eq!(client.last_request_round_trip_ms(), 0.0);
        }
        client.shutdown();
        server.join().unwrap();
    }

    #[test]
    fn diagnostic_query_surface_respects_build_mode() {
        let hub = Hub::default();
        let commands = [
            Command::DiagnosticLogs {
                query: Default::default(),
            },
            Command::DiagnosticTimeline {
                query: waku_protocol::DiagnosticTimelineQuery {
                    session_id: Some(Uuid::new_v4()),
                    turn_id: None,
                },
            },
            Command::DiagnosticCounters,
            Command::DiagnosticVerbosity {
                update: Some(Default::default()),
            },
        ];
        for command in commands {
            let result = hub.diagnostic_command(&command).unwrap();
            assert_eq!(result.is_ok(), cfg!(debug_assertions));
            if !cfg!(debug_assertions) {
                assert!(result.unwrap_err().to_string().contains("release builds"));
            }
        }
        hub.record_diagnostic(diagnostic_record("build gate"));
        if !cfg!(debug_assertions) {
            assert!(hub.state.lock().diagnostics.is_empty());
        }
    }

    #[test]
    #[cfg(debug_assertions)]
    fn diagnostic_queries_filter_at_ingestion_and_preserve_stale_counter_sample_times() {
        use waku_protocol::{
            DiagnosticCounterReport, DiagnosticProcess, DiagnosticVerbosityOverride,
        };
        let hub = Hub::default();
        let mut detail = diagnostic_record("detail");
        detail.level = DiagnosticLevel::Debug;
        detail.source = "pump".into();
        hub.record_diagnostic(detail.clone());
        assert_eq!(hub.state.lock().next_diagnostic_sequence, 0);
        let verbosity = DiagnosticVerbosity {
            default_level: DiagnosticLevel::Info,
            overrides: vec![DiagnosticVerbosityOverride {
                process: Some(DiagnosticProcess::App),
                source: Some("pump".into()),
                level: DiagnosticLevel::Debug,
            }],
        };
        hub.diagnostic_command(&Command::DiagnosticVerbosity {
            update: Some(verbosity),
        })
        .unwrap()
        .unwrap();
        hub.record_diagnostic(detail.clone());
        let mut other = detail.clone();
        other.source = "render".into();
        hub.record_diagnostic(other);
        assert_eq!(hub.state.lock().next_diagnostic_sequence, 1);
        hub.diagnostic_command(&Command::DiagnosticVerbosity {
            update: Some(Default::default()),
        })
        .unwrap()
        .unwrap();
        hub.record_diagnostic(detail);
        assert_eq!(hub.state.lock().next_diagnostic_sequence, 1);
        for count in [1.0, 2.0] {
            hub.report_diagnostic_counters(DiagnosticCounterReport {
                timestamp_ms: 123,
                origin: DiagnosticOrigin::App {
                    pid: 1,
                    version: "test".into(),
                },
                values: [("framesDrawn".into(), count)].into(),
            });
            let ResponsePayload::DiagnosticCounters { result } = hub
                .diagnostic_command(&Command::DiagnosticCounters)
                .unwrap()
                .unwrap()
            else {
                panic!("counters")
            };
            assert_eq!(result.processes.len(), 2);
            assert_eq!(result.processes[0].values["framesDrawn"], count);
            assert_eq!(result.processes[0].timestamp_ms, 123);
            assert!(result.observed_at_ms > 123);
            assert_eq!(result.processes[1].values["diagnosticFiltered"], 3.0);
        }
    }

    #[test]
    #[cfg(debug_assertions)]
    fn diagnostic_counter_retention_and_values_are_bounded() {
        let hub = Hub::default();
        for pid in 1..=140 {
            let mut values: std::collections::BTreeMap<_, _> = (0..200)
                .map(|i| (format!("counter-{i}"), i as f64))
                .collect();
            values.insert("invalid".into(), f64::NAN);
            values.insert("x".repeat(129), 1.0);
            hub.report_diagnostic_counters(DiagnosticCounterReport {
                timestamp_ms: pid as u64,
                origin: DiagnosticOrigin::App {
                    pid,
                    version: "test".into(),
                },
                values,
            });
        }
        let state = hub.state.lock();
        assert_eq!(state.diagnostic_reports.len(), 128);
        assert_eq!(state.diagnostic_reports.front().unwrap().timestamp_ms, 13);
        assert!(
            state
                .diagnostic_reports
                .iter()
                .all(|r| r.values.len() <= 128 && r.values.values().all(|v| v.is_finite()))
        );
    }

    #[test]
    #[cfg(debug_assertions)]
    fn diagnostic_client_queries_and_runtime_policy_work_over_the_existing_socket() {
        use waku_protocol::{
            DiagnosticProcess, DiagnosticQuery, DiagnosticTimelineQuery,
            DiagnosticVerbosityOverride,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(TestBackend::default()),
                Arc::new(AtomicBool::new(false)),
                ServerOptions {
                    allow_shutdown: true,
                    ..Default::default()
                },
            )
            .unwrap()
        });
        let client = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let app = DiagnosticOrigin::App {
            pid: 42,
            version: "test".into(),
        };
        assert!(!client.diagnostic_enabled(DiagnosticLevel::Debug, &app, "pump"));
        let policy = DiagnosticVerbosity {
            default_level: DiagnosticLevel::Info,
            overrides: vec![DiagnosticVerbosityOverride {
                process: Some(DiagnosticProcess::App),
                source: Some("pump".into()),
                level: DiagnosticLevel::Debug,
            }],
        };
        assert_eq!(
            client.diagnostic_verbosity(Some(policy.clone())).unwrap(),
            policy
        );
        assert!(client.diagnostic_enabled(DiagnosticLevel::Debug, &app, "pump"));
        // A new connection sees the current policy without a separate query.
        let observer = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        observer.diagnostic_counters().unwrap(); // socket-order barrier for initial policy
        assert!(observer.diagnostic_enabled(DiagnosticLevel::Debug, &app, "pump"));
        let session_id = Uuid::new_v4();
        let turn_id = Uuid::new_v4();
        for (source, message, level) in [
            ("turn", "submitted", DiagnosticLevel::Info),
            ("pump", "commit", DiagnosticLevel::Debug),
            ("turn", "streaming", DiagnosticLevel::Info),
        ] {
            client
                .record_diagnostic(
                    DiagnosticRecord::new(level, app.clone(), source, message).with_correlation(
                        waku_protocol::DiagnosticCorrelation {
                            session_id: Some(session_id),
                            turn_id: Some(turn_id),
                            ..Default::default()
                        },
                    ),
                )
                .unwrap();
        }
        client
            .report_diagnostic_counters(DiagnosticCounterReport {
                timestamp_ms: 123,
                origin: app.clone(),
                values: [("framesDrawn".into(), 10.0), ("pumpCommits".into(), 2.0)].into(),
            })
            .unwrap();
        let logs = client
            .diagnostic_logs(DiagnosticQuery {
                session_id: Some(session_id),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(logs.events.len(), 3);
        let timeline = client
            .diagnostic_timeline(DiagnosticTimelineQuery {
                session_id: Some(session_id),
                turn_id: Some(turn_id),
            })
            .unwrap();
        assert!(!timeline.complete);
        assert_eq!(
            timeline.last_phase,
            Some(waku_protocol::DiagnosticPhase::Streaming)
        );
        let first = client.diagnostic_counters().unwrap();
        assert_eq!(first.processes[0].values["framesDrawn"], 10.0);
        client
            .report_diagnostic_counters(DiagnosticCounterReport {
                timestamp_ms: 124,
                origin: app.clone(),
                values: [("framesDrawn".into(), 11.0), ("pumpCommits".into(), 3.0)].into(),
            })
            .unwrap();
        let second = client.diagnostic_counters().unwrap();
        assert!(
            second.processes[0].values["framesDrawn"] > first.processes[0].values["framesDrawn"]
        );
        assert!(
            second.processes[0].values["pumpCommits"] > first.processes[0].values["pumpCommits"]
        );
        assert!(
            second.processes.last().unwrap().values["requestsCompleted"]
                > first.processes.last().unwrap().values["requestsCompleted"]
        );
        assert!(client.last_request_round_trip_ms() > 0.0);
        client
            .diagnostic_verbosity(Some(Default::default()))
            .unwrap();
        observer.diagnostic_counters().unwrap();
        assert!(!observer.diagnostic_enabled(DiagnosticLevel::Debug, &app, "pump"));
        client
            .record_diagnostic(DiagnosticRecord::new(
                DiagnosticLevel::Debug,
                app,
                "pump",
                "hidden",
            ))
            .unwrap();
        assert!(
            client
                .diagnostic_logs(DiagnosticQuery {
                    source: Some("pump".into()),
                    ..Default::default()
                })
                .unwrap()
                .events
                .iter()
                .all(|e| e.record.message != "hidden")
        );
        client.shutdown();
        server.join().unwrap();
    }

    #[test]
    fn a_failed_request_becomes_the_daemons_own_record() {
        struct ErrorBackend;

        impl Backend for ErrorBackend {
            fn handle(
                &self,
                _request: Request,
                _events: EventSink,
            ) -> anyhow::Result<ResponsePayload> {
                anyhow::bail!("the provider refused the turn")
            }
        }

        let hub = Arc::new(Hub::default());
        let session_id = Uuid::new_v4();
        let (outgoing, responses) = unbounded();
        handle_request(
            Request {
                request_id: Uuid::new_v4(),
                session_id,
                runtime_id: Uuid::new_v4(),
                command: Command::GetSettings,
            },
            outgoing,
            0,
            Arc::new(ErrorBackend),
            hub.clone(),
        );
        assert!(matches!(
            responses.try_recv(),
            Ok(ServerMessage::Response {
                outcome: ResponseOutcome::Error { .. },
                ..
            })
        ));

        let (messages, stream) = unbounded();
        hub.subscribe(&[], None, Subscriber::new(messages).0);
        let ServerMessage::Diagnostic(event) = stream.recv().unwrap() else {
            panic!("expected the daemon's own record");
        };
        assert_eq!(event.record.level, DiagnosticLevel::Error);
        assert_eq!(event.record.source, "request");
        assert_eq!(event.record.message, "the provider refused the turn");
        assert_eq!(event.record.correlation.session_id, Some(session_id));
        assert_eq!(
            event.record.origin,
            DiagnosticOrigin::Daemon {
                pid: std::process::id(),
            }
        );
    }

    #[test]
    fn task_state_revisions_notify_other_clients_only() {
        let hub = Hub::default();
        let (source_tx, source_rx) = unbounded();
        let source_id = hub.subscribe(&[], None, Subscriber::new(source_tx).0);
        let (observer_tx, observer_rx) = unbounded();
        hub.subscribe(&[], None, Subscriber::new(observer_tx).0);

        hub.task_state_changed(source_id);

        assert!(source_rx.try_recv().is_err());
        assert!(matches!(
            observer_rx.recv_timeout(Duration::from_secs(1)),
            Ok(ServerMessage::TaskStateChanged { revision: 1 })
        ));
    }

    #[test]
    fn websocket_task_state_changes_reach_another_client() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(TaskStateBackend::default()),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let source = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let observer = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let source_revisions = source.subscribe_task_state();
        let observer_revisions = observer.subscribe_task_state();
        // The handshake can finish before the server registers its subscriber.
        // Complete the observer's initial load before another client publishes.
        assert!(matches!(
            observer
                .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
                .unwrap(),
            ResponsePayload::TaskState { .. }
        ));
        let session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        let session_id = session.id;

        assert!(matches!(
            source
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    Command::SaveTaskState {
                        projects: Vec::new(),
                        tasks: Vec::new(),
                        live_session_ids: vec![session_id],
                        skeleton_session_ids: Vec::new(),
                        sessions: vec![session],
                    },
                )
                .unwrap(),
            ResponsePayload::TaskStateSaved { .. }
        ));
        assert_eq!(
            observer_revisions.recv_timeout(Duration::from_secs(1)),
            Ok(1)
        );
        assert!(source_revisions.try_recv().is_err());
        let ResponsePayload::TaskState { sessions, .. } = observer
            .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
            .unwrap()
        else {
            panic!("expected daemon task state");
        };
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session_id);

        // Streaming checkpoints update transcript detail and `updated_at`,
        // but do not change anything rendered in another client's task
        // catalog. They must not trigger a list reload for every stream save.
        let mut checkpoint = sessions[0].clone();
        checkpoint.updated_at = checkpoint.updated_at.saturating_add(1);
        source
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: Vec::new(),
                    tasks: Vec::new(),
                    live_session_ids: vec![session_id],
                    skeleton_session_ids: Vec::new(),
                    sessions: vec![checkpoint],
                },
            )
            .unwrap();
        assert!(
            observer_revisions
                .recv_timeout(Duration::from_millis(100))
                .is_err()
        );

        // Desktop persistence uses fire-and-forget notifications, while Web
        // uses requests. Both directions must wake the other application's
        // catalog without echoing back to the source connection.
        let second = AgentSession::new(Uuid::new_v4(), ProviderKind::Claude);
        let second_id = second.id;
        observer
            .notify(
                Uuid::nil(),
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: Vec::new(),
                    tasks: Vec::new(),
                    live_session_ids: vec![session_id, second_id],
                    skeleton_session_ids: Vec::new(),
                    sessions: vec![second],
                },
            )
            .unwrap();
        assert_eq!(source_revisions.recv_timeout(Duration::from_secs(1)), Ok(2));
        assert!(observer_revisions.try_recv().is_err());
        let ResponsePayload::TaskState { sessions, .. } = source
            .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
            .unwrap()
        else {
            panic!("expected daemon task state");
        };
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|session| session.id == second_id));

        source.shutdown();
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn stale_projection_cannot_resurrect_a_removed_session() {
        let root = std::env::temp_dir().join(format!("waku-remove-race-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("app.db")),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(backend),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let stale_client = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let remover = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let project = Project::from_path(root.join("repo"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);
        session.begin_turn("persist me");
        stale_client
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: vec![project.clone()],
                    tasks: Vec::new(),
                    live_session_ids: vec![session.id],
                    skeleton_session_ids: Vec::new(),
                    sessions: vec![session.clone()],
                },
            )
            .unwrap();
        remover
            .request(session.id, Uuid::nil(), Command::RemoveSession)
            .unwrap();
        let ResponsePayload::TaskStateSaved { sessions } = stale_client
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::SaveTaskState {
                    projects: vec![project],
                    tasks: Vec::new(),
                    live_session_ids: vec![session.id],
                    skeleton_session_ids: Vec::new(),
                    sessions: vec![session],
                },
            )
            .unwrap()
        else {
            panic!("expected task-state save response");
        };
        assert!(sessions.is_empty());
        let ResponsePayload::TaskState { sessions, .. } = stale_client
            .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
            .unwrap()
        else {
            panic!("expected task state");
        };
        assert!(sessions.is_empty());

        stale_client.shutdown();
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A task that was running when the app died must survive the next launch.
    ///
    /// The restarted client interrupts the orphaned task without hydrating it,
    /// so the save carries a list projection. `detail_loaded` never crosses the
    /// wire, and without the skeleton ids that empty shell replaced the stored
    /// transcript and the task's rows were deleted as if the client had
    /// removed it.
    #[cfg(unix)]
    #[test]
    fn a_projection_save_after_restart_keeps_the_interrupted_task() {
        let root = std::env::temp_dir().join(format!("waku-skeleton-save-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let database = root.join("app.db");
        let hub = Arc::new(Hub::default());

        let project = Project::from_path(root.join("repo"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);
        session.status = SessionStatus::Working;
        session.begin_turn("keep this task");
        session.push_message(crate::model::MessageRole::Assistant, "partial answer");
        session.provider_cursor = Some(crate::model::ProviderResumeCursor::Codex {
            thread_id: "thread".into(),
        });
        let session_id = session.id;

        let save = |backend: &WakuBackend,
                    sessions: Vec<AgentSession>,
                    skeleton_session_ids: Vec<Uuid>| {
            backend
                .handle(
                    Request {
                        request_id: Uuid::nil(),
                        session_id: Uuid::nil(),
                        runtime_id: Uuid::nil(),
                        command: Command::SaveTaskState {
                            projects: vec![project.clone()],
                            tasks: Vec::new(),
                            live_session_ids: vec![session_id],
                            skeleton_session_ids,
                            sessions,
                        },
                    },
                    hub.event_sink(Uuid::nil(), Uuid::nil()),
                )
                .unwrap()
        };

        // The first daemon persists the task while it is still running.
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(database.clone()),
        )
        .unwrap();
        save(&backend, vec![session.clone()], Vec::new());
        drop(backend);

        // The app and daemon restart. The client loads the list and interrupts
        // the task it can no longer attach to, all while holding a skeleton.
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(database.clone()),
        )
        .unwrap();
        let ResponsePayload::TaskState { mut sessions, .. } = backend
            .handle(
                Request {
                    request_id: Uuid::nil(),
                    session_id: Uuid::nil(),
                    runtime_id: Uuid::nil(),
                    command: Command::LoadTaskState,
                },
                hub.event_sink(Uuid::nil(), Uuid::nil()),
            )
            .unwrap()
        else {
            panic!("expected daemon task state");
        };
        assert_eq!(sessions.len(), 1, "the stored task is listed");
        let mut skeleton = sessions.remove(0);
        assert!(!skeleton.detail_loaded);
        skeleton.status = SessionStatus::Idle;
        let skeleton_id = skeleton.id;
        // The projection crosses the wire without its process-local marker; the
        // client names it in the save so the daemon can restore the marker.
        let wire: AgentSession =
            serde_json::from_slice(&serde_json::to_vec(&skeleton).unwrap()).unwrap();
        assert!(wire.detail_loaded, "the marker is process-local");
        save(&backend, vec![wire], vec![skeleton_id]);
        drop(backend);

        // Nothing about the task's stored detail was overwritten or removed.
        let store = StateStore::daemon(database.clone());
        let mut state = store.load().unwrap();
        assert_eq!(state.sessions.len(), 1, "the interrupted task kept its row");
        let mut restored = state.sessions.remove(0);
        assert_eq!(restored.status, SessionStatus::Idle);
        store.hydrate(&mut restored).unwrap();
        assert_eq!(restored.turns.len(), 1);
        assert_eq!(restored.messages.len(), 2);
        assert!(restored.provider_cursor.is_some());

        std::fs::remove_dir_all(root).unwrap();
    }

    /// A declared task is daemon-owned: the save must persist it, and the
    /// task-state projection must carry it back with the conversation filed
    /// under it.
    #[cfg(unix)]
    #[test]
    fn a_declared_task_survives_the_save_and_returns_in_the_task_state_projection() {
        let root = std::env::temp_dir().join(format!("waku-task-roundtrip-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let database = root.join("app.db");
        let hub = Arc::new(Hub::default());
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(database.clone()),
        )
        .unwrap();
        let sink = || hub.event_sink(Uuid::nil(), Uuid::nil());
        let request = |command: Command| Request {
            request_id: Uuid::nil(),
            session_id: Uuid::nil(),
            runtime_id: Uuid::nil(),
            command,
        };

        let project = Project::from_path(root.join("repo"));
        let mut task = Task::new("Fix the thing", TaskState::Doing, 0);
        task.workspace = Some(TaskWorkspace::Worktree {
            project_id: project.id,
            path: root.join("worktree"),
            branch: "feat/fix".into(),
        });
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);
        session.begin_turn("ask once");
        session.task_id = Some(task.id);
        let task_id = task.id;
        let session_id = session.id;

        backend
            .handle(
                request(Command::SaveTaskState {
                    projects: vec![project.clone()],
                    tasks: vec![task.clone()],
                    live_session_ids: vec![session_id],
                    skeleton_session_ids: Vec::new(),
                    sessions: vec![session.clone()],
                }),
                sink(),
            )
            .unwrap();

        let ResponsePayload::TaskState {
            sessions, tasks, ..
        } = backend
            .handle(request(Command::LoadTaskState), sink())
            .unwrap()
        else {
            panic!("expected daemon task state");
        };
        assert_eq!(tasks, vec![task.clone()]);
        assert_eq!(sessions[0].task_id, Some(task_id));

        // A rebuilt process reads the task back out of SQLite.
        let stored = StateStore::daemon(database).load().unwrap();
        assert_eq!(stored.tasks, vec![task]);
        assert_eq!(stored.sessions.len(), 1);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn stale_issue_snapshots_cannot_erase_a_newer_brief() {
        let root = std::env::temp_dir().join(format!("waku-issue-order-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let hub = Arc::new(Hub::default());
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("app.db")),
        )
        .unwrap();
        let save = |task: Task| {
            backend
                .handle(
                    Request {
                        request_id: Uuid::new_v4(),
                        session_id: Uuid::nil(),
                        runtime_id: Uuid::nil(),
                        command: Command::SaveTaskState {
                            projects: Vec::new(),
                            tasks: vec![task],
                            live_session_ids: Vec::new(),
                            skeleton_session_ids: Vec::new(),
                            sessions: Vec::new(),
                        },
                    },
                    hub.event_sink(Uuid::nil(), Uuid::nil()),
                )
                .unwrap()
        };
        let original = Task::new("Repair parser", TaskState::Backlog, 0);
        save(original.clone());
        let mut edited = original.clone();
        edited.description = "Preserve this brief".into();
        edited.touch();
        save(edited.clone());
        save(original);
        let mut same_revision = edited.clone();
        same_revision.description.clear();
        save(same_revision);
        assert_eq!(
            StateStore::daemon(root.join("app.db"))
                .load()
                .unwrap()
                .tasks,
            vec![edited]
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn rejected_issue_write_does_not_become_authoritative_in_memory() {
        let root =
            std::env::temp_dir().join(format!("waku-issue-write-failure-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let hub = Arc::new(Hub::default());
        let database = root.join("app.db");
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(database.clone()),
        )
        .unwrap();
        let request = |command| Request {
            request_id: Uuid::new_v4(),
            session_id: Uuid::nil(),
            runtime_id: Uuid::nil(),
            command,
        };
        let command = |tasks| Command::SaveTaskState {
            projects: Vec::new(),
            tasks,
            live_session_ids: Vec::new(),
            skeleton_session_ids: Vec::new(),
            sessions: Vec::new(),
        };
        let original = Task::new("Repair parser", TaskState::Backlog, 0);
        backend
            .handle(
                request(command(vec![original.clone()])),
                hub.event_sink(Uuid::nil(), Uuid::nil()),
            )
            .unwrap();
        let connection = rusqlite::Connection::open(database).unwrap();
        connection.execute_batch("CREATE TRIGGER reject_issue BEFORE INSERT ON tasks BEGIN SELECT RAISE(ABORT, 'injected issue write failure'); END;").unwrap();
        let mut edited = original.clone();
        edited.description = "Uncommitted change".into();
        edited.touch();
        let added = Task::new("Not committed", TaskState::Doing, 1);
        assert!(
            backend
                .handle(
                    request(command(vec![edited, added])),
                    hub.event_sink(Uuid::nil(), Uuid::nil())
                )
                .is_err()
        );
        let ResponsePayload::TaskState { tasks, .. } = backend
            .handle(
                request(Command::LoadTaskState),
                hub.event_sink(Uuid::nil(), Uuid::nil()),
            )
            .unwrap()
        else {
            panic!("expected task state");
        };
        assert_eq!(tasks, vec![original]);
        std::fs::remove_dir_all(root).ok();
    }

    /// A composer pick made without a following turn must reach a rebuilt
    /// window. Both the pick and the rebuild's task-list load cross the daemon,
    /// so the pick is saved by id and the list projection has to carry the
    /// model, its traits, the runtime mode, and the agent preset back —
    /// otherwise the composer shows the defaults until `HydrateSession` lands
    /// and then jumps.
    #[cfg(unix)]
    #[test]
    fn a_pick_saved_without_a_turn_returns_the_composer_picks_in_the_rebuild_projection() {
        let root = std::env::temp_dir().join(format!("waku-pick-rebuild-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let database = root.join("app.db");
        let hub = Arc::new(Hub::default());
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(database.clone()),
        )
        .unwrap();
        let sink = || hub.event_sink(Uuid::nil(), Uuid::nil());
        let request = |command: Command| Request {
            request_id: Uuid::nil(),
            session_id: Uuid::nil(),
            runtime_id: Uuid::nil(),
            command,
        };

        let project = Project::from_path(root.join("repo"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);
        session.begin_turn("ask once");
        session.push_message(crate::model::MessageRole::Assistant, "answer");
        let session_id = session.id;
        let save = |session: AgentSession, skeletons: Vec<Uuid>| {
            backend
                .handle(
                    request(Command::SaveTaskState {
                        projects: vec![project.clone()],
                        tasks: Vec::new(),
                        live_session_ids: vec![session_id],
                        skeleton_session_ids: skeletons,
                        sessions: vec![session],
                    }),
                    sink(),
                )
                .unwrap()
        };
        save(session, Vec::new());

        // The selected session is hydrated, the pick is applied to it, and the
        // pick's save carries no skeleton id — the same shape every composer
        // picker handler produces.
        let ResponsePayload::Session {
            session: Some(picked),
        } = backend
            .handle(request(Command::HydrateSession { session_id }), sink())
            .unwrap()
        else {
            panic!("the stored task hydrates")
        };
        let mut picked = picked;
        picked.model = Some("gpt-5".into());
        picked.reasoning_effort = Some("high".into());
        picked.service_tier = Some("fast".into());
        picked.context_window = Some("1m".into());
        picked.runtime_mode = RuntimeMode::Ask;
        picked.agent_preset = Some("code".into());
        save(picked, Vec::new());

        let ResponsePayload::TaskState { sessions, .. } = backend
            .handle(request(Command::LoadTaskState), sink())
            .unwrap()
        else {
            panic!("expected daemon task state")
        };
        let projection = &sessions[0];
        assert!(!projection.detail_loaded, "the list is still a projection");
        assert_eq!(projection.model.as_deref(), Some("gpt-5"));
        assert_eq!(projection.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(projection.service_tier.as_deref(), Some("fast"));
        assert_eq!(projection.context_window.as_deref(), Some("1m"));
        assert_eq!(projection.runtime_mode, RuntimeMode::Ask);
        assert_eq!(projection.agent_preset.as_deref(), Some("code"));
        assert!(
            projection.messages.is_empty(),
            "the transcript stays detail the list never carries"
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    fn diagnostic_record(message: &str) -> DiagnosticRecord {
        DiagnosticRecord::new(
            crate::protocol::DiagnosticLevel::Info,
            crate::protocol::DiagnosticOrigin::App {
                pid: std::process::id(),
                version: "test".into(),
            },
            "turn",
            message,
        )
    }

    fn daemon_record(message: &str) -> DiagnosticRecord {
        DiagnosticRecord::new(
            crate::protocol::DiagnosticLevel::Info,
            crate::protocol::DiagnosticOrigin::Daemon {
                pid: std::process::id(),
            },
            "driver",
            message,
        )
    }

    #[test]
    fn combined_protocol_rejects_board_only_and_rpa_only_clients() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(TestBackend::default()),
                server_shutdown,
                ServerOptions::default(),
            )
            .unwrap();
        });
        for protocol_version in [13, 15, 16] {
            let (mut socket, _) = tungstenite::connect(format!("ws://{address}/v1")).unwrap();
            socket
                .send(Message::Text(
                    serde_json::to_string(&ClientMessage::Hello {
                        protocol_version,
                        client_id: Uuid::new_v4(),
                        token: "secret".into(),
                        resume_from: vec![],
                        diagnostic_from: None,
                    })
                    .unwrap()
                    .into(),
                ))
                .unwrap();
            let rejected: ServerMessage =
                serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
            assert!(
                matches!(rejected, ServerMessage::Rejected { message } if message.contains(&format!("expected {}", waku_protocol::PROTOCOL_VERSION)))
            );
        }
        let current = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        current.disconnect();
        shutdown.store(true, Ordering::Release);
        server.join().unwrap();
    }

    #[test]
    fn websocket_round_trip_sequences_provider_events() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(TestBackend::default()),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        assert!(
            DaemonClient::connect(&address.to_string(), "wrong-secret".into()).is_err(),
            "the server must reject a client before it can issue requests"
        );
        let client = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let session_id = Uuid::new_v4();
        let runtime_id = Uuid::new_v4();
        let response = client
            .request(
                session_id,
                runtime_id,
                Command::Start {
                    options: WireDriverStartOptions {
                        provider: "codex".into(),
                        binary: PathBuf::from("codex"),
                        cwd: PathBuf::from("."),
                        mode: "fullAccess".into(),
                        model: None,
                        reasoning_effort: None,
                        service_tier: None,
                        context_window: None,
                        agent_preset: None,
                        computer_use_enabled: false,
                        provider_cursor: None,
                    },
                },
            )
            .unwrap();
        assert!(matches!(
            response,
            ResponsePayload::Started {
                supports_steer: true
            }
        ));
        // Start can emit before a refreshed app discovers and subscribes to
        // the daemon-owned runtime. The client must retain that replay.
        let events = client.subscribe(session_id, runtime_id);
        let event = events.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(event.runtime_id, runtime_id);
        assert_eq!(event.sequence, 1);
        assert_eq!(event.event.kind, "connected");

        client.shutdown();
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(1)),
            Err(RecvTimeoutError::Disconnected)
        ));
        server.join().unwrap();
    }

    #[test]
    fn late_client_attaches_to_replay_and_live_runtime_events() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(TestBackend::default()),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let source = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let session_id = Uuid::new_v4();
        let runtime_id = Uuid::new_v4();
        let source_events = source.subscribe(session_id, runtime_id);
        source
            .request(
                session_id,
                runtime_id,
                Command::Start {
                    options: WireDriverStartOptions {
                        provider: "codex".into(),
                        binary: PathBuf::from("codex"),
                        cwd: PathBuf::from("."),
                        mode: "fullAccess".into(),
                        model: None,
                        reasoning_effort: None,
                        service_tier: None,
                        context_window: None,
                        agent_preset: None,
                        computer_use_enabled: false,
                        provider_cursor: None,
                    },
                },
            )
            .unwrap();
        assert_eq!(
            source_events
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .event
                .kind,
            "connected"
        );

        let late = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        assert!(matches!(
            late.request(session_id, Uuid::nil(), Command::AttachSession)
                .unwrap(),
            ResponsePayload::SessionRuntime {
                runtime_id: Some(attached),
                supports_steer: true,
            } if attached == runtime_id
        ));
        let late_events = late.subscribe(session_id, runtime_id);
        let replayed = late_events.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(replayed.sequence, 1);
        assert_eq!(replayed.event.kind, "connected");

        source
            .request(
                session_id,
                runtime_id,
                Command::Prompt {
                    prompt: "streamed from the first client".into(),
                    turn_id: None,
                    message_id: None,
                },
            )
            .unwrap();
        for events in [&source_events, &late_events] {
            let live = events.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(live.sequence, 2);
            assert_eq!(live.event.kind, "textDelta");
            assert_eq!(live.event.payload, json!("streamed from the first client"));
        }

        source
            .request(session_id, runtime_id, Command::CloseSession)
            .unwrap();
        let after_close = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        assert!(matches!(
            after_close
                .request(session_id, Uuid::nil(), Command::AttachSession)
                .unwrap(),
            ResponsePayload::SessionRuntime {
                runtime_id: None,
                supports_steer: true,
            }
        ));
        let stale_events = after_close.subscribe(session_id, runtime_id);
        assert!(
            stale_events
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "an explicitly closed runtime must not replay into future clients"
        );

        source.shutdown();
        server.join().unwrap();
    }

    #[test]
    fn the_stream_mixes_app_and_daemon_records_in_one_sequence() {
        let hub = Arc::new(Hub::default());
        let session_id = Uuid::new_v4();
        let runtime_id = Uuid::new_v4();
        let sink = hub.event_sink(session_id, runtime_id);
        hub.record_diagnostic(diagnostic_record("app first"));
        sink.record(|| Some(daemon_record("daemon second")));
        hub.record_diagnostic(diagnostic_record("app third"));

        let (messages, stream) = unbounded();
        hub.subscribe(&[], None, Subscriber::new(messages).0);
        let mut events = Vec::new();
        for _ in 0..3 {
            let ServerMessage::Diagnostic(event) = stream.recv().unwrap() else {
                panic!("expected a diagnostic record");
            };
            events.push(event);
        }
        assert_eq!(
            events
                .iter()
                .map(|event| (event.sequence, event.record.message.as_str()))
                .collect::<Vec<_>>(),
            [(1, "app first"), (2, "daemon second"), (3, "app third")]
        );
    }

    #[test]
    fn provider_stderr_reaches_the_stream_attributed_to_its_runtime() {
        let hub = Arc::new(Hub::default());
        hub.state.lock().diagnostic_verbosity.default_level = DiagnosticLevel::Debug;
        let session_id = Uuid::new_v4();
        let runtime_id = Uuid::new_v4();
        let diagnostics = crate::diagnostics::ProviderDiagnostics::new(
            crate::diagnostics::ProviderRuntime {
                provider: crate::model::ProviderKind::Pi,
                session_id,
                runtime_id,
            },
            {
                let sink = hub.event_sink(session_id, runtime_id);
                move |record| sink.record(|| Some(record))
            },
        );
        let (sender, _receiver) = crate::driver::event_channel_with_diagnostics(
            smol::channel::unbounded().0,
            Some(diagnostics),
        );
        sender.record_stderr("extension warning");

        let (messages, stream) = unbounded();
        hub.subscribe(&[], None, Subscriber::new(messages).0);
        let ServerMessage::Diagnostic(event) = stream.recv().unwrap() else {
            panic!("expected a diagnostic record");
        };
        assert_eq!(event.record.source, "stderr");
        assert_eq!(event.record.message, "extension warning");
        assert_eq!(event.record.correlation.session_id, Some(session_id));
        assert_eq!(
            event.record.origin,
            crate::protocol::DiagnosticOrigin::Provider {
                provider: crate::model::ProviderKind::Pi,
                runtime_id,
            }
        );
    }

    #[test]
    fn an_expired_cursor_is_named_and_the_retained_tail_still_replays() {
        let hub = Arc::new(Hub::default());
        let total = MAX_DIAGNOSTIC_EVENTS + 10;
        for sequence in 1..=total {
            hub.record_diagnostic(diagnostic_record(&format!("record {sequence}")));
        }
        let oldest = (total - MAX_DIAGNOSTIC_EVENTS + 1) as u64;
        let ResponsePayload::DiagnosticCounters { result } = hub
            .diagnostic_command(&Command::DiagnosticCounters)
            .unwrap()
            .unwrap()
        else {
            panic!("counters")
        };
        assert_eq!(result.processes[0].values["retentionDropped"], 10.0);
        assert_eq!(
            result.processes[0].values["diagnosticRetained"],
            MAX_DIAGNOSTIC_EVENTS as f64
        );

        // A cursor older than retention is told so, then sees exactly the
        // retained tail in order rather than a silent gap.
        let (messages, stream) = unbounded();
        hub.subscribe(
            &[],
            Some(DiagnosticCursor {
                epoch: hub.epoch,
                sequence: 1,
            }),
            Subscriber::new(messages).0,
        );
        let ServerMessage::DiagnosticCursorExpired { expiry } = stream.recv().unwrap() else {
            panic!("expected the expiry notice first");
        };
        assert_eq!(expiry.requested_sequence, 1);
        assert_eq!(expiry.oldest_sequence, oldest);
        let mut replayed = Vec::new();
        while let Ok(ServerMessage::Diagnostic(event)) = stream.try_recv() {
            replayed.push(event.sequence);
        }
        assert_eq!(replayed.len(), MAX_DIAGNOSTIC_EVENTS);
        assert_eq!(replayed.first().copied(), Some(oldest));
        assert_eq!(replayed.last().copied(), Some(total as u64));
        assert_eq!(replayed, (oldest..=total as u64).collect::<Vec<_>>());

        // A cursor still inside retention is not expired and replays the
        // records after it, once each.
        let (messages, stream) = unbounded();
        hub.subscribe(
            &[],
            Some(DiagnosticCursor {
                epoch: hub.epoch,
                sequence: total as u64 - 1,
            }),
            Subscriber::new(messages).0,
        );
        let ServerMessage::Diagnostic(event) = stream.recv().unwrap() else {
            panic!("a cursor inside retention must not expire");
        };
        assert_eq!(event.sequence, total as u64);
        assert!(stream.try_recv().is_err());

        // A fresh client has no cursor and reads the retained tail.
        let (messages, stream) = unbounded();
        hub.subscribe(&[], None, Subscriber::new(messages).0);
        let mut count = 0u64;
        while let Ok(ServerMessage::Diagnostic(event)) = stream.try_recv() {
            count += 1;
            assert_eq!(event.sequence, oldest + count - 1);
        }
        assert_eq!(count, MAX_DIAGNOSTIC_EVENTS as u64);
    }

    #[test]
    fn the_daemon_writes_ordered_ndjson_records_to_files() {
        let directory = std::env::temp_dir().join(format!("waku-hub-log-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let log = crate::diagnostics::DiagnosticLog::open(&directory).unwrap();
        let hub = Arc::new(Hub::new(Some(log)));
        for sequence in 1..=5 {
            hub.record_diagnostic(diagnostic_record(&format!("record {sequence}")));
        }
        hub.flush_diagnostics();

        let mut written = Vec::new();
        for entry in std::fs::read_dir(directory.join("diagnostics")).unwrap() {
            written.push(std::fs::read_to_string(entry.unwrap().path()).unwrap());
        }
        written.sort();
        let lines = written
            .join("")
            .lines()
            .map(|line| serde_json::from_str::<DiagnosticEvent>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            lines
                .iter()
                .map(|event| (event.sequence, event.record.message.as_str()))
                .collect::<Vec<_>>(),
            [
                (1, "record 1"),
                (2, "record 2"),
                (3, "record 3"),
                (4, "record 4"),
                (5, "record 5"),
            ]
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn unavailable_diagnostic_files_do_not_stop_the_daemon() {
        let root = std::env::temp_dir().join(format!("waku-bad-diagnostics-{}", Uuid::new_v4()));
        std::fs::write(&root, b"not a directory").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let diagnostics_directory = root.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(TestBackend::default()),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    diagnostics_directory: Some(diagnostics_directory),
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let client = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let stream = client.subscribe_diagnostics();
        let DiagnosticMessage::Event(event) = stream.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected the unavailable-files record");
        };
        assert_eq!(event.record.level, DiagnosticLevel::Warn);
        assert_eq!(event.record.source, "diagnostics");
        assert!(event.record.message.contains("unavailable"));

        client.shutdown();
        server.join().unwrap();
        std::fs::remove_file(&root).unwrap();
    }

    #[test]
    fn a_resumed_client_replays_diagnostic_records_without_gaps() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(TestBackend::default()),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let producer = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        for message in ["one", "two", "three", "four"] {
            producer
                .record_diagnostic(diagnostic_record(message))
                .unwrap();
        }
        // One request after the records: messages on one connection are
        // handled in order, so the answer proves the daemon sequenced them.
        producer
            .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
            .unwrap();

        // A client with no cursor reads the retained tail from the handshake.
        let fresh = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let fresh_stream = fresh.subscribe_diagnostics();
        let mut seen = Vec::new();
        for expected in ["one", "two", "three", "four"] {
            let message = fresh_stream.recv_timeout(Duration::from_secs(1)).unwrap();
            let DiagnosticMessage::Event(event) = message else {
                panic!("a fresh client must not see an expiry notice")
            };
            assert_eq!(event.record.message, expected);
            assert_eq!(event.sequence, seen.len() as u64 + 1);
            seen.push(event.record.message.clone());
        }
        assert_eq!(seen, ["one", "two", "three", "four"]);
        let cursor = fresh.last_diagnostic_cursor().unwrap();
        assert_eq!(cursor.sequence, 4);

        // A resumed client asks for what came after sequence two and reads
        // the rest once, in order.
        let resumed = DaemonClient::connect_with_cursors(
            &address.to_string(),
            "secret".into(),
            Vec::new(),
            Some(DiagnosticCursor {
                epoch: cursor.epoch,
                sequence: 2,
            }),
        )
        .unwrap();
        let resumed_stream = resumed.subscribe_diagnostics();
        for (index, expected) in ["three", "four"].into_iter().enumerate() {
            let message = resumed_stream.recv_timeout(Duration::from_secs(1)).unwrap();
            let DiagnosticMessage::Event(event) = message else {
                panic!("a cursor inside retention must not expire")
            };
            assert_eq!(event.record.message, expected);
            assert_eq!(event.sequence, index as u64 + 3);
        }
        assert_eq!(resumed.last_diagnostic_cursor().unwrap().sequence, 4);
        assert!(
            resumed_stream
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "the replay stops at the cursor's edge"
        );

        producer.shutdown();
        fresh.shutdown();
        resumed.shutdown();
        server.join().unwrap();
    }

    #[test]
    fn a_client_behind_retention_sees_the_expiry_notice_and_the_retained_tail() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(TestBackend::default()),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let producer = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let total = MAX_DIAGNOSTIC_EVENTS + 2;
        for sequence in 1..=total {
            producer
                .record_diagnostic(diagnostic_record(&format!("record {sequence}")))
                .unwrap();
        }
        // One request after the records: the answer proves the daemon
        // sequenced all of them before the client connects.
        producer
            .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
            .unwrap();
        let cursor = producer.last_diagnostic_cursor().unwrap();
        assert_eq!(cursor.sequence, total as u64);
        let epoch = cursor.epoch;

        let client = DaemonClient::connect_with_cursors(
            &address.to_string(),
            "secret".into(),
            Vec::new(),
            Some(DiagnosticCursor { epoch, sequence: 1 }),
        )
        .unwrap();
        let stream = client.subscribe_diagnostics();
        let DiagnosticMessage::Expired(expiry) =
            stream.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected the expiry notice first");
        };
        assert_eq!(expiry.requested_sequence, 1);
        assert_eq!(expiry.oldest_sequence, 3);
        let DiagnosticMessage::Event(event) = stream.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected the retained tail after the notice");
        };
        assert_eq!(event.sequence, expiry.oldest_sequence);
        // The reader keeps consuming the tail while the test runs, so the
        // live cursor has advanced past the event just read; it must never
        // move backwards from it.
        assert!(
            client
                .last_diagnostic_cursor()
                .is_some_and(|cursor| cursor.sequence >= event.sequence)
        );

        producer.shutdown();
        client.shutdown();
        server.join().unwrap();
    }

    #[test]
    fn a_reconnecting_client_sees_each_diagnostic_record_once_in_order() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(TestBackend::default()),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let producer = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        for message in ["one", "two", "three"] {
            producer
                .record_diagnostic(diagnostic_record(message))
                .unwrap();
        }
        producer
            .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
            .unwrap();

        // The first reader consumes the stream from the beginning.
        let reader = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let stream = reader.subscribe_diagnostics();
        for (index, expected) in ["one", "two", "three"].into_iter().enumerate() {
            let DiagnosticMessage::Event(event) =
                stream.recv_timeout(Duration::from_secs(1)).unwrap()
            else {
                panic!("expected a diagnostic record");
            };
            assert_eq!(event.sequence, index as u64 + 1);
            assert_eq!(event.record.message, expected);
        }
        let cursor = reader.last_diagnostic_cursor().unwrap();

        // Records produced after that cursor must arrive exactly once on the
        // replacement connection, in order, and the consumed ones must not be
        // replayed. The old connection is left open: a replacement client is
        // the reconnect this test exercises.
        for message in ["four", "five"] {
            producer
                .record_diagnostic(diagnostic_record(message))
                .unwrap();
        }
        producer
            .request(Uuid::nil(), Uuid::nil(), Command::LoadTaskState)
            .unwrap();
        let resumed = DaemonClient::connect_with_cursors(
            &address.to_string(),
            "secret".into(),
            Vec::new(),
            Some(cursor),
        )
        .unwrap();
        let resumed_stream = resumed.subscribe_diagnostics();
        for (index, expected) in ["four", "five"].into_iter().enumerate() {
            let DiagnosticMessage::Event(event) =
                resumed_stream.recv_timeout(Duration::from_secs(1)).unwrap()
            else {
                panic!("expected a diagnostic record");
            };
            assert_eq!(event.sequence, cursor.sequence + index as u64 + 1);
            assert_eq!(event.record.message, expected);
        }
        assert!(
            resumed_stream
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "no record may be delivered twice"
        );

        producer.shutdown();
        resumed.shutdown();
        server.join().unwrap();
    }

    #[test]
    fn remote_supervisor_reconnects_without_losing_the_daemon_runtime() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        // Keep the address reserved between servers. If the first listener is
        // dropped before the replacement binds, a parallel test can claim the
        // newly freed ephemeral port and make this test fail with AddrInUse.
        let replacement_listener = listener.try_clone().unwrap();
        let address = listener.local_addr().unwrap();
        let backend = Arc::new(TestBackend::default());
        let first_shutdown = Arc::new(AtomicBool::new(false));
        let first_server = {
            let backend = backend.clone();
            let shutdown = first_shutdown.clone();
            std::thread::spawn(move || {
                serve(
                    listener,
                    "secret".into(),
                    backend,
                    shutdown,
                    ServerOptions::default(),
                )
                .unwrap()
            })
        };

        let supervisor = DaemonSupervisor::connect(&address.to_string(), "secret".into()).unwrap();
        let clients = supervisor.subscribe_clients();
        let initial = clients.recv_timeout(Duration::from_secs(1)).unwrap();
        let session_id = Uuid::new_v4();
        let runtime_id = Uuid::new_v4();
        initial
            .request(
                session_id,
                runtime_id,
                Command::Start {
                    options: WireDriverStartOptions {
                        provider: "codex".into(),
                        binary: PathBuf::from("codex"),
                        cwd: PathBuf::from("."),
                        mode: "fullAccess".into(),
                        model: None,
                        reasoning_effort: None,
                        service_tier: None,
                        context_window: None,
                        agent_preset: None,
                        computer_use_enabled: false,
                        provider_cursor: None,
                    },
                },
            )
            .unwrap();

        first_shutdown.store(true, Ordering::Release);
        first_server.join().unwrap();

        let second_shutdown = Arc::new(AtomicBool::new(false));
        let second_server = {
            let backend = backend.clone();
            let shutdown = second_shutdown.clone();
            std::thread::spawn(move || {
                serve(
                    replacement_listener,
                    "secret".into(),
                    backend,
                    shutdown,
                    ServerOptions::default(),
                )
                .unwrap()
            })
        };

        let replacement = clients.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!initial.same_connection(&replacement));
        assert!(matches!(
            replacement
                .request(session_id, Uuid::nil(), Command::AttachSession)
                .unwrap(),
            ResponsePayload::SessionRuntime {
                runtime_id: Some(attached),
                supports_steer: true,
            } if attached == runtime_id
        ));

        second_shutdown.store(true, Ordering::Release);
        second_server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn daemon_terminal_retains_a_bounded_recent_window() {
        // A small window keeps the fixture cheap; the daemon's production
        // window is `RETAINED_OUTPUT_BYTES`, and the retention semantics are
        // the same for either size.
        const RETAINED_BYTES: usize = 8 * 1024;
        let root = std::env::temp_dir().join(format!("waku-terminal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let hub = Arc::new(Hub::default());
        let session_id = Uuid::new_v4();
        let runtime_id = Uuid::new_v4();
        hub.begin_runtime(session_id, runtime_id);
        let (messages, events) = unbounded();
        let (subscriber, _kicked) = Subscriber::new(messages);
        hub.subscribe(&[], None, subscriber);
        let terminal = crate::terminal::DaemonTerminal::open_with_shell_and_retention(
            &root,
            80,
            24,
            hub.event_sink(session_id, runtime_id),
            terminal_test_shell(
                "printf 'oldest-window\\n'; awk 'BEGIN { while (i++ < 60000) printf \"x\" }'; printf 'newest-window\\n'",
            ),
            RETAINED_BYTES,
        )
        .unwrap();

        // Collect everything the terminal delivers until the shell exits, so
        // the retained window is final and teardown starts from an idle shell.
        let mut delivered = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "daemon terminal never exited"
            );
            let Ok(message) = events.recv_timeout(Duration::from_millis(100)) else {
                continue;
            };
            let ServerMessage::Event(event) = message else {
                continue;
            };
            match event.event.kind.as_str() {
                "terminalOutput" => delivered.extend(
                    base64::engine::general_purpose::STANDARD
                        .decode(event.event.payload["data"].as_str().unwrap())
                        .unwrap(),
                ),
                "terminalExited" => break,
                _ => {}
            }
        }

        let retained = terminal.retained_snapshot().0;
        assert!(
            delivered.len() > RETAINED_BYTES,
            "fixture did not produce more output than the retained window: {} bytes",
            delivered.len()
        );
        assert_eq!(
            retained,
            delivered[delivered.len() - RETAINED_BYTES..],
            "retained output is not exactly the newest delivered bytes"
        );
        assert!(
            !retained
                .windows(b"oldest-window".len())
                .any(|window| window == b"oldest-window"),
            "output older than the retention window was not dropped"
        );

        drop(terminal);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn websocket_terminal_attach_replays_missed_output_and_size() {
        let root = std::env::temp_dir().join(format!("waku-terminal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("app.db")),
        )
        .unwrap()
        .with_terminal_shell(terminal_test_shell(
            "printf 'retained-attach-marker\\n'; while IFS= read -r line; do :; done",
        ));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(backend),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let terminal_id = Uuid::new_v4();
        let task_id = Uuid::new_v4();
        let opener = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let opening_events = opener.subscribe(terminal_id, terminal_id);
        opener
            .request(
                terminal_id,
                terminal_id,
                Command::OpenTerminal {
                    task_id,
                    cwd: root.clone(),
                    cols: 100,
                    rows: 30,
                },
            )
            .unwrap();
        // Wait until the shell's marker has been delivered, so the retained
        // output is already final and the terminal is idle.
        terminal_output_until(&opening_events, b"retained-attach-marker");

        // A client that was never attached reconstructs the missed bytes from
        // the attach snapshot instead of the ephemeral event stream.
        let attacher = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let attached = attacher
            .request(
                terminal_id,
                terminal_id,
                Command::AttachTerminal { task_id },
            )
            .unwrap();
        let ResponsePayload::TerminalSnapshot(snapshot) = attached else {
            panic!("attach did not return a terminal snapshot: {attached:?}");
        };
        let data = snapshot.data;
        assert!(
            data.windows(b"retained-attach-marker".len())
                .any(|window| window == b"retained-attach-marker"),
            "attach replay did not carry the output the second client missed: {:?}",
            String::from_utf8_lossy(&data)
        );
        assert_eq!(
            (snapshot.cols, snapshot.rows),
            (100, 30),
            "attach reported the wrong terminal size"
        );

        // The reported size follows a resize.
        opener
            .request(
                terminal_id,
                terminal_id,
                Command::ResizeTerminal {
                    cols: 132,
                    rows: 43,
                },
            )
            .unwrap();
        let resized = attacher
            .request(
                terminal_id,
                terminal_id,
                Command::AttachTerminal { task_id },
            )
            .unwrap();
        let ResponsePayload::TerminalSnapshot(snapshot) = resized else {
            panic!("attach did not return a terminal snapshot: {resized:?}");
        };
        assert_eq!((snapshot.cols, snapshot.rows), (132, 43));

        opener.shutdown();
        attacher.shutdown();
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A client that attaches while the shell is still writing must combine
    /// the attach snapshot with the live events that follow it. The snapshot
    /// and the first live emission share the terminal's retained lock, so the
    /// boundary is exact: every advertised byte appears once, and a hole is
    /// detectable as a jump in the per-terminal output sequence instead of
    /// silently duplicating or dropping output.
    #[cfg(unix)]
    #[test]
    fn websocket_terminal_attach_boundary_has_no_duplicates_or_gaps() {
        let root = std::env::temp_dir().join(format!("waku-terminal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("app.db")),
        )
        .unwrap()
        .with_terminal_shell(terminal_test_shell(
            // Pause a finite numbered stream until the client has its attach
            // snapshot. A fast producer can otherwise finish before attach,
            // making the live-boundary assertion depend on machine speed.
            // Both chunks keep consecutive counters so duplicates and gaps
            // remain observable; the final marker permits natural teardown.
            "awk 'BEGIN { pad = sprintf(\"%64s\", \"\"); for (i = 0; i < 100; i++) printf \"%08d%s\\n\", i, pad }'; IFS= read -r continuation; awk 'BEGIN { pad = sprintf(\"%64s\", \"\"); for (i = 100; i < 3000; i++) printf \"%08d%s\\n\", i, pad; printf \"burst-done\\n\" }'",
        ));
        let backend = Arc::new(backend);
        let observed_backend = backend.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                backend,
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let terminal_id = Uuid::new_v4();
        let task_id = Uuid::new_v4();
        let client = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let events = client.subscribe(terminal_id, terminal_id);
        client
            .request(
                terminal_id,
                terminal_id,
                Command::OpenTerminal {
                    task_id,
                    cwd: root.clone(),
                    cols: 80,
                    rows: 24,
                },
            )
            .unwrap();
        // Wait until the shell is mid-flood before attaching, so the snapshot
        // and the live stream genuinely overlap.
        terminal_output_until(&events, b"00000001");

        let attached = client
            .request(
                terminal_id,
                terminal_id,
                Command::AttachTerminal { task_id },
            )
            .unwrap();
        let ResponsePayload::TerminalSnapshot(snapshot) = attached else {
            panic!("attach did not return a terminal snapshot: {attached:?}");
        };
        let (data, sequence) = (snapshot.data, snapshot.sequence);

        // Release the second chunk only after the snapshot boundary exists.
        // The PTY may echo this line; it has no numbered prefix and is ignored
        // by the consecutive-counter assertion below.
        client
            .request(
                terminal_id,
                terminal_id,
                Command::WriteTerminal {
                    data: b"continue\n".to_vec(),
                },
            )
            .unwrap();

        let mut reconstructed = data.clone();
        let mut boundary = sequence;
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < deadline && reconstructed.len() < data.len() + 50_000 {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let Ok(event) = events.recv_timeout(remaining) else {
                break;
            };
            if event.event.kind != "terminalOutput" {
                continue;
            }
            let batch = base64::engine::general_purpose::STANDARD
                .decode(event.event.payload["data"].as_str().unwrap())
                .unwrap();
            let end = event.event.payload["sequence"]
                .as_u64()
                .expect("terminalOutput did not carry its output sequence");
            let start = end - batch.len() as u64;
            if end <= boundary {
                // Wholly covered by the snapshot the client already replayed.
                continue;
            }
            assert_eq!(
                start, boundary,
                "the live stream jumped from sequence {boundary} to {start}"
            );
            reconstructed.extend_from_slice(&batch);
            boundary = end;
        }
        assert!(
            reconstructed.len() > data.len(),
            "attach did not observe live output after the snapshot"
        );

        // Every line leads with a fixed-width counter, so a duplicated or
        // skipped byte shows up as a non-consecutive pair of line numbers. The
        // reconstruction may begin or end mid-line, so only a leading counter
        // followed by non-digit filler counts.
        let mut previous: Option<u64> = None;
        let mut counted = 0_usize;
        for line in reconstructed.split(|byte| *byte == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.len() < 9 || !line[..8].iter().all(u8::is_ascii_digit) {
                continue;
            }
            let value: u64 = std::str::from_utf8(&line[..8]).unwrap().parse().unwrap();
            if let Some(previous) = previous {
                assert_eq!(
                    value,
                    previous + 1,
                    "the reconstructed terminal stream duplicated or skipped output at {value}"
                );
            }
            previous = Some(value);
            counted += 1;
        }
        assert!(
            counted > 4,
            "the reconstruction held too little output to judge"
        );

        // Let the burst finish before stopping the server so its shell exits
        // on its own; killing a shell mid-write leaves the PTY child stuck
        // exiting and blocks the daemon teardown.
        terminal_attach_marker_with_diagnostics(&events, boundary, || {
            // Read the same snapshot in process, bypassing the transport whose
            // delivery is under investigation. AttachTerminal is read-only.
            observed_backend.handle(
                Request {
                    request_id: Uuid::new_v4(),
                    session_id: terminal_id,
                    runtime_id: terminal_id,
                    command: Command::AttachTerminal { task_id },
                },
                Arc::new(Hub::default()).event_sink(terminal_id, terminal_id),
            )
        });

        shutdown.store(true, Ordering::Release);
        client.shutdown();
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Closing the surface (or the window) drops the desktop's client without
    /// ending the shell. A client that arrives afterwards attaches to the same
    /// terminal, reconstructs the grid from the replay, and finds the process
    /// still able to answer input.
    #[cfg(unix)]
    #[test]
    fn websocket_terminal_survives_client_disconnect_and_reattaches() {
        let root = std::env::temp_dir().join(format!("waku-terminal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("app.db")),
        )
        .unwrap()
        .with_terminal_shell(terminal_test_shell(
            "printf 'daemon-shell-ready\\n'; while IFS= read -r line; do printf 'echo:%s\\n' \"$line\"; done",
        ));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(backend),
                server_shutdown,
                ServerOptions {
                    // The first client disconnects by shutting its own socket
                    // down; a client-owned shutdown must not end the daemon.
                    allow_shutdown: false,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let terminal_id = Uuid::new_v4();
        let task_id = Uuid::new_v4();
        let first = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let first_events = first.subscribe(terminal_id, terminal_id);
        first
            .request(
                terminal_id,
                terminal_id,
                Command::OpenTerminal {
                    task_id,
                    cwd: root.clone(),
                    cols: 80,
                    rows: 24,
                },
            )
            .unwrap();
        terminal_output_until(&first_events, b"daemon-shell-ready");
        // Simulates the window tearing down: the client goes away, the daemon
        // and its shell do not.
        first.shutdown();

        let second = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let second_events = second.subscribe(terminal_id, terminal_id);
        let attached = second
            .request(
                terminal_id,
                terminal_id,
                Command::AttachTerminal { task_id },
            )
            .unwrap();
        let ResponsePayload::TerminalSnapshot(snapshot) = attached else {
            panic!("attach did not return a terminal snapshot: {attached:?}");
        };
        let data = snapshot.data;
        assert!(
            data.windows(b"daemon-shell-ready".len())
                .any(|window| window == b"daemon-shell-ready"),
            "reattach did not replay the grid the disconnected client saw"
        );

        second
            .request(
                terminal_id,
                terminal_id,
                Command::WriteTerminal {
                    data: b"still-alive\r".to_vec(),
                },
            )
            .unwrap();
        terminal_output_until(&second_events, b"echo:still-alive");

        shutdown.store(true, Ordering::Release);
        second.shutdown();
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Closing a terminal is its explicit end: the daemon terminates and reaps
    /// the shell and releases the terminal, so a later attach has nothing left
    /// to reattach to.
    #[cfg(unix)]
    #[test]
    fn websocket_terminal_close_terminates_the_shell_and_releases_the_terminal() {
        let root = std::env::temp_dir().join(format!("waku-terminal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("app.db")),
        )
        .unwrap()
        .with_terminal_shell(terminal_test_shell(
            "printf 'close-ready:%s\\n' \"$$\"; while IFS= read -r line; do :; done",
        ));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(backend),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let client = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let terminal_id = Uuid::new_v4();
        let task_id = Uuid::new_v4();
        let events = client.subscribe(terminal_id, terminal_id);
        client
            .request(
                terminal_id,
                terminal_id,
                Command::OpenTerminal {
                    task_id,
                    cwd: root.clone(),
                    cols: 80,
                    rows: 24,
                },
            )
            .unwrap();
        let ready = terminal_output_until(&events, b"\n");
        let child_pid: libc::pid_t = String::from_utf8_lossy(&ready)
            .trim()
            .strip_prefix("close-ready:")
            .expect("the shell reported its pid")
            .parse()
            .unwrap();

        client
            .request(terminal_id, terminal_id, Command::CloseTerminal)
            .unwrap();

        assert!(
            !process_is_alive(child_pid),
            "the shell outlived the terminal that closed: pid {child_pid}"
        );
        let attached = client.request(
            terminal_id,
            terminal_id,
            Command::AttachTerminal { task_id },
        );
        assert!(
            matches!(attached, Ok(ResponsePayload::TerminalAbsent)),
            "the closed terminal was still attachable: {attached:?}"
        );

        shutdown.store(true, Ordering::Release);
        client.shutdown();
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Removing a task disposes the terminals it owns — the shell ends with the
    /// task — while another task's terminal keeps running.    #[cfg(unix)]
    #[test]
    fn websocket_removing_a_task_disposes_its_terminals() {
        let root = std::env::temp_dir().join(format!("waku-terminal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("app.db")),
        )
        .unwrap()
        .with_terminal_shell(terminal_test_shell(
            "printf 'task-shell-ready:%s\\n' \"$$\"; while IFS= read -r line; do :; done",
        ));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(backend),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let client = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let removed_task = Uuid::new_v4();
        let kept_task = Uuid::new_v4();
        let removed_terminal = Uuid::new_v4();
        let kept_terminal = Uuid::new_v4();

        let removed_events = client.subscribe(removed_terminal, removed_terminal);
        client
            .request(
                removed_terminal,
                removed_terminal,
                Command::OpenTerminal {
                    task_id: removed_task,
                    cwd: root.clone(),
                    cols: 80,
                    rows: 24,
                },
            )
            .unwrap();
        let ready = terminal_output_until(&removed_events, b"\n");
        let child_pid: libc::pid_t = String::from_utf8_lossy(&ready)
            .trim()
            .strip_prefix("task-shell-ready:")
            .expect("the shell reported its pid")
            .parse()
            .unwrap();

        let kept_events = client.subscribe(kept_terminal, kept_terminal);
        client
            .request(
                kept_terminal,
                kept_terminal,
                Command::OpenTerminal {
                    task_id: kept_task,
                    cwd: root.clone(),
                    cols: 80,
                    rows: 24,
                },
            )
            .unwrap();
        terminal_output_until(&kept_events, b"\n");

        client
            .request(removed_task, Uuid::nil(), Command::RemoveSession)
            .unwrap();

        assert!(
            !process_is_alive(child_pid),
            "the removed task's shell kept running: pid {child_pid}"
        );
        let attached = client.request(
            removed_terminal,
            removed_terminal,
            Command::AttachTerminal {
                task_id: removed_task,
            },
        );
        assert!(
            matches!(attached, Ok(ResponsePayload::TerminalAbsent)),
            "a removed task's terminal was still attachable: {attached:?}"
        );
        let kept = client.request(
            kept_terminal,
            kept_terminal,
            Command::AttachTerminal { task_id: kept_task },
        );
        assert!(
            matches!(kept, Ok(ResponsePayload::TerminalSnapshot(_))),
            "removing one task took another task's terminal: {kept:?}"
        );

        shutdown.store(true, Ordering::Release);
        client.shutdown();
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    /// An attach to an id the daemon never opened reports explicit absence and
    /// creates nothing, so a client can open a terminal there without an error
    /// being indistinguishable from a live terminal's loss.
    #[cfg(unix)]
    #[test]
    fn websocket_terminal_attach_reports_absence_for_an_unknown_terminal() {
        let root = std::env::temp_dir().join(format!("waku-terminal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("app.db")),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(backend),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let client = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let terminal_id = Uuid::new_v4();
        let task_id = Uuid::new_v4();
        for attach in [1, 2] {
            let response = client
                .request(
                    terminal_id,
                    terminal_id,
                    Command::AttachTerminal { task_id },
                )
                .unwrap();
            assert!(
                matches!(response, ResponsePayload::TerminalAbsent),
                "attach {attach} did not report absence: {response:?}"
            );
        }

        shutdown.store(true, Ordering::Release);
        client.shutdown();
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A terminal is scoped to the task that opened it, so a client naming
    /// another task is refused rather than shown a shell it does not own.
    #[cfg(unix)]
    #[test]
    fn websocket_terminal_attach_refuses_another_tasks_terminal() {
        let root = std::env::temp_dir().join(format!("waku-terminal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("app.db")),
        )
        .unwrap()
        .with_terminal_shell(terminal_test_shell(
            "printf 'owner-ready\\n'; while IFS= read -r line; do :; done",
        ));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(backend),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let client = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let terminal_id = Uuid::new_v4();
        let owner = Uuid::new_v4();
        let events = client.subscribe(terminal_id, terminal_id);
        client
            .request(
                terminal_id,
                terminal_id,
                Command::OpenTerminal {
                    task_id: owner,
                    cwd: root.clone(),
                    cols: 80,
                    rows: 24,
                },
            )
            .unwrap();
        terminal_output_until(&events, b"owner-ready");

        let other = client.request(
            terminal_id,
            terminal_id,
            Command::AttachTerminal {
                task_id: Uuid::new_v4(),
            },
        );
        assert!(
            other.is_err(),
            "another task attached to a terminal it does not own: {other:?}"
        );
        let owned = client.request(
            terminal_id,
            terminal_id,
            Command::AttachTerminal { task_id: owner },
        );
        assert!(
            matches!(owned, Ok(ResponsePayload::TerminalSnapshot(_))),
            "the owning task could not attach: {owned:?}"
        );

        shutdown.store(true, Ordering::Release);
        client.shutdown();
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Whether the process is still in the process table. The daemon reaps a
    /// shell it terminates, so a terminated one answers `ESRCH`.
    #[cfg(unix)]
    fn process_is_alive(pid: libc::pid_t) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[cfg(unix)]
    #[test]
    fn websocket_terminal_rejects_writes_from_a_superseded_runtime() {
        let root = std::env::temp_dir().join(format!("waku-terminal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("app.db")),
        )
        .unwrap()
        .with_terminal_shell(terminal_test_shell(
            "printf 'runtime-ready\\n'; while IFS= read -r line; do printf 'received:%s\\n' \"$line\"; done",
        ));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(backend),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let terminal_id = Uuid::new_v4();
        let task_id = Uuid::new_v4();
        let superseded_runtime = Uuid::new_v4();
        let replacement_runtime = Uuid::new_v4();
        let client = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let superseded_events = client.subscribe(terminal_id, superseded_runtime);
        client
            .request(
                terminal_id,
                superseded_runtime,
                Command::OpenTerminal {
                    task_id,
                    cwd: root.clone(),
                    cols: 80,
                    rows: 24,
                },
            )
            .unwrap();
        terminal_output_until(&superseded_events, b"runtime-ready");

        // Replacing the runtime swaps the terminal under the same id.
        let replacement_events = client.subscribe(terminal_id, replacement_runtime);
        client
            .request(
                terminal_id,
                replacement_runtime,
                Command::OpenTerminal {
                    task_id,
                    cwd: root.clone(),
                    cols: 80,
                    rows: 24,
                },
            )
            .unwrap();
        terminal_output_until(&replacement_events, b"runtime-ready");

        let stale = client.request(
            terminal_id,
            superseded_runtime,
            Command::WriteTerminal {
                data: b"stale-write\r".to_vec(),
            },
        );
        assert!(
            stale.is_err(),
            "a write from a superseded runtime reached the replacement terminal: {stale:?}"
        );

        client
            .request(
                terminal_id,
                replacement_runtime,
                Command::WriteTerminal {
                    data: b"valid-write\r".to_vec(),
                },
            )
            .unwrap();
        let output = terminal_output_until(&replacement_events, b"received:valid-write");
        assert!(
            !output
                .windows(b"received:stale-write".len())
                .any(|window| window == b"received:stale-write"),
            "the replacement shell executed the superseded runtime's input"
        );

        client.shutdown();
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn websocket_terminal_flood_delivers_bounded_batches() {
        websocket_terminal_output_shape(
            "awk 'BEGIN { while (i++ < 30000) printf \"xxxxxxxxxx\" }'; printf '\\nflood-done\\n'",
            b"flood-done",
            b'x',
            300_000,
        );
    }

    #[cfg(unix)]
    #[test]
    fn websocket_terminal_coalesces_many_small_writes() {
        websocket_terminal_output_shape(
            "i=0; while [ $i -lt 2000 ]; do printf 'x'; i=$((i+1)); done; printf '\\nsmall-done\\n'",
            b"small-done",
            b'x',
            2000,
        );
    }

    /// Runs a shell that writes `marker`-terminated output containing
    /// `expected_filler` bytes of `filler`, then asserts output still arrives
    /// in full while the delivered `terminalOutput` events stay within the
    /// daemon's flush cadence: at most one event per flush interval, plus one
    /// per `FLUSH_BYTES` of output, however many writes the shell made.
    #[cfg(unix)]
    fn websocket_terminal_output_shape(
        script: &str,
        marker: &[u8],
        filler: u8,
        expected_filler: usize,
    ) {
        const MAX_EVENT_BYTES: usize = 64 * 1024;
        let root = std::env::temp_dir().join(format!("waku-terminal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("app.db")),
        )
        .unwrap()
        .with_terminal_shell(terminal_test_shell(script));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(backend),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        // Measured from before the terminal starts, so the batching bound
        // below covers the whole window the daemon could have flushed in.
        let started = std::time::Instant::now();
        let client = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let terminal_id = Uuid::new_v4();
        let task_id = Uuid::new_v4();
        let events = client.subscribe(terminal_id, terminal_id);
        client
            .request(
                terminal_id,
                terminal_id,
                Command::OpenTerminal {
                    task_id,
                    cwd: root.clone(),
                    cols: 80,
                    rows: 24,
                },
            )
            .unwrap();

        let deadline = started + Duration::from_secs(30);
        let mut delivered = Vec::new();
        let mut output_events = 0_usize;
        let mut largest_event = 0_usize;
        while std::time::Instant::now() < deadline
            && !delivered
                .windows(marker.len())
                .any(|window| window == marker)
        {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let Ok(event) = events.recv_timeout(remaining) else {
                break;
            };
            if event.event.kind != "terminalOutput" {
                continue;
            }
            let data = base64::engine::general_purpose::STANDARD
                .decode(event.event.payload["data"].as_str().unwrap())
                .unwrap();
            output_events += 1;
            largest_event = largest_event.max(data.len());
            delivered.extend(data);
        }

        assert!(
            delivered
                .windows(marker.len())
                .any(|window| window == marker),
            "terminal output never reached {marker:?}"
        );
        let filler_bytes = delivered.iter().filter(|byte| **byte == filler).count();
        assert_eq!(
            filler_bytes, expected_filler,
            "terminal delivery dropped or duplicated output bytes"
        );
        // The daemon flushes at most once per interval and once per threshold,
        // so the event count is bounded by output size and elapsed time rather
        // than by how many writes the shell made. The slack absorbs the first
        // flush, which is immediate, plus scheduling jitter.
        let elapsed = started.elapsed();
        let max_events = 4
            + delivered.len() / crate::terminal::FLUSH_BYTES
            + elapsed.as_millis() as usize
                / crate::terminal::FLUSH_INTERVAL.as_millis().max(1) as usize;
        assert!(
            output_events <= max_events,
            "terminal delivery was not batched: {output_events} events (bound {max_events}) for {} output bytes in {elapsed:?}",
            delivered.len()
        );
        assert!(
            largest_event <= MAX_EVENT_BYTES,
            "a single terminal event carried {largest_event} bytes"
        );

        client.shutdown();
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn terminal_exit_event_preserves_child_ownership_until_drop() {
        let root = std::env::temp_dir().join(format!("waku-terminal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let hub = Arc::new(Hub::default());
        let session = Uuid::new_v4();
        let runtime = Uuid::new_v4();
        hub.begin_runtime(session, runtime);
        let (messages, events) = unbounded();
        let (subscriber, _kicked) = Subscriber::new(messages);
        hub.subscribe(&[], None, subscriber);
        let terminal = crate::terminal::DaemonTerminal::open_with_shell(
            &root,
            80,
            24,
            hub.event_sink(session, runtime),
            terminal_test_shell("echo $$ > shell.pid; printf 'done\n'"),
        )
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let message = events
                .recv_timeout(remaining)
                .expect("terminal did not report exit");
            if matches!(message, ServerMessage::Event(event) if event.event.kind == "terminalExited")
            {
                break;
            }
        }
        let pid: libc::pid_t = std::fs::read_to_string(root.join("shell.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // A PTY can report EOF just before its child becomes waitable. Poll
        // without consuming status, but stop immediately if ownership is lost.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let (result, observed_pid) = loop {
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut status,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            let observed_pid = unsafe { status.si_pid() };
            if result != 0 || observed_pid != 0 {
                break (result, observed_pid);
            }
            assert!(
                std::time::Instant::now() < deadline,
                "exited shell never became waitable"
            );
            std::thread::sleep(Duration::from_millis(4));
        };
        drop(terminal);
        let reaped = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut status,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        let error = std::io::Error::last_os_error();
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(
            result, 0,
            "reader reaped the child while the terminal still owns its group"
        );
        assert_eq!(observed_pid, pid);
        assert_eq!(reaped, -1);
        assert_eq!(
            error.raw_os_error(),
            Some(libc::ECHILD),
            "drop must reap its direct child"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dropping_terminal_terminates_hangup_ignoring_group_children() {
        terminal_drop_terminates_group_children(TerminalGroupFixture::Waiting);
    }

    #[cfg(unix)]
    #[test]
    fn dropping_terminal_terminates_group_children_after_shell_exit() {
        terminal_drop_terminates_group_children(TerminalGroupFixture::Exited);
    }

    #[cfg(unix)]
    #[test]
    fn dropping_flooding_terminal_terminates_pipeline_group() {
        terminal_drop_terminates_group_children(TerminalGroupFixture::Flooding);
    }

    #[cfg(unix)]
    enum TerminalGroupFixture {
        Waiting,
        Exited,
        Flooding,
    }

    #[cfg(unix)]
    fn terminal_drop_terminates_group_children(fixture: TerminalGroupFixture) {
        let root = std::env::temp_dir().join(format!("waku-terminal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let hub = Arc::new(Hub::default());
        let terminal = crate::terminal::DaemonTerminal::open_with_shell(
            &root,
            80,
            24,
            hub.event_sink(Uuid::new_v4(), Uuid::new_v4()),
            terminal_test_shell(match fixture {
                TerminalGroupFixture::Waiting => {
                    "trap '' HUP; echo $$ > shell.pid; (sleep 5; kill -KILL 0) & sleep 120 & echo $! > child.pid; wait"
                }
                TerminalGroupFixture::Exited => {
                    "trap '' HUP; echo $$ > shell.pid; (sleep 5; kill -KILL 0) & sleep 120 & echo $! > child.pid; exit 0"
                }
                TerminalGroupFixture::Flooding => {
                    "trap '' HUP; echo $$ > shell.pid; (sleep 5; kill -KILL 0) & yes x | cat & echo $! > child.pid; wait"
                }
            }),
        )
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let read_pid = |name: &str| -> libc::pid_t {
            loop {
                if let Ok(text) = std::fs::read_to_string(root.join(name))
                    && let Ok(pid) = text.trim().parse()
                {
                    break pid;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "shell did not publish {name}"
                );
                std::thread::sleep(Duration::from_millis(4));
            }
        };
        let shell_pid = read_pid("shell.pid");
        let child_pid = read_pid("child.pid");
        // Each fixture has an in-group watchdog. If teardown regresses, it
        // signals its own current group after five seconds. No parent-side
        // cleanup retains a raw group ID after the terminal reaps its leader.
        if matches!(fixture, TerminalGroupFixture::Flooding) {
            while terminal.retained_snapshot().1 < 64 * 1024 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "fixture pipeline did not flood"
                );
                std::thread::sleep(Duration::from_millis(4));
            }
        }
        if matches!(fixture, TerminalGroupFixture::Exited) {
            loop {
                let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
                let result = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        shell_pid as libc::id_t,
                        &mut status,
                        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                    )
                };
                assert_eq!(
                    result, 0,
                    "terminal shell was reaped before terminal ownership ended"
                );
                if unsafe { status.si_pid() } != 0 {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "fixture shell did not exit"
                );
                std::thread::sleep(Duration::from_millis(4));
            }
        }
        let (dropped, finished) = bounded(1);
        std::thread::spawn(move || {
            drop(terminal);
            let _ = dropped.send(());
        });
        let completed = finished.recv_timeout(Duration::from_secs(3)).is_ok();
        // An orphan can briefly remain as a zombie until init reaps it. ps is
        // portable across the supported Unix platforms; kill(pid, 0) alone
        // would incorrectly count that already-terminated child as running.
        let running = || {
            let output = std::process::Command::new("/bin/ps")
                .args(["-o", "stat=", "-p", &child_pid.to_string()])
                .output()
                .expect("inspect fixture child state");
            assert!(
                output.status.success()
                    || (output.status.code() == Some(1) && output.stderr.is_empty()),
                "could not inspect fixture child: {output:?}"
            );
            let state = String::from_utf8(output.stdout).unwrap();
            !state.trim().is_empty() && !state.trim_start().starts_with('Z')
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while running() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(4));
        }
        let child_running = running();
        std::fs::remove_dir_all(root).unwrap();
        assert!(completed, "terminal drop did not finish");
        assert!(
            !child_running,
            "terminal drop left its HUP-ignoring child running"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dropping_an_idle_terminal_does_not_wait_for_output() {
        let root = std::env::temp_dir().join(format!("waku-terminal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let hub = Arc::new(Hub::default());
        let terminal = crate::terminal::DaemonTerminal::open_with_shell(
            &root,
            80,
            24,
            hub.event_sink(Uuid::new_v4(), Uuid::new_v4()),
            terminal_test_shell("while IFS= read -r line; do :; done"),
        )
        .unwrap();
        let (dropped, finished) = bounded(1);
        std::thread::spawn(move || {
            drop(terminal);
            let _ = dropped.send(());
        });

        assert!(
            finished.recv_timeout(Duration::from_secs(3)).is_ok(),
            "dropping an idle daemon terminal blocked on its output reader"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn websocket_terminal_round_trip_streams_input_and_output() {
        websocket_terminal_round_trip(false);
    }

    #[cfg(unix)]
    #[test]
    fn websocket_terminal_close_does_not_wait_for_a_shell_ignoring_hangup() {
        websocket_terminal_round_trip(true);
    }

    #[cfg(unix)]
    fn terminal_test_shell(script: &str) -> alacritty_terminal::tty::Shell {
        // Do not load the developer's or CI runner's login files, prompt
        // plugins, or terminal capability queries in a transport test.
        alacritty_terminal::tty::Shell::new("/bin/sh".into(), vec!["-c".into(), script.into()])
    }

    #[cfg(unix)]
    fn websocket_terminal_round_trip(ignore_hangup: bool) {
        let root = std::env::temp_dir().join(format!("waku-terminal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("app.db")),
        )
        .unwrap()
        .with_terminal_shell(terminal_test_shell(&format!(
            "{}\nprintf 'ready:%s\\n' \"$$\"\nwhile IFS= read -r line; do printf 'received:%s\\n' \"$line\"; done",
            if ignore_hangup { "trap '' HUP" } else { ":" },
        )));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let server = std::thread::spawn(move || {
            serve(
                listener,
                "secret".into(),
                Arc::new(backend),
                server_shutdown,
                ServerOptions {
                    allow_shutdown: true,
                    ..ServerOptions::default()
                },
            )
            .unwrap()
        });

        let client = DaemonClient::connect(&address.to_string(), "secret".into()).unwrap();
        let terminal_id = Uuid::new_v4();
        let task_id = Uuid::new_v4();
        let events = client.subscribe(terminal_id, terminal_id);
        assert!(matches!(
            client
                .request(
                    terminal_id,
                    terminal_id,
                    Command::OpenTerminal {
                        task_id,
                        cwd: root.clone(),
                        cols: 80,
                        rows: 24,
                    },
                )
                .unwrap(),
            ResponsePayload::Ack
        ));
        // Wait until the shell has installed its signal handler. Receiving
        // local echo alone does not prove that shell startup has completed.
        let ready = terminal_output_until(&events, b"\n");
        let child_pid: libc::pid_t = String::from_utf8_lossy(&ready)
            .trim()
            .strip_prefix("ready:")
            .unwrap()
            .parse()
            .unwrap();
        client
            .request(
                terminal_id,
                terminal_id,
                Command::WriteTerminal {
                    data: b"waku-terminal-round-trip\r".to_vec(),
                },
            )
            .unwrap();

        // The response prefix is absent from the input, so a PTY echo cannot
        // satisfy this assertion before the child has actually read it.
        terminal_output_until(&events, b"received:waku-terminal-round-trip");

        let (closed, finished) = bounded(1);
        let closing_client = client.clone();
        let close = std::thread::spawn(move || {
            let _ = closed.send(closing_client.request(
                terminal_id,
                terminal_id,
                Command::CloseTerminal,
            ));
        });
        let result = finished.recv_timeout(Duration::from_secs(3));
        if result.is_err() {
            // Clean up the fixture even when shutdown regresses, and fail
            // here instead of waiting for the client's 120-second timeout.
            unsafe {
                libc::kill(child_pid, libc::SIGKILL);
            }
        }
        client.shutdown();
        server.join().unwrap();
        close.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            matches!(result, Ok(Ok(ResponsePayload::Ack))),
            "closing daemon terminal did not complete: {result:?}"
        );
    }

    /// Temporary #91 diagnostics: a late marker never turns failure into a pass.
    #[cfg(unix)]
    fn terminal_attach_marker_with_diagnostics(
        events: &Receiver<SequencedEvent>,
        mut received_sequence: u64,
        snapshot: impl Fn() -> anyhow::Result<ResponsePayload>,
    ) {
        const MARKER: &[u8] = b"burst-done";
        let started = std::time::Instant::now();
        let original_deadline = started + Duration::from_secs(3);
        // Six extra seconds allow the observed ~500 lines/second macOS
        // stream to finish its 3000 lines, but only as diagnostic evidence.
        // The original three-second success deadline remains unchanged.
        let observation_deadline = original_deadline + Duration::from_secs(6);
        let mut output = Vec::new();
        let mut arrivals = Vec::new();
        let mut expired = false;
        let report_snapshot = |phase: &str, received_sequence: u64| match snapshot() {
            Ok(ResponsePayload::TerminalSnapshot(snapshot)) => eprintln!(
                "attach-marker {phase}: elapsed={:?}, received_sequence={received_sequence}, retained_sequence={}, retained_bytes={}, retained_marker={}",
                started.elapsed(),
                snapshot.sequence,
                snapshot.data.len(),
                snapshot
                    .data
                    .windows(MARKER.len())
                    .any(|bytes| bytes == MARKER),
            ),
            other => eprintln!("attach-marker {phase}: snapshot={other:?}"),
        };
        loop {
            // Match the original helper: a marker decoded from the receive
            // begun before the deadline succeeds even if scheduling resumes
            // after it. Once expiry was observed, a later marker still fails.
            if output.windows(MARKER.len()).any(|bytes| bytes == MARKER) {
                break;
            }
            let now = std::time::Instant::now();
            if !expired && now >= original_deadline {
                expired = true;
                report_snapshot("original-deadline", received_sequence);
                eprintln!("attach-marker arrivals before deadline: {arrivals:?}");
                arrivals.clear();
            }
            if std::time::Instant::now() >= observation_deadline {
                break;
            }
            let deadline = if expired {
                observation_deadline
            } else {
                original_deadline
            };
            let event = match events
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            {
                Ok(event) => event,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            };
            if event.event.kind == "terminalOutput" {
                let batch = base64::engine::general_purpose::STANDARD
                    .decode(event.event.payload["data"].as_str().unwrap())
                    .unwrap();
                let end = event.event.payload["sequence"].as_u64().unwrap();
                assert_eq!(
                    end - batch.len() as u64,
                    received_sequence,
                    "diagnostic marker stream has a gap or duplicate"
                );
                received_sequence = end;
                output.extend(batch);
            }
            arrivals.push((started.elapsed(), event.event.kind, received_sequence));
        }
        if expired || !output.windows(MARKER.len()).any(|bytes| bytes == MARKER) {
            report_snapshot("observation-end", received_sequence);
            eprintln!(
                "attach-marker later arrivals: {arrivals:?}; eventual_marker={}",
                output.windows(MARKER.len()).any(|bytes| bytes == MARKER)
            );
        }
        assert!(
            !expired && output.windows(MARKER.len()).any(|bytes| bytes == MARKER),
            "daemon terminal did not return the shell marker by the original three-second deadline"
        );
    }

    #[cfg(unix)]
    fn terminal_output_until(events: &Receiver<SequencedEvent>, marker: &[u8]) -> Vec<u8> {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut output = Vec::new();
        let mut seen_events = Vec::new();
        while std::time::Instant::now() < deadline
            && !output.windows(marker.len()).any(|window| window == marker)
        {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let Ok(event) = events.recv_timeout(remaining) else {
                break;
            };
            seen_events.push(event.event.kind.clone());
            if event.event.kind != "terminalOutput" {
                continue;
            }
            let data = event.event.payload["data"].as_str().unwrap();
            output.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .unwrap(),
            );
        }
        assert!(
            output.windows(marker.len()).any(|window| window == marker),
            "daemon terminal did not return the shell marker; events={seen_events:?}, output={}",
            String::from_utf8_lossy(&output)
        );
        output
    }

    #[test]
    fn nil_request_ids_execute_without_responses_or_cache_entries() {
        let (outgoing, responses) = unbounded();
        let hub = Arc::new(Hub::default());
        let handled = handle_request(
            Request {
                request_id: Uuid::nil(),
                session_id: Uuid::nil(),
                runtime_id: Uuid::nil(),
                command: Command::GetSettings,
            },
            outgoing,
            0,
            Arc::new(TestBackend::default()),
            hub.clone(),
        );

        assert!(handled.executed);
        assert!(matches!(handled.outcome, ResponseOutcome::Ok { .. }));
        assert!(responses.try_recv().is_err());
        assert!(hub.cached_response(Uuid::nil()).is_none());
    }

    #[test]
    fn browser_origins_are_denied_unless_explicitly_allowed() {
        let request = HandshakeRequest::builder()
            .uri("/v1")
            .header(ORIGIN, "https://app.waku.test")
            .body(())
            .unwrap();
        let response = HandshakeResponse::new(());
        assert_eq!(
            validate_handshake(&request, response, &HashSet::new())
                .unwrap_err()
                .status(),
            StatusCode::FORBIDDEN
        );

        let allowed = HashSet::from(["https://app.waku.test".to_owned()]);
        assert!(validate_handshake(&request, HandshakeResponse::new(()), &allowed).is_ok());
        let native = HandshakeRequest::builder().uri("/v1").body(()).unwrap();
        assert!(validate_handshake(&native, HandshakeResponse::new(()), &HashSet::new()).is_ok());

        let react_native = HandshakeRequest::builder()
            .uri("/v1")
            .header(ORIGIN, "http://192.168.0.114:34125")
            .header(NATIVE_CLIENT_HEADER, NATIVE_CLIENT_HEADER_VALUE)
            .body(())
            .unwrap();
        assert!(
            validate_handshake(&react_native, HandshakeResponse::new(()), &HashSet::new()).is_ok()
        );

        let forged_native = HandshakeRequest::builder()
            .uri("/v1")
            .header(ORIGIN, "https://attacker.example")
            .header(NATIVE_CLIENT_HEADER, "browser")
            .body(())
            .unwrap();
        assert_eq!(
            validate_handshake(&forged_native, HandshakeResponse::new(()), &HashSet::new())
                .unwrap_err()
                .status(),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn daemon_tokens_require_an_exact_match() {
        assert!(token_matches("secret", "secret"));
        assert!(!token_matches("secret", "Secret"));
        assert!(!token_matches("secret", "secret-extra"));
    }

    #[test]
    fn replaced_runtime_ignores_late_events_from_the_old_generation() {
        let hub = Arc::new(Hub::default());
        let session_id = Uuid::new_v4();
        let old_runtime_id = Uuid::new_v4();
        let new_runtime_id = Uuid::new_v4();
        let (outgoing, events) = unbounded();
        hub.subscribe(&[], None, Subscriber::new(outgoing).0);

        hub.begin_runtime(session_id, old_runtime_id);
        let old_sink = hub.event_sink(session_id, old_runtime_id);
        old_sink
            .send(WireDriverEvent::new("old", serde_json::Value::Null))
            .unwrap();
        assert!(matches!(
            events.recv().unwrap(),
            ServerMessage::Event(event) if event.runtime_id == old_runtime_id
        ));

        hub.begin_runtime(session_id, new_runtime_id);
        old_sink
            .send(WireDriverEvent::new("stale", serde_json::Value::Null))
            .unwrap();
        hub.event_sink(session_id, new_runtime_id)
            .send(WireDriverEvent::new("new", serde_json::Value::Null))
            .unwrap();

        let ServerMessage::Event(event) = events.recv().unwrap() else {
            panic!("expected a daemon event");
        };
        assert_eq!(event.runtime_id, new_runtime_id);
        assert_eq!(event.sequence, 1);
        assert_eq!(event.event.kind, "new");
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn replay_cursor_from_an_old_daemon_epoch_does_not_hide_new_events() {
        let hub = Arc::new(Hub::default());
        let session_id = Uuid::new_v4();
        let runtime_id = Uuid::new_v4();
        hub.begin_runtime(session_id, runtime_id);
        hub.event_sink(session_id, runtime_id)
            .send(WireDriverEvent::new("new-daemon", serde_json::Value::Null))
            .unwrap();

        let (outgoing, events) = unbounded();
        hub.subscribe(
            &[ReplayCursor {
                session_id,
                runtime_id,
                epoch: Uuid::nil(),
                sequence: u64::MAX,
            }],
            None,
            Subscriber::new(outgoing).0,
        );

        let ServerMessage::Event(event) = events.recv().unwrap() else {
            panic!("expected the new daemon event to replay");
        };
        assert_eq!(event.epoch, hub.epoch);
        assert_eq!(event.sequence, 1);
    }

    #[test]
    fn subscriber_that_cannot_keep_up_is_kicked_and_removed() {
        let hub = Arc::new(Hub::default());
        let session_id = Uuid::new_v4();
        let runtime_id = Uuid::new_v4();
        // Capacity one and no drainer: the second queued message overflows.
        let (outgoing, events) = bounded(1);
        let (subscriber, kicked) = Subscriber::new(outgoing);
        hub.subscribe(&[], None, subscriber);
        hub.begin_runtime(session_id, runtime_id);
        let sink = hub.event_sink(session_id, runtime_id);

        sink.send(WireDriverEvent::new("one", serde_json::Value::Null))
            .unwrap();
        // Nobody drains, so a second queued message overflows: the subscriber
        // is dropped from the hub and its connection is told to close.
        sink.send(WireDriverEvent::new("two", serde_json::Value::Null))
            .unwrap();
        assert!(kicked.try_recv().is_ok());

        assert!(matches!(
            events.try_recv(),
            Ok(ServerMessage::Event(event)) if event.event.kind == "one"
        ));
        assert!(events.try_recv().is_err());
        sink.send(WireDriverEvent::new("three", serde_json::Value::Null))
            .unwrap();
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn response_cache_evicts_oversized_and_byte_budget_exceeding_outcomes() {
        let hub = Hub::default();
        let oversized_id = Uuid::new_v4();
        let oversized = ResponseOutcome::Ok {
            payload: ResponsePayload::Cursor {
                cursor: Some(serde_json::Value::String(
                    "x".repeat(MAX_CACHED_RESPONSE_BYTES + 1),
                )),
            },
        };
        hub.cache_response(oversized_id, oversized);
        // A single outcome larger than the whole byte budget is never
        // retained; retrying the request runs it again instead of pinning a
        // >64 MB payload in daemon memory.
        assert!(hub.cached_response(oversized_id).is_none());

        let kept_id = Uuid::new_v4();
        hub.cache_response(
            kept_id,
            ResponseOutcome::Ok {
                payload: ResponsePayload::Ack,
            },
        );
        assert!(hub.cached_response(kept_id).is_some());
        assert!(hub.cached_response(oversized_id).is_none());
    }

    struct BlockingProbeBackend {
        probe_started: Sender<()>,
        release_probe: Receiver<()>,
    }

    impl Backend for BlockingProbeBackend {
        fn handle(&self, request: Request, _: EventSink) -> anyhow::Result<ResponsePayload> {
            if matches!(request.command, Command::ProbeProvider { .. }) {
                self.probe_started.send(()).unwrap();
                self.release_probe.recv().unwrap();
            }
            Ok(ResponsePayload::Ack)
        }
    }

    #[test]
    fn slow_background_command_does_not_block_session_hydration() {
        let (outgoing, response_rx) = unbounded();
        let (probe_started, probe_started_rx) = bounded(1);
        let (release_probe, release_probe_rx) = bounded(1);
        let backend: Arc<dyn Backend> = Arc::new(BlockingProbeBackend {
            probe_started,
            release_probe: release_probe_rx,
        });
        let hub = Arc::new(Hub::default());
        let dispatcher = RequestDispatcher::new(backend, hub);

        let probe_id = Uuid::new_v4();
        dispatcher.dispatch(
            Request {
                request_id: probe_id,
                session_id: Uuid::nil(),
                runtime_id: Uuid::nil(),
                command: Command::ProbeProvider {
                    provider: crate::model::ProviderKind::Codex,
                    binary_override: None,
                    discover_models: false,
                    probe_version: false,
                },
            },
            outgoing.clone(),
            0,
        );
        probe_started_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();

        let hydration_id = Uuid::new_v4();
        dispatcher.dispatch(
            Request {
                request_id: hydration_id,
                session_id: Uuid::nil(),
                runtime_id: Uuid::nil(),
                command: Command::HydrateSession {
                    session_id: Uuid::new_v4(),
                },
            },
            outgoing,
            0,
        );
        assert!(matches!(
            response_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            ServerMessage::Response { request_id, .. } if request_id == hydration_id
        ));

        release_probe.send(()).unwrap();
        assert!(matches!(
            response_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            ServerMessage::Response { request_id, .. } if request_id == probe_id
        ));
    }

    struct RuntimeOrderingBackend {
        blocked_session_id: Uuid,
        handled: Sender<(Uuid, &'static str)>,
        release_start: Receiver<()>,
    }

    impl Backend for RuntimeOrderingBackend {
        fn handle(&self, request: Request, _: EventSink) -> anyhow::Result<ResponsePayload> {
            let command = match request.command {
                Command::Start { .. } => {
                    self.handled.send((request.session_id, "start")).unwrap();
                    if request.session_id == self.blocked_session_id {
                        self.release_start.recv().unwrap();
                    }
                    return Ok(ResponsePayload::Started {
                        supports_steer: true,
                    });
                }
                Command::Prompt { .. } => "prompt",
                Command::CloseSession => "close",
                _ => "other",
            };
            self.handled.send((request.session_id, command)).unwrap();
            Ok(ResponsePayload::Ack)
        }
    }

    #[test]
    fn runtime_commands_are_ordered_per_session_without_blocking_other_sessions() {
        let blocked_session_id = Uuid::new_v4();
        let blocked_runtime_id = Uuid::new_v4();
        let other_session_id = Uuid::new_v4();
        let other_runtime_id = Uuid::new_v4();
        let (handled, handled_rx) = unbounded();
        let (release_start, release_start_rx) = bounded(1);
        let dispatcher = RequestDispatcher::new(
            Arc::new(RuntimeOrderingBackend {
                blocked_session_id,
                handled,
                release_start: release_start_rx,
            }),
            Arc::new(Hub::default()),
        );
        let (start_outgoing, start_responses) = unbounded();
        let (second_client_outgoing, second_client_responses) = unbounded();
        let (other_outgoing, other_responses) = unbounded();

        let blocked_start_id = Uuid::new_v4();
        dispatcher.dispatch(
            Request {
                request_id: blocked_start_id,
                session_id: blocked_session_id,
                runtime_id: blocked_runtime_id,
                command: Command::Start {
                    options: test_start_options(),
                },
            },
            start_outgoing,
            0,
        );
        assert_eq!(
            handled_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            (blocked_session_id, "start")
        );

        let prompt_id = Uuid::new_v4();
        dispatcher.dispatch(
            Request {
                request_id: prompt_id,
                session_id: blocked_session_id,
                runtime_id: blocked_runtime_id,
                command: Command::Prompt {
                    prompt: "after start".into(),
                    turn_id: None,
                    message_id: None,
                },
            },
            second_client_outgoing,
            0,
        );

        let other_start_id = Uuid::new_v4();
        dispatcher.dispatch(
            Request {
                request_id: other_start_id,
                session_id: other_session_id,
                runtime_id: other_runtime_id,
                command: Command::Start {
                    options: test_start_options(),
                },
            },
            other_outgoing.clone(),
            0,
        );
        assert_eq!(
            handled_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            (other_session_id, "start")
        );
        assert!(matches!(
            other_responses
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            ServerMessage::Response { request_id, .. } if request_id == other_start_id
        ));
        assert!(handled_rx.recv_timeout(Duration::from_millis(50)).is_err());

        release_start.send(()).unwrap();
        assert!(matches!(
            start_responses
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            ServerMessage::Response { request_id, .. } if request_id == blocked_start_id
        ));
        assert_eq!(
            handled_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            (blocked_session_id, "prompt")
        );
        assert!(matches!(
            second_client_responses
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            ServerMessage::Response { request_id, .. } if request_id == prompt_id
        ));

        let blocked_close_id = Uuid::new_v4();
        dispatcher.dispatch(
            Request {
                request_id: blocked_close_id,
                session_id: blocked_session_id,
                runtime_id: blocked_runtime_id,
                command: Command::CloseSession,
            },
            other_outgoing.clone(),
            0,
        );
        let other_close_id = Uuid::new_v4();
        dispatcher.dispatch(
            Request {
                request_id: other_close_id,
                session_id: other_session_id,
                runtime_id: other_runtime_id,
                command: Command::CloseSession,
            },
            other_outgoing,
            0,
        );
        let mut close_responses = [false; 2];
        for _ in 0..2 {
            let ServerMessage::Response { request_id, .. } = other_responses
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
            else {
                panic!("expected a close response");
            };
            if request_id == blocked_close_id {
                close_responses[0] = true;
            } else if request_id == other_close_id {
                close_responses[1] = true;
            }
        }
        assert_eq!(close_responses, [true, true]);
    }

    fn test_start_options() -> WireDriverStartOptions {
        WireDriverStartOptions {
            provider: "codex".into(),
            binary: PathBuf::from("codex"),
            cwd: PathBuf::from("."),
            mode: "fullAccess".into(),
            model: None,
            reasoning_effort: None,
            service_tier: None,
            context_window: None,
            agent_preset: None,
            computer_use_enabled: false,
            provider_cursor: None,
        }
    }
}

#[cfg(test)]
mod bounded_transcript_tests {
    use super::*;
    use crate::{daemon::WakuBackend, persistence::StateStore, settings::DaemonSettingsStore};

    #[test]
    fn transcript_rpc_is_debug_only_and_preserves_domain_errors() {
        let root = std::env::temp_dir().join(format!("waku-transcript-rpc-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("app.db")),
        )
        .unwrap();
        let hub = Arc::new(Hub::default());
        let outcome = backend.handle(
            Request {
                request_id: Uuid::new_v4(),
                session_id: Uuid::nil(),
                runtime_id: Uuid::nil(),
                command: Command::ReadTranscript {
                    conversation_id: Uuid::new_v4(),
                    query: waku_protocol::TranscriptQuery::Index {
                        collection: waku_protocol::TranscriptCollection::Messages,
                        offset: 0,
                        limit: 50,
                    },
                },
            },
            hub.event_sink(Uuid::nil(), Uuid::nil()),
        );
        if cfg!(debug_assertions) {
            let ResponsePayload::Transcript { result: Err(error) } = outcome.unwrap() else {
                panic!("expected typed missing-conversation error")
            };
            assert_eq!(error.code, "conversation_not_found");
        } else {
            assert!(
                outcome
                    .unwrap_err()
                    .to_string()
                    .contains("only available in debug builds")
            );
        }
        drop(backend);
        std::fs::remove_dir_all(root).unwrap();
    }
}
