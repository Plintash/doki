//! The finite UI executor. The daemon owns waits, scenarios, cursors and logs;
//! this queue only dispatches one input or captures one observation at a time.
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

use gpui::{App, AsyncApp, Global};
use serde_json::{Value, json};
use uuid::Uuid;
use waku_client::{
    AutomationAction, AutomationCall, AutomationError, AutomationExecution,
    AutomationExecutorEvent, AutomationExecutorMessage, AutomationExecutorOutcome,
    AutomationExecutorReply, AutomationExecutorRequest, AutomationFailureBundle,
    AutomationInstance, AutomationObservation, AutomationStream, AutomationTarget, DaemonClient,
};

use super::{Action, Error, Request, Target, TreeDiff, Waku};

const UI: u8 = 1;
const MODEL: u8 = 2;
const HEALTH: u8 = 4;
const PAINT_SECTIONS: &[&str] = &[
    "tree", "route", "board", "composer", "panels", "toast", "ui",
];
const LIVE_SECTIONS: &[&str] = &[
    "task",
    "selection",
    "route",
    "board",
    "composer",
    "panels",
    "toast",
    "ui",
];
static ATTACHED: AtomicBool = AtomicBool::new(false);
static INTEREST: AtomicU8 = AtomicU8::new(0);
static CAPTURING: AtomicBool = AtomicBool::new(false);
static DIRTY: AtomicU8 = AtomicU8::new(0);
static QUEUED: AtomicBool = AtomicBool::new(false);
static WORK: OnceLock<smol::channel::Sender<Work>> = OnceLock::new();

struct Started;
impl Global for Started {}

enum Work {
    Connected(DaemonClient),
    Disconnected(DaemonClient),
    Message(DaemonClient, AutomationExecutorMessage),
    Observe,
    IdentityWarning(String),
}
enum Outbound {
    Reply(DaemonClient, AutomationExecutorReply),
    Event(DaemonClient, AutomationExecutorEvent, Arc<Value>),
    Warning(DaemonClient, String),
}

pub(super) fn attached() -> bool {
    cfg!(debug_assertions) && ATTACHED.load(Ordering::Relaxed)
}

/// Render and model observers only enqueue an edge. Capturing happens after
/// their entity leases have returned, and our own draw cannot rearm the queue.
pub(crate) fn identity_warning(message: String) {
    if cfg!(debug_assertions)
        && let Some(sender) = WORK.get()
    {
        let _ = sender.try_send(Work::IdentityWarning(message));
    }
}

/// Call after a model mutation is committed: persistence, event-pump commits,
/// catalog/detail hydration and optimistic unsaved session edits. A generic
/// root notification is not a model change; animation and pulse frames use
/// `painted` and must never serialize stored history or scan runtime health.
pub(crate) fn model_changed() {
    enqueue_observation(UI | MODEL | HEALTH);
}

/// A paint (including focus, scroll and child-only repaints) does not imply
/// that every stored transcript changed. Never serialize the full model here.
pub(crate) fn painted() {
    enqueue_observation(UI);
}

fn enqueue_observation(changed: u8) {
    let requested = interested_changes(changed, INTEREST.load(Ordering::Relaxed));
    if !cfg!(debug_assertions) || requested == 0 || CAPTURING.load(Ordering::Relaxed) {
        return;
    }
    DIRTY.fetch_or(requested, Ordering::Relaxed);
    schedule_dirty();
}

/// Keep model-driven health fresh even when only UI events are subscribed.
/// A paint carries UI alone and can never request full model or health work.
fn interested_changes(changed: u8, interest: u8) -> u8 {
    let health = if interest & (UI | MODEL) != 0 {
        HEALTH
    } else {
        0
    };
    changed & (interest | health)
}

fn schedule_dirty() {
    if DIRTY.load(Ordering::Relaxed) == 0 || QUEUED.swap(true, Ordering::Relaxed) {
        return;
    }
    if WORK
        .get()
        .is_none_or(|sender| sender.try_send(Work::Observe).is_err())
    {
        QUEUED.store(false, Ordering::Relaxed);
    }
}

struct CaptureGuard;
impl CaptureGuard {
    fn new() -> Self {
        CAPTURING.store(true, Ordering::Relaxed);
        Self
    }
}
impl Drop for CaptureGuard {
    fn drop(&mut self) {
        CAPTURING.store(false, Ordering::Relaxed);
    }
}

/// Installed before the first workspace is constructed, independently of the
/// optional legacy listener. The observer lives at app scope, not window scope.
pub(crate) fn init(cx: &mut App) {
    if !cfg!(debug_assertions) || cx.try_global::<Started>().is_some() {
        return;
    }
    cx.set_global(Started);
    // P0 counter reporting must survive retirement of the legacy listener.
    // This samples memory only; it neither draws nor polls the UI/runtime.
    cx.spawn(async |cx: &mut AsyncApp| {
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(1))
                .await;
            cx.update(|app| crate::diagnostics::publish_counters(app));
        }
    })
    .detach();
    let (sender, receiver) = smol::channel::bounded(256);
    let _ = WORK.set(sender);
    cx.observe_new::<Waku>(|_, _, cx| {
        let entity = cx.entity();
        cx.observe(&entity, |_, _, _| painted()).detach();
        // Registration/attachment can precede the first workspace, or a new
        // window can appear during an existing session. MainWindow has not
        // necessarily installed this entity yet, and its lease may still be
        // on the stack. Defer until the effect cycle returns; no platform
        // frame callback is required for an occluded starting window.
        cx.defer(|app| {
            super::sync_automation(app);
            model_changed();
        });
    })
    .detach();
    let (outgoing, output) = smol::channel::bounded(8);
    std::thread::Builder::new()
        .name("waku-rpa-replies".into())
        .spawn(move || {
            while let Ok(message) = output.recv_blocking() {
                let result = match message {
                    Outbound::Reply(client, reply) => client.reply_automation_executor(reply),
                    Outbound::Event(client, mut event, health) => {
                        // Health can cover every task/runtime. Its clone and
                        // final wire-size check belong off the UI executor.
                        event.observation = observation_with_health(event.observation, &health);
                        client.publish_automation_event(event)
                    }
                    Outbound::Warning(client, message) => {
                        eprintln!("waku-rpa identity: {message}");
                        client.record_diagnostic(waku_client::DiagnosticRecord::new(
                            waku_client::DiagnosticLevel::Warn,
                            waku_client::DiagnosticOrigin::App {
                                pid: std::process::id(),
                                version: env!("CARGO_PKG_VERSION").into(),
                            },
                            "rpa.identity",
                            message,
                        ))
                    }
                };
                if let Err(error) = result {
                    eprintln!("waku-rpa: could not publish executor response: {error}");
                }
            }
        })
        .expect("start RPA reply worker");
    cx.spawn(async move |cx: &mut AsyncApp| {
        let mut client: Option<DaemonClient> = None;
        let mut warned = HashSet::new();
        let mut sessions = HashMap::<Uuid, (Value, usize)>::new();
        let mut previous_ui = Value::Null;
        let mut previous_model = HashMap::new();
        let mut model_sections = vec!["state".into()];
        let mut health = Arc::new(Value::Null);
        while let Ok(work) = receiver.recv().await {
            match work {
                Work::IdentityWarning(message) => {
                    if let Some(client) = &client {
                        if warned.len() < 512 && warned.insert(message.clone()) {
                            let _ = outgoing
                                .send(Outbound::Warning(client.clone(), message))
                                .await;
                        }
                    }
                }
                Work::Disconnected(source) => {
                    if client
                        .as_ref()
                        .is_some_and(|current| current.same_connection(&source))
                    {
                        client = None;
                        sessions.clear();
                        previous_ui = Value::Null;
                        previous_model.clear();
                        health = Arc::new(Value::Null);
                        ATTACHED.store(false, Ordering::Relaxed);
                        INTEREST.store(0, Ordering::Relaxed);
                        DIRTY.store(0, Ordering::Relaxed);
                        cx.update(super::sync_automation);
                    }
                }
                Work::Connected(current) => {
                    client = Some(current);
                    sessions.clear();
                    previous_ui = Value::Null;
                    previous_model.clear();
                    health = Arc::new(Value::Null);
                    ATTACHED.store(false, Ordering::Relaxed);
                    INTEREST.store(0, Ordering::Relaxed);
                    cx.update(super::sync_automation);
                }
                Work::Message(
                    source,
                    AutomationExecutorMessage::Interest {
                        instance_id,
                        attached,
                        streams,
                        model_sections: requested_sections,
                    },
                ) => {
                    if instance_id != crate::rpa::service::generation()
                        || !current_source(&client, &source)
                    {
                        continue;
                    }
                    ATTACHED.store(attached, Ordering::Relaxed);
                    INTEREST.store(interest_bits(attached, &streams), Ordering::Relaxed);
                    let requested_sections = if requested_sections.is_empty() {
                        vec!["state".into()]
                    } else {
                        requested_sections
                    };
                    if model_sections != requested_sections
                        || !streams.contains(&AutomationStream::Model)
                    {
                        previous_model.clear();
                    }
                    model_sections = requested_sections;
                    if !attached {
                        sessions.clear();
                        previous_ui = Value::Null;
                        previous_model.clear();
                        health = Arc::new(Value::Null);
                    }
                    cx.update(super::sync_automation);
                    model_changed();
                }
                Work::Message(source, AutomationExecutorMessage::Request(request)) => {
                    if !current_source(&client, &source) {
                        continue;
                    }
                    let _guard = CaptureGuard::new();
                    let previous = sessions
                        .get(&request.session_id)
                        .map(|(value, _)| value.clone())
                        .unwrap_or(Value::Null);
                    let outcome = bounded_outcome(execute(cx, &request, &previous).await);
                    let observed = match &outcome {
                        AutomationExecutorOutcome::Ok { execution } => {
                            Some(&execution.observation.observed)
                        }
                        AutomationExecutorOutcome::Error { error } => {
                            error.bundle.as_ref().map(|bundle| &bundle.observed)
                        }
                    };
                    if let Some(observed) = observed {
                        cache_baseline(&mut sessions, request.session_id, observed);
                    }
                    drop(_guard);
                    let _ = outgoing
                        .send(Outbound::Reply(
                            source,
                            AutomationExecutorReply {
                                request_id: request.request_id,
                                outcome,
                            },
                        ))
                        .await;
                    // Mutations have already drawn their effect. Reuse a live
                    // observation for subscriptions, outside any entity lease.
                    model_changed();
                }
                Work::Observe => {
                    QUEUED.store(false, Ordering::Relaxed);
                    let interest = interested_changes(
                        DIRTY.swap(0, Ordering::Relaxed),
                        INTEREST.load(Ordering::Relaxed),
                    );
                    let Some(client) = &client else {
                        continue;
                    };
                    if interest == 0 {
                        continue;
                    }
                    let _guard = CaptureGuard::new();
                    refresh_subscription_health(interest, &mut health, || subscription_health(cx));
                    let mut events = Vec::new();
                    if interest & UI != 0 {
                        let observed = subscription_snapshot(cx, &paint_sections());
                        let observation = bounded_observation(observation(&previous_ui, observed));
                        previous_ui = observation.observed.clone();
                        events.push(Outbound::Event(
                            client.clone(),
                            AutomationExecutorEvent {
                                instance_id: crate::rpa::service::generation(),
                                stream: AutomationStream::Ui,
                                observation,
                            },
                            health.clone(),
                        ));
                    }
                    for observation in capture_model_observations(
                        interest,
                        &model_sections,
                        &mut previous_model,
                        |sections| subscription_snapshot(cx, sections),
                    ) {
                        events.push(Outbound::Event(
                            client.clone(),
                            AutomationExecutorEvent {
                                instance_id: crate::rpa::service::generation(),
                                stream: AutomationStream::Model,
                                observation,
                            },
                            health.clone(),
                        ));
                    }
                    drop(_guard);
                    for event in events {
                        if outgoing.send(event).await.is_err() {
                            return;
                        }
                    }
                }
            }
            // If a signal arrived while the finite queue was full, enqueue
            // its retained edge as soon as one work item has freed capacity.
            schedule_dirty();
        }
    })
    .detach();
}

fn current_source(current: &Option<DaemonClient>, source: &DaemonClient) -> bool {
    current
        .as_ref()
        .is_some_and(|current| current.same_connection(source))
        && !source.is_disconnected()
}

/// Count encoded bytes without allocating a second full serialized snapshot.
fn bounded_size(value: &impl serde::Serialize, limit: usize) -> Option<usize> {
    struct Counter {
        bytes: usize,
        limit: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes = self.bytes.saturating_add(bytes.len());
            if self.bytes > self.limit {
                return Err(std::io::Error::other("observation size limit"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { bytes: 0, limit };
    serde_json::to_writer(&mut counter, value)
        .ok()
        .map(|_| counter.bytes)
}

fn size_error() -> AutomationError {
    AutomationError::new(
        "observation_too_large",
        "executor observation exceeds 4 MiB; select fewer snapshot sections or use a smaller scratch fixture",
    )
}

fn bounded_observation(observation: AutomationObservation) -> AutomationObservation {
    if bounded_size(&observation, waku_client::MAX_AUTOMATION_OBSERVATION_BYTES).is_some() {
        return observation;
    }
    let mut observed = json!({ "observationError": size_error(), "state": null, "tree": null });
    // The background health attachment can also cross the wire-size bound.
    // Preserve which capture failed so another client's wait stays independent.
    if let Some(section) = observation
        .observed
        .get("observationSection")
        .and_then(Value::as_str)
    {
        observed["observationSection"] = json!(section);
    }
    AutomationObservation {
        observed,
        ..Default::default()
    }
}

/// Called only by the background reply worker; foreground baselines contain
/// state/tree, never the potentially collection-sized health summary.
fn observation_with_health(
    mut observation: AutomationObservation,
    health: &Value,
) -> AutomationObservation {
    observation.observed["health"] = health.clone();
    bounded_observation(observation)
}

fn bounded_outcome(outcome: AutomationExecutorOutcome) -> AutomationExecutorOutcome {
    match outcome {
        AutomationExecutorOutcome::Ok { execution }
            if bounded_size(
                &execution.observation,
                waku_client::MAX_AUTOMATION_OBSERVATION_BYTES,
            )
            .is_none() =>
        {
            AutomationExecutorOutcome::Error {
                error: size_error(),
            }
        }
        AutomationExecutorOutcome::Error { mut error } => {
            if error.bundle.as_ref().is_some_and(|bundle| {
                bounded_size(bundle, waku_client::MAX_AUTOMATION_OBSERVATION_BYTES).is_none()
            }) {
                error.bundle = None;
                error
                    .message
                    .push_str("; evidence exceeds 4 MiB, select fewer snapshot sections");
            }
            AutomationExecutorOutcome::Error { error }
        }
        outcome => outcome,
    }
}

fn cache_baseline(sessions: &mut HashMap<Uuid, (Value, usize)>, session: Uuid, observed: &Value) {
    const CACHE_BYTES: usize = 16 * 1024 * 1024;
    let Some(bytes) = bounded_size(observed, waku_client::MAX_AUTOMATION_OBSERVATION_BYTES) else {
        return;
    };
    sessions.remove(&session);
    while sessions.len() >= waku_client::MAX_AUTOMATION_SESSIONS
        || sessions.values().map(|(_, bytes)| bytes).sum::<usize>() + bytes > CACHE_BYTES
    {
        let Some(oldest) = sessions.keys().next().copied() else {
            break;
        };
        sessions.remove(&oldest);
    }
    sessions.insert(session, (observed.clone(), bytes));
}

/// Subscribe before registering, then follow every supervisor replacement.
/// All blocking transport and lifecycle work stays on this background worker.
pub(crate) fn connect(supervisor: waku_client::DaemonSupervisor) {
    if !cfg!(debug_assertions) {
        return;
    }
    let Some(sender) = WORK.get().cloned() else {
        return;
    };
    std::thread::Builder::new().name("waku-rpa-executor".into()).spawn(move || {
        let clients = supervisor.subscribe_clients();
        let Ok(mut client) = clients.recv() else { return; };
        loop {
            let incoming = client.subscribe_automation_executor();
            if sender.send_blocking(Work::Connected(client.clone())).is_err() { return; }
            if let Err(error) = client.register_automation_executor(instance()) {
                eprintln!("waku-rpa: could not register executor: {error}");
            }
            loop {
                crossbeam_channel::select! {
                    recv(clients) -> replacement => {
                        let Ok(replacement) = replacement else { return; };
                        client = replacement;
                        break;
                    }
                    recv(incoming) -> message => {
                        match message {
                            Ok(message) => if sender.send_blocking(Work::Message(client.clone(), message)).is_err() { return; },
                            Err(_) => {
                                if sender.send_blocking(Work::Disconnected(client.clone())).is_err() { return; }
                                let Ok(replacement) = clients.recv() else { return; };
                                client = replacement;
                                break;
                            }
                        }
                    }
                }
            }
        }
    }).expect("start RPA connection worker");
}

fn instance() -> AutomationInstance {
    let identity = crate::rpa::service::instance_identity();
    AutomationInstance {
        instance_id: crate::rpa::service::generation(),
        checkout: identity["checkout"].as_str().unwrap().into(),
        database: identity["database"].as_str().unwrap().into(),
        pid: std::process::id(),
        build: env!("WAKU_BUILD_ID").into(),
        version: env!("CARGO_PKG_VERSION").into(),
        executor_connected: true,
        scratch: std::env::var_os("WAKU_RPA_INSTANCE_ID").is_some()
            && std::env::var_os("WAKU_RPA_SCRATCH_NAME").is_some(),
        headless: false,
    }
}

fn interest_bits(attached: bool, streams: &[AutomationStream]) -> u8 {
    if !attached {
        return 0;
    }
    streams.iter().fold(0, |bits, stream| {
        bits | match stream {
            AutomationStream::Ui => UI,
            AutomationStream::Model => MODEL,
            AutomationStream::Diagnostic => 0,
        }
    })
}
fn live_sections(tree: bool) -> Vec<String> {
    LIVE_SECTIONS
        .iter()
        .copied()
        .chain(tree.then_some("tree"))
        .map(str::to_owned)
        .collect()
}
fn paint_sections() -> Vec<String> {
    PAINT_SECTIONS
        .iter()
        .map(|section| (*section).into())
        .collect()
}

fn capture_model_observations(
    changed: u8,
    sections: &[String],
    previous: &mut HashMap<String, Value>,
    mut capture: impl FnMut(&[String]) -> Value,
) -> Vec<AutomationObservation> {
    if changed & MODEL == 0 {
        return Vec::new();
    }
    let default_sections = ["state".into()];
    let sections = if sections.is_empty() {
        &default_sections
    } else {
        sections
    };
    let mut observations = Vec::new();
    for section in sections {
        // A full-state subscriber must not make a small section wait inherit
        // unrelated history or another requested section's size failure.
        let mut observed = capture(std::slice::from_ref(section));
        observed["observationSection"] = json!(section);
        let before = previous.get(section).unwrap_or(&Value::Null);
        let observation = bounded_observation(observation(before, observed));
        let changed = observation.observed["state"] != before["state"]
            || observation.observed["observationError"] != before["observationError"];
        // At most one bounded baseline for each supported selection. Invalid
        // section requests can report errors but never grow this cache.
        if section == "state" || super::STATE_SECTIONS.contains(&section.as_str()) {
            previous.insert(section.clone(), observation.observed.clone());
        }
        if changed {
            observations.push(observation);
        }
    }
    observations
}

fn subscription_snapshot(cx: &mut AsyncApp, sections: &[String]) -> Value {
    let tree = sections.iter().any(|section| section == "tree");
    cx.update(|app| {
        super::with_workspace(app, tree, |waku, window, cx| {
            waku.rpa_snapshot_without_health(window, cx, sections)
        })
        .unwrap_or_else(|| Err(super::app_not_ready()))
        .unwrap_or_else(|error| {
            json!({ "state": null, "tree": null, "observationError": {
                "code": error.code, "message": error.message,
            } })
        })
    })
}

fn subscription_health(cx: &mut AsyncApp) -> Value {
    cx.update(|app| {
        super::with_workspace(app, false, |waku, _window, cx| waku.rpa_health(cx))
            .unwrap_or_else(|| super::unavailable_health(app))
    })
}

fn refresh_subscription_health(
    changed: u8,
    health: &mut Arc<Value>,
    refresh: impl FnOnce() -> Value,
) {
    if changed & HEALTH != 0 {
        *health = Arc::new(refresh());
    }
}

fn snapshot(cx: &mut AsyncApp, sections: &[String]) -> Result<Value, Error> {
    let tree = sections.is_empty() || sections.iter().any(|section| section == "tree");
    cx.update(|app| {
        super::with_workspace(app, tree, |waku, window, cx| {
            waku.rpa_snapshot_sections(window, cx, sections)
        })
        .unwrap_or_else(|| Err(super::app_not_ready()))
    })
}
fn unavailable(cx: &mut AsyncApp) -> Value {
    cx.update(
        |app| json!({ "state": null, "tree": null, "health": super::unavailable_health(app) }),
    )
}

/// Persisted fallback runs off the UI executor and never calls hydration or
/// selection handlers. Live reads observe the app's current, possibly unsaved
/// state; separate calls do not promise an immutable streaming snapshot.
async fn transcript(
    cx: &mut AsyncApp,
    conversation_id: Uuid,
    query: &waku_client::TranscriptQuery,
) -> Result<Value, Error> {
    query
        .validate()
        .map_err(|error| Error::new(error.code, error.message))?;
    enum Read {
        Live(Result<waku_client::TranscriptResult, waku_client::TranscriptError>),
        Persisted(waku_client::DaemonSupervisor),
    }
    let read = cx.update(|app| {
        super::with_workspace(app, false, |waku, _window, _cx| {
            let session = waku
                .state
                .sessions
                .iter()
                .find(|session| session.id == conversation_id)
                .ok_or_else(|| {
                    Error::new("not_found", format!("no conversation `{conversation_id}`"))
                        .with_target(conversation_id.to_string())
                })?;
            Ok(if session.detail_loaded {
                Read::Live(waku_client::transcript::read_transcript(session, query))
            } else {
                Read::Persisted(waku.daemon.clone())
            })
        })
        .unwrap_or_else(|| Err(super::app_not_ready()))
    })?;
    match read {
        Read::Live(result) => result
            .map(|result| super::json_of(&result))
            .map_err(|error| Error::new(error.code, error.message)),
        Read::Persisted(daemon) => {
            let query = query.clone();
            cx.background_executor()
                .spawn(async move {
                    waku_client::persistence::read_transcript(&daemon, conversation_id, query)
                })
                .await
                .map(|result| super::json_of(&result))
                .map_err(|error| {
                    if let Some(domain) = error
                        .get_ref()
                        .and_then(|error| error.downcast_ref::<waku_client::TranscriptError>())
                    {
                        Error::new(domain.code.clone(), domain.message.clone())
                    } else {
                        Error::new("transcript_read_failed", error.to_string())
                    }
                })
        }
    }
}

async fn execute(
    cx: &mut AsyncApp,
    request: &AutomationExecutorRequest,
    previous: &Value,
) -> AutomationExecutorOutcome {
    let is_input = !matches!(
        request.call,
        AutomationCall::Describe
            | AutomationCall::Snapshot { .. }
            | AutomationCall::Transcript { .. }
    );
    let sections = match &request.call {
        AutomationCall::Snapshot { sections } => sections.clone(),
        AutomationCall::Describe | AutomationCall::Transcript { .. } => live_sections(false),
        _ => live_sections(true),
    };
    let before = if is_input {
        snapshot(cx, &live_sections(true)).unwrap_or_else(|_| unavailable(cx))
    } else {
        previous.clone()
    };
    let result = if request.instance_id != crate::rpa::service::generation() {
        Err(Error::new(
            "executor_gone",
            "the request targets another app generation",
        ))
    } else {
        match &request.call {
            AutomationCall::Describe => Ok(
                json!({ "instance": instance(), "verbs": super::VERBS, "snapshotSections": super::STATE_SECTIONS, "capabilities": super::info::capabilities() }),
            ),
            AutomationCall::Snapshot { .. } => snapshot(cx, &sections),
            AutomationCall::Transcript {
                conversation_id,
                query,
            } => transcript(cx, *conversation_id, query).await,
            AutomationCall::Invoke { verb, args } => {
                let request = Request::Invoke {
                    verb: verb.clone(),
                    args: args.clone(),
                };
                record_action(cx, &request);
                super::query(cx, request)
            }
            AutomationCall::Act { action, target } => dispatch(cx, *action, target),
            AutomationCall::Type { text, target } => {
                let request = Request::Type {
                    text: text.clone(),
                    target: target.as_ref().map(legacy_target),
                };
                record_action(cx, &request);
                super::dispatch(cx, request)
            }
            AutomationCall::Key { chord } => {
                let request = Request::Key {
                    chord: chord.clone(),
                };
                record_action(cx, &request);
                super::dispatch(cx, request)
            }
        }
    };
    // Snapshot itself already captured the requested frame. Mutations capture
    // after input returns, never during its workspace lease.
    let observed = match (&request.call, &result) {
        (AutomationCall::Snapshot { .. }, Ok(snapshot)) => snapshot.clone(),
        _ => snapshot(cx, &sections).unwrap_or_else(|_| unavailable(cx)),
    };
    let observation = observation(&before, observed);
    match result {
        Ok(value) => AutomationExecutorOutcome::Ok {
            execution: AutomationExecution { value, observation },
        },
        Err(error) => AutomationExecutorOutcome::Error {
            error: AutomationError {
                code: error.code,
                message: error.message,
                target: error.target,
                nearest: error.nearest,
                bundle: Some(AutomationFailureBundle {
                    observed: observation.observed,
                    tree_diff: observation.tree_diff,
                    recent_events: None,
                    counters: waku_client::DiagnosticCounters {
                        observed_at_ms: crate::model::unix_time_millis(),
                        processes: vec![cx.update(|app| crate::diagnostics::counters(app))],
                    },
                    diagnostic_errors: Vec::new(),
                }),
            },
        },
    }
}

fn record_action(cx: &mut AsyncApp, request: &Request) {
    let correlation = super::selected_correlation(cx);
    if let Some(record) = crate::diagnostics::action(request, || correlation) {
        cx.update(|app| crate::diagnostics::record(app, || Some(record)));
    }
}
fn legacy_target(target: &AutomationTarget) -> Target {
    match target {
        AutomationTarget::Id(id) => Target::Id(id.clone()),
        AutomationTarget::Node(node) => Target::Node(node.clone()),
    }
}
fn dispatch(
    cx: &mut AsyncApp,
    action: AutomationAction,
    target: &AutomationTarget,
) -> Result<Value, Error> {
    let target = legacy_target(target);
    match action {
        AutomationAction::Click | AutomationAction::Focus => {
            let action = match action {
                AutomationAction::Click => Action::Click,
                _ => Action::Focus,
            };
            let request = Request::Act { action, target };
            record_action(cx, &request);
            super::dispatch(cx, request)
        }
        _ => {
            let correlation = super::selected_correlation(cx);
            cx.update(|app| crate::diagnostics::record(app, || Some(waku_client::DiagnosticRecord::new(
                waku_client::DiagnosticLevel::Info,
                waku_client::DiagnosticOrigin::App { pid: std::process::id(), version: env!("CARGO_PKG_VERSION").into() },
                "action", "scroll",
            ).with_fields(json!({ "action": action, "target": super::target_description(&target) })).with_correlation(correlation))));
            cx.update(|app| {
                super::with_window(app, true, |window, app| {
                    let node = super::resolve_target_details(window, &target)?;
                    let bounds = &node["bounds"];
                    let x = bounds["x"].as_f64().unwrap_or_default()
                        + bounds["width"].as_f64().unwrap_or_default() / 2.0;
                    let y = bounds["y"].as_f64().unwrap_or_default()
                        + bounds["height"].as_f64().unwrap_or_default() / 2.0;
                    let (dx, dy) = match action {
                        AutomationAction::ScrollUp => (0.0, 3.0),
                        AutomationAction::ScrollDown => (0.0, -3.0),
                        AutomationAction::ScrollLeft => (3.0, 0.0),
                        AutomationAction::ScrollRight => (-3.0, 0.0),
                        _ => unreachable!(),
                    };
                    window.dispatch_event(
                        gpui::PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                            position: gpui::point(gpui::px(x as f32), gpui::px(y as f32)),
                            delta: gpui::ScrollDelta::Lines(gpui::point(dx, dy)),
                            ..Default::default()
                        }),
                        app,
                    );
                    Ok(json!({ "scrolled": action, "node": node["node"] }))
                })
                .unwrap_or_else(|| Err(super::app_not_ready()))
            })
        }
    }
}

fn observation(before: &Value, observed: Value) -> AutomationObservation {
    let baseline_state = match (before["state"].as_object(), observed["state"].as_object()) {
        (Some(before), Some(after)) => Value::Object(
            before
                .iter()
                .filter(|(key, _)| after.contains_key(*key))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        ),
        _ => before["state"].clone(),
    };
    let state_diff = state_diff(&baseline_state, &observed["state"]);
    let tree_diff = super::json_of(&TreeDiff::between(&before["tree"], &observed["tree"]));
    AutomationObservation {
        observed,
        tree_diff,
        state_diff,
    }
}

/// JSON pointer changes, including deletions. Arrays are model-owned ordered
/// collections and travel as one changed value rather than positional guesses.
fn state_diff(before: &Value, after: &Value) -> Value {
    fn visit(before: &Value, after: &Value, pointer: &str, changes: &mut Vec<Value>) {
        if before == after {
            return;
        }
        if let (Some(before), Some(after)) = (before.as_object(), after.as_object()) {
            for (key, value) in after {
                let pointer = format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"));
                visit(
                    before.get(key).unwrap_or(&Value::Null),
                    value,
                    &pointer,
                    changes,
                );
            }
            for key in before.keys().filter(|key| !after.contains_key(*key)) {
                changes.push(json!({ "pointer": format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1")), "removed": true }));
            }
        } else {
            changes.push(json!({ "pointer": pointer, "value": after }));
        }
    }
    let mut changes = Vec::new();
    visit(before, after, "", &mut changes);
    json!(changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_model_changes_refresh_subscription_health_and_full_state() {
        assert_eq!(interest_bits(false, &[AutomationStream::Ui]), 0);
        assert_eq!(interest_bits(true, &[AutomationStream::Diagnostic]), 0);
        assert_eq!(
            interest_bits(true, &[AutomationStream::Ui, AutomationStream::Model]),
            UI | MODEL
        );
        assert_eq!(interested_changes(UI, UI | MODEL), UI);
        assert_eq!(interested_changes(UI, MODEL), 0);
        assert_eq!(interested_changes(UI | MODEL | HEALTH, 0), 0);
        assert_eq!(interested_changes(UI | MODEL | HEALTH, UI), UI | HEALTH);
        assert_eq!(
            interested_changes(UI | MODEL | HEALTH, UI | MODEL),
            UI | MODEL | HEALTH
        );

        let mut health = Arc::new(json!({"sessions":{"known":4}}));
        let queued_health = health.clone();
        refresh_subscription_health(interested_changes(UI, UI | MODEL), &mut health, || {
            panic!("a paint must not scan runtime/task health")
        });
        assert!(Arc::ptr_eq(&health, &queued_health));
        // Initial interest uses the same changed edge. UI-only subscribers
        // also refresh health, without subscribing to complete model state.
        refresh_subscription_health(
            interested_changes(UI | MODEL | HEALTH, UI),
            &mut health,
            || json!({"sessions":{"known":5}}),
        );
        assert_eq!(health["sessions"]["known"], 5);
        assert_eq!(queued_health["sessions"]["known"], 4);
    }

    #[test]
    fn paint_skips_transcript_serialization_but_model_changes_remain_observable() {
        use crate::model::{AgentSession, Message, MessageRole, ProviderKind};
        use std::cell::Cell;

        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Pi);
        session.messages.push(Message::new(
            MessageRole::Assistant,
            "long transcript ".repeat(32_768),
        ));
        session
            .subagent_runs
            .push(waku_client::subagent::SubagentRun {
                id: "child".into(),
                state: waku_client::subagent::SubagentState::Completed,
                ..Default::default()
            });
        let serialized_sessions = Cell::new(0);
        let capture = |sections: &[String], session: &AgentSession| {
            // This is the same lazy section dispatcher used by Waku. Any
            // session/model accidentally requested by paint reaches the real
            // session serializer, including messages and runtime-only fields.
            super::super::serialize_state_sections(sections, |section| match section {
                "model" | "session" => {
                    serialized_sessions.set(serialized_sessions.get() + 1);
                    let session = super::super::observable_session(session);
                    if section == "model" {
                        json!({"sessions":[session]})
                    } else {
                        session
                    }
                }
                _ => json!({"section":section}),
            })
        };

        let state = capture(&paint_sections(), &session);
        assert_eq!(serialized_sessions.get(), 0);
        assert!(state.get("session").is_none());
        assert!(state.get("model").is_none());
        assert_eq!(state.as_object().unwrap().len(), 6);
        for name in ["route", "board", "composer", "panels", "toast", "ui"] {
            assert!(state.get(name).is_some(), "{name}");
        }
        assert!(paint_sections().iter().any(|section| section == "tree"));
        let paint = bounded_observation(observation(
            &Value::Null,
            json!({"state":state,"tree":{"nodes":[]}}),
        ));
        assert!(bounded_size(&paint, 1024).is_some());
        assert!(paint.observed.get("health").is_none());

        let before = capture(&["state".into()], &session);
        assert_eq!(serialized_sessions.get(), 2);
        session.messages[0].content.push_str("model update");
        session.subagent_runs[0].id = "updated-child".into();
        let after = capture(&["state".into()], &session);
        assert_eq!(serialized_sessions.get(), 4);
        assert!(
            after["session"]["messages"][0]["content"]
                .as_str()
                .unwrap()
                .ends_with("model update")
        );
        assert_eq!(after["session"]["subagent_runs"][0]["id"], "updated-child");
        assert_eq!(after["model"]["sessions"][0], after["session"]);
        let changes = observation(&json!({"state":before}), json!({"state":after}));
        assert!(
            changes
                .state_diff
                .as_array()
                .unwrap()
                .iter()
                .any(|change| { change["pointer"] == "/session/messages" })
        );
        assert!(
            changes
                .state_diff
                .as_array()
                .unwrap()
                .iter()
                .any(|change| { change["pointer"] == "/model/sessions" })
        );
    }

    #[test]
    fn root_animation_notifications_never_capture_large_history_but_model_edges_do() {
        use crate::model::{AgentSession, Message, MessageRole, ProviderKind};
        use std::cell::Cell;

        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Pi);
        session.messages.push(Message::new(
            MessageRole::Assistant,
            "x".repeat(waku_client::MAX_AUTOMATION_OBSERVATION_BYTES),
        ));
        let serializations = Cell::new(0);
        let mut previous = HashMap::new();
        let capture = |sections: &[String], session: &AgentSession| {
            super::super::serialize_state_sections(sections, |section| match section {
                "session" | "model" => {
                    serializations.set(serializations.get() + 1);
                    super::super::observable_session(session)
                }
                _ => json!({"section":section}),
            })
        };
        // The root observer and paint hook both request UI only, including
        // panel slides, pulse clocks, overlays and FPS-driven root frames.
        // Exercise the same capture gate used by Work::Observe while an
        // explicit full-model subscriber is also attached.
        for _ in 0..120 {
            let changed = interested_changes(UI, UI | MODEL);
            assert!(
                capture_model_observations(changed, &["state".into()], &mut previous, |sections| {
                    json!({"state": capture(sections, &session)})
                })
                .is_empty()
            );
            let state = capture(&paint_sections(), &session);
            assert!(state.get("session").is_none());
        }
        assert_eq!(serializations.get(), 0);
        // A state wait for a small section must not serialize an unrelated
        // huge transcript even on a genuine committed model notification.
        let changed = interested_changes(UI | MODEL | HEALTH, UI | MODEL);
        let composer = capture_model_observations(
            changed,
            &["composer".into()],
            &mut previous,
            |sections| json!({"state": capture(sections, &session)}),
        )
        .remove(0);
        assert!(composer.observed["state"].get("composer").is_some());
        assert_eq!(serializations.get(), 0);
        session.service_tier = Some("new-model-field-value".into());
        let selected =
            capture_model_observations(changed, &["session".into()], &mut previous, |sections| {
                let state = capture(sections, &session);
                assert_eq!(state["session"]["service_tier"], "new-model-field-value");
                json!({"state": state})
            })
            .remove(0);
        assert_eq!(serializations.get(), 1);
        // Explicit oversized sections continue to fail clearly; implicit
        // control waits never request this section in the first place.
        assert_eq!(
            selected.observed["observationError"]["code"],
            "observation_too_large"
        );
        assert_eq!(selected.observed["observationSection"], "session");
    }

    #[test]
    fn implicit_observation_uses_metadata_even_for_an_oversized_selected_history() {
        use crate::model::{AgentSession, Message, MessageRole, ProviderKind};
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Pi);
        session.messages.push(Message::new(
            MessageRole::Assistant,
            "x".repeat(5 * 1024 * 1024),
        ));
        for tree in [false, true] {
            let state =
                super::super::serialize_state_sections(
                    &live_sections(tree),
                    |section| match section {
                        "session" | "model" => panic!("implicit observation serialized history"),
                        "selection" => super::super::info::session_summary(&session),
                        _ => Value::Null,
                    },
                );
            assert_eq!(state["selection"]["historyCounts"]["messages"], 1);
            assert!(serde_json::to_vec(&state).unwrap().len() < 4096);
        }
    }

    #[test]
    fn selected_sections_stay_independent_of_full_state_and_each_other() {
        use crate::model::{AgentSession, Message, MessageRole, ProviderKind};

        let mut selected = AgentSession::new(Uuid::new_v4(), ProviderKind::Pi);
        let mut stored = AgentSession::new(Uuid::new_v4(), ProviderKind::Pi);
        stored.messages.push(Message::new(
            MessageRole::Assistant,
            "x".repeat(waku_client::MAX_AUTOMATION_OBSERVATION_BYTES),
        ));
        let sections = vec!["state".into(), "session".into(), "composer".into()];
        let mut previous = HashMap::new();
        let capture = |sections: &[String], selected: &AgentSession, text: &str| {
            json!({"state": super::super::serialize_state_sections(sections, |section| match section {
                "model" => json!({"sessions":[
                    super::super::observable_session(selected),
                    super::super::observable_session(&stored),
                ]}),
                "session" => super::super::observable_session(selected),
                "composer" => json!({"text":text}),
                _ => Value::Null,
            })})
        };
        let first = capture_model_observations(MODEL, &sections, &mut previous, |sections| {
            capture(sections, &selected, "before")
        });
        assert_eq!(first.len(), 3);
        assert_eq!(first[0].observed["observationSection"], "state");
        assert_eq!(
            first[0].observed["observationError"]["code"],
            "observation_too_large"
        );
        assert_eq!(
            first[1].observed["state"]["session"]["id"],
            json!(selected.id)
        );
        assert!(first[1].observed.get("observationError").is_none());
        assert_eq!(first[2].observed["state"]["composer"]["text"], "before");
        selected.service_tier = Some("updated".into());
        let current = capture_model_observations(MODEL, &sections, &mut previous, |sections| {
            capture(sections, &selected, "after")
        });
        // The unchanged full-state error is suppressed. Both independent
        // selected sections still publish current values and useful diffs.
        assert_eq!(current.len(), 2);
        assert_eq!(
            current[0].observed["state"]["session"]["service_tier"],
            "updated"
        );
        assert_eq!(
            current[1].state_diff,
            json!([{"pointer":"/composer/text","value":"after"}])
        );
        selected.messages = stored.messages.clone();
        let oversized =
            capture_model_observations(MODEL, &sections[1..], &mut previous, |sections| {
                capture(sections, &selected, "still current")
            });
        assert_eq!(oversized.len(), 2);
        assert_eq!(oversized[0].observed["observationSection"], "session");
        assert_eq!(
            oversized[0].observed["observationError"]["code"],
            "observation_too_large"
        );
        assert_eq!(
            oversized[1].observed["state"]["composer"]["text"],
            "still current"
        );
        assert!(oversized[1].observed.get("observationError").is_none());

        let default = capture_model_observations(MODEL, &[], &mut HashMap::new(), |sections| {
            assert_eq!(sections, &[String::from("state")]);
            capture(sections, &selected, "default")
        });
        assert_eq!(default[0].observed["observationSection"], "state");
        capture_model_observations(
            MODEL,
            &["unknown".into()],
            &mut previous,
            |_| json!({"observationError":{"code":"unknown_section"}}),
        );
        assert_eq!(previous.len(), 3);
    }

    #[test]
    fn paint_only_refreshes_layout_without_model_capture() {
        let mut previous = HashMap::new();
        let mut before = Value::Null;
        for viewport in [320, 720] {
            let changed = interested_changes(UI, UI | MODEL);
            assert!(
                capture_model_observations(changed, &["ui".into()], &mut previous, |_| {
                    panic!("resize-only paints must not capture model state")
                })
                .is_empty()
            );
            let state = super::super::serialize_state_sections(&paint_sections(), |section| {
                match section {
                    "ui" => {
                        json!({"sidebarViewport": viewport, "sidebarRows": 4, "sidebarRowCount": 4})
                    }
                    "session" | "model" => panic!("resize must not serialize stored history"),
                    _ => Value::Null,
                }
            });
            let paint = bounded_observation(observation(&before, json!({"state": state})));
            assert_eq!(paint.observed["state"]["ui"]["sidebarViewport"], viewport);
            if viewport == 720 {
                assert!(paint.state_diff.as_array().unwrap().contains(&json!({
                    "pointer":"/ui/sidebarViewport", "value":720,
                })));
            }
            before = paint.observed;
        }
    }

    #[test]
    fn cached_health_is_attached_and_bounded_on_the_reply_worker() {
        let paint = observation(
            &Value::Null,
            json!({"state":{"route":{}},"tree":{"nodes":[]}}),
        );
        let health = json!({"sessions":{"known":42},"providers":[{"sessionId":"conversation"}]});
        let wire = observation_with_health(paint.clone(), &health);
        assert_eq!(wire.observed["health"], health);
        assert_eq!(wire.state_diff, paint.state_diff);
        assert!(paint.observed.get("health").is_none());
        let huge_health =
            json!({"providers":"x".repeat(waku_client::MAX_AUTOMATION_OBSERVATION_BYTES)});
        assert!(bounded_size(&paint, 1024).is_some());
        assert_eq!(
            observation_with_health(paint.clone(), &huge_health).observed["observationError"]["code"],
            "observation_too_large"
        );
        let mut scoped = paint;
        scoped.observed["observationSection"] = json!("session");
        let failed = observation_with_health(scoped, &huge_health);
        assert_eq!(
            failed.observed["observationError"]["code"],
            "observation_too_large"
        );
        assert_eq!(failed.observed["observationSection"], "session");
    }
    #[test]
    fn encoded_observations_and_baselines_are_bounded() {
        assert_eq!(bounded_size(&json!("abc"), 5), Some(5));
        assert_eq!(bounded_size(&json!("abc"), 4), None);
        let huge = "x".repeat(waku_client::MAX_AUTOMATION_OBSERVATION_BYTES);
        let observed = bounded_observation(AutomationObservation {
            observed: json!({"state": huge}),
            ..Default::default()
        });
        assert_eq!(
            observed.observed["observationError"]["code"],
            "observation_too_large"
        );
        let mut cache = HashMap::new();
        for _ in 0..waku_client::MAX_AUTOMATION_SESSIONS + 1 {
            cache_baseline(&mut cache, Uuid::new_v4(), &json!({"state":1}));
        }
        assert_eq!(cache.len(), waku_client::MAX_AUTOMATION_SESSIONS);
    }
    #[test]
    fn omitted_snapshot_sections_are_not_reported_as_deleted_model() {
        let observation = observation(
            &json!({"state":{"model":{"sessions":[]},"composer":{"text":"a"}}}),
            json!({"state":{"composer":{"text":"b"}}}),
        );
        assert_eq!(
            observation.state_diff,
            json!([{"pointer":"/composer/text","value":"b"}])
        );
    }

    #[test]
    fn model_diffs_name_changed_and_removed_fields() {
        assert_eq!(
            state_diff(&json!({"a/b": 1, "old": 1}), &json!({"a/b": 2})),
            json!([
                {"pointer":"/a~1b", "value":2}, {"pointer":"/old", "removed":true}
            ])
        );
    }
    #[test]
    fn build_and_process_generation_are_separate() {
        let first = instance();
        let second = instance();
        assert_eq!(first.instance_id, second.instance_id);
        assert_eq!(first.build, second.build);
        assert_ne!(first.build, first.version);
    }
}
