//! A provider's live subagent runs, decoded for the composer.
//!
//! `pi-subagents` reports a run's progress in two structured places: the
//! `subagent` tool result's `details` while a foreground run streams, and the
//! `subagent-async` widget — a JSON snapshot in RPC mode — for the detached
//! runs that outlive the tool call. Both decode into this shape so a client
//! renders the run's children and what each is doing instead of a line of
//! provider JSON.
//!
//! Only what the surface shows is kept: a run's own state, its children's
//! states and live activity, and when each started. Everything here is live
//! provider state, not conversation state.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// The state `pi-subagents` reports for a run or one of its children.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum SubagentState {
    Queued,
    #[default]
    Running,
    Completed,
    Failed,
    Stopped,
    Paused,
    Partial,
    Detached,
}

impl SubagentState {
    /// Whether the state still expects the run to make progress on its own.
    /// Detached is not live on purpose: the child outlived its tool call, and
    /// its report is the async widget's own run from then on.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

/// What a run or child is doing right now, when the provider says.
///
/// A report names the open tool with what it was called on, or the newest
/// line of the child's own output; each is optional, and a surface falls back
/// to the state label when neither is known.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SubagentActivity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_tool_args: Option<String>,
    /// The newest line of the child's own output, for a child with no tool
    /// open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_line: Option<String>,
}

/// One child of a run: the unit a reader watches while a fan-out runs.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SubagentChild {
    /// The child's run id, workflow key, or launch index — whatever the
    /// provider uses to name this child within its run.
    pub id: String,
    pub label: String,
    pub state: SubagentState,
    /// When the child started, so a surface can show how long it has run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<SubagentActivity>,
    /// The child's failure, when it has one worth naming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Nested children, for a workflow child that launched its own runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<SubagentChild>,
}

impl SubagentChild {
    /// Whether the child or any of its descendants still expects progress.
    pub fn is_live(&self) -> bool {
        self.state.is_live() || self.children.iter().any(Self::is_live)
    }
}

/// One `pi-subagents` run: a launch that is running or has just settled.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SubagentRun {
    /// The provider's run id. A foreground run uses the tool call it streams
    /// through, so a client can replace one update with the next.
    pub id: String,
    /// The run's display name: an agent name, the agents of a multi-child
    /// launch, or a workflow's own label.
    pub label: String,
    pub state: SubagentState,
    /// Whether the run outlives the tool call that launched it. A detached
    /// run's report is the extension's async widget — the level snapshot a
    /// client replaces wholesale — not the tool call's stream.
    pub detached: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<SubagentActivity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<SubagentChild>,
}

impl SubagentRun {
    /// Whether the run or any of its children still expects progress.
    pub fn is_live(&self) -> bool {
        self.state.is_live() || self.children.iter().any(SubagentChild::is_live)
    }
}

/// The complete set of runs the provider still reports for one session.
///
/// A detached snapshot is a level signal: each report replaces the detached
/// runs it describes, and runs that left it are gone.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SubagentFleet {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runs: Vec<SubagentRun>,
}
