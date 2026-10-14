//! Disposable automation owns a daemon as well as an app. An app data-path
//! override alone would still load and save the working daemon's task state.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use crossbeam_channel::Sender;
use parking_lot::Mutex;
use uuid::Uuid;
use waku_client::{AutomationMessage, DaemonAutomationError, DaemonClient};
use waku_protocol::{
    APP_EXECUTABLE_ENV, AutomationCommand, AutomationDiff, AutomationError, AutomationExpiry,
    AutomationInstance, AutomationProgram, AutomationResult, AutomationRun, AutomationScratchSeed,
    AutomationSession, AutomationStatus, DAEMON_ADDRESS_ENV, DAEMON_TOKEN_ENV, DaemonReady,
    PROTOCOL_VERSION, ServerMessage,
};

use crate::model::TaskWorkspace;
use crate::persistence::{PersistedState, StateStore};

const MAX_INSTANCES: usize = 4;
const MAX_SESSIONS: usize = 64;
const MAX_RESULTS: usize = 32;
const MAX_RESULT_BYTES: usize = 32 * 1024 * 1024;
const START_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_STARTUP_STDERR_BYTES: usize = 4096;

pub(crate) struct ScratchManager {
    inner: Arc<Inner>,
}

struct Inner {
    root: PathBuf,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    instances: HashMap<Uuid, Arc<Instance>>,
    // Hidden from discovery, but retained until stop has reaped the children
    // and removed their directory so concurrent removal can join teardown.
    removing: HashMap<Uuid, Arc<Instance>>,
    sessions: HashMap<Uuid, Arc<Route>>,
    subscribers: HashMap<u64, Arc<AtomicBool>>,
    results: VecDeque<CachedRun>,
    result_bytes: usize,
    creating: usize,
    attaching: usize,
    stopped: bool,
}

struct Instance {
    identity: AutomationInstance,
    address: String,
    // This capability never leaves the controller's private memory/environment.
    token: String,
    control: DaemonClient,
    owned: Mutex<Option<OwnedScratch>>,
    run: Mutex<Option<RunOwner>>,
}

#[derive(Clone)]
struct RunOwner {
    session: AutomationSession,
    run_id: Option<Uuid>,
}

struct Route {
    instance: Arc<Instance>,
    session: Mutex<AutomationSession>,
    // Serialize a session's attach/resume and calls, never all scratch sessions.
    operation: Mutex<()>,
    connection: Mutex<Option<Arc<Connection>>>,
}

struct Connection {
    client: DaemonClient,
    subscriber: u64,
    outgoing: Sender<ServerMessage>,
    subscriber_alive: Arc<AtomicBool>,
    alive: AtomicBool,
}

impl Connection {
    fn close(&self) {
        if self.alive.swap(false, Ordering::AcqRel) {
            self.client.disconnect();
        }
    }
    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire) && self.subscriber_alive.load(Ordering::Acquire)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.close();
    }
}

struct CachedRun {
    session: AutomationSession,
    run: AutomationRun,
    bytes: usize,
}

struct OwnedScratch {
    directory: PathBuf,
    daemon: Option<Child>,
    app: Option<Child>,
    daemon_stderr: Option<StartupStderr>,
    app_stderr: Option<StartupStderr>,
}

// Keep only a small, redacted tail in memory. Pipes are drained for the owned
// lifetime, so noisy children cannot block or fill the scratch filesystem.
struct StartupStderr {
    tail: Arc<Mutex<StderrTail>>,
    #[cfg(unix)]
    wake: Option<std::os::unix::net::UnixStream>,
    finished: Arc<AtomicBool>,
    reader: Option<std::thread::JoinHandle<()>>,
}

struct StderrTail {
    bytes: VecDeque<u8>,
    // Withhold a possible token prefix across read/retention boundaries.
    pending: Vec<u8>,
    token: Vec<u8>,
}

impl StderrTail {
    fn new(token: &str) -> Self {
        Self {
            bytes: VecDeque::with_capacity(MAX_STARTUP_STDERR_BYTES),
            pending: Vec::new(),
            token: token.as_bytes().to_vec(),
        }
    }

    fn keep(&mut self, byte: u8) {
        if self.bytes.len() == MAX_STARTUP_STDERR_BYTES {
            self.bytes.pop_front();
        }
        self.bytes.push_back(byte);
    }

    fn append(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.pending.push(byte);
            while !self.pending.is_empty() && !self.token.starts_with(&self.pending) {
                let safe = self.pending.remove(0);
                self.keep(safe);
            }
            if !self.token.is_empty() && self.pending == self.token {
                for &byte in b"[redacted]" {
                    self.keep(byte);
                }
                self.pending.clear();
            }
        }
    }

    fn finish(&mut self) {
        // Even an incomplete capability at EOF must not escape in an error.
        if !self.pending.is_empty() {
            for &byte in b"[redacted]" {
                self.keep(byte);
            }
            self.pending.clear();
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes.iter().copied().collect::<Vec<_>>())
            .chars()
            .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
            .collect::<String>()
            .trim()
            .to_owned()
    }
}

impl StartupStderr {
    fn capture(child: &mut Child, token: &str) -> anyhow::Result<Option<Self>> {
        #[cfg(unix)]
        {
            use std::io::Read as _;
            use std::os::fd::AsRawFd as _;
            let mut stderr = child.stderr.take().context("scratch stderr unavailable")?;
            // Read only after poll signals readiness. Nonblocking mode handles
            // spurious readiness without preventing the cancellation wake.
            let fd = stderr.as_raw_fd();
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags == -1
                || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
            {
                return Err(std::io::Error::last_os_error())
                    .context("could not capture scratch stderr");
            }
            let tail = Arc::new(Mutex::new(StderrTail::new(token)));
            let (wake, wake_reader) = std::os::unix::net::UnixStream::pair()?;
            let finished = Arc::new(AtomicBool::new(false));
            let reader_tail = tail.clone();
            let reader_finished = finished.clone();
            let reader = std::thread::Builder::new()
                .name("waku-scratch-stderr".into())
                .spawn(move || {
                    let mut chunk = [0; 4096];
                    let mut wait = [
                        libc::pollfd {
                            fd,
                            events: libc::POLLIN,
                            revents: 0,
                        },
                        libc::pollfd {
                            fd: wake_reader.as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        },
                    ];
                    loop {
                        // No idle polling: closing the paired endpoint wakes
                        // Drop even if a descendant still holds stderr open.
                        if unsafe { libc::poll(wait.as_mut_ptr(), wait.len() as _, -1) } < 0 {
                            if std::io::Error::last_os_error().kind()
                                == std::io::ErrorKind::Interrupted
                            {
                                continue;
                            }
                            break;
                        }
                        if wait[1].revents != 0 {
                            break;
                        }
                        match stderr.read(&mut chunk) {
                            Ok(0) => break,
                            Ok(count) => reader_tail.lock().append(&chunk[..count]),
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock
                                        | std::io::ErrorKind::Interrupted
                                ) => {}
                            Err(_) => break,
                        }
                    }
                    reader_tail.lock().finish();
                    reader_finished.store(true, Ordering::Release);
                })?;
            Ok(Some(Self {
                tail,
                wake: Some(wake),
                finished,
                reader: Some(reader),
            }))
        }
        #[cfg(not(unix))]
        {
            let _ = (child, token);
            Ok(None)
        }
    }

    fn evidence(&self, exited: bool) -> String {
        // Usually EOF is already observed. Give the reader a bounded chance to
        // consume an exited child's final crash line before taking the snapshot.
        let deadline = Instant::now() + Duration::from_millis(100);
        while exited && !self.finished.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.tail.lock().text()
    }
}

impl Drop for StartupStderr {
    fn drop(&mut self) {
        #[cfg(unix)]
        drop(self.wake.take());
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl OwnedScratch {
    fn startup_failure(&mut self, message: &str) -> anyhow::Error {
        let mut evidence = message.to_owned();
        for (name, child, stderr) in [
            ("daemon", &mut self.daemon, &self.daemon_stderr),
            ("app", &mut self.app, &self.app_stderr),
        ] {
            let (status, exited) = match child {
                Some(child) => match child.try_wait() {
                    Ok(Some(status)) => (format!("exited with {status}"), true),
                    Ok(None) => ("still running".into(), false),
                    Err(error) => (format!("status unavailable: {error}"), false),
                },
                None => ("not started".into(), false),
            };
            evidence.push_str(&format!("; {name}: {status}"));
            if let Some(stderr) = stderr {
                let tail = stderr.evidence(exited);
                if !tail.is_empty() {
                    evidence.push_str(&format!("; {name} stderr tail: {tail}"));
                }
            }
        }
        anyhow::anyhow!(evidence)
    }
}

impl Drop for OwnedScratch {
    fn drop(&mut self) {
        for child in [&mut self.app, &mut self.daemon].into_iter().flatten() {
            // Each child leads a process group created by us. Never discover
            // or kill processes by name, executable path, or untrusted pid.
            if matches!(child.try_wait(), Ok(None)) {
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

impl Instance {
    fn identity(&self) -> AutomationInstance {
        let mut identity = self.identity.clone();
        identity.executor_connected = self.owned.lock().as_mut().is_some_and(|owned| {
            (self.identity.headless
                || owned
                    .app
                    .as_mut()
                    .is_some_and(|child| matches!(child.try_wait(), Ok(None))))
                && owned
                    .daemon
                    .as_mut()
                    .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
        });
        identity
    }

    fn stop(&self) {
        let mut owned = self.owned.lock();
        let Some(processes) = owned.as_mut() else {
            return;
        };
        // Only this private, instance-owned control transport can stop its
        // daemon. Let backend shutdown close terminals/provider children that
        // have their own process groups before the bounded kill/reap fallback.
        self.control.shutdown();
        let deadline = Instant::now() + STOP_TIMEOUT;
        while processes
            .daemon
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        self.control.disconnect();
        drop(owned.take());
    }
}

impl ScratchManager {
    pub(crate) fn new(data_directory: &Path) -> Self {
        Self {
            inner: Arc::new(Inner {
                root: data_directory.join("rpa-scratch"),
                state: Mutex::new(State::default()),
            }),
        }
    }

    pub(crate) fn handle(&self, command: &AutomationCommand) -> anyhow::Result<AutomationResult> {
        if !cfg!(debug_assertions) {
            return Ok(failure(
                "unavailable",
                "scratch automation requires a debug build",
            ));
        }
        match command {
            AutomationCommand::ScratchCreate {
                name,
                seed,
                headless,
            } => self.create(name.as_deref(), seed, *headless),
            AutomationCommand::ScratchRemove { instance_id } => {
                remove_instance(&self.inner, *instance_id);
                Ok(AutomationResult::ScratchRemoved {
                    instance_id: *instance_id,
                })
            }
            _ => bail!("not a scratch lifecycle command"),
        }
    }

    fn create(
        &self,
        name: Option<&str>,
        seed: &AutomationScratchSeed,
        headless: bool,
    ) -> anyhow::Result<AutomationResult> {
        self.create_program(name, seed, headless, None)
    }

    fn create_program(
        &self,
        name: Option<&str>,
        seed: &AutomationScratchSeed,
        headless: bool,
        cancellation: Option<(&AtomicBool, Instant)>,
    ) -> anyhow::Result<AutomationResult> {
        {
            let mut state = self.inner.state.lock();
            if state.stopped
                || state.instances.len() + state.removing.len() + state.creating >= MAX_INSTANCES
            {
                return Ok(failure("capacity", "scratch instance limit reached"));
            }
            state.creating += 1;
        }
        let result = (|| {
            let app = if headless {
                PathBuf::new()
            } else {
                std::env::var_os(APP_EXECUTABLE_ENV)
                    .map(PathBuf::from)
                    .context("the daemon has no owning app executable")?
            };
            let daemon = std::env::current_exe().context("could not locate owning daemon")?;
            launch_mode(
                &self.inner.root,
                &daemon,
                &app,
                name,
                seed,
                headless,
                cancellation,
            )
        })();
        let mut state = self.inner.state.lock();
        state.creating -= 1;
        let instance = result?;
        if state.stopped {
            drop(state);
            instance.stop();
            return Ok(failure("unavailable", "the owning daemon is stopping"));
        }
        let identity = instance.identity.clone();
        state
            .instances
            .insert(identity.instance_id, Arc::new(instance));
        Ok(AutomationResult::ScratchCreated { instance: identity })
    }

    pub(crate) fn forward(
        &self,
        command: &AutomationCommand,
        outgoing: Sender<ServerMessage>,
        subscriber: u64,
    ) -> Option<anyhow::Result<AutomationResult>> {
        if matches!(command, AutomationCommand::Instances) {
            return Some(Ok(AutomationResult::Instances {
                instances: self
                    .inner
                    .state
                    .lock()
                    .instances
                    .values()
                    .map(|instance| instance.identity())
                    .collect(),
            }));
        }
        if let AutomationCommand::ScenarioResult { session_id, run_id }
        | AutomationCommand::ScenarioAbort { session_id, run_id } = command
        {
            if let Some(run) = cached_run(&self.inner, *session_id, *run_id) {
                return Some(Ok(AutomationResult::Scenario { run }));
            }
        }
        if let AutomationCommand::Attach { instance_id } = command {
            let instance = self
                .inner
                .state
                .lock()
                .instances
                .get(instance_id)
                .cloned()?;
            return Some(self.attach(instance, outgoing, subscriber));
        }
        let session_id = command_session(command)?;
        if let Some(session) = self
            .inner
            .state
            .lock()
            .results
            .iter()
            .rev()
            .find(|entry| entry.session.session_id == session_id)
            .map(|entry| entry.session.clone())
        {
            let diff = |after: Option<waku_protocol::AutomationCursor>| AutomationDiff {
                events: vec![],
                cursor: session.cursor,
                expiry: after
                    .filter(|after| {
                        after.epoch != session.cursor.epoch
                            || after.sequence < session.cursor.sequence
                    })
                    .map(|requested| AutomationExpiry {
                        requested,
                        oldest_sequence: session.cursor.sequence.saturating_add(1),
                        cursor: session.cursor,
                    }),
            };
            return Some(Ok(match command {
                AutomationCommand::Resume { after, .. } => AutomationResult::Attached {
                    replay: diff(*after),
                    session,
                },
                AutomationCommand::Diff { after, .. } => AutomationResult::Diff {
                    diff: diff(Some(*after)),
                },
                AutomationCommand::Detach { .. } => AutomationResult::Detached,
                _ => failure(
                    "scratch_finished",
                    "scratch scenario finished; its final result remains available",
                ),
            }));
        }
        let route = self.inner.state.lock().sessions.get(&session_id).cloned()?;
        Some(self.forward_session(route, session_id, command, outgoing, subscriber))
    }

    fn subscriber(&self, id: u64) -> Arc<AtomicBool> {
        self.inner
            .state
            .lock()
            .subscribers
            .entry(id)
            .or_insert_with(|| Arc::new(AtomicBool::new(true)))
            .clone()
    }

    fn connect(
        &self,
        instance: &Instance,
        subscriber: u64,
        outgoing: Sender<ServerMessage>,
    ) -> anyhow::Result<Arc<Connection>> {
        let subscriber_alive = self.subscriber(subscriber);
        let client = DaemonClient::connect(&instance.address, instance.token.clone())?;
        Ok(Arc::new(Connection {
            client,
            subscriber,
            outgoing,
            subscriber_alive,
            alive: AtomicBool::new(true),
        }))
    }

    fn attach(
        &self,
        instance: Arc<Instance>,
        outgoing: Sender<ServerMessage>,
        subscriber: u64,
    ) -> anyhow::Result<AutomationResult> {
        {
            let mut state = self.inner.state.lock();
            if state.sessions.len() + state.attaching >= MAX_SESSIONS {
                return Ok(failure("capacity", "too many scratch automation sessions"));
            }
            state.attaching += 1;
        }
        let result = (|| {
            let connection = self.connect(&instance, subscriber, outgoing.clone())?;
            let result = request(
                &connection.client,
                AutomationCommand::Attach {
                    instance_id: instance.identity.instance_id,
                },
            )?;
            if let AutomationResult::Attached { session, replay } = &result {
                let mut state = self.inner.state.lock();
                if !connection.is_alive()
                    || !state.instances.contains_key(&instance.identity.instance_id)
                {
                    connection.close();
                    return Ok(failure(
                        "disconnected",
                        "controller disconnected while attaching",
                    ));
                }
                let route = Arc::new(Route {
                    instance: instance.clone(),
                    session: Mutex::new(session.clone()),
                    operation: Mutex::new(()),
                    connection: Mutex::new(Some(connection.clone())),
                });
                state.sessions.insert(session.session_id, route);
                drop(state);
                relay(
                    &self.inner,
                    &connection,
                    session.session_id,
                    instance.identity.instance_id,
                    outgoing,
                    replay.expiry.clone(),
                )?;
            }
            Ok(result)
        })();
        self.inner.state.lock().attaching -= 1;
        result
    }

    fn forward_session(
        &self,
        route: Arc<Route>,
        session_id: Uuid,
        command: &AutomationCommand,
        outgoing: Sender<ServerMessage>,
        subscriber: u64,
    ) -> anyhow::Result<AutomationResult> {
        let _operation = route.operation.lock();
        if !self
            .inner
            .state
            .lock()
            .instances
            .contains_key(&route.instance.identity.instance_id)
        {
            if let AutomationCommand::ScenarioResult { run_id, .. }
            | AutomationCommand::ScenarioAbort { run_id, .. } = command
            {
                if let Some(run) = cached_run(&self.inner, session_id, *run_id) {
                    return Ok(AutomationResult::Scenario { run });
                }
            }
            return Ok(failure(
                "session_expired",
                "scratch instance has been removed",
            ));
        }
        if matches!(command, AutomationCommand::Detach { .. })
            && route
                .instance
                .run
                .lock()
                .as_ref()
                .is_some_and(|run| run.session.session_id == session_id)
        {
            return Ok(failure(
                "scratch_run_active",
                "abort the scratch scenario before detaching its session",
            ));
        }
        let resuming = matches!(command, AutomationCommand::Resume { .. });
        let connection = if resuming {
            if let Some(old) = route.connection.lock().take() {
                old.close();
            }
            self.connect(&route.instance, subscriber, outgoing.clone())?
        } else {
            let connection = route.connection.lock().clone();
            match connection
                .filter(|connection| connection.subscriber == subscriber && connection.is_alive())
            {
                Some(connection) => connection,
                None => {
                    return Ok(failure(
                        "session_detached",
                        "resume this scratch session before using it",
                    ));
                }
            }
        };
        let starting = matches!(
            command,
            AutomationCommand::ScenarioStart { .. } | AutomationCommand::ReplayStart { .. }
        );
        if starting {
            let mut run = route.instance.run.lock();
            if run.is_some() {
                return Ok(failure(
                    "scratch_run_exists",
                    "a scratch instance owns one scenario; create another scratch instance",
                ));
            }
            *run = Some(RunOwner {
                session: route.session.lock().clone(),
                run_id: None,
            });
        }
        let result = request(&connection.client, command.clone());
        // A terminal push may precede this response and close its child
        // transport during teardown. The cached result is authoritative.
        if result.is_err() {
            let state = self.inner.state.lock();
            if let Some(entry) = state.results.iter().rev().find(|entry| {
                entry.session.session_id == session_id
                    && match command {
                        AutomationCommand::ScenarioStart { .. }
                        | AutomationCommand::ReplayStart { .. } => true,
                        AutomationCommand::ScenarioResult { run_id, .. }
                        | AutomationCommand::ScenarioAbort { run_id, .. } => {
                            entry.run.run_id == *run_id
                        }
                        _ => false,
                    }
            }) {
                return Ok(AutomationResult::Scenario {
                    run: entry.run.clone(),
                });
            }
        }
        if starting {
            match &result {
                Ok(AutomationResult::Scenario { run }) => {
                    // A terminal event can race the initial response. Never
                    // replace the retained final evidence with that pending ack.
                    if let Some(final_run) = cached_run(&self.inner, session_id, run.run_id) {
                        return Ok(AutomationResult::Scenario { run: final_run });
                    }
                    *route.instance.run.lock() = Some(RunOwner {
                        session: route.session.lock().clone(),
                        run_id: Some(run.run_id),
                    });
                    if terminal(run) {
                        finish_run(
                            &self.inner,
                            route.instance.identity.instance_id,
                            session_id,
                            run.clone(),
                        );
                    } else {
                        monitor_run(&self.inner, &route.instance, session_id, run.clone())?;
                    }
                }
                Ok(AutomationResult::Error { .. }) => *route.instance.run.lock() = None,
                // A transport failure may have lost an accepted start's ack.
                // Keep its reservation until its terminal event or explicit
                // removal; never launch a second run over that uncertainty.
                _ => {}
            }
        }
        let result = result?;
        if let AutomationResult::Attached { session, .. }
        | AutomationResult::Subscribed { session } = &result
        {
            *route.session.lock() = session.clone();
        }
        if resuming && matches!(result, AutomationResult::Attached { .. }) {
            let state = self.inner.state.lock();
            if !connection.is_alive()
                || !state
                    .instances
                    .contains_key(&route.instance.identity.instance_id)
            {
                connection.close();
                return Ok(failure(
                    "disconnected",
                    "controller disconnected while resuming",
                ));
            }
            *route.connection.lock() = Some(connection.clone());
            drop(state);
            relay(
                &self.inner,
                &connection,
                session_id,
                route.instance.identity.instance_id,
                outgoing,
                match &result {
                    AutomationResult::Attached { replay, .. } => replay.expiry.clone(),
                    _ => None,
                },
            )?;
        }
        if (matches!(command, AutomationCommand::Detach { .. })
            && matches!(result, AutomationResult::Detached))
            || matches!(&result, AutomationResult::Error { error } if error.code == "session_expired")
        {
            connection.close();
            self.inner.state.lock().sessions.remove(&session_id);
        }
        if let AutomationResult::Scenario { run } = &result {
            if terminal(run) {
                finish_run(
                    &self.inner,
                    route.instance.identity.instance_id,
                    session_id,
                    run.clone(),
                );
            }
        }
        Ok(result)
    }

    pub(crate) fn disconnected(&self, subscriber: u64) {
        let state = &mut *self.inner.state.lock();
        if let Some(alive) = state.subscribers.remove(&subscriber) {
            alive.store(false, Ordering::Release);
        }
        for route in state.sessions.values() {
            let mut connection = route.connection.lock();
            if connection
                .as_ref()
                .is_some_and(|connection| connection.subscriber == subscriber)
            {
                if let Some(connection) = connection.take() {
                    connection.close();
                }
            }
        }
    }

    /// Execute one program on a fresh owned target. The guard removes only
    /// this instance on every return, including cancellation and transport error.
    pub(crate) fn run_program(
        &self,
        program: &AutomationProgram,
        seed: &AutomationScratchSeed,
        headless: bool,
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> std::result::Result<AutomationRun, AutomationError> {
        let cancellation = Some((cancel, deadline));
        check_program_deadline(cancellation).map_err(program_error)?;
        if let AutomationProgram::Replay { replay, parameters } = program {
            crate::automation_recording::preflight(replay, parameters)?;
        }
        let created = self
            .create_program(Some("unattended job"), seed, headless, cancellation)
            .map_err(program_error)?;
        let identity = match created {
            AutomationResult::ScratchCreated { instance } => instance,
            AutomationResult::Error { error } => return Err(error),
            _ => {
                return Err(AutomationError::new(
                    "job_failed",
                    "unexpected scratch creation response",
                ));
            }
        };
        struct OwnedProgram {
            inner: Arc<Inner>,
            instance_id: Uuid,
        }
        impl Drop for OwnedProgram {
            fn drop(&mut self) {
                remove_instance(&self.inner, self.instance_id);
            }
        }
        let _owned = OwnedProgram {
            inner: self.inner.clone(),
            instance_id: identity.instance_id,
        };
        let instance = self
            .inner
            .state
            .lock()
            .instances
            .get(&identity.instance_id)
            .cloned()
            .ok_or_else(|| AutomationError::new("job_aborted", "owned scratch was removed"))?;
        let request = |command| -> std::result::Result<AutomationResult, AutomationError> {
            check_program_deadline(cancellation).map_err(program_error)?;
            instance
                .control
                .automation_with_timeout(
                    command,
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(REQUEST_TIMEOUT),
                )
                .map_err(program_error)
        };
        let session_id = match request(AutomationCommand::Attach {
            instance_id: identity.instance_id,
        })? {
            AutomationResult::Attached { session, .. } => session.session_id,
            _ => return Err(AutomationError::new("job_failed", "scratch did not attach")),
        };
        let command = match program {
            AutomationProgram::Scenario { scenario } => AutomationCommand::ScenarioStart {
                session_id,
                scenario: scenario.clone(),
            },
            AutomationProgram::Replay { replay, parameters } => AutomationCommand::ReplayStart {
                session_id,
                replay: replay.clone(),
                parameters: parameters.clone(),
            },
        };
        let mut run = match request(command)? {
            AutomationResult::Scenario { run } => run,
            _ => {
                return Err(AutomationError::new(
                    "job_failed",
                    "scratch did not start its program",
                ));
            }
        };
        while matches!(run.status, AutomationStatus::Pending) {
            if let Err(error) = check_program_deadline(cancellation) {
                return Ok(interrupt_program(
                    &instance,
                    session_id,
                    run,
                    program_error(error),
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
            match request(AutomationCommand::ScenarioResult {
                session_id,
                run_id: run.run_id,
            }) {
                Ok(AutomationResult::Scenario { run: latest }) => run = latest,
                Err(error) => {
                    // Cancellation can arrive during the sleep or an RPC. Keep
                    // the active program's evidence on both interruption paths.
                    if let Err(interruption) = check_program_deadline(cancellation) {
                        return Ok(interrupt_program(
                            &instance,
                            session_id,
                            run,
                            program_error(interruption),
                        ));
                    }
                    run.status = AutomationStatus::Failed;
                    run.error = Some(error);
                    return Ok(run);
                }
                _ => {
                    run.status = AutomationStatus::Failed;
                    run.error = Some(AutomationError::new(
                        "job_failed",
                        "scratch lost its program result",
                    ));
                    return Ok(run);
                }
            }
        }
        Ok(run)
    }

    pub(crate) fn shutdown(&self) {
        let instances = {
            let mut state = self.inner.state.lock();
            state.stopped = true;
            for alive in state.subscribers.values() {
                alive.store(false, Ordering::Release);
            }
            for route in state.sessions.values() {
                if let Some(connection) = route.connection.lock().take() {
                    connection.close();
                }
            }
            state.sessions.clear();
            state.subscribers.clear();
            let instances = std::mem::take(&mut state.instances);
            state.removing.extend(instances);
            state.removing.values().cloned().collect::<Vec<_>>()
        };
        for instance in instances {
            stop_instance(&self.inner, instance);
        }
    }
}

impl Drop for ScratchManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn interrupt_program(
    instance: &Instance,
    session_id: Uuid,
    mut run: AutomationRun,
    failure: AutomationError,
) -> AutomationRun {
    // Fetch the executor's latest first-failure/step evidence before owned
    // cleanup. A stalled executor still leaves the most recently observed run.
    if let Ok(AutomationResult::Scenario { run: aborted }) =
        instance.control.automation_with_timeout(
            AutomationCommand::ScenarioAbort {
                session_id,
                run_id: run.run_id,
            },
            Duration::from_secs(1),
        )
    {
        run = aborted;
    }
    run.status = if failure.code == "job_aborted" {
        AutomationStatus::Aborted
    } else {
        AutomationStatus::Failed
    };
    run.error = Some(failure);
    run
}

fn startup_slice_deadline(
    deadline: Instant,
    cancellation: Option<(&AtomicBool, Instant)>,
) -> Instant {
    // Poll even a stalled handshake/daemon so cancellation remains observable.
    let deadline = deadline.min(Instant::now() + Duration::from_millis(250));
    cancellation.map_or(deadline, |(_, job_deadline)| deadline.min(job_deadline))
}

fn check_program_deadline(cancellation: Option<(&AtomicBool, Instant)>) -> anyhow::Result<()> {
    if let Some((cancel, deadline)) = cancellation {
        if cancel.load(Ordering::Acquire) {
            return Err(anyhow::anyhow!(DaemonAutomationError {
                error: AutomationError::new("job_aborted", "job was cancelled")
            }));
        }
        if Instant::now() >= deadline {
            return Err(anyhow::anyhow!(DaemonAutomationError {
                error: AutomationError::new("job_timeout", "job exceeded its overall deadline")
            }));
        }
    }
    Ok(())
}

fn program_error(error: anyhow::Error) -> AutomationError {
    match error.downcast::<DaemonAutomationError>() {
        Ok(error) => error.error,
        Err(error) => AutomationError::new("job_failed", error.to_string()),
    }
}

fn command_session(command: &AutomationCommand) -> Option<Uuid> {
    match command {
        AutomationCommand::Resume { session_id, .. }
        | AutomationCommand::Detach { session_id }
        | AutomationCommand::Call { session_id, .. }
        | AutomationCommand::Diff { session_id, .. }
        | AutomationCommand::Subscribe { session_id, .. }
        | AutomationCommand::Wait { session_id, .. }
        | AutomationCommand::WaitResult { session_id, .. }
        | AutomationCommand::RecordingStart { session_id, .. }
        | AutomationCommand::RecordingStatus { session_id }
        | AutomationCommand::RecordingStop { session_id }
        | AutomationCommand::RecordingCancel { session_id }
        | AutomationCommand::ReplayStart { session_id, .. }
        | AutomationCommand::ScenarioStart { session_id, .. }
        | AutomationCommand::ScenarioResult { session_id, .. }
        | AutomationCommand::ScenarioAbort { session_id, .. } => Some(*session_id),
        _ => None,
    }
}

fn failure(code: &str, message: &str) -> AutomationResult {
    AutomationResult::Error {
        error: AutomationError::new(code, message),
    }
}

fn request(client: &DaemonClient, command: AutomationCommand) -> anyhow::Result<AutomationResult> {
    client
        .automation_with_timeout(command, REQUEST_TIMEOUT)
        .or_else(|error| {
            if let Some(typed) = error.downcast_ref::<DaemonAutomationError>() {
                Ok(AutomationResult::Error {
                    error: typed.error.clone(),
                })
            } else {
                Err(error)
            }
        })
}

fn terminal(run: &AutomationRun) -> bool {
    !matches!(run.status, AutomationStatus::Pending)
}

fn cached_run(inner: &Inner, session_id: Uuid, run_id: Uuid) -> Option<AutomationRun> {
    inner
        .state
        .lock()
        .results
        .iter()
        .find(|entry| entry.session.session_id == session_id && entry.run.run_id == run_id)
        .map(|entry| entry.run.clone())
}

fn take_instance(state: &mut State, instance_id: Uuid) -> Option<Arc<Instance>> {
    let instance = if let Some(instance) = state.instances.remove(&instance_id) {
        state.removing.insert(instance_id, instance.clone());
        Some(instance)
    } else {
        state.removing.get(&instance_id).cloned()
    };
    state.sessions.retain(|_, route| {
        if route.instance.identity.instance_id != instance_id {
            return true;
        }
        if let Some(connection) = route.connection.lock().take() {
            connection.close();
        }
        false
    });
    instance
}

fn stop_instance(inner: &Inner, instance: Arc<Instance>) {
    // Instance::stop holds the owned lock through process reaping and directory
    // deletion, including when another remover has already started stopping it.
    instance.stop();
    inner
        .state
        .lock()
        .removing
        .remove(&instance.identity.instance_id);
}

fn remove_instance(inner: &Inner, instance_id: Uuid) {
    let instance = take_instance(&mut inner.state.lock(), instance_id);
    if let Some(instance) = instance {
        stop_instance(inner, instance);
    }
}

fn finish_run(inner: &Inner, instance_id: Uuid, session_id: Uuid, run: AutomationRun) {
    if !terminal(&run) {
        return;
    }
    let instance = {
        let mut state = inner.state.lock();
        let Some(instance) = state.instances.get(&instance_id) else {
            return;
        };
        let Some(owner) = instance.run.lock().clone().filter(|owner| {
            owner.session.session_id == session_id && owner.run_id.is_none_or(|id| id == run.run_id)
        }) else {
            return;
        };
        let mut session = owner.session;
        session.cursor = run.cursor;
        session.instance.executor_connected = false;
        if let Some(route) = state.sessions.get(&session_id) {
            if let Some(connection) = route
                .connection
                .lock()
                .as_ref()
                .filter(|connection| connection.is_alive())
            {
                let _ = connection
                    .outgoing
                    .try_send(ServerMessage::AutomationRunChanged {
                        session_id,
                        run: run.clone(),
                    });
            }
        }
        let bytes = serde_json::to_vec(&run).map_or(0, |bytes| bytes.len());
        state.result_bytes += bytes;
        state.results.push_back(CachedRun {
            session,
            run,
            bytes,
        });
        while state.results.len() > MAX_RESULTS
            || (state.result_bytes > MAX_RESULT_BYTES && state.results.len() > 1)
        {
            if let Some(old) = state.results.pop_front() {
                state.result_bytes -= old.bytes;
            }
        }
        take_instance(&mut state, instance_id)
    };
    if let Some(instance) = instance {
        stop_instance(inner, instance);
    }
}

fn relay(
    inner: &Arc<Inner>,
    connection: &Arc<Connection>,
    session_id: Uuid,
    instance_id: Uuid,
    outgoing: Sender<ServerMessage>,
    mut acknowledged_expiry: Option<AutomationExpiry>,
) -> anyhow::Result<()> {
    let messages = connection.client.subscribe_automation(session_id);
    let connection = Arc::downgrade(connection);
    let inner = Arc::downgrade(inner);
    std::thread::Builder::new()
        .name("waku-scratch-events".into())
        .spawn(move || {
            loop {
                let Some(connection) = connection
                    .upgrade()
                    .filter(|connection| connection.is_alive())
                else {
                    break;
                };
                let message = match messages.recv_timeout(Duration::from_millis(100)) {
                    Ok(message) => message,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                    Err(_) => {
                        connection.close();
                        break;
                    }
                };
                let completed = match &message {
                    AutomationMessage::RunChanged(run) if terminal(run) => Some(run.clone()),
                    _ => None,
                };
                let wire = match message {
                    AutomationMessage::Event(event) => {
                        Some(ServerMessage::AutomationEvent { event })
                    }
                    AutomationMessage::WaitResolved(wait) => {
                        Some(ServerMessage::AutomationWaitResolved { session_id, wait })
                    }
                    AutomationMessage::RunChanged(run) if terminal(&run) => None,
                    AutomationMessage::RunChanged(run) => {
                        Some(ServerMessage::AutomationRunChanged { session_id, run })
                    }
                    // No unsolicited expiry wire message exists. Stop delivery;
                    // Resume/Diff reports the child's authoritative gap and cursor.
                    AutomationMessage::Expired(expiry) => {
                        let already_reported = acknowledged_expiry.take().is_some_and(|known| {
                            known.requested == expiry.requested
                                && known.cursor == expiry.cursor
                                && known.oldest_sequence == expiry.oldest_sequence
                        });
                        if !already_reported {
                            connection.close();
                        }
                        None
                    }
                };
                if wire.is_some_and(|wire| outgoing.try_send(wire).is_err()) {
                    connection.close();
                }
                if let Some(run) = completed {
                    if let Some(inner) = inner.upgrade() {
                        finish_run(&inner, instance_id, session_id, run);
                    }
                }
            }
        })?;
    Ok(())
}

fn monitor_run(
    inner: &Arc<Inner>,
    instance: &Arc<Instance>,
    session_id: Uuid,
    mut last_run: AutomationRun,
) -> anyhow::Result<()> {
    let run_id = last_run.run_id;
    let inner: Weak<Inner> = Arc::downgrade(inner);
    let instance = Arc::downgrade(instance);
    // This connection never attaches. It only retrieves the run's result if
    // the controller goes away, so no abandoned UI subscription is kept alive.
    std::thread::Builder::new()
        .name("waku-scratch-run".into())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(250));
                let (Some(inner), Some(instance)) = (inner.upgrade(), instance.upgrade()) else {
                    break;
                };
                if !inner
                    .state
                    .lock()
                    .instances
                    .contains_key(&instance.identity.instance_id)
                {
                    break;
                }
                match request(
                    &instance.control,
                    AutomationCommand::ScenarioResult { session_id, run_id },
                ) {
                    Ok(AutomationResult::Scenario { run }) if terminal(&run) => {
                        finish_run(&inner, instance.identity.instance_id, session_id, run);
                        break;
                    }
                    Ok(AutomationResult::Scenario { run }) => last_run = run,
                    result => {
                        last_run.status = AutomationStatus::Failed;
                        last_run.error = Some(match result {
                            Ok(AutomationResult::Error { error }) => error,
                            Err(error) => {
                                AutomationError::new("scratch_unavailable", error.to_string())
                            }
                            _ => AutomationError::new(
                                "scratch_unavailable",
                                "scratch daemon lost its run result",
                            ),
                        });
                        finish_run(&inner, instance.identity.instance_id, session_id, last_run);
                        break;
                    }
                }
            }
        })?;
    Ok(())
}

fn launch(
    root: &Path,
    daemon_executable: &Path,
    app_executable: &Path,
    name: Option<&str>,
    seed: &AutomationScratchSeed,
) -> anyhow::Result<Instance> {
    launch_mode(
        root,
        daemon_executable,
        app_executable,
        name,
        seed,
        false,
        None,
    )
}

fn launch_mode(
    root: &Path,
    daemon_executable: &Path,
    app_executable: &Path,
    name: Option<&str>,
    seed: &AutomationScratchSeed,
    headless: bool,
    cancellation: Option<(&AtomicBool, Instant)>,
) -> anyhow::Result<Instance> {
    check_program_deadline(cancellation)?;
    let id = Uuid::new_v4();
    std::fs::create_dir_all(root)?;
    let directory = root.canonicalize()?.join(id.to_string());
    std::fs::create_dir(&directory)?;
    let mut owned = OwnedScratch {
        directory,
        daemon: None,
        app: None,
        daemon_stderr: None,
        app_stderr: None,
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&owned.directory, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut seed = seed.clone();
    if headless && seed.projects.is_empty() {
        seed.projects.push(crate::model::Project::from_path(
            owned.directory.join("project"),
        ));
    }
    seed_database(&owned.directory, &seed)?;
    for child in ["home", "config", "data", "cache", "state", "runtime", "tmp"] {
        std::fs::create_dir_all(owned.directory.join(child))?;
    }
    // Scratch starts provider-free. No inherited profile, automatic model
    // discovery, or provider startup can reach the working account.
    let settings = crate::DaemonSettings {
        disabled_providers: crate::model::ProviderKind::ALL.to_vec(),
        ..Default::default()
    };
    crate::DaemonSettingsStore::open(owned.directory.join("home/.waku/settings.json"))?
        .replace(settings)?;
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let mut daemon = isolated_command(daemon_executable, &owned.directory);
    daemon
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .env(DAEMON_TOKEN_ENV, &token)
        .env(APP_EXECUTABLE_ENV, app_executable)
        .stdout(Stdio::piped());
    if headless {
        daemon
            .arg("--automation-headless")
            .env("WAKU_RPA_INSTANCE_ID", id.to_string())
            .env("WAKU_RPA_SCRATCH_NAME", name.unwrap_or("automation"))
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY");
    }
    owned.daemon = Some(daemon.spawn().context("could not start scratch daemon")?);
    owned.daemon_stderr = StartupStderr::capture(owned.daemon.as_mut().unwrap(), &token)?;
    let stdout = owned
        .daemon
        .as_mut()
        .unwrap()
        .stdout
        .take()
        .context("scratch daemon has no readiness stream")?;
    let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
    std::thread::Builder::new()
        .name("waku-scratch-ready".into())
        .spawn(move || {
            let result = (|| -> anyhow::Result<DaemonReady> {
                use std::io::Read as _;
                let mut reader = BufReader::new(stdout.take(64 * 1024));
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line)? == 0 {
                        bail!("scratch daemon exited before readiness");
                    }
                    if let Ok(ready) = serde_json::from_str::<DaemonReady>(&line) {
                        return Ok(ready);
                    }
                }
            })();
            let _ = ready_tx.send(result);
        })?;
    let startup_deadline = Instant::now() + START_TIMEOUT;
    let ready = loop {
        check_program_deadline(cancellation)?;
        match ready_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(result) => {
                break result.map_err(|_| {
                    owned.startup_failure("scratch daemon did not report readiness")
                })?;
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout)
                if Instant::now() < startup_deadline =>
            {
                continue;
            }
            Err(_) => return Err(owned.startup_failure("scratch daemon did not become ready")),
        }
    };
    let address: std::net::SocketAddr = ready
        .address
        .parse()
        .context("invalid scratch daemon address")?;
    if ready.protocol_version != PROTOCOL_VERSION
        || !address.ip().is_loopback()
        || ready.pid != owned.daemon.as_ref().unwrap().id()
    {
        bail!("scratch daemon reported an incompatible identity or address");
    }
    let connect_deadline = Instant::now() + START_TIMEOUT;
    let control = loop {
        check_program_deadline(cancellation)?;
        let slice_deadline = startup_slice_deadline(connect_deadline, cancellation);
        match DaemonClient::connect_with_deadline(&ready.address, token.clone(), slice_deadline) {
            Ok(control) => break control,
            Err(_) if Instant::now() < connect_deadline => {
                if owned.daemon.as_mut().unwrap().try_wait()?.is_some() {
                    return Err(owned.startup_failure("scratch daemon exited during connection"));
                }
            }
            Err(_) => return Err(owned.startup_failure("scratch control connection timed out")),
        }
    };
    if !headless {
        let mut app = isolated_command(app_executable, &owned.directory);
        app.env(DAEMON_ADDRESS_ENV, &ready.address)
            .env(DAEMON_TOKEN_ENV, &token)
            .env("WAKU_RPA_INSTANCE_ID", id.to_string())
            .env(
                "WAKU_RPA_SCRATCH_NAME",
                name.filter(|name| !name.trim().is_empty())
                    .unwrap_or("automation"),
            );
        owned.app = Some(app.spawn().context("could not start scratch app")?);
        owned.app_stderr = StartupStderr::capture(owned.app.as_mut().unwrap(), &token)?;
    }
    let deadline = Instant::now() + START_TIMEOUT;
    let identity = loop {
        check_program_deadline(cancellation)?;
        if let Ok(AutomationResult::Instances { instances }) = control.automation_with_timeout(
            AutomationCommand::Instances,
            startup_slice_deadline(deadline, cancellation)
                .saturating_duration_since(Instant::now()),
        ) {
            if let Some(identity) = instances
                .into_iter()
                .find(|instance| instance.instance_id == id && instance.executor_connected)
            {
                break identity;
            }
        }
        if owned
            .app
            .as_mut()
            .map(|app| app.try_wait())
            .transpose()?
            .flatten()
            .is_some()
            || owned.daemon.as_mut().unwrap().try_wait()?.is_some()
        {
            control.disconnect();
            return Err(owned.startup_failure("scratch child exited before executor registration"));
        }
        if Instant::now() >= deadline {
            control.disconnect();
            return Err(
                owned.startup_failure("scratch app did not register its automation executor")
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    if identity.database != owned.directory.join("app.db").display().to_string()
        || !identity.scratch
        || identity.headless != headless
        || identity.pid
            != if headless {
                ready.pid
            } else {
                owned.app.as_ref().unwrap().id()
            }
    {
        control.disconnect();
        bail!("scratch app registered without the isolated database or process identity");
    }
    Ok(Instance {
        identity,
        address: ready.address,
        token,
        control,
        owned: Mutex::new(Some(owned)),
        run: Mutex::new(None),
    })
}

fn isolated_command(executable: &Path, directory: &Path) -> Command {
    let mut command = Command::new(executable);
    // Deliberately allowlist only process/GUI plumbing. In particular, never
    // inherit provider keys, agent profiles, credential paths, or original HOME.
    command.env_clear();
    for key in [
        "PATH",
        "DISPLAY",
        "XAUTHORITY",
        // Explicit renderer selection is platform plumbing, not user/profile
        // configuration. In particular, headless GPUI needs the parent's
        // software Vulkan driver instead of silently selecting absent hardware.
        "LIBGL_ALWAYS_SOFTWARE",
        "VK_ICD_FILENAMES",
        "VK_DRIVER_FILES",
        "LANG",
        "LC_ALL",
        "LD_LIBRARY_PATH",
        "DYLD_LIBRARY_PATH",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    for (key, child) in [
        ("HOME", "home"),
        ("USERPROFILE", "home"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_RUNTIME_DIR", "runtime"),
        ("TMPDIR", "tmp"),
        ("TMP", "tmp"),
        ("TEMP", "tmp"),
    ] {
        command.env(key, directory.join(child));
    }
    command
        .env("WAKU_RPA_DATA_DIR", directory)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0).stderr(Stdio::piped());
    }
    command
}

fn seed_database(directory: &Path, seed: &AutomationScratchSeed) -> anyhow::Result<()> {
    let mut state = PersistedState::empty();
    for project in &seed.projects {
        if state
            .projects
            .iter()
            .any(|existing| existing.id == project.id)
        {
            bail!("scratch seed has duplicate project ids");
        }
        let mut project = project.clone();
        // Seed content is metadata, never permission to edit the source checkout.
        project.path = directory.join("projects").join(project.id.to_string());
        std::fs::create_dir_all(&project.path)?;
        state.projects.push(project);
    }
    for task in &seed.tasks {
        if state.tasks.iter().any(|existing| existing.id == task.id) {
            bail!("scratch seed has duplicate task ids");
        }
        let mut task = task.clone();
        task.workspace = match task.workspace {
            Some(workspace) => {
                let project_id = match workspace {
                    TaskWorkspace::Local { project_id }
                    | TaskWorkspace::NewWorktree { project_id }
                    | TaskWorkspace::Worktree { project_id, .. } => project_id,
                };
                if !state
                    .projects
                    .iter()
                    .any(|project| project.id == project_id)
                {
                    bail!("scratch seed task refers to a missing project");
                }
                // Never retain a real checkout's worktree path in a disposable
                // instance. Deferred work is still only a plan in its own project.
                Some(match workspace {
                    TaskWorkspace::NewWorktree { .. } => TaskWorkspace::NewWorktree { project_id },
                    _ => TaskWorkspace::Local { project_id },
                })
            }
            None => None,
        };
        state.tasks.push(task);
    }
    for session in &seed.sessions {
        if !state
            .projects
            .iter()
            .any(|project| project.id == session.project_id)
        {
            bail!("scratch seed session refers to a missing project");
        }
        if state
            .sessions
            .iter()
            .any(|existing| existing.id == session.id)
        {
            bail!("scratch seed has duplicate session ids");
        }
        let mut session = session.clone();
        // Session-only fixtures must not preserve references to issues in the
        // working database. Explicitly seeded issues keep valid local bindings.
        session.task_id = session
            .task_id
            .filter(|task_id| state.tasks.iter().any(|task| task.id == *task_id));
        if let Some(task_id) = session.task_id {
            let task = state.tasks.iter().find(|task| task.id == task_id).unwrap();
            let project_id = match task.workspace {
                Some(TaskWorkspace::Local { project_id })
                | Some(TaskWorkspace::NewWorktree { project_id })
                | Some(TaskWorkspace::Worktree { project_id, .. }) => Some(project_id),
                None => None,
            };
            if project_id.is_some_and(|project_id| project_id != session.project_id) {
                bail!("scratch seed session and issue refer to different projects");
            }
        }
        session.workspace = Default::default();
        session.provider_cursor = None;
        session.provider_session_id = None;
        session.runtime_event_cursor = None;
        session.status = Default::default();
        session.detail_loaded = true;
        session.queued_messages.clear();
        state.push_session(session);
    }
    state.selected_project = state.projects.first().map(|project| project.id);
    state.selected_session = state.sessions.first().map(|session| session.id);
    StateStore::new(directory.join("app.db")).save_scratch_seed(&mut state)?;
    Ok(())
}

#[cfg(all(test, unix, debug_assertions))]
mod tests {
    use super::*;
    use crate::{Backend, EventSink, Request, ResponsePayload};
    use crossbeam_channel::{Receiver, bounded};
    use serde_json::json;
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt as _;
    use waku_client::AutomationExecutorMessage;
    use waku_protocol::{
        AutomationCall, AutomationCursor, AutomationExecution, AutomationExecutorOutcome,
        AutomationExecutorReply, AutomationFailureBundle, AutomationObservation,
        AutomationScenario, AutomationStep, AutomationStepOperation, AutomationStream,
        DiagnosticCounters,
    };

    struct NoBackend {
        shutdown_marker: PathBuf,
    }
    impl Backend for NoBackend {
        fn handle(&self, _: Request, _: EventSink) -> anyhow::Result<ResponsePayload> {
            bail!("fixture supports only automation")
        }
        fn shutdown(&self) {
            std::fs::write(&self.shutdown_marker, "child backend shutdown").unwrap();
        }
    }

    fn fixture_error() -> AutomationError {
        AutomationError {
            code: "fixture_failure".into(),
            message: "preserve the child's evidence".into(),
            target: Some("fixture.target".into()),
            nearest: Some(vec!["fixture.neighbor".into()]),
            bundle: Some(AutomationFailureBundle {
                observed: json!({"fixture":"child", "frame":7}),
                tree_diff: json!({"changed":["fixture.target"]}),
                recent_events: None,
                counters: DiagnosticCounters {
                    observed_at_ms: 7,
                    processes: vec![],
                },
                diagnostic_errors: vec!["fixture diagnostic detail".into()],
            }),
        }
    }

    fn assert_failure(error: &AutomationError) {
        let expected = fixture_error();
        assert_eq!(error.code, expected.code);
        assert_eq!(error.message, expected.message);
        assert_eq!(error.target, expected.target);
        assert_eq!(error.nearest, expected.nearest);
        let bundle = error.bundle.as_ref().unwrap();
        let expected = expected.bundle.unwrap();
        assert_eq!(bundle.observed, expected.observed);
        assert_eq!(bundle.tree_diff, expected.tree_diff);
        assert_eq!(bundle.diagnostic_errors, expected.diagnostic_errors);
        assert!(bundle.recent_events.is_some());
    }

    // The test executable is also a deterministic app/daemon fixture. Shell
    // launchers exec it (preserving pid), so launch() exercises real child
    // ownership, env isolation, authentication and normal protocol readiness.
    #[test]
    fn scratch_process_entry() {
        let Ok(role) = std::env::var("WAKU_SCRATCH_TEST_ROLE") else {
            return;
        };
        if role == "environment" {
            let command = isolated_command(Path::new("/bin/true"), Path::new("/scratch-fixture"));
            let vars: HashMap<_, _> = command.get_envs().collect();
            for key in [
                "DISPLAY",
                "XAUTHORITY",
                "LIBGL_ALWAYS_SOFTWARE",
                "VK_ICD_FILENAMES",
                "VK_DRIVER_FILES",
            ] {
                assert_eq!(
                    vars[std::ffi::OsStr::new(key)],
                    std::env::var_os(key).as_deref()
                );
            }
            for key in [
                "OPENAI_API_KEY",
                "CODEX_HOME",
                "WAKU_SCRATCH_TEST_ROLE",
                DAEMON_TOKEN_ENV,
            ] {
                assert!(!vars.contains_key(std::ffi::OsStr::new(key)));
            }
            return;
        }
        std::fs::write(
            shutdown_marker(&StateStore::default_path()).with_extension(format!("{role}.pid")),
            std::process::id().to_string(),
        )
        .unwrap();
        if role == "failed-daemon" || role == "failed-app" {
            std::io::stderr()
                .write_all(&vec![b'x'; MAX_STARTUP_STDERR_BYTES * 8])
                .unwrap();
            eprintln!(
                "\n{role}: fixture renderer unavailable; capability={}",
                std::env::var(DAEMON_TOKEN_ENV).unwrap()
            );
            std::io::stderr().flush().unwrap();
            std::process::exit(27);
        }
        if role == "daemon" || role == "stubborn-daemon" || role == "stalled-daemon" {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let ready = DaemonReady {
                address: listener.local_addr().unwrap().to_string(),
                protocol_version: PROTOCOL_VERSION,
                pid: std::process::id(),
            };
            println!("\n{}", serde_json::to_string(&ready).unwrap());
            std::io::stdout().flush().unwrap();
            if role == "stalled-daemon" {
                let _socket = listener.accept().unwrap();
                std::thread::sleep(Duration::from_secs(60));
                return;
            }
            crate::serve(
                listener,
                std::env::var(DAEMON_TOKEN_ENV).unwrap(),
                Arc::new(NoBackend {
                    shutdown_marker: shutdown_marker(&StateStore::default_path()),
                }),
                Arc::new(AtomicBool::new(false)),
                crate::ServerOptions {
                    allow_shutdown: role == "daemon",
                    ..Default::default()
                },
            )
            .unwrap();
            return;
        }
        let client = DaemonClient::connect(
            &std::env::var(DAEMON_ADDRESS_ENV).unwrap(),
            std::env::var(DAEMON_TOKEN_ENV).unwrap(),
        )
        .unwrap();
        let messages = client.subscribe_automation_executor();
        let identity = AutomationInstance {
            instance_id: std::env::var("WAKU_RPA_INSTANCE_ID")
                .unwrap()
                .parse()
                .unwrap(),
            checkout: "scratch-test-checkout".into(),
            database: StateStore::default_path().display().to_string(),
            pid: std::process::id(),
            build: "scratch-fixture".into(),
            version: "test".into(),
            executor_connected: false,
            scratch: true,
            headless: false,
        };
        client
            .automation(AutomationCommand::RegisterExecutor { instance: identity })
            .unwrap();
        while let Ok(message) = messages.recv() {
            match message {
                AutomationExecutorMessage::Interest { attached, .. } => {
                    std::fs::write(
                        StateStore::default_path().with_file_name("interest"),
                        attached.to_string(),
                    )
                    .unwrap();
                }
                AutomationExecutorMessage::Request(request) => {
                    if matches!(&request.call, AutomationCall::Invoke { verb, .. } if verb == "slow")
                    {
                        std::thread::sleep(Duration::from_millis(600));
                    }
                    let outcome = if matches!(&request.call, AutomationCall::Invoke { verb, .. } if verb == "fail")
                    {
                        AutomationExecutorOutcome::Error {
                            error: fixture_error(),
                        }
                    } else {
                        AutomationExecutorOutcome::Ok {
                            execution: AutomationExecution {
                                value: json!({"fixture": "child"}),
                                observation: AutomationObservation {
                                    observed: json!({"tree":{"frame":7,"nodes":[]},"state":{"composer":{"text":"fixture"}}}),
                                    tree_diff: json!({"changed":[]}),
                                    state_diff: json!([]),
                                },
                            },
                        }
                    };
                    client
                        .reply_automation_executor(AutomationExecutorReply {
                            request_id: request.request_id,
                            outcome,
                        })
                        .unwrap();
                }
            }
        }
    }

    fn shutdown_marker(database: &Path) -> PathBuf {
        let owned = database.parent().unwrap();
        owned.parent().unwrap().parent().unwrap().join(format!(
            "shutdown-{}",
            owned.file_name().unwrap().to_string_lossy()
        ))
    }

    fn launcher(directory: &Path, role: &str) -> PathBuf {
        let path = directory.join(role);
        let executable = std::env::current_exe().unwrap();
        let escaped = executable.to_string_lossy().replace('\'', "'\\''");
        std::fs::write(&path, format!("#!/bin/sh\nexport WAKU_SCRATCH_TEST_ROLE={role}\nexec '{escaped}' --exact automation_scratch::tests::scratch_process_entry --nocapture\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn isolated_children_inherit_renderer_plumbing_without_credentials() {
        // Set the environment only in a child harness, never globally in this
        // multithreaded test process.
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "automation_scratch::tests::scratch_process_entry",
                "--nocapture",
            ])
            .env("WAKU_SCRATCH_TEST_ROLE", "environment")
            .env("DISPLAY", ":123")
            .env("XAUTHORITY", "/fixture/xauthority")
            .env("LIBGL_ALWAYS_SOFTWARE", "1")
            .env("VK_ICD_FILENAMES", "/fixture/lavapipe.json")
            .env("VK_DRIVER_FILES", "/fixture/lavapipe-modern.json")
            .env("OPENAI_API_KEY", "fixture-not-a-secret")
            .env("CODEX_HOME", "/private/fixture")
            .env(DAEMON_TOKEN_ENV, "fixture-capability")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn stderr_tail_is_bounded_and_redacts_across_chunks_and_retention() {
        let token = "aab-fixture-capability";
        for split in 0..=token.len() {
            let mut tail = StderrTail::new(token);
            tail.append(&vec![b'x'; MAX_STARTUP_STDERR_BYTES * 4]);
            tail.append(&token.as_bytes()[..split]);
            tail.append(&token.as_bytes()[split..]);
            tail.append(b"\nrenderer failed\n");
            tail.finish();
            assert_eq!(tail.bytes.len(), MAX_STARTUP_STDERR_BYTES);
            assert!(tail.text().ends_with("[redacted]\nrenderer failed"));
            assert!(!tail.text().contains(token));
        }
        let mut tail = StderrTail::new(token);
        tail.append(b"prefix aaab-fixture-capability ");
        tail.append(&token.as_bytes()[..9]);
        tail.finish();
        assert_eq!(tail.text(), "prefix a[redacted] [redacted]");
    }

    #[test]
    fn stderr_reader_stops_promptly_with_an_open_descendant_pipe() {
        use std::os::unix::process::CommandExt as _;
        let directory =
            std::env::temp_dir().join(format!("waku-scratch-reader-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut owned = OwnedScratch {
            directory,
            daemon: None,
            app: Some(
                Command::new("/bin/sh")
                    .args(["-c", "sleep 30 & wait"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .process_group(0)
                    .spawn()
                    .unwrap(),
            ),
            daemon_stderr: None,
            app_stderr: None,
        };
        let reader =
            StartupStderr::capture(owned.app.as_mut().unwrap(), "fixture-capability").unwrap();
        let started = Instant::now();
        drop(reader);
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(owned.app.as_mut().unwrap().try_wait().unwrap().is_none());
        // OwnedScratch's ordinary drop still stops the child and its group.
    }

    #[test]
    fn managed_startup_bounds_a_stalled_handshake_and_observes_cancellation() {
        for cancel_early in [false, true] {
            let directory =
                std::env::temp_dir().join(format!("waku-scratch-stalled-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&directory).unwrap();
            let root = directory.join("rpa-scratch");
            let cancel = Arc::new(AtomicBool::new(false));
            let canceller = cancel_early.then(|| {
                let cancel = cancel.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(100));
                    cancel.store(true, Ordering::Release);
                })
            });
            let started = Instant::now();
            let deadline = started
                + if cancel_early {
                    Duration::from_secs(30)
                } else {
                    Duration::from_millis(300)
                };
            let error = launch_mode(
                &root,
                &launcher(&directory, "stalled-daemon"),
                &launcher(&directory, "app"),
                None,
                &AutomationScratchSeed::default(),
                false,
                Some((&cancel, deadline)),
            )
            .err()
            .expect("stalled startup must stop");
            assert_eq!(
                program_error(error).code,
                if cancel_early {
                    "job_aborted"
                } else {
                    "job_timeout"
                }
            );
            assert!(started.elapsed() < Duration::from_secs(2));
            assert!(std::fs::read_dir(&root).unwrap().next().is_none());
            if let Some(canceller) = canceller {
                canceller.join().unwrap();
            }
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn failed_launch_reports_child_status_and_bounded_stderr_then_cleans_up() {
        for (daemon, app, failed, not_started) in [
            ("failed-daemon", "app", "daemon", true),
            ("daemon", "failed-app", "app", false),
        ] {
            let directory =
                std::env::temp_dir().join(format!("waku-scratch-failure-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&directory).unwrap();
            let root = directory.join("rpa-scratch");
            let error = launch(
                &root,
                &launcher(&directory, daemon),
                &launcher(&directory, app),
                None,
                &AutomationScratchSeed::default(),
            )
            .err()
            .expect("fixture child must fail")
            .to_string();
            assert!(
                error.contains(&format!("{failed}: exited with exit status: 27")),
                "{error}"
            );
            assert!(error.contains(&format!("{failed} stderr tail:")), "{error}");
            assert!(
                error.contains("fixture renderer unavailable; capability=[redacted]"),
                "{error}"
            );
            assert!(
                error.len() < MAX_STARTUP_STDERR_BYTES + 512,
                "failure must stay bounded"
            );
            assert_eq!(error.contains("app: not started"), not_started);
            assert!(
                std::fs::read_dir(&root).unwrap().next().is_none(),
                "failed scratch directory leaked"
            );
            let mut count = 0;
            for file in std::fs::read_dir(&directory).unwrap() {
                let path = file.unwrap().path();
                if path.extension().is_some_and(|extension| extension == "pid") {
                    let pid: i32 = std::fs::read_to_string(&path).unwrap().parse().unwrap();
                    assert_eq!(
                        unsafe { libc::kill(pid, 0) },
                        -1,
                        "failed launch child leaked"
                    );
                    count += 1;
                }
            }
            assert_eq!(count, if not_started { 1 } else { 2 });
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    struct Fixture {
        directory: PathBuf,
        manager: ScratchManager,
        instance: Arc<Instance>,
        outgoing: Sender<ServerMessage>,
        incoming: Receiver<ServerMessage>,
    }
    impl Fixture {
        fn new() -> Self {
            Self::with_shutdown(true)
        }
        fn with_shutdown(allow_shutdown: bool) -> Self {
            let directory =
                std::env::temp_dir().join(format!("waku-scratch-test-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&directory).unwrap();
            let launcher = |role| launcher(&directory, role);
            let manager = ScratchManager::new(&directory);
            let instance = Arc::new(
                launch(
                    &manager.inner.root,
                    &launcher(if allow_shutdown {
                        "daemon"
                    } else {
                        "stubborn-daemon"
                    }),
                    &launcher("app"),
                    Some("../untrusted/name"),
                    &AutomationScratchSeed::default(),
                )
                .unwrap(),
            );
            manager
                .inner
                .state
                .lock()
                .instances
                .insert(instance.identity.instance_id, instance.clone());
            let (outgoing, incoming) = bounded(1024);
            Self {
                directory,
                manager,
                instance,
                outgoing,
                incoming,
            }
        }
        fn command(&self, command: AutomationCommand, subscriber: u64) -> AutomationResult {
            self.manager
                .forward(&command, self.outgoing.clone(), subscriber)
                .unwrap()
                .unwrap()
        }
        fn attach(&self, subscriber: u64) -> AutomationSession {
            match self.command(
                AutomationCommand::Attach {
                    instance_id: self.instance.identity.instance_id,
                },
                subscriber,
            ) {
                AutomationResult::Attached { session, .. } => session,
                other => panic!("{other:?}"),
            }
        }
        fn await_interest(&self, expected: bool) {
            let path = PathBuf::from(&self.instance.identity.database).with_file_name("interest");
            until(|| {
                std::fs::read_to_string(&path).ok().as_deref()
                    == Some(if expected { "true" } else { "false" })
            });
        }
        fn await_result(&self, session: Uuid, run: Uuid) -> AutomationRun {
            until(|| cached_run(&self.manager.inner, session, run).is_some());
            cached_run(&self.manager.inner, session, run).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.manager.shutdown();
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }
    fn until(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while !predicate() {
            assert!(Instant::now() < deadline, "condition did not settle");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn scenario(verb: &str) -> AutomationScenario {
        AutomationScenario {
            name: "scratch-run".into(),
            steps: vec![AutomationStep {
                name: "fixture-step".into(),
                operation: AutomationStepOperation::Call {
                    call: AutomationCall::Invoke {
                        verb: verb.into(),
                        args: json!({}),
                    },
                },
            }],
        }
    }

    #[test]
    fn seed_and_environment_cannot_mutate_working_tasks_drafts_or_settings() {
        use crate::model::{AgentSession, Project, ProviderKind};
        let directory = std::env::temp_dir().join(format!("waku-scratch-seed-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let original = directory.join("original");
        let scratch = directory.join("scratch");
        std::fs::create_dir_all(&original).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();
        let mut original_state = PersistedState::fresh(original.join("workspace"));
        original_state.sessions[0]
            .messages
            .push(crate::model::Message::new(
                crate::model::MessageRole::User,
                "working task stays unchanged",
            ));
        StateStore::new(original.join("app.db"))
            .save(&mut original_state)
            .unwrap();
        for (file, data) in [
            ("composer-drafts.json", "private working draft"),
            ("settings.json", "private working settings"),
        ] {
            std::fs::write(original.join(file), data).unwrap();
        }
        let snapshot: HashMap<_, _> = std::fs::read_dir(&original)
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                path.is_file().then(|| {
                    let bytes = std::fs::read(&path).unwrap();
                    (path, bytes)
                })
            })
            .collect();
        let project = Project::from_path(original.join("must-not-touch"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);
        session.title = "seeded fixture".into();

        seed_database(
            &scratch,
            &AutomationScratchSeed {
                projects: vec![project.clone()],
                tasks: vec![],
                sessions: vec![session.clone()],
            },
        )
        .unwrap();
        let store = StateStore::new(scratch.join("app.db"));
        let mut state = store.load().unwrap();
        assert_eq!(state.sessions[0].id, session.id);
        assert_eq!(state.sessions[0].title, "seeded fixture");
        let mut ordinary = PersistedState::empty();
        ordinary.projects = state.projects.clone();
        ordinary.push_session(session.clone());
        let ordinary_store = StateStore::new(directory.join("ordinary/app.db"));
        ordinary_store.save(&mut ordinary).unwrap();
        assert!(
            ordinary_store.load().unwrap().sessions.is_empty(),
            "ordinary save must still omit never-started drafts"
        );
        assert!(state.projects[0].path.starts_with(&scratch));
        assert!(!original.join("must-not-touch").exists());
        state.session_mut(session.id).unwrap().title = "scratch edit".into();
        store.save(&mut state).unwrap();
        store.hydrate(&mut state.sessions[0]).unwrap();
        assert!(
            state.sessions[0].messages.is_empty(),
            "seeding must not invent transcript content"
        );
        state.mark_session_dirty(session.id);
        store.save(&mut state).unwrap();
        assert!(
            store.load().unwrap().sessions.is_empty(),
            "hydrated empty seed follows the ordinary draft lifecycle"
        );
        for (path, before) in snapshot {
            assert_eq!(std::fs::read(path).unwrap(), before);
        }
        let command = isolated_command(Path::new("/bin/true"), &scratch);
        let vars: HashMap<_, _> = command
            .get_envs()
            .filter_map(|(key, value)| {
                value.map(|value| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect();
        assert_eq!(vars["HOME"], scratch.join("home").display().to_string());
        assert_eq!(vars["WAKU_RPA_DATA_DIR"], scratch.display().to_string());
        assert!(!vars.contains_key("OPENAI_API_KEY"));
        assert!(!vars.contains_key("CODEX_HOME"));
        assert!(!vars.contains_key(DAEMON_TOKEN_ENV));
        drop(store);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn scratch_issue_bindings_are_local_and_issue_drafts_survive_ordinary_save() {
        use crate::model::{AgentSession, Project, ProviderKind, Task, TaskState};
        let directory =
            std::env::temp_dir().join(format!("waku-scratch-issues-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let project = Project::from_path(directory.join("foreign-checkout"));
        let mut task = Task::new("Seeded issue", TaskState::Doing, 0);
        task.workspace = Some(TaskWorkspace::Worktree {
            project_id: project.id,
            path: directory.join("foreign-worktree"),
            branch: "feature/never-use".into(),
        });
        let mut bound = AgentSession::new(project.id, ProviderKind::Pi);
        bound.task_id = Some(task.id);
        let mut orphan = AgentSession::new(project.id, ProviderKind::Pi);
        orphan.task_id = Some(Uuid::new_v4());
        seed_database(
            &directory,
            &AutomationScratchSeed {
                projects: vec![project.clone()],
                tasks: vec![task.clone()],
                sessions: vec![bound.clone(), orphan.clone()],
            },
        )
        .unwrap();
        let store = StateStore::new(directory.join("app.db"));
        let mut state = store.load().unwrap();
        assert_eq!(state.tasks.len(), 1);
        assert_eq!(
            state.tasks[0].workspace,
            Some(TaskWorkspace::Local {
                project_id: project.id
            })
        );
        assert_eq!(
            state
                .sessions
                .iter()
                .find(|session| session.id == bound.id)
                .unwrap()
                .task_id,
            Some(task.id)
        );
        assert_eq!(
            state
                .sessions
                .iter()
                .find(|session| session.id == orphan.id)
                .unwrap()
                .task_id,
            None
        );
        for session in &mut state.sessions {
            store.hydrate(session).unwrap();
        }
        state.mark_session_dirty(bound.id);
        state.mark_session_dirty(orphan.id);
        store.save(&mut state).unwrap();
        let saved = store.load().unwrap();
        assert_eq!(
            saved.sessions.len(),
            1,
            "ordinary save retains the issue draft only"
        );
        assert_eq!(saved.sessions[0].id, bound.id);
        assert!(!directory.join("foreign-checkout").exists());
        assert!(!directory.join("foreign-worktree").exists());
        drop(store);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn scratch_issue_seed_rejects_cross_project_and_duplicate_bindings() {
        use crate::model::{AgentSession, Project, ProviderKind, Task, TaskState};
        let directory =
            std::env::temp_dir().join(format!("waku-scratch-invalid-issues-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let first = Project::from_path(directory.join("first-foreign"));
        let second = Project::from_path(directory.join("second-foreign"));
        let mut task = Task::new("Issue", TaskState::Backlog, 0);
        task.workspace = Some(TaskWorkspace::NewWorktree {
            project_id: first.id,
        });
        let mut session = AgentSession::new(second.id, ProviderKind::Pi);
        session.task_id = Some(task.id);
        let mut seed = AutomationScratchSeed {
            projects: vec![first.clone(), second],
            tasks: vec![task.clone()],
            sessions: vec![session],
        };
        assert!(
            seed_database(&directory, &seed)
                .unwrap_err()
                .to_string()
                .contains("different projects")
        );
        seed.sessions.clear();
        seed.tasks.push(task.clone());
        assert!(
            seed_database(&directory, &seed)
                .unwrap_err()
                .to_string()
                .contains("duplicate task")
        );
        seed.tasks.pop();
        seed.projects.clear();
        assert!(
            seed_database(&directory, &seed)
                .unwrap_err()
                .to_string()
                .contains("missing project")
        );
        assert!(!directory.join("app.db").exists());
        assert!(!first.path.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn owned_children_are_reaped_and_only_owned_directory_is_removed() {
        let fixture = Fixture::new();
        let directory = PathBuf::from(&fixture.instance.identity.database)
            .parent()
            .unwrap()
            .to_path_buf();
        let (app, daemon) = {
            let owned = fixture.instance.owned.lock();
            let owned = owned.as_ref().unwrap();
            (
                owned.app.as_ref().unwrap().id(),
                owned.daemon.as_ref().unwrap().id(),
            )
        };
        std::fs::write(fixture.directory.join("keep"), "working instance").unwrap();
        for _ in 0..2 {
            assert!(matches!(
                fixture
                    .manager
                    .handle(&AutomationCommand::ScratchRemove {
                        instance_id: fixture.instance.identity.instance_id
                    })
                    .unwrap(),
                AutomationResult::ScratchRemoved { .. }
            ));
        }
        assert!(!directory.exists());
        assert!(fixture.directory.join("keep").exists());
        assert!(
            shutdown_marker(Path::new(&fixture.instance.identity.database)).exists(),
            "dedicated child control must request backend shutdown"
        );
        for pid in [app, daemon] {
            assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        }
    }

    #[test]
    fn concurrent_removal_joins_automatic_teardown_before_acknowledging() {
        let fixture = Fixture::new();
        let instance_id = fixture.instance.identity.instance_id;
        let session = fixture.attach(1);
        let run = AutomationRun {
            run_id: Uuid::new_v4(),
            name: "concurrent-teardown".into(),
            status: AutomationStatus::Failed,
            steps: vec![],
            cursor: session.cursor,
            error: Some(fixture_error()),
        };
        *fixture.instance.run.lock() = Some(RunOwner {
            session: session.clone(),
            run_id: Some(run.run_id),
        });
        std::thread::scope(|scope| {
            // Force the exact race: the final result and registry removal are
            // visible, while actual process/directory teardown cannot proceed.
            let owned = fixture.instance.owned.lock();
            let processes = owned.as_ref().unwrap();
            let pids = (
                processes.app.as_ref().unwrap().id(),
                processes.daemon.as_ref().unwrap().id(),
            );
            let finish = scope.spawn(|| {
                finish_run(
                    &fixture.manager.inner,
                    instance_id,
                    session.session_id,
                    run.clone(),
                );
            });
            until(|| cached_run(&fixture.manager.inner, session.session_id, run.run_id).is_some());
            assert!(
                !fixture
                    .manager
                    .inner
                    .state
                    .lock()
                    .instances
                    .contains_key(&instance_id)
            );
            assert!(Path::new(&fixture.instance.identity.database).exists());
            let (started, starting) = bounded(1);
            let (acknowledged, acknowledgment) = bounded(1);
            let manager = &fixture.manager;
            let remove = scope.spawn(move || {
                started.send(()).unwrap();
                let result = manager.handle(&AutomationCommand::ScratchRemove { instance_id });
                acknowledged.send(result).unwrap();
            });
            starting.recv_timeout(Duration::from_secs(2)).unwrap();
            let early_ack = acknowledgment.recv_timeout(Duration::from_millis(100));
            drop(owned);
            assert!(
                matches!(early_ack, Err(crossbeam_channel::RecvTimeoutError::Timeout)),
                "removal acknowledged before owned teardown could finish: {early_ack:?}"
            );
            assert!(matches!(
                acknowledgment
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap(),
                AutomationResult::ScratchRemoved { instance_id: removed } if removed == instance_id
            ));
            // No settling/polling or joining the automatic teardown after the
            // explicit removal acknowledgment and before checking its effects.
            assert!(!Path::new(&fixture.instance.identity.database).exists());
            for pid in [pids.0, pids.1] {
                assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
            }
            finish.join().unwrap();
            remove.join().unwrap();
        });
        assert!(fixture.manager.inner.state.lock().removing.is_empty());
    }

    #[test]
    fn refused_control_shutdown_uses_bounded_owned_process_fallback() {
        let fixture = Fixture::with_shutdown(false);
        let daemon = fixture
            .instance
            .owned
            .lock()
            .as_ref()
            .unwrap()
            .daemon
            .as_ref()
            .unwrap()
            .id();
        let started = Instant::now();
        fixture.manager.shutdown();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(unsafe { libc::kill(daemon as i32, 0) }, -1);
        assert!(!Path::new(&fixture.instance.identity.database).exists());
        assert!(!shutdown_marker(Path::new(&fixture.instance.identity.database)).exists());
    }

    #[test]
    fn private_route_preserves_errors_cursors_and_resume_without_leaking_interest() {
        let fixture = Fixture::new();
        let session = fixture.attach(1);
        fixture.await_interest(true);
        let subscribed = fixture.command(
            AutomationCommand::Subscribe {
                session_id: session.session_id,
                streams: vec![AutomationStream::Ui],
            },
            1,
        );
        assert!(matches!(subscribed, AutomationResult::Subscribed { .. }));
        let called = fixture.command(
            AutomationCommand::Call {
                session_id: session.session_id,
                call: AutomationCall::Describe,
            },
            1,
        );
        let cursor = match called {
            AutomationResult::Called { result } => result.cursor,
            other => panic!("{other:?}"),
        };
        assert_eq!(cursor.epoch, session.cursor.epoch);
        assert!(cursor.sequence > session.cursor.sequence);
        let failure = fixture.command(
            AutomationCommand::Call {
                session_id: session.session_id,
                call: AutomationCall::Invoke {
                    verb: "fail".into(),
                    args: json!({}),
                },
            },
            1,
        );
        match failure {
            AutomationResult::Error { error } => assert_failure(&error),
            other => panic!("{other:?}"),
        }
        fixture.manager.disconnected(1);
        fixture.await_interest(false);
        assert!(fixture.instance.owned.lock().is_some());
        assert!(
            !shutdown_marker(Path::new(&fixture.instance.identity.database)).exists(),
            "session disconnect must not shut down the child daemon"
        );
        let resumed = fixture.command(
            AutomationCommand::Resume {
                session_id: session.session_id,
                after: Some(session.cursor),
            },
            2,
        );
        match resumed {
            AutomationResult::Attached {
                session: resumed,
                replay,
            } => {
                assert_eq!(resumed.session_id, session.session_id);
                assert_eq!(resumed.cursor.epoch, cursor.epoch);
                assert!(!replay.events.is_empty());
            }
            other => panic!("{other:?}"),
        }
        fixture.await_interest(true);
        fixture.manager.disconnected(1); // A late old disconnect cannot close the new lease.
        assert!(matches!(
            fixture.command(
                AutomationCommand::Call {
                    session_id: session.session_id,
                    call: AutomationCall::Describe
                },
                2
            ),
            AutomationResult::Called { .. }
        ));
        fixture.manager.disconnected(2);
        fixture.await_interest(false);
        let expired = fixture.command(
            AutomationCommand::Resume {
                session_id: session.session_id,
                after: Some(AutomationCursor {
                    epoch: Uuid::new_v4(),
                    sequence: 0,
                }),
            },
            6,
        );
        assert!(
            matches!(expired, AutomationResult::Attached { replay, .. } if replay.expiry.is_some())
        );
        fixture.await_interest(true);
        std::thread::sleep(Duration::from_millis(150));
        assert!(matches!(
            fixture.command(
                AutomationCommand::Call {
                    session_id: session.session_id,
                    call: AutomationCall::Describe
                },
                6
            ),
            AutomationResult::Called { .. }
        ));
        fixture.manager.disconnected(6);
        fixture.await_interest(false);
    }

    #[test]
    fn final_failure_survives_automatic_teardown_and_cli_resume() {
        let fixture = Fixture::new();
        let session = fixture.attach(3);
        let started = fixture.command(
            AutomationCommand::ScenarioStart {
                session_id: session.session_id,
                scenario: scenario("fail"),
            },
            3,
        );
        let run = match started {
            AutomationResult::Scenario { run } => run,
            other => panic!("{other:?}"),
        };
        let completed = fixture.await_result(session.session_id, run.run_id);
        assert!(matches!(completed.status, AutomationStatus::Failed));
        assert_eq!(completed.steps.len(), 1);
        assert_failure(completed.error.as_ref().unwrap());
        until(|| fixture.instance.owned.lock().is_none());
        assert!(!Path::new(&fixture.instance.identity.database).exists());
        let resumed = fixture.command(
            AutomationCommand::Resume {
                session_id: session.session_id,
                after: Some(AutomationCursor {
                    epoch: Uuid::new_v4(),
                    sequence: 0,
                }),
            },
            4,
        );
        assert!(
            matches!(resumed, AutomationResult::Attached { session, replay } if !session.instance.executor_connected && replay.expiry.is_some())
        );
        let retained = fixture.command(
            AutomationCommand::ScenarioResult {
                session_id: session.session_id,
                run_id: run.run_id,
            },
            4,
        );
        assert!(
            matches!(retained, AutomationResult::Scenario { run } if matches!(run.status, AutomationStatus::Failed) && run.error.as_ref().unwrap().bundle.is_some())
        );
        assert!(fixture.incoming.try_iter().any(|message| matches!(message, ServerMessage::AutomationRunChanged { run, .. } if terminal(&run))));
    }

    #[test]
    fn disconnected_run_is_collected_without_permanent_session_attachment() {
        let fixture = Fixture::new();
        let session = fixture.attach(5);
        let started = fixture.command(
            AutomationCommand::ScenarioStart {
                session_id: session.session_id,
                scenario: scenario("slow"),
            },
            5,
        );
        let run = match started {
            AutomationResult::Scenario { run } => run,
            other => panic!("{other:?}"),
        };
        let second = fixture.command(
            AutomationCommand::ScenarioStart {
                session_id: session.session_id,
                scenario: scenario("slow"),
            },
            5,
        );
        assert!(
            matches!(second, AutomationResult::Error { error } if error.code == "scratch_run_exists")
        );
        fixture.manager.disconnected(5);
        let completed = fixture.await_result(session.session_id, run.run_id);
        assert!(matches!(completed.status, AutomationStatus::Succeeded));
        until(|| fixture.instance.owned.lock().is_none());
    }

    #[test]
    fn invalid_seed_rejects_unknown_projects_without_creating_foreign_paths() {
        let directory =
            std::env::temp_dir().join(format!("waku-scratch-bad-seed-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let session =
            crate::model::AgentSession::new(Uuid::new_v4(), crate::model::ProviderKind::Codex);
        assert!(
            seed_database(
                &directory,
                &AutomationScratchSeed {
                    projects: vec![],
                    tasks: vec![],
                    sessions: vec![session]
                }
            )
            .is_err()
        );
        assert!(!directory.join("app.db").exists());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
