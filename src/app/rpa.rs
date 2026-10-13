//! The workspace side of the RPA bridge.
//!
//! [`start_pump`] consumes calls from the local endpoint on the foreground
//! executor; each call observes or operates the workspace through the same
//! code paths the UI uses: actions dispatch accessibility actions on the
//! frame's tree, typing sends real keystrokes to the focused input, and
//! snapshots read the frame's tree plus live view state.

use std::time::{Duration, Instant};

use gpui::{App, AsyncApp, Context, Window};
use serde_json::{Value, json};

use super::Waku;
use crate::main_window::MainWindow;
use crate::rpa::protocol::{self, Action, Condition, Error, Reply, Request, Target};
use crate::rpa::service::Inbound;

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
            let outcome = execute(cx, call.request).await;
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
    let attached = crate::rpa::service::attached() > 0;
    for handle in app.windows() {
        let _ = handle.update(app, |_root, window, _app| set_automation(window, attached));
    }
}

async fn execute(cx: &mut AsyncApp, request: Request) -> Result<Value, Error> {
    match request {
        Request::Wait {
            condition,
            timeout_ms,
        } => wait_for(cx, &condition, timeout_ms).await,
        // Input reaches the window without an entity lease held: dispatching
        // a keystroke or a click can draw synchronously, and a draw renders
        // the workspace, which must not happen while the workspace is already
        // leased. Other requests only read state, which needs the lease.
        request @ (Request::Act { .. } | Request::Type { .. } | Request::Key { .. }) => {
            dispatch(cx, request)
        }
        Request::Snapshot => take_snapshot(cx),
        request => query(cx, request),
    }
}

/// A request served against the workspace: describe, invoke, or the read side
/// of a snapshot. Taking the workspace lease is safe here because none of
/// these dispatches input.
fn query(cx: &mut AsyncApp, request: Request) -> Result<Value, Error> {
    let draws = matches!(request, Request::Snapshot);
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

fn take_snapshot(cx: &mut AsyncApp) -> Result<Value, Error> {
    cx.update(|app| {
        with_workspace(app, true, |waku, window, cx| waku.rpa_snapshot(window, cx))
            .unwrap_or_else(|| Err(app_not_ready()))
    })
}

fn current_state(cx: &mut AsyncApp) -> Option<Value> {
    cx.update(|app| with_workspace(app, false, |waku, _window, cx| waku.rpa_state(cx)))
}

async fn wait_for(
    cx: &mut AsyncApp,
    condition: &Condition,
    timeout_ms: u64,
) -> Result<Value, Error> {
    let started = Instant::now();
    let timeout = Duration::from_millis(timeout_ms.max(1));
    let needs_frame = matches!(condition, Condition::Node { .. } | Condition::Text { .. });
    loop {
        let observed = cx.update(|app| {
            with_workspace(app, needs_frame, |waku, window, cx| {
                let snapshot = waku.rpa_snapshot(window, cx)?;
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
            let state = current_state(cx);
            let mut error = Error::new(
                "timeout",
                format!(
                    "timed out after {} ms waiting for {}",
                    timeout_ms,
                    condition.describe()
                ),
            );
            if let Some(state) = state {
                error = error.with_state(state);
            }
            return Err(error);
        }
        cx.background_executor()
            .timer(protocol::WAIT_POLL_INTERVAL)
            .await;
    }
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
    let attached = crate::rpa::service::attached() > 0;
    set_automation(window, attached);
    if attached && draw {
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
        .map(Vec::as_slice)
        .unwrap_or_default()
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
                "instance": json_of(crate::rpa::service::discovery()),
                "attachedClients": crate::rpa::service::attached(),
                "automation": window.is_a11y_automation_enabled(),
                "treeReady": window.a11y_automation_snapshot().is_some(),
                "state": self.rpa_state(cx),
            })),
            Request::Snapshot => self.rpa_snapshot(window, cx),
            Request::Invoke { verb, args } => self.rpa_invoke(&verb, &args, window, cx),
            Request::Act { .. }
            | Request::Type { .. }
            | Request::Key { .. }
            | Request::Wait { .. } => {
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
                self.select_session(id, cx);
                Ok(json!({ "opened": id }))
            }
            "selectProject" => {
                let id = uuid("id")?;
                self.select_project(id, cx);
                Ok(json!({ "selected": id }))
            }
            "newTask" => {
                self.new_session_action(&crate::NewSession, window, cx);
                Ok(json!({ "opened": "new" }))
            }
            other => Err(Error::new(
                "unknown_verb",
                format!("no verb `{other}`; known verbs: openTask, selectProject, newTask"),
            )),
        }
    }

    /// The current frame's tree plus the app's observable state.
    pub(crate) fn rpa_snapshot(&self, window: &Window, cx: &App) -> Result<Value, Error> {
        let tree = match window.a11y_automation_snapshot() {
            Some(text) => serde_json::from_str::<Value>(&text).map_err(|error| {
                Error::new(
                    "invalid_tree",
                    format!("the frame's tree is not valid JSON: {error}"),
                )
            })?,
            None => Value::Null,
        };
        Ok(json!({
            "instance": json_of(crate::rpa::service::discovery()),
            "window": {
                "title": crate::instance::window_title(),
                "width": f32::from(window.viewport_size().width),
                "height": f32::from(window.viewport_size().height),
                "focused": window.is_window_active(),
            },
            "state": self.rpa_state(cx),
            "tree": tree,
        }))
    }

    /// The app-level state section of a snapshot, derived from live view
    /// state: nothing here is cached or read from the database.
    pub(crate) fn rpa_state(&self, cx: &App) -> Value {
        let selected = self.state.selected_session;
        let tasks = self
            .state
            .sessions
            .iter()
            .map(|session| {
                json!({
                    "id": session.id,
                    "title": session.title,
                    "autoTitle": session.auto_title,
                    "status": json_of(&session.status),
                    "provider": json_of(&session.provider),
                    "projectId": session.project_id,
                    "updatedAt": session.updated_at,
                    "active": Some(session.id) == selected,
                })
            })
            .collect::<Vec<_>>();
        let session = self.selected_session().map(|session| {
            json!({
                "id": session.id,
                "title": session.title,
                "status": json_of(&session.status),
                "provider": json_of(&session.provider),
                "model": session.model,
                "detailLoaded": session.detail_loaded,
                "messages": session.messages.iter().rev().take(20).collect::<Vec<_>>().into_iter().rev().map(|message| json!({
                    "id": message.id,
                    "turnId": message.turn_id,
                    "role": json_of(&message.role),
                    "text": message.display_content.as_deref().unwrap_or(&message.content),
                })).collect::<Vec<_>>(),
                "subagentRuns": session.subagent_runs.iter().map(|run| json!({
                    "id": run.id,
                    "label": run.label,
                    "state": json_of(&run.state),
                    "detached": run.detached,
                    "startedAtMs": run.started_at_ms,
                    "activity": run.activity.as_ref().map(json_of),
                    "children": run.children.iter().map(|child| json!({
                        "id": child.id,
                        "label": child.label,
                        "state": json_of(&child.state),
                        "error": child.error,
                        "children": child.children.len(),
                    })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
                "turns": session.turns.iter().rev().take(4).collect::<Vec<_>>().into_iter().rev().map(|turn| json!({
                    "id": turn.id,
                    "status": json_of(&turn.status),
                    "startedAt": turn.started_at,
                    "completedAt": turn.completed_at,
                })).collect::<Vec<_>>(),
            })
        });
        let toast = self.toast.as_ref().map(|toast| {
            json!({
                "message": toast.message,
                "tone": format!("{:?}", toast.tone),
            })
        });
        json!({
            "route": {
                "surface": if self.settings_page.is_some() { "settings" } else { "waku" },
                "settingsPage": self.settings_page.map(|page| format!("{page:?}")),
            },
            "projects": self.state.projects.iter().map(|project| json!({
                "id": project.id,
                "name": project.display_name(),
                "path": project.path.display().to_string(),
                "selected": Some(project.id) == self.state.selected_project,
            })).collect::<Vec<_>>(),
            "tasks": tasks,
            "session": session,
            "composer": {
                "text": self.composer.read(cx).content(cx),
            },
            "toast": toast,
            "panels": {
                "sidebar": self.state.sidebar_visible,
                "rightPanel": self.state.right_panel_visible,
            },
            "ui": {
                "sidebarRowCount": self.sidebar_list_state.item_count(),
                "sidebarRows": self.sidebar_row_cache.borrow().len(),
                "sidebarViewport": f32::from(self.sidebar_list_state.viewport_bounds().size.height),
            },
        })
    }
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
            let node = resolve_target(window, &target)?;
            let (name, accessible) = match action {
                Action::Click => ("click", gpui::AccessibleAction::Click),
                Action::Focus => ("focus", gpui::AccessibleAction::Focus),
            };
            window.dispatch_a11y_action(node, accessible, None, app);
            Ok(json!({ "acted": name, "node": node.0.to_string() }))
        }
        Request::Type { text, target } => {
            if let Some(target) = &target {
                let node = resolve_target(window, target)?;
                window.dispatch_a11y_action(node, gpui::AccessibleAction::Focus, None, app);
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
fn resolve_target(window: &Window, target: &Target) -> Result<gpui::accesskit::NodeId, Error> {
    // The window hands back the tree itself (`{ frame, nodes, ... }`), not a
    // full snapshot; read it directly rather than looking for a `tree` field.
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
    let nodes: &[Value] = tree["nodes"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let description = match target {
        Target::Node(node) => format!("node {node}"),
        Target::Id(id) => format!("id `{id}`"),
    };
    let matches = |node: &Value| match target {
        Target::Node(wanted) => node["node"].as_str() == Some(wanted.as_str()),
        Target::Id(wanted) => node["element_id"].as_str() == Some(wanted.as_str()),
    };
    let found: Vec<&Value> = nodes.iter().filter(|node| matches(node)).collect();
    let selected = match found.len() {
        0 => {
            return Err(
                Error::new("not_found", format!("no control matches {description}"))
                    .with_target(description)
                    .with_nearest(nearest_ids(nodes, target))
                    .with_state(json!({ "tree": tree })),
            );
        }
        1 => found[0],
        _ => {
            return Err(Error::new(
                "ambiguous",
                format!("{description} matches {} controls", found.len()),
            )
            .with_target(description)
            .with_nearest(nearest_ids(nodes, target)));
        }
    };
    selected["node"]
        .as_str()
        .and_then(|value| value.parse::<u64>().ok())
        .map(gpui::accesskit::NodeId)
        .ok_or_else(|| {
            Error::new(
                "not_actionable",
                format!("{description} has no node handle in this frame"),
            )
        })
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
