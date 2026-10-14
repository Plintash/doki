//! The workspace side of the RPA bridge.
//!
//! [`start_pump`] consumes calls from the local endpoint on the foreground
//! executor; each call observes or operates the workspace through the same
//! code paths the UI uses: actions dispatch accessibility actions on the
//! frame's tree, typing sends real keystrokes to the focused input, and
//! snapshots read the frame's tree plus live view state.

pub(crate) mod daemon;
mod info;

use std::time::{Duration, Instant};

use gpui::{App, AsyncApp, Context, Window};
use serde_json::{Value, json};

use super::Waku;
use crate::main_window::MainWindow;
use crate::rpa::protocol::{
    self, Action, Condition, Error, FailureBundle, Reply, Request, Target, TreeDiff,
};
use crate::rpa::service::Inbound;

const VERBS: &[&str] = &[
    "openTask",
    "newTask",
    "openSession",
    "newSession",
    "selectProject",
    "submitPrompt",
    "cancelPrompt",
    "answerPermission",
    "openSettings",
    "togglePanel",
    "chooseProvider",
    "chooseModel",
];

/// Start consuming bridge calls. Called once from `run` after the endpoint is
/// published; a no-op when this build has no endpoint.
pub(crate) fn start_pump(cx: &mut App) {
    let receiver = crate::rpa::service::inbound();
    // Drop the automation tree once the last client leaves; the per-second
    // cadence keeps a frame from paying for an accessibility tree nobody is
    // reading.
    cx.spawn(async move |cx: &mut AsyncApp| {
        loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            cx.update(sync_automation);
        }
    })
    .detach();
    cx.spawn(async move |cx: &mut AsyncApp| {
        while let Ok(Inbound { call, reply }) = receiver.recv().await {
            let id = call.id;
            let outcome = match call.request {
                Ok(request) => execute(cx, request).await,
                Err(error) => {
                    let correlation = selected_correlation(cx);
                    let baseline = observe(cx, false);
                    let started_at_ms = crate::model::unix_time_millis();
                    Err(
                        with_failure_evidence(cx, error, baseline, correlation, started_at_ms)
                            .await,
                    )
                }
            };
            let _ = reply.send(match outcome {
                Ok(result) => Reply::ok(id, result),
                Err(error) => Reply::error(id, error),
            });
        }
    })
    .detach();
}

/// Keep a window's automation tree in step with the attached clients.
fn set_automation(window: &mut Window, attached: bool) {
    if window.is_a11y_automation_enabled() != attached {
        window.set_a11y_automation(attached);
    }
}

/// Drop the tree once the last client leaves. Polled once a second so the
/// disconnect is observed without a second control channel.
fn sync_automation(app: &mut App) {
    let attached = crate::rpa::service::attached() > 0 || daemon::attached();
    for handle in app.windows() {
        let _ = handle.update(app, |_root, window, _app| set_automation(window, attached));
    }
}

/// The one failure boundary for authenticated requests. The baseline belongs
/// to this call, not a global last snapshot shared by unrelated clients.
async fn execute(cx: &mut AsyncApp, request: Request) -> Result<Value, Error> {
    if !cfg!(debug_assertions) {
        return Err(Error::new(
            "unavailable",
            "RPA is available only in debug builds",
        ));
    }
    let started_at_ms = crate::model::unix_time_millis();
    let correlation = selected_correlation(cx);
    // Read-only calls do not need to force an extra frame on the successful
    // path. Actions/waits need a fresh baseline so their failure diff is real.
    let baseline = match request {
        Request::Act { .. }
        | Request::Type { .. }
        | Request::Key { .. }
        | Request::Invoke { .. }
        | Request::Wait { .. } => observe(cx, true),
        _ => observe(cx, false),
    };
    if let Some(record) = crate::diagnostics::action(&request, || correlation.clone()) {
        cx.update(|app| crate::diagnostics::record(app, || Some(record)));
    }
    let outcome = match request {
        Request::Wait {
            condition,
            timeout_ms,
        } => wait_for(cx, &condition, timeout_ms).await,
        request @ (Request::Logs { .. }
        | Request::Timeline { .. }
        | Request::Counters
        | Request::Verbosity { .. }) => diagnostic_query(cx, request).await,
        // A dispatch can draw synchronously. No workspace lease survives into
        // it, and evidence is collected only after dispatch has returned.
        request @ (Request::Act { .. } | Request::Type { .. } | Request::Key { .. }) => {
            dispatch(cx, request)
        }
        request => query(cx, request),
    };
    match outcome {
        Ok(value) => Ok(value),
        Err(error) => {
            Err(with_failure_evidence(cx, error, baseline, correlation, started_at_ms).await)
        }
    }
}

async fn diagnostic_query(cx: &mut AsyncApp, request: Request) -> Result<Value, Error> {
    let client = cx.update(|app| {
        crate::diagnostics::publish_counters(app);
        diagnostic_client(app)
    })?;
    let query = cx.background_executor().spawn(async move {
        let result = match request {
            Request::Logs { query } => client.diagnostic_logs(query).map(|value| json_of(&value)),
            Request::Timeline { query } => client
                .diagnostic_timeline(query)
                .map(|value| json_of(&value)),
            Request::Counters => client.diagnostic_counters().map(|value| json_of(&value)),
            Request::Verbosity { update } => client
                .diagnostic_verbosity(update)
                .map(|value| json_of(&value)),
            _ => unreachable!("only diagnostic requests reach the background query"),
        };
        result.map_err(|error| Error::new("diagnostic_query_failed", error.to_string()))
    });
    let timeout = cx.background_executor().timer(Duration::from_secs(5));
    futures_lite::future::race(query, async move {
        timeout.await;
        Err(Error::new(
            "diagnostic_query_timeout",
            "daemon diagnostic query timed out after 5000 ms",
        ))
    })
    .await
}

/// Observe without retaining a workspace lease. A failed tree capture still
/// yields the available state and health, including startup/disconnect errors.
fn observe(cx: &mut AsyncApp, draw: bool) -> Value {
    let sections = [
        "tree",
        "selection",
        "route",
        "board",
        "composer",
        "panels",
        "toast",
    ]
    .map(str::to_owned);
    cx.update(|app| {
        with_workspace(app, draw, |waku, window, cx| {
            match waku.rpa_snapshot_sections(window, cx, &sections) {
                Ok(snapshot) => snapshot,
                Err(error) => json!({
                    "state": waku.rpa_state_sections(cx, &sections),
                    "health": waku.rpa_health(cx),
                    "tree": null,
                    "observationError": { "code": error.code, "message": error.message },
                }),
            }
        })
        .unwrap_or_else(|| {
            json!({
                "state": null,
                "health": unavailable_health(app),
                "tree": null,
                "observationError": { "code": "app_not_ready", "message": app_not_ready().message },
            })
        })
    })
}

async fn with_failure_evidence(
    cx: &mut AsyncApp,
    error: Error,
    baseline: Value,
    correlation: waku_client::DiagnosticCorrelation,
    started_at_ms: u64,
) -> Error {
    // Full refresh before every observed tree, including occluded windows.
    let observed = observe(cx, true);
    let app_counters = cx.update(|app| crate::diagnostics::counters(app));
    let captured_at_ms = crate::model::unix_time_millis();
    let mut bundle = FailureBundle {
        tree_diff: TreeDiff::between(&baseline["tree"], &observed["tree"]),
        observed,
        recent_events: None,
        counters: waku_client::DiagnosticCounters {
            observed_at_ms: captured_at_ms,
            processes: vec![app_counters.clone()],
        },
        diagnostic_errors: Vec::new(),
    };
    let client = cx.update(|app| {
        crate::diagnostics::publish_counters(app);
        diagnostic_client(app)
    });
    if let Ok(client) = client {
        let query = waku_client::DiagnosticQuery {
            session_id: correlation.session_id,
            since_ms: Some(started_at_ms.saturating_sub(60_000)),
            until_ms: Some(captured_at_ms),
            limit: Some(128),
            ..Default::default()
        };
        let logs_client = client.clone();
        let logs = cx
            .background_executor()
            .spawn(async move { logs_client.diagnostic_logs(query) });
        let counters = cx
            .background_executor()
            .spawn(async move { client.diagnostic_counters() });
        let logs_timeout = cx.background_executor().timer(Duration::from_secs(2));
        let counters_timeout = cx.background_executor().timer(Duration::from_secs(2));
        // Fetch independently: a hung counter query must not discard logs
        // already collected, and neither query may hold up the UI executor.
        let (logs, counters) = futures_lite::future::zip(
            futures_lite::future::race(
                async move { logs.await.map_err(|error| error.to_string()) },
                async move {
                    logs_timeout.await;
                    Err("timed out after 2000 ms".into())
                },
            ),
            futures_lite::future::race(
                async move { counters.await.map_err(|error| error.to_string()) },
                async move {
                    counters_timeout.await;
                    Err("timed out after 2000 ms".into())
                },
            ),
        )
        .await;
        match logs {
            Ok(logs) => bundle.recent_events = Some(logs),
            Err(error) => bundle.diagnostic_errors.push(format!("logs: {error}")),
        }
        match counters {
            Ok(mut counters) => {
                // The app was sampled synchronously with the failed frame;
                // prefer it to the daemon's last published gauge.
                counters
                    .processes
                    .retain(|report| report.origin != app_counters.origin);
                counters.processes.push(app_counters);
                bundle.counters = counters;
            }
            Err(error) => bundle.diagnostic_errors.push(format!("counters: {error}")),
        }
    } else if let Err(error) = client {
        bundle.diagnostic_errors.push(format!(
            "recent events and daemon counters unavailable: {}",
            error.message
        ));
    }
    error.with_bundle(bundle)
}

/// The task and turn the workspace currently shows, for attributing an action
/// to the context it was taken in. Read-only: it draws nothing and holds no
/// lease beyond the lookup.
fn selected_correlation(cx: &mut AsyncApp) -> waku_client::DiagnosticCorrelation {
    cx.update(|app| {
        with_workspace(app, false, |waku, _window, _cx| {
            waku.selected_session().map(crate::diagnostics::correlation)
        })
        .flatten()
    })
    .unwrap_or_default()
}

/// A request served against the workspace: describe, invoke, or the read side
/// of a snapshot. Taking the workspace lease is safe here because none of
/// these dispatches input.
fn query(cx: &mut AsyncApp, request: Request) -> Result<Value, Error> {
    let draws = matches!(request, Request::Snapshot { .. });
    cx.update(|app| {
        match with_workspace(app, draws, |waku, window, cx| {
            waku.rpa_execute(request, window, cx)
        }) {
            Some(result) => result,
            None => Err(app_not_ready()),
        }
    })
}

/// A request that dispatches input, served at the window level with no
/// workspace lease held.
fn dispatch(cx: &mut AsyncApp, request: Request) -> Result<Value, Error> {
    cx.update(|app| {
        match with_window(app, true, |window, app| {
            dispatch_rpa_input(window, app, request)
        }) {
            Some(result) => result,
            None => Err(app_not_ready()),
        }
    })
}

/// Serialize model data into a snapshot. These are plain serializable values;
/// a failure would be a bug, not a state to quietly fall back from.
fn json_of<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("snapshot data serializes")
}

fn app_not_ready() -> Error {
    Error::new(
        "app_not_ready",
        "the app has no workspace window yet (the daemon may still be connecting)",
    )
}

async fn wait_for(
    cx: &mut AsyncApp,
    condition: &Condition,
    timeout_ms: u64,
) -> Result<Value, Error> {
    let started = Instant::now();
    let timeout = Duration::from_millis(timeout_ms);
    let needs_frame = matches!(condition, Condition::Node { .. } | Condition::Text { .. });
    let sections = match condition {
        Condition::Node { .. } | Condition::Text { .. } => vec!["tree".to_owned()],
        Condition::State { pointer, .. } | Condition::StateContains { pointer, .. } => vec![
            pointer
                .strip_prefix('/')
                .and_then(|pointer| pointer.split('/').next())
                .filter(|section| !section.is_empty())
                .map(|section| section.replace("~1", "/").replace("~0", "~"))
                .unwrap_or_else(|| "state".into()),
        ],
    };
    loop {
        let observed = cx.update(|app| {
            with_workspace(app, needs_frame, |waku, window, cx| {
                let snapshot = waku.rpa_snapshot_sections(window, cx, &sections)?;
                Ok(condition_holds(condition, &snapshot))
            })
        });
        let held = match observed {
            Some(result) => result?,
            None => return Err(app_not_ready()),
        };
        if held {
            return Ok(json!({
                "held": true,
                "elapsedMs": started.elapsed().as_millis() as u64,
            }));
        }
        if started.elapsed() >= timeout {
            return Err(wait_timeout(condition, timeout_ms));
        }
        cx.background_executor()
            .timer(protocol::WAIT_POLL_INTERVAL)
            .await;
    }
}

fn wait_timeout(condition: &Condition, timeout_ms: u64) -> Error {
    Error::new(
        "timeout",
        format!(
            "timed out after {} ms waiting for {}",
            timeout_ms,
            condition.describe()
        ),
    )
}

/// Find the main window, keep its automation tree in step with the attached
/// clients, optionally draw the frame this call observes, and run `f` against
/// the window. No entity lease is held.
fn with_window<R>(
    app: &mut App,
    draw: bool,
    f: impl FnOnce(&mut Window, &mut App) -> R,
) -> Option<R> {
    let mut f = Some(f);
    for handle in app.windows() {
        let has_workspace = handle
            .update(app, |root, _window, app| {
                root.downcast::<MainWindow>()
                    .ok()
                    .is_some_and(|main| main.read(app).workspace().is_some())
            })
            .unwrap_or(false);
        if !has_workspace {
            continue;
        }
        let f = f.take()?;
        return handle
            .update(app, |_root, window, app| {
                prepare_window(window, app, draw);
                f(window, app)
            })
            .ok();
    }
    None
}

/// As [`with_window`], then lease the workspace for the caller. The frame (if
/// requested) is drawn before the lease is taken, because a draw renders the
/// workspace and must not run while it is leased.
fn with_workspace<R>(
    app: &mut App,
    draw: bool,
    f: impl FnOnce(&mut Waku, &mut Window, &mut Context<Waku>) -> R,
) -> Option<R> {
    let mut f = Some(f);
    for handle in app.windows() {
        let workspace = handle
            .update(app, |root, window, app| {
                let main = root.downcast::<MainWindow>().ok()?;
                let workspace = main.read(app).workspace()?;
                prepare_window(window, app, draw);
                Some(workspace)
            })
            .ok()
            .flatten();
        let Some(workspace) = workspace else {
            continue;
        };
        let f = f.take()?;
        return handle
            .update(app, |_root, window, app| {
                workspace.update(app, |waku, cx| f(waku, window, cx))
            })
            .ok();
    }
    None
}

/// Keep the automation tree in step with the attached clients and draw the
/// frame a call should observe. A window that is occluded has no display
/// link, so waiting for one would time out; `refresh` first, because cached
/// views reuse their last paint and a reused view emits no accessibility
/// nodes, which would leave the tree a fragment of the UI.
fn prepare_window(window: &mut Window, app: &mut App, draw: bool) {
    let attached = crate::rpa::service::attached() > 0 || daemon::attached();
    set_automation(window, attached);
    if draw {
        window.refresh();
        let _ = window.draw(app);
    }
}

fn condition_holds(condition: &Condition, snapshot: &Value) -> bool {
    match condition {
        Condition::Node { id, present } => {
            let found = snapshot_tree_nodes(snapshot)
                .iter()
                .any(|node| node["element_id"].as_str() == Some(id.as_str()));
            found == *present
        }
        Condition::Text { id, contains } => snapshot_tree_nodes(snapshot).iter().any(|node| {
            if node["element_id"].as_str() != Some(id.as_str()) {
                return false;
            }
            ["label", "value", "description"].iter().any(|field| {
                node[*field]
                    .as_str()
                    .is_some_and(|text| text.contains(contains.as_str()))
            })
        }),
        Condition::State { pointer, equals } => snapshot["state"]
            .pointer(pointer)
            .is_some_and(|value| value == equals),
        Condition::StateContains { pointer, contains } => snapshot["state"]
            .pointer(pointer)
            .is_some_and(|value| value_contains(value, contains)),
    }
}

/// Whether a state value contains the text: a string contains it directly, an
/// array contains it in any element (or any element's `text` field), and an
/// object contains it in any field.
fn value_contains(value: &Value, contains: &str) -> bool {
    match value {
        Value::String(text) => text.contains(contains),
        Value::Array(items) => items.iter().any(|item| value_contains(item, contains)),
        Value::Object(fields) => fields.values().any(|field| value_contains(field, contains)),
        _ => false,
    }
}

fn snapshot_tree_nodes(snapshot: &Value) -> &[Value] {
    snapshot["tree"]["nodes"]
        .as_array()
        .expect("snapshots always carry a tree with a nodes array")
        .as_slice()
}

/// Startup state alone is not a liveness check: a Ready supervisor can hold
/// a disconnected client while its replacement is still being established.
fn daemon_health(cx: &App) -> Value {
    match crate::daemon::state(cx) {
        crate::daemon::DaemonState::Idle => {
            json!({ "state": "idle", "connected": false, "error": null })
        }
        crate::daemon::DaemonState::Connecting => {
            json!({ "state": "connecting", "connected": false, "error": null })
        }
        crate::daemon::DaemonState::Failed(error) => {
            json!({ "state": "failed", "connected": false, "error": error })
        }
        crate::daemon::DaemonState::Ready(daemon) => {
            let Some(client) = daemon.try_client() else {
                return transitioning_daemon_health();
            };
            let mut health = connected_daemon_health(client.is_disconnected());
            health["requestRoundTripMs"] = json!(client.last_request_round_trip_ms());
            health
        }
    }
}

/// Optional diagnostics must never wait for the supervisor's lifecycle lock
/// on the UI thread. An unavailable sample is distinct from a dropped link.
fn diagnostic_client(cx: &App) -> Result<waku_client::DaemonClient, Error> {
    let daemon = crate::daemon::connected(cx)
        .ok_or_else(|| Error::new("daemon_unavailable", "the daemon has not connected"))?;
    let client = daemon.try_client().ok_or_else(|| Error::new(
        "daemon_unavailable", "the daemon connection is transitioning; its current transport is temporarily unavailable",
    ))?;
    if client.is_disconnected() {
        return Err(Error::new(
            "daemon_unavailable",
            "the app's link to the daemon has dropped",
        ));
    }
    Ok(client)
}

fn transitioning_daemon_health() -> Value {
    json!({
        "state": "transitioning",
        "connected": null,
        "error": "the daemon connection is transitioning; its current transport is temporarily unavailable",
        "requestRoundTripMs": null,
    })
}

fn connected_daemon_health(disconnected: bool) -> Value {
    json!({
        "state": if disconnected { "disconnected" } else { "connected" },
        "connected": !disconnected,
        "error": disconnected.then_some("the app's link to the daemon has dropped"),
    })
}

fn last_error_summary(daemon: &Value) -> Value {
    if !daemon["error"].is_null() {
        return json!({ "source": "daemon", "message": daemon["error"] });
    }
    crate::diagnostics::last_error().map(|record| json!({
        "timestampMs": record.timestamp_ms,
        "source": record.source,
        "message": record.fields.get("error").and_then(Value::as_str).unwrap_or(&record.message),
        "sessionId": record.correlation.session_id,
        "turnId": record.correlation.turn_id,
    })).unwrap_or(Value::Null)
}

fn unavailable_health(cx: &App) -> Value {
    let daemon = daemon_health(cx);
    json!({
        "lastError": last_error_summary(&daemon),
        "daemon": daemon,
        "workspace": "unavailable",
        "sessions": { "selected": null, "active": [], "known": 0 },
        "providers": [],
        "pendingPermissions": [],
        "pendingUserInput": [],
        "counters": crate::diagnostics::counters(cx),
    })
}

impl Waku {
    /// Serve one read-side bridge call against this workspace. Input is
    /// dispatched at the window level instead (see [`dispatch_rpa_input`]),
    /// because a dispatch can draw synchronously and a draw renders the
    /// workspace, which must not happen while this lease is held.
    pub(crate) fn rpa_execute(
        &mut self,
        request: Request,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Value, Error> {
        match request {
            Request::Describe => Ok(json!({
                "instance": crate::rpa::service::instance_identity(),
                "attachedClients": crate::rpa::service::attached(),
                "verbs": VERBS,
                "snapshotSections": STATE_SECTIONS,
                "automation": window.is_a11y_automation_enabled(),
                "treeReady": window.a11y_automation_snapshot().is_some(),
                "capabilities": info::capabilities(),
                "state": self.rpa_state_sections(cx, &info::CONTEXT_SECTIONS.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>()),
                "health": self.rpa_health(cx),
            })),
            Request::Snapshot { sections } => self.rpa_snapshot_sections(window, cx, &sections),
            Request::Invoke { verb, args } => self.rpa_invoke(&verb, &args, window, cx),
            Request::Act { .. }
            | Request::Type { .. }
            | Request::Key { .. }
            | Request::Wait { .. }
            | Request::Logs { .. }
            | Request::Timeline { .. }
            | Request::Counters
            | Request::Verbosity { .. } => {
                unreachable!("input and waits are dispatched outside the workspace lease")
            }
        }
    }

    /// Workspace verbs. Each one calls the handler its control calls, so a
    /// scenario set up through a verb lands in exactly the state a user
    /// would reach.
    fn rpa_invoke(
        &mut self,
        verb: &str,
        args: &Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Value, Error> {
        let uuid = |name: &str| {
            args.get(name)
                .and_then(Value::as_str)
                .and_then(|value| uuid::Uuid::parse_str(value).ok())
                .ok_or_else(|| {
                    Error::new("invalid_args", format!("`{verb}` wants a uuid in `{name}`"))
                })
        };
        match verb {
            "openTask" => {
                let id = uuid("id")?;
                require_task(&self.state.tasks, id)?;
                self.open_task_from_card(id, cx);
                Ok(json!({ "opened": id }))
            }
            "openSession" => {
                let id = uuid("id")?;
                if !self.state.sessions.iter().any(|session| session.id == id) {
                    return Err(Error::new("not_found", format!("no session `{id}`"))
                        .with_target(id.to_string()));
                }
                self.select_session(id, cx);
                Ok(json!({ "opened": id }))
            }
            "selectProject" => {
                let id = uuid("id")?;
                if !self.state.projects.iter().any(|project| project.id == id) {
                    return Err(Error::new("not_found", format!("no project `{id}`"))
                        .with_target(id.to_string()));
                }
                self.select_project(id, cx);
                Ok(json!({ "selected": id }))
            }
            "newTask" => {
                let title = new_task_title(args)?;
                self.new_task_action(&crate::NewTask, window, cx);
                if let Some(title) = title {
                    // Use the dialog's own field and commit handler. This
                    // preserves Board project scope and deferred worktree intent.
                    if let Some(dialog) = self.new_task_dialog.as_ref() {
                        dialog
                            .input
                            .update(cx, |input, cx| input.set_content(title, cx));
                    }
                    self.commit_new_task(cx);
                    Ok(json!({ "opened": self.selected_task }))
                } else {
                    Ok(json!({ "opened": "newTask" }))
                }
            }
            "newSession" => {
                if args.get("taskId").is_some() {
                    let id = uuid("taskId")?;
                    let task = require_task(&self.state.tasks, id)?;
                    if !super::task_details::task_project_id(task).is_some_and(|project_id| {
                        self.state
                            .projects
                            .iter()
                            .any(|project| project.id == project_id)
                    }) {
                        return Err(Error::new(
                            "disabled",
                            "choose a project for the issue before starting a session",
                        )
                        .with_target(id.to_string()));
                    }
                    self.start_task_session(id, window, cx);
                } else {
                    // Standalone means standalone even while an issue is open.
                    self.new_session_action(&crate::NewSession, window, cx);
                }
                let opened = self
                    .pending_session_activation
                    .as_ref()
                    .map(|pending| pending.session_id)
                    .or(self.state.selected_session);
                Ok(json!({ "opened": opened }))
            }
            "submitPrompt" => {
                let session = self.selected_session().ok_or_else(|| {
                    Error::new("not_found", "select a session before submitting a prompt")
                })?;
                if self.submission_preparations.contains(&session.id)
                    || self.response_fork_preparations.contains_key(&session.id)
                {
                    return Err(Error::new(
                        "disabled",
                        "the session is preparing; wait before submitting",
                    )
                    .with_target("composer"));
                }
                if self.model_picker_has_no_providers() {
                    return Err(Error::new("disabled", "no enabled provider is available")
                        .with_target("composer"));
                }
                let prompt = match args.get("text") {
                    Some(Value::String(text)) => text.clone(),
                    None => self.composer.read(cx).content(cx).to_owned(),
                    _ => {
                        return Err(Error::new(
                            "invalid_args",
                            "`submitPrompt.text` must be a string",
                        ));
                    }
                };
                if prompt.trim().is_empty()
                    && self.composer_attachments.is_empty()
                    && self.composer_annotations.is_empty()
                {
                    return Err(Error::new("invalid_args", "the prompt is empty"));
                }
                // Exactly the send-button path: attachment/local-command
                // processing, draft consumption, then submit/steer/queue.
                if let Some(submission) = self.submission_with_attachments(&prompt, cx) {
                    self.composer.update(cx, |input, cx| input.clear(cx));
                    self.submit_composer_submission(submission, cx);
                }
                Ok(json!({ "submitted": true }))
            }
            "cancelPrompt" => {
                if self.selected_session().is_none() {
                    return Err(Error::new("not_found", "no session is selected"));
                }
                self.cancel_turn(cx);
                Ok(json!({ "cancelled": true }))
            }
            "answerPermission" => {
                let request_id = required_string(args, "requestId", verb)?;
                let option_id = required_string(args, "optionId", verb)?;
                let pending = self
                    .state
                    .selected_session
                    .and_then(|id| self.runtimes.get(&id))
                    .and_then(|runtime| runtime.pending_permission.as_ref())
                    .ok_or_else(|| {
                        Error::new(
                            "not_found",
                            "the selected session has no pending permission request",
                        )
                        .with_target(request_id)
                    })?;
                if pending.request_id != request_id {
                    return Err(Error::new(
                        "not_found",
                        "the named permission is not pending on the selected session",
                    )
                    .with_target(request_id));
                }
                if !pending.options.iter().any(|option| option.id == option_id) {
                    return Err(Error::new(
                        "invalid_args",
                        "the named permission option is unavailable",
                    )
                    .with_target(option_id));
                }
                let message = args
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.respond_permission(request_id.to_owned(), option_id.to_owned(), message, cx);
                Ok(json!({ "answered": request_id, "optionId": option_id }))
            }
            "openSettings" => {
                let page = settings_page(
                    args.get("page")
                        .and_then(Value::as_str)
                        .unwrap_or("general"),
                )?;
                self.open_settings_action(&crate::OpenSettings, window, cx);
                self.open_settings_page(page, cx);
                Ok(json!({ "page": format!("{page:?}") }))
            }
            "togglePanel" => {
                let panel = required_string(args, "panel", verb)?;
                match panel {
                    "sidebar" => self.toggle_sidebar_action(&crate::ToggleSidebar, window, cx),
                    "rightPanel" => {
                        self.toggle_right_panel_action(&crate::ToggleRightPanel, window, cx)
                    }
                    _ => {
                        return Err(Error::new(
                            "invalid_args",
                            "`togglePanel.panel` must be sidebar or rightPanel",
                        ));
                    }
                }
                Ok(json!({ "toggled": panel }))
            }
            "chooseProvider" | "chooseModel" => {
                let provider = match args.get("provider") {
                    Some(value) => serde_json::from_value(value.clone())
                        .map_err(|_| Error::new("invalid_args", "unknown provider"))?,
                    None => self
                        .selected_session()
                        .map(|session| session.provider)
                        .ok_or_else(|| Error::new("not_found", "no session is selected"))?,
                };
                if !self.provider_enabled(provider) {
                    return Err(Error::new(
                        "disabled",
                        "the provider is unavailable or switched off",
                    )
                    .with_target(format!("{provider:?}")));
                }
                let probe = self
                    .provider_probe(provider)
                    .ok_or_else(|| Error::new("not_found", "provider has no model catalog"))?;
                let model = if let Some(model) = args.get("model").and_then(Value::as_str) {
                    model.to_owned()
                } else if verb == "chooseProvider" {
                    probe
                        .preferred_model()
                        .map(|model| model.id.clone())
                        .ok_or_else(|| {
                            Error::new("not_found", "provider has no selectable model")
                        })?
                } else {
                    return Err(Error::new("invalid_args", "`chooseModel` wants a model id"));
                };
                if !probe.models.iter().any(|candidate| candidate.id == model) {
                    return Err(Error::new(
                        "not_found",
                        "the model is not in this provider's catalog",
                    )
                    .with_target(model));
                }
                if !self
                    .selected_session()
                    .is_some_and(|session| session.can_choose_model(provider))
                {
                    return Err(Error::new(
                        "disabled",
                        "the selected session cannot change to this provider/model now",
                    ));
                }
                self.choose_model(provider, model.clone(), cx);
                Ok(json!({ "provider": provider, "model": model }))
            }
            other => Err(Error::new(
                "unknown_verb",
                format!("no verb `{other}`; known verbs: {}", VERBS.join(", ")),
            )),
        }
    }

    fn rpa_snapshot_sections(
        &self,
        window: &Window,
        cx: &App,
        sections: &[String],
    ) -> Result<Value, Error> {
        let mut snapshot = self.rpa_snapshot_without_health(window, cx, sections)?;
        snapshot["health"] = self.rpa_health(cx);
        Ok(snapshot)
    }

    /// Subscription captures attach their model-refreshed health on the reply
    /// worker. A paint must not walk runtimes or clone their cached health.
    fn rpa_snapshot_without_health(
        &self,
        window: &Window,
        cx: &App,
        sections: &[String],
    ) -> Result<Value, Error> {
        validate_sections(sections)?;
        let wants_tree = sections.is_empty() || sections.iter().any(|s| s == "tree");
        let tree = if wants_tree {
            window
                .a11y_automation_snapshot()
                .ok_or_else(|| {
                    Error::new(
                        "no_frame",
                        "the window has not drawn an automation frame yet",
                    )
                })
                .and_then(|text| {
                    serde_json::from_str::<Value>(&text).map_err(|error| {
                        Error::new(
                            "invalid_tree",
                            format!("the frame's tree is not valid JSON: {error}"),
                        )
                    })
                })?
        } else {
            Value::Null
        };
        audit_tree(&tree);
        Ok(json!({
            "instance": crate::rpa::service::instance_identity(),
            "window": {
                "title": crate::instance::window_title(),
                "width": f32::from(window.viewport_size().width),
                "height": f32::from(window.viewport_size().height),
                "focused": window.is_window_active(),
            },
            "state": self.rpa_state_sections(cx, sections),
            "tree": tree,
        }))
    }

    /// A summary from live memory, not a second diagnostic event stream.
    /// Explicit calls and model notifications refresh this; paints reuse the
    /// last summary without walking the task/runtime collections again.
    fn rpa_health(&self, cx: &App) -> Value {
        let daemon = daemon_health(cx);
        let mut last_error = last_error_summary(&daemon);
        let mut permissions = Vec::new();
        let mut user_input = Vec::new();
        let mut providers = Vec::new();
        let mut backlog = 0;
        let mut runtimes = self.runtimes.iter().collect::<Vec<_>>();
        runtimes.sort_by_key(|(id, _)| **id);
        for (session_id, runtime) in runtimes {
            backlog += runtime.events.len() + runtime.pending_events.len();
            let session = self
                .state
                .sessions
                .iter()
                .find(|session| session.id == *session_id);
            providers.push(json!({
                "sessionId": session_id,
                "provider": session.map(|session| session.provider),
                "runtimeId": session.and_then(|session| session.runtime_event_cursor.map(|cursor| cursor.runtime_id)),
                "state": session.map(|session| session.status),
                "streamPhase": runtime.stream_phase.map(|phase| match phase {
                    super::StreamPhase::Text => "text",
                    super::StreamPhase::Reasoning => "reasoning",
                    super::StreamPhase::Activity => "activity",
                }),
                "eventBacklog": runtime.events.len() + runtime.pending_events.len(),
                "lastError": runtime.last_driver_error,
            }));
            if let Some(permission) = &runtime.pending_permission {
                permissions.push(json!({
                    "kind": "provider", "sessionId": session_id,
                    "requestId": permission.request_id, "title": permission.title,
                }));
            }
            if let Some(permission) = &runtime.pending_computer_approval {
                permissions.push(json!({
                    "kind": "computer", "sessionId": session_id,
                    "requestId": permission.request.call_id, "tool": permission.request.tool,
                    "sensitive": permission.sensitive,
                }));
            }
            if let Some(input) = &runtime.pending_user_input {
                user_input.push(json!({
                    "sessionId": session_id, "requestId": input.request_id,
                    "questionIndex": input.question_index, "questionCount": input.questions.len(),
                }));
            }
        }
        if last_error.is_null()
            && let Some((session_id, runtime)) = self
                .runtimes
                .iter()
                .filter(|(_, runtime)| runtime.last_driver_error.is_some())
                .max_by_key(|(_, runtime)| runtime.last_active_at)
        {
            last_error = json!({ "source": "driver", "sessionId": session_id, "message": runtime.last_driver_error });
        }
        crate::diagnostics::set_event_backlog(backlog);
        json!({
            "daemon": daemon,
            "workspace": "ready",
            "lastError": last_error,
            "sessions": {
                "known": self.state.sessions.len(),
                "selected": self.selected_session().map(|session| json!({
                    "sessionId": session.id, "state": session.status, "provider": session.provider,
                    "turnId": session.active_turn_id(), "detailLoaded": session.detail_loaded,
                })),
                "active": self.state.sessions.iter().filter(|session| session.status.is_busy()).map(|session| json!({
                    "sessionId": session.id, "state": session.status, "provider": session.provider,
                    "turnId": session.active_turn_id(),
                })).collect::<Vec<_>>(),
            },
            "providers": providers,
            "pendingPermissions": permissions,
            "pendingUserInput": user_input,
            "counters": crate::diagnostics::counters(cx),
        })
    }

    /// Serialized model data stays complete as the model evolves. Only the
    /// genuinely UI-owned state is enumerated here. A section is serialized
    /// only when selected, so a frame subscription never walks every transcript.
    fn rpa_state_sections(&self, cx: &App, sections: &[String]) -> Value {
        serialize_state_sections(sections, |section| {
            match section {
            "model" => {
                let mut model = json_of(&self.state);
                model["sessions"] =
                    Value::Array(self.state.sessions.iter().map(observable_session).collect());
                model
            }
            "session" => self.selected_session().map(observable_session).unwrap_or(Value::Null),
            "selection" => self.selected_session().map(info::session_summary).unwrap_or(Value::Null),
            "providers" => self.rpa_providers(),
            "runtime" => self.rpa_runtime(),
            "workspace" => self.rpa_workspace(),
            "task" => self.selected_task.and_then(|id| self.state.tasks.iter().find(|task| task.id == id)).map(json_of).unwrap_or(Value::Null),
            "tasks" => json_of(&self.state.tasks),
            "sessions" => observable_session_list(&self.state.sessions),
            "projects" => json_of(&self.state.projects),
            "route" => json!({
                "surface": if self.settings_page.is_some() { "settings" } else { "waku" },
                "settingsPage": self.settings_page.map(|page| format!("{page:?}")),
                "selectedProject": self.state.selected_project,
                "selectedSession": self.state.selected_session,
                "selectedTask": self.selected_task,
                "view": if self.settings_page.is_some() { "settings" } else if self.board_open && self.selected_task.is_some() { "issue" } else if self.board_open { "board" } else { "session" },
            }),
            "board" => json!({
                "open": self.board_open,
                "selectedTask": self.selected_task,
                "projectFilter": self.board_project_filter,
                "search": self.board_search_query,
                "activityFilter": match self.board_activity_filter {
                    super::board::BoardActivityFilter::All => "all",
                    super::board::BoardActivityFilter::Running => "running",
                    super::board::BoardActivityFilter::Attention => "attention",
                },
                "newTaskDialog": self.new_task_dialog.is_some(),
                "completionTask": self.pending_task_completion.as_ref().map(|confirm| confirm.task_id),
                "savePending": self.task_save_in_flight || self.task_save_pending,
                "saveError": self.task_save_error,
            }),
            "composer" => json!({
                "text": self.composer.read(cx).content(cx),
                "preparing": self.state.selected_session.is_some_and(|id| self.submission_preparations.contains(&id) || self.response_fork_preparations.contains_key(&id)),
                "providerAvailable": !self.model_picker_has_no_providers(),
                "attachmentCount": self.composer_attachments.len(),
                "annotationCount": self.composer_annotations.len(),
            }),
            "toast" => self.toast.as_ref().map(|toast| json!({ "message": toast.message, "tone": format!("{:?}", toast.tone) })).unwrap_or(Value::Null),
            "panels" => json!({ "sidebar": self.sidebar_visible, "rightPanel": self.right_panel_visible }),
            "ui" => json!({
                "annotations": {
                    "selectionActive": !self.transcript_selection.selection.borrow().is_empty(),
                    "toolbarVisible": self.selection_toolbar.borrow().is_some(),
                    "commentOpen": self.selection_toolbar.borrow().as_ref().is_some_and(|toolbar| toolbar.comment_open),
                    "commentText": self.selection_toolbar.borrow().as_ref().filter(|toolbar| toolbar.comment_open).and_then(|_| self.annotation_prompt_input.borrow().as_ref().map(|input| input.read(cx).content().to_owned())),
                    "editor": self.annotation_editor.borrow().as_ref().map(|editor| json!({"annotationId":editor.id, "text":self.annotation_editor_input.borrow().as_ref().map(|input| input.read(cx).content().to_owned())})),
                    "previewVisible": self.annotation_preview_visible.get(),
                },
                "sidebarRowCount": self.sidebar_list_state.item_count(),
                "sidebarRows": self.sidebar_row_cache.borrow().len(),
                "sidebarViewport": f32::from(self.sidebar_list_state.viewport_bounds().size.height),
            }),
            _ => unreachable!("only known state sections are serialized"),
        }
        })
    }
}

const STATE_SECTIONS: &[&str] = &[
    "model",
    "task",
    "tasks",
    "session",
    "sessions",
    "projects",
    "route",
    "board",
    "composer",
    "toast",
    "panels",
    "ui",
    "selection",
    "providers",
    "runtime",
    "workspace",
];

/// Board issue identity must never be inferred from a conversation UUID.
fn require_task(
    tasks: &[crate::model::Task],
    id: uuid::Uuid,
) -> Result<&crate::model::Task, Error> {
    tasks.iter().find(|task| task.id == id).ok_or_else(|| {
        Error::new(
            "not_found",
            format!("no issue `{id}`; openSession accepts conversation ids"),
        )
        .with_target(id.to_string())
    })
}

fn new_task_title(args: &Value) -> Result<Option<String>, Error> {
    match args.get("title") {
        None => Ok(None),
        Some(Value::String(title)) if !title.trim().is_empty() => Ok(Some(title.trim().to_owned())),
        _ => Err(Error::new(
            "invalid_args",
            "`newTask.title` must be a nonempty string",
        )),
    }
}

/// List projections expose conversation identity and issue bindings without
/// copying transcript history. `tasks` is reserved for the actual Board issues.
fn observable_session_list(sessions: &[crate::model::AgentSession]) -> Value {
    json_of(
        &sessions
            .iter()
            .map(info::session_summary)
            .collect::<Vec<_>>(),
    )
}

/// Filter before serializing: omitted sections must never touch model data,
/// even transiently. In particular, a paint cannot serialize a transcript and
/// discard it afterward.
fn serialize_state_sections(
    sections: &[String],
    mut serialize: impl FnMut(&str) -> Value,
) -> Value {
    let all = sections.is_empty() || sections.iter().any(|section| section == "state");
    Value::Object(
        STATE_SECTIONS
            .iter()
            .filter(|name| all || sections.iter().any(|section| section == **name))
            .map(|name| ((*name).into(), serialize(name)))
            .collect(),
    )
}

fn audit_tree(tree: &Value) {
    if !cfg!(debug_assertions) {
        return;
    }
    for node in tree["nodes"].as_array().into_iter().flatten() {
        if node["role"] == "Window"
            || !node["actions"]
                .as_array()
                .is_some_and(|actions| !actions.is_empty())
        {
            continue;
        }
        if node["element_id"]
            .as_str()
            .is_none_or(|id| id.trim().is_empty())
            || node["role"]
                .as_str()
                .is_none_or(|role| role == "GenericContainer" || role.is_empty())
        {
            daemon::identity_warning(format!(
                "rendered interactive node lacks semantic identity: role={}, source={}, id={}",
                node["role"], node["source_location"], node["element_id"]
            ));
        }
    }
}

fn validate_sections(sections: &[String]) -> Result<(), Error> {
    const KNOWN: &[&str] = &[
        "tree",
        "state",
        "model",
        "task",
        "tasks",
        "session",
        "sessions",
        "projects",
        "route",
        "board",
        "composer",
        "toast",
        "panels",
        "ui",
        "selection",
        "providers",
        "runtime",
        "workspace",
    ];
    if let Some(section) = sections
        .iter()
        .find(|section| !KNOWN.contains(&section.as_str()))
    {
        return Err(Error::new(
            "invalid_args",
            format!(
                "unknown snapshot section `{section}`; expected {}",
                KNOWN.join(", ")
            ),
        ));
    }
    Ok(())
}

fn required_string<'a>(args: &'a Value, field: &str, verb: &str) -> Result<&'a str, Error> {
    args.get(field).and_then(Value::as_str).ok_or_else(|| {
        Error::new(
            "invalid_args",
            format!("`{verb}` wants a string in `{field}`"),
        )
    })
}

fn settings_page(name: &str) -> Result<super::SettingsPage, Error> {
    use super::SettingsPage;
    match name {
        "general" => Ok(SettingsPage::General),
        "providers" => Ok(SettingsPage::Providers),
        "skills" => Ok(SettingsPage::Skills),
        "usage" => Ok(SettingsPage::Usage),
        "daemon" => Ok(SettingsPage::Daemon),
        "computerUse" => Ok(SettingsPage::ComputerUse),
        "appearance" => Ok(SettingsPage::Appearance),
        _ => Err(Error::new(
            "invalid_args",
            format!("unknown settings page `{name}`"),
        )),
    }
}

/// These two runtime-only model fields intentionally do not persist. Flatten
/// the ordinary model serialization rather than retyping its public fields.
#[derive(serde::Serialize)]
struct ObservableSession<'a> {
    #[serde(flatten)]
    model: &'a crate::model::AgentSession,
    subagent_runs: &'a [waku_client::subagent::SubagentRun],
    detail_loaded: bool,
}

fn observable_session(session: &crate::model::AgentSession) -> Value {
    json_of(&ObservableSession {
        model: session,
        subagent_runs: &session.subagent_runs,
        detail_loaded: session.detail_loaded,
    })
}

/// Dispatch one input request at the window level: resolve the target against
/// the current frame's tree and send the action, focus, keystrokes or text.
/// No entity lease is held here, because input can draw synchronously.
fn dispatch_rpa_input(
    window: &mut Window,
    app: &mut App,
    request: Request,
) -> Result<Value, Error> {
    match request {
        Request::Act { action, target } => {
            let (name, accessible) = match action {
                Action::Click => ("click", gpui::AccessibleAction::Click),
                Action::Focus => ("focus", gpui::AccessibleAction::Focus),
            };
            let node = resolve_target(
                window,
                &target,
                match action {
                    Action::Click => "Click",
                    Action::Focus => "Focus",
                },
            )?;
            window.dispatch_a11y_action(node, accessible, None, app);
            Ok(json!({ "acted": name, "node": node.0.to_string() }))
        }
        Request::Type { text, target } => {
            if let Some(target) = &target {
                let details = resolve_target_details(window, target)?;
                if !matches!(
                    details["role"].as_str(),
                    Some(
                        "TextInput"
                            | "MultilineTextInput"
                            | "SearchInput"
                            | "PasswordInput"
                            | "EditableComboBox"
                    )
                ) {
                    return Err(Error::new(
                        "not_actionable",
                        format!(
                            "{} is not an editable text control",
                            target_description(target)
                        ),
                    )
                    .with_target(target_description(target)));
                }
                let node = resolve_target(window, target, "Focus")?;
                window.dispatch_a11y_action(node, gpui::AccessibleAction::Focus, None, app);
                // The input handler is registered by paint. A newly focused
                // field must paint before the first character, also occluded.
                prepare_window(window, app, true);
            }
            let mut typed = 0usize;
            for character in text.chars() {
                let source = match character {
                    '\n' => "enter".to_owned(),
                    '\t' => "tab".to_owned(),
                    _ => character.to_string(),
                };
                let keystroke = gpui::Keystroke::parse(&source).map_err(|error| {
                    Error::new(
                        "invalid_type",
                        format!("`{character}` cannot be typed: {error}"),
                    )
                })?;
                window.dispatch_keystroke(keystroke, app);
                typed += 1;
            }
            Ok(json!({ "typed": typed }))
        }
        Request::Key { chord } => {
            if chord.trim().is_empty() {
                return Err(Error::new(
                    "invalid_chord",
                    "send at least one keyboard chord",
                ));
            }
            let mut sent = 0usize;
            for part in chord.split_whitespace() {
                match gpui::Keystroke::parse(part) {
                    Ok(keystroke) => {
                        window.dispatch_keystroke(keystroke, app);
                        sent += 1;
                    }
                    Err(error) => {
                        return Err(Error::new(
                            "invalid_chord",
                            format!("`{part}` is not a valid keystroke: {error}"),
                        ));
                    }
                }
            }
            Ok(json!({ "keys": sent }))
        }
        _ => Err(Error::new("internal", "not an input request")),
    }
}

/// Resolve a target against the current frame's tree. Prefers exact matches;
/// refuses ambiguity and unknown targets with the closest ids, so a typo
/// costs one call instead of a guessing loop.
fn resolve_target(
    window: &Window,
    target: &Target,
    action: &str,
) -> Result<gpui::accesskit::NodeId, Error> {
    let selected = resolve_target_details(window, target)?;
    if !selected["actions"]
        .as_array()
        .is_some_and(|actions| actions.iter().any(|known| known == action))
    {
        return Err(Error::new(
            "not_actionable",
            format!(
                "{} does not support {action}; supported actions: {}",
                target_description(target),
                selected["actions"]
            ),
        )
        .with_target(target_description(target)));
    }
    selected["node"]
        .as_str()
        .and_then(|value| value.parse::<u64>().ok())
        .map(gpui::accesskit::NodeId)
        .ok_or_else(|| {
            Error::new(
                "not_actionable",
                "the control has no node handle in this frame",
            )
            .with_target(target_description(target))
        })
}

fn target_description(target: &Target) -> String {
    match target {
        Target::Node(node) => format!("node {node}"),
        Target::Id(id) => format!("id `{id}`"),
    }
}

fn resolve_target_details(window: &Window, target: &Target) -> Result<Value, Error> {
    let raw = window.a11y_automation_snapshot().ok_or_else(|| {
        Error::new(
            "no_frame",
            "the window has not drawn an automation frame yet",
        )
    })?;
    let tree = serde_json::from_str::<Value>(&raw).map_err(|error| {
        Error::new(
            "invalid_tree",
            format!("the frame's tree is not valid JSON: {error}"),
        )
    })?;
    select_target(
        &tree,
        target,
        f32::from(window.viewport_size().width) as f64,
        f32::from(window.viewport_size().height) as f64,
    )
}

fn select_target(tree: &Value, target: &Target, width: f64, height: f64) -> Result<Value, Error> {
    let nodes = tree["nodes"]
        .as_array()
        .ok_or_else(|| Error::new("invalid_tree", "the frame tree has no nodes array"))?;
    let description = target_description(target);
    let found = nodes
        .iter()
        .filter(|node| match target {
            Target::Node(wanted) => node["node"].as_str() == Some(wanted.as_str()),
            Target::Id(wanted) => node["element_id"].as_str() == Some(wanted.as_str()),
        })
        .collect::<Vec<_>>();
    let selected =
        match found.as_slice() {
            [] => return Err(Error::new(
                "not_found",
                format!(
                    "no rendered control matches {description}; it may be off screen or virtualized"
                ),
            )
            .with_target(description)
            .with_nearest(nearest_ids(nodes, target))),
            [selected] => *selected,
            _ => {
                return Err(Error::new(
                    "ambiguous",
                    format!(
                        "{description} matches {} controls; use a node handle",
                        found.len()
                    ),
                )
                .with_target(description)
                .with_nearest(nearest_ids(nodes, target)));
            }
        };
    if selected["disabled"] == true {
        return Err(Error::new(
            "disabled",
            format!("{description} is disabled in the current frame"),
        )
        .with_target(description));
    }
    let bounds = &selected["bounds"];
    let x = bounds["x"].as_f64().unwrap_or(f64::NAN);
    let y = bounds["y"].as_f64().unwrap_or(f64::NAN);
    let w = bounds["width"].as_f64().unwrap_or(0.0);
    let h = bounds["height"].as_f64().unwrap_or(0.0);
    if selected["hidden"] == true
        || ![x, y, w, h].iter().all(|v| v.is_finite())
        || w <= 0.0
        || h <= 0.0
        || x + w / 2.0 >= width
        || y + h / 2.0 >= height
        || x + w / 2.0 < 0.0
        || y + h / 2.0 < 0.0
    {
        return Err(Error::new(
            "off_screen",
            format!("{description} has no visible hit area; scroll it into view first"),
        )
        .with_target(description));
    }
    Ok(selected.clone())
}

/// The closest-looking ids for an error message: ids sharing a token with the
/// wanted target first, then a sample of what is on screen.
fn nearest_ids(nodes: &[Value], target: &Target) -> Vec<String> {
    let wanted = match target {
        Target::Node(node) => node.as_str(),
        Target::Id(id) => id.as_str(),
    };
    let tokens: Vec<&str> = wanted
        .split(['.', '-', '_', ' '])
        .filter(|token| token.len() > 2)
        .collect();
    let mut ids: Vec<String> = nodes
        .iter()
        .filter_map(|node| node["element_id"].as_str().map(str::to_owned))
        .collect();
    ids.sort();
    ids.dedup();
    let mut scored: Vec<(usize, String)> = ids
        .into_iter()
        .map(|id| {
            let score = tokens.iter().filter(|token| id.contains(**token)).count();
            (score, id)
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().take(12).map(|(_, id)| id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_model_keeps_runtime_fields_without_retyping_persisted_fields() {
        let mut session =
            crate::model::AgentSession::new(uuid::Uuid::new_v4(), crate::model::ProviderKind::Pi);
        session.detail_loaded = true;
        session.subagent_runs = vec![waku_client::subagent::SubagentRun {
            id: "fan-out".into(),
            state: waku_client::subagent::SubagentState::Completed,
            ..Default::default()
        }];
        let model = json_of(&session);
        let observed = observable_session(&session);
        for (key, value) in model.as_object().unwrap() {
            assert_eq!(observed[key], *value, "{key}");
        }
        assert_eq!(observed["detail_loaded"], true);
        assert_eq!(observed["subagent_runs"][0]["state"], "completed");
        assert_eq!(observed["subagent_runs"][0]["id"], "fan-out");
    }

    #[test]
    fn issues_and_conversations_have_separate_identity_and_sections() {
        use crate::model::{AgentSession, ProviderKind, Task, TaskState, TaskWorkspace};
        let project = uuid::Uuid::new_v4();
        let mut task = Task::new("Issue with two conversations", TaskState::Backlog, 0);
        task.description = "Persist the issue independently of its conversations".into();
        task.workspace = Some(TaskWorkspace::NewWorktree {
            project_id: project,
        });
        let mut first = AgentSession::new(project, ProviderKind::Pi);
        first.task_id = Some(task.id);
        first.messages.push(crate::model::Message::new(
            crate::model::MessageRole::Assistant,
            "History belongs in session detail, not the conversation list",
        ));
        let mut second = AgentSession::new(project, ProviderKind::Pi);
        second.task_id = Some(task.id);
        let tasks = vec![task.clone()];
        let sessions = vec![first.clone(), second.clone()];
        assert_eq!(require_task(&tasks, task.id).unwrap().id, task.id);
        let error = require_task(&tasks, first.id).unwrap_err();
        assert_eq!(error.code, "not_found");
        assert!(error.message.contains("openSession"));

        let selected = vec!["tasks".into(), "sessions".into()];
        assert!(validate_sections(&selected).is_ok());
        let state = serialize_state_sections(&selected, |section| match section {
            "tasks" => json_of(&tasks),
            "sessions" => observable_session_list(&sessions),
            _ => panic!("unselected section must not serialize"),
        });
        assert_eq!(state["tasks"], json_of(&tasks));
        assert_eq!(state["tasks"][0]["id"], task.id.to_string());
        assert_eq!(state["tasks"][0]["workspace"]["kind"], "newWorktree");
        assert_eq!(state["sessions"].as_array().unwrap().len(), 2);
        assert_eq!(state["sessions"][0]["id"], first.id.to_string());
        assert_eq!(state["sessions"][1]["id"], second.id.to_string());
        for session in state["sessions"].as_array().unwrap() {
            assert_eq!(session["task_id"], task.id.to_string());
            assert!(session.get("messages").is_none());
            assert!(session.get("detailLoaded").is_some());
        }
        // Empty issues remain observable even when there are no conversations.
        assert_eq!(json_of(&tasks).as_array().unwrap().len(), 1);
        assert_eq!(observable_session_list(&[]), json!([]));
    }

    #[test]
    fn new_issue_title_is_optional_but_cannot_silently_discard_invalid_input() {
        assert_eq!(new_task_title(&json!({})).unwrap(), None);
        assert_eq!(
            new_task_title(&json!({"title":"  Empty issue  "})).unwrap(),
            Some("Empty issue".into())
        );
        for title in [Value::Null, json!(false), json!(42), json!(" ")] {
            assert_eq!(
                new_task_title(&json!({"title":title})).unwrap_err().code,
                "invalid_args"
            );
        }
    }

    #[test]
    fn semantic_targets_reject_disabled_hidden_ambiguous_and_missing_controls() {
        let node = json!({"node":"12", "element_id":"test.button", "role":"Button", "bounds":{"x":4,"y":5,"width":20,"height":10}, "actions":["Click"]});
        let target = Target::Id("test.button".into());
        assert_eq!(
            select_target(&json!({"nodes":[node.clone()]}), &target, 100.0, 100.0).unwrap(),
            node
        );
        for (field, value, code) in [
            ("disabled", json!(true), "disabled"),
            ("hidden", json!(true), "off_screen"),
            (
                "bounds",
                json!({"x":200,"y":5,"width":20,"height":10}),
                "off_screen",
            ),
        ] {
            let mut changed = node.clone();
            changed[field] = value;
            let error =
                select_target(&json!({"nodes":[changed]}), &target, 100.0, 100.0).unwrap_err();
            assert_eq!(error.code, code);
            assert!(error.target.is_some());
        }
        assert_eq!(
            select_target(&json!({"nodes":[node.clone(),node]}), &target, 100.0, 100.0)
                .unwrap_err()
                .code,
            "ambiguous"
        );
        let error = select_target(&json!({"nodes":[]}), &target, 100.0, 100.0).unwrap_err();
        assert_eq!(error.code, "not_found");
        assert!(error.message.contains("off screen"));
    }

    #[test]
    fn selectable_snapshot_sections_reject_silent_typos() {
        assert!(validate_sections(&[]).is_ok());
        assert!(
            validate_sections(&[
                "tree".into(),
                "model".into(),
                "task".into(),
                "session".into(),
                "sessions".into(),
                "board".into()
            ])
            .is_ok()
        );
        assert_eq!(
            validate_sections(&["sessoin".into()]).unwrap_err().code,
            "invalid_args"
        );
    }

    #[test]
    fn a_supervisor_transition_is_unknown_not_a_stale_connected_sample() {
        let health = transitioning_daemon_health();
        assert_eq!(health["state"], "transitioning");
        assert!(health["connected"].is_null());
        assert!(health["requestRoundTripMs"].is_null());
        assert!(
            health["error"]
                .as_str()
                .unwrap()
                .contains("temporarily unavailable")
        );
    }

    #[test]
    fn a_ready_but_dropped_connection_is_reported_as_disconnected() {
        let health = connected_daemon_health(true);
        assert_eq!(health["state"], "disconnected");
        assert_eq!(health["connected"], false);
        assert!(health["error"].as_str().unwrap().contains("dropped"));
        let connected = connected_daemon_health(false);
        assert_eq!(connected["state"], "connected");
        assert!(connected["error"].is_null());
    }

    #[cfg(debug_assertions)]
    #[gpui::test]
    async fn every_request_failure_carries_health_diff_and_counters(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.set_global(crate::daemon::DaemonState::Failed(
                "connection refused".into(),
            ))
        });
        let mut app = cx.to_async();
        for request in [
            Request::Describe,
            Request::Snapshot { sections: vec![] },
            Request::Act {
                action: Action::Click,
                target: Target::Id("missing".into()),
            },
            Request::Type {
                text: "hello".into(),
                target: None,
            },
            Request::Key {
                chord: "enter".into(),
            },
            Request::Invoke {
                verb: "unknown".into(),
                args: Value::Null,
            },
            Request::Wait {
                condition: Condition::Node {
                    id: "missing".into(),
                    present: true,
                },
                timeout_ms: 0,
            },
            Request::Logs {
                query: Default::default(),
            },
            Request::Counters,
            Request::Timeline {
                query: Default::default(),
            },
            Request::Verbosity { update: None },
        ] {
            let error = execute(&mut app, request).await.unwrap_err();
            let diagnostic = serde_json::to_string_pretty(&error).unwrap();
            let bundle = error
                .bundle
                .unwrap_or_else(|| panic!("no failure evidence: {diagnostic}"));
            assert_eq!(
                bundle.observed["health"]["daemon"]["state"], "failed",
                "{diagnostic}"
            );
            assert_eq!(
                bundle.observed["health"]["lastError"]["message"], "connection refused",
                "{diagnostic}"
            );
            assert!(!bundle.tree_diff.observed_available, "{diagnostic}");
            assert!(!bundle.counters.processes.is_empty(), "{diagnostic}");
            assert!(
                bundle.counters.processes[0]
                    .values
                    .contains_key("framesDrawn"),
                "{diagnostic}"
            );
            assert!(bundle.recent_events.is_none(), "{diagnostic}");
            assert!(!bundle.diagnostic_errors.is_empty(), "{diagnostic}");
        }
    }

    #[cfg(debug_assertions)]
    #[gpui::test]
    async fn timeout_and_rejected_calls_keep_the_same_failure_evidence(
        cx: &mut gpui::TestAppContext,
    ) {
        let mut app = cx.to_async();
        let condition = Condition::State {
            pointer: "/composer/text".into(),
            equals: json!("missing"),
        };
        for error in [
            wait_timeout(&condition, 0),
            Error::new("unknown_verb", "no such verb"),
            Error::new("invalid_call", "missing action"),
            Error::new("invalid_chord", "invalid keystroke"),
            Error::new("not_found", "missing button")
                .with_target("button")
                .with_nearest(vec!["send".into()]),
        ] {
            let code = error.code.clone();
            let error = with_failure_evidence(
                &mut app,
                error,
                json!({"tree": {"nodes": []}}),
                Default::default(),
                0,
            )
            .await;
            let diagnostic = serde_json::to_string_pretty(&error).unwrap();
            assert_eq!(error.code, code, "{diagnostic}");
            let bundle = error.bundle.expect("failure evidence");
            assert!(bundle.tree_diff.baseline_available, "{diagnostic}");
            assert_eq!(
                bundle.observed["health"]["workspace"], "unavailable",
                "{diagnostic}"
            );
            assert_eq!(bundle.counters.processes.len(), 1, "{diagnostic}");
        }
    }

    #[test]
    fn conditions_read_the_snapshot_shape() {
        let snapshot = json!({
            "state": { "composer": { "text": "hi" } },
            "tree": { "nodes": [
                { "element_id": "composer.send", "label": "Send" },
                { "element_id": "sidebar.task.7", "label": "Fix the thing" },
            ] },
        });
        assert!(condition_holds(
            &Condition::Node {
                id: "composer.send".into(),
                present: true
            },
            &snapshot
        ));
        assert!(!condition_holds(
            &Condition::Node {
                id: "composer.nope".into(),
                present: true
            },
            &snapshot
        ));
        assert!(condition_holds(
            &Condition::Text {
                id: "sidebar.task.7".into(),
                contains: "the thing".into()
            },
            &snapshot
        ));
        assert!(condition_holds(
            &Condition::State {
                pointer: "/composer/text".into(),
                equals: json!("hi")
            },
            &snapshot
        ));
        assert!(!condition_holds(
            &Condition::State {
                pointer: "/composer/text".into(),
                equals: json!("no")
            },
            &snapshot
        ));
    }

    #[test]
    fn nearest_ids_prefer_matching_tokens() {
        let nodes = vec![
            json!({ "element_id": "composer.send" }),
            json!({ "element_id": "composer.model-picker" }),
            json!({ "element_id": "sidebar.new-task" }),
        ];
        let nearest = nearest_ids(&nodes, &Target::Id("composer.submit".into()));
        assert_eq!(nearest[0], "composer.model-picker");
        assert_eq!(nearest[1], "composer.send");
    }
}
