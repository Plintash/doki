//! Decoding `pi-subagents`' structured progress reports.
//!
//! The extension reports a run in two places, and both are structured:
//!
//! * the `subagent` tool result's `details` — and the partial results it
//!   streams while the call is open — for a foreground run, with one entry per
//!   child (or a `workflowChildren` roster for a workflow);
//! * the `subagent-async` widget, which in RPC mode is a single JSON line
//!   describing the detached runs that outlive the tool call.
//!
//! Decoding both here, once, keeps the UI frame path free of provider JSON.
//! Nothing here is lossless on purpose: only what the composer shows crosses
//! into [`SubagentRun`], and the provider's own caps bound the strings and
//! lists it carries.

use serde_json::Value;
use waku_protocol::subagent::{
    SubagentActivity, SubagentChild, SubagentFleet, SubagentRun, SubagentState,
};

/// The key `pi-subagents` publishes its async status widget under in RPC
/// mode, and the prefix of the one JSON line it carries. The body is a
/// snapshot of the runs the extension still holds, which is what makes the
/// widget a level signal a client can read instead of prose.
pub(super) const ASYNC_WIDGET_KEY: &str = "subagent-async";
pub(super) const ASYNC_WIDGET_PREFIX: &str = "PI_SUBAGENT_ASYNC_JSON:";
/// One async-status widget report: the decoded runs, whether the snapshot
/// named every run the extension holds (`complete` is false when its
/// `omitted.runs` count left some out, because a run this report does not
/// name is then not necessarily one that left), and how many children its
/// own caps left out across every run.
pub(super) struct AsyncReport {
    pub(super) fleet: SubagentFleet,
    pub(super) complete: bool,
    pub(super) omitted_children: usize,
}

/// A detached fleet from the extension's async widget.
///
/// `None` means the widget's lines are not that snapshot and the caller
/// should treat it as an ordinary widget. Missing lines are the extension's
/// own clear — it takes the widget down when it holds no run worth showing —
/// so they decode as an empty, complete fleet.
pub(super) fn async_widget_report(lines: Option<&[String]>) -> Option<AsyncReport> {
    let Some(line) = lines.and_then(<[String]>::first) else {
        return Some(AsyncReport {
            fleet: SubagentFleet::default(),
            complete: true,
            omitted_children: 0,
        });
    };
    let snapshot: Value = serde_json::from_str(line.strip_prefix(ASYNC_WIDGET_PREFIX)?).ok()?;
    let runs = snapshot.get("runs")?.as_array()?;
    Some(AsyncReport {
        fleet: SubagentFleet {
            runs: runs.iter().filter_map(async_run).collect(),
        },
        complete: snapshot
            .pointer("/omitted/runs")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            == 0,
        omitted_children: snapshot
            .pointer("/omitted/children")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize,
    })
}

fn async_run(node: &Value) -> Option<SubagentRun> {
    // A run is a detached child: the same node shape, one flag apart, with
    // no failure of its own to report.
    let child = async_child(node)?;
    Some(SubagentRun {
        id: child.id,
        label: child.label,
        state: child.state,
        detached: true,
        started_at_ms: child.started_at_ms,
        activity: child.activity,
        children: child.children,
    })
}

fn async_children(children: Option<&Value>) -> Vec<SubagentChild> {
    children
        .and_then(Value::as_array)
        .map(|children| children.iter().filter_map(async_child).collect())
        .unwrap_or_default()
}

fn async_child(node: &Value) -> Option<SubagentChild> {
    let id = text(node.get("id"))?;
    Some(SubagentChild {
        label: text(node.get("label")).unwrap_or_else(|| id.clone()),
        id,
        state: state(node.get("state")),
        started_at_ms: timestamp(node.get("startedAt")),
        activity: node.get("activity").and_then(counters),
        error: None,
        children: async_children(node.get("children")),
    })
}

/// The live progress one `subagent` tool call reports.
///
/// `None` means these details are not a foreground run's report: an unknown
/// mode, or the launch receipt of a detached run — its live report is the
/// async widget, and showing the receipt too would double the row.
pub(super) fn run_from_tool_details(
    details: &Value,
    tool_call_id: &str,
    now_ms: i64,
) -> Option<SubagentRun> {
    let mode = details.get("mode").and_then(Value::as_str)?;
    if !matches!(mode, "single" | "parallel" | "chain" | "workflow") {
        return None;
    }
    // The async receipts name their detached run by `asyncId`; a foreground
    // report never carries one.
    if details.get("asyncId").is_some() {
        return None;
    }
    let mut children = details
        .get("results")
        .and_then(Value::as_array)
        .map(|results| result_children(results, now_ms))
        .unwrap_or_default();
    if children.is_empty() && mode == "workflow" {
        children = workflow_children(details.get("workflowChildren"), now_ms);
    }
    let label = text(details.get("label"))
        .or_else(|| (children.len() == 1).then(|| children[0].label.clone()))
        // The modes name how a launch is wired, not what it is called; a run
        // with nothing else to call it gets the surface's own word.
        .unwrap_or_else(|| tr!("subagent.title"));
    Some(SubagentRun {
        id: text(details.get("runId")).unwrap_or_else(|| tool_call_id.to_owned()),
        label,
        state: run_state(mode, details, &children),
        detached: false,
        started_at_ms: children
            .iter()
            .filter_map(|child| child.started_at_ms)
            .min(),
        activity: None,
        children,
    })
}

fn result_children(results: &[Value], now_ms: i64) -> Vec<SubagentChild> {
    results
        .iter()
        .enumerate()
        .filter_map(|(position, result)| result_child(result, position, now_ms))
        .collect()
}

fn result_child(result: &Value, position: usize, now_ms: i64) -> Option<SubagentChild> {
    let progress = result
        .get("progress")
        .filter(|progress| !progress.is_null());
    let label = agent_label(result).or_else(|| text(result.get("workflowKey")))?;
    let index = count(result.get("index")).unwrap_or(position as u32);
    let id = text(result.get("runId"))
        .or_else(|| text(result.get("workflowKey")))
        // The provider's index is the child's stable identity within the run,
        // so a result that named no run still keeps its own start instant.
        .unwrap_or_else(|| format!("child:{index}"));
    let state = progress
        .and_then(|progress| progress.get("status"))
        .map(|status| state(Some(status)))
        .unwrap_or_else(|| result_state(result));
    Some(SubagentChild {
        id,
        label,
        state,
        started_at_ms: elapsed_start_ms(progress, now_ms),
        activity: progress.and_then(progress_activity),
        error: text(result.get("error")),
        children: nested_children(result.get("children")),
    })
}

fn nested_children(children: Option<&Value>) -> Vec<SubagentChild> {
    children
        .and_then(Value::as_array)
        .map(|children| children.iter().filter_map(nested_child).collect())
        .unwrap_or_default()
}

fn nested_child(node: &Value) -> Option<SubagentChild> {
    let id = text(node.get("id"))?;
    let label = agent_label(node).unwrap_or_else(|| id.clone());
    let state = match node.get("state") {
        Some(state) => state_from_text(state.as_str().unwrap_or_default()),
        None => result_state(node),
    };
    Some(SubagentChild {
        id,
        label,
        state,
        started_at_ms: timestamp(node.get("startedAt")),
        activity: counters(node),
        error: text(node.get("error")),
        children: nested_children(node.get("children")),
    })
}

fn workflow_children(summary: Option<&Value>, now_ms: i64) -> Vec<SubagentChild> {
    summary
        .and_then(|summary| summary.get("children"))
        .and_then(Value::as_array)
        .map(|children| {
            children
                .iter()
                .filter_map(|node| {
                    let id = text(node.get("childId")).or_else(|| text(node.get("runId")))?;
                    let label = agent_label(node).unwrap_or_else(|| id.clone());
                    let activity = node.get("activity");
                    Some(SubagentChild {
                        id,
                        label,
                        state: state(node.get("state")),
                        // A workflow child's duration is its age so far; the
                        // row shows how long it has been working.
                        started_at_ms: activity
                            .and_then(|activity| elapsed_start_ms(Some(activity), now_ms)),
                        activity: activity.and_then(counters),
                        error: None,
                        children: Vec::new(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The activity one `AgentProgress` reports: the tool that is open, or the
/// newest line of output when none is.
fn progress_activity(progress: &Value) -> Option<SubagentActivity> {
    let activity = SubagentActivity {
        current_tool: text(progress.get("currentTool")),
        current_tool_args: text(progress.get("currentToolArgs")),
        last_line: progress
            .get("recentOutput")
            .and_then(Value::as_array)
            .and_then(|lines| lines.iter().rev().find_map(Value::as_str))
            .map(str::to_owned),
    };
    (activity != SubagentActivity::default()).then_some(activity)
}

/// The activity every other surface shares: just the open tool. The snapshot
/// and the workflow roster carry no arguments or output lines; `AgentProgress`
/// adds those in [`progress_activity`].
fn counters(value: &Value) -> Option<SubagentActivity> {
    text(value.get("currentTool")).map(|current_tool| SubagentActivity {
        current_tool: Some(current_tool),
        ..SubagentActivity::default()
    })
}

/// The run's own state: what the workflow roster says, or what its children
/// together imply. A child that is still working outranks any settled
/// sibling, and a failure outranks a clean settle.
fn run_state(mode: &str, details: &Value, children: &[SubagentChild]) -> SubagentState {
    if children.iter().any(SubagentChild::is_live) {
        return SubagentState::Running;
    }
    if mode == "workflow"
        && let Some(workflow_state) = details
            .get("workflowChildren")
            .and_then(|summary| summary.get("workflowState"))
    {
        return state(Some(workflow_state));
    }
    if children
        .iter()
        .any(|child| child.state == SubagentState::Failed)
    {
        return SubagentState::Failed;
    }
    if children
        .iter()
        .any(|child| matches!(child.state, SubagentState::Stopped | SubagentState::Paused))
    {
        return SubagentState::Stopped;
    }
    if !children.is_empty() {
        return SubagentState::Completed;
    }
    if details.get("stopped").and_then(Value::as_bool) == Some(true) {
        return SubagentState::Stopped;
    }
    if details.get("timedOut").and_then(Value::as_bool) == Some(true) {
        return SubagentState::Failed;
    }
    SubagentState::Running
}

/// A result with no `progress` still carries its outcome.
fn result_state(result: &Value) -> SubagentState {
    if result.get("detached").and_then(Value::as_bool) == Some(true) {
        return SubagentState::Detached;
    }
    if result.get("stopped").and_then(Value::as_bool) == Some(true)
        || result.get("interrupted").and_then(Value::as_bool) == Some(true)
    {
        return SubagentState::Stopped;
    }
    if result.get("timedOut").and_then(Value::as_bool) == Some(true) {
        return SubagentState::Failed;
    }
    match result.get("exitCode").and_then(Value::as_i64) {
        Some(0) => SubagentState::Completed,
        Some(_) => SubagentState::Failed,
        None => SubagentState::Running,
    }
}

/// The provider words a settled run both `complete` and `completed`, and a
/// waiting one both `queued` and `pending`; anything unknown keeps the run
/// live, because acting on an unknown outcome is worse than waiting for a
/// known one.
fn state(value: Option<&Value>) -> SubagentState {
    state_from_text(value.and_then(Value::as_str).unwrap_or_default())
}

fn state_from_text(value: &str) -> SubagentState {
    match value {
        "queued" | "pending" => SubagentState::Queued,
        "running" => SubagentState::Running,
        "completed" | "complete" => SubagentState::Completed,
        "failed" | "rejected" => SubagentState::Failed,
        "stopped" => SubagentState::Stopped,
        "paused" => SubagentState::Paused,
        "partial" => SubagentState::Partial,
        "detached" => SubagentState::Detached,
        // A state a later version words differently keeps the run live, the
        // way an unknown child outcome does.
        _ => SubagentState::Running,
    }
}

/// A live report's `durationMs` converted to a start instant, so a surface
/// keeps counting between updates instead of freezing at the last report.
fn elapsed_start_ms(progress: Option<&Value>, now_ms: i64) -> Option<i64> {
    let duration = progress?.get("durationMs")?.as_i64()?;
    Some(now_ms.saturating_sub(duration))
}

/// The name a child is shown by: its agent, or the session name the launcher
/// derived for it.
fn agent_label(node: &Value) -> Option<String> {
    text(node.get("agent")).or_else(|| text(node.get("sessionName")))
}

fn text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn count(value: Option<&Value>) -> Option<u32> {
    u32::try_from(value?.as_u64()?).ok()
}

fn timestamp(value: Option<&Value>) -> Option<i64> {
    value?.as_i64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A foreground workflow's live report is a `workflowChildren` roster
    /// rather than a `progress` array; its children carry the activity the
    /// composer shows, and a child that outlived its call keeps the detached
    /// state the roster words it with.
    #[test]
    fn a_workflow_roster_names_the_children_the_composer_shows() {
        let details = json!({
            "mode": "workflow",
            "runId": "run-1",
            "results": [],
            "workflowChildren": {
                "version": 1,
                "parentToolCallId": "tool-1",
                "workflowRunId": "run-1",
                "inventoryComplete": false,
                "workflowState": "running",
                "children": [
                    {"childId": "l01", "runId": "child-1", "agent": "code-auditor",
                     "sessionName": "code-auditor · review diff", "state": "running",
                     "activity": {"currentTool": "Edit", "durationMs": 5_000}},
                    {"childId": "l02", "agent": "test-runner", "state": "completed"},
                    {"childId": "l03", "agent": "legacy-scout", "state": "detached"},
                    // The roster always names its children; one without an id
                    // is not a row the card can track.
                    {"agent": "anonymous"}
                ]
            }
        });

        let run = run_from_tool_details(&details, "tool-1", 10_000).expect("a workflow run");
        assert_eq!(run.id, "run-1");
        assert!(!run.detached);
        assert_eq!(run.state, SubagentState::Running);
        assert_eq!(run.children.len(), 3);
        assert_eq!(run.children[0].label, "code-auditor");
        assert_eq!(run.children[0].started_at_ms, Some(5_000));
        assert_eq!(
            run.children[0]
                .activity
                .as_ref()
                .and_then(|activity| activity.current_tool.as_deref()),
            Some("Edit")
        );
        assert_eq!(run.children[1].state, SubagentState::Completed);
        assert!(
            !run.children[2].is_live(),
            "a detached child's report is the async widget's run"
        );
    }

    /// The snapshot's `runs` shape is the only thing that distinguishes it;
    /// a line without it is a widget the caller should show as text.
    #[test]
    fn a_widget_line_without_the_snapshot_shape_is_not_decoded() {
        let line = format!("{ASYNC_WIDGET_PREFIX}not json");
        assert!(async_widget_report(Some(&[line])).is_none());
    }

    /// A partial report that left runs out is not a complete list.
    #[test]
    fn an_omitted_run_makes_the_report_incomplete() {
        let line = format!(
            "{ASYNC_WIDGET_PREFIX}{}",
            json!({
                "kind": "pi-subagents.async-status-snapshot",
                "runs": [{"id": "run-1", "label": "scout", "state": "running"}],
                "omitted": {"runs": 2},
            })
        );
        let report = async_widget_report(Some(&[line])).expect("the snapshot");
        assert!(!report.complete);
        assert_eq!(report.fleet.runs.len(), 1);
    }
}
