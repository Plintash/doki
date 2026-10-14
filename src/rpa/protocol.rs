//! The debug RPA wire protocol.
//!
//! One JSON object per line over a loopback TCP connection. Every call
//! carries the protocol version and the instance token the app published;
//! every reply is correlated by the call's id. The surface stays deliberately
//! small — observation, semantic action, and waiting — so a client closes an
//! interaction without guessing coordinates or sleeping.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped whenever the shape of [`Request`] or the error contract changes.
/// Both ends refuse a mismatch instead of guessing.
pub const PROTOCOL_VERSION: u32 = 3;

pub const DEFAULT_WAIT_TIMEOUT_MS: u64 = 10_000;
/// How often a wait re-reads the app. One display frame, so a condition that
/// just became true is observed on the next poll.
pub const WAIT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Instance identity: which checkout, database, build and process answered.
    Describe,
    /// The current frame's semantic tree plus the app's observable state.
    Snapshot,
    /// Read the instance's merged diagnostic stream from its daemon.
    Logs {
        query: waku_client::DiagnosticQuery,
    },
    Timeline {
        query: waku_client::DiagnosticTimelineQuery,
    },
    Counters,
    Verbosity {
        update: Option<waku_client::DiagnosticVerbosity>,
    },
    /// One semantic action on one control.
    Act {
        action: Action,
        target: Target,
    },
    /// Focus a target (when given) and type text one character at a time.
    Type {
        text: String,
        #[serde(default)]
        target: Option<Target>,
    },
    /// Send a chord like `secondary-l` or `cmd-shift-p`, space separated for
    /// a sequence.
    Key {
        chord: String,
    },
    /// A workspace verb: `openTask`, `selectProject`, `newTask`. Verbs call
    /// the same app behavior their controls do.
    Invoke {
        verb: String,
        #[serde(default)]
        args: Value,
    },
    /// Wait for a condition to become true, observing the app rather than
    /// sleeping. Fails with the last observed state on timeout.
    Wait {
        condition: Condition,
        #[serde(default = "default_wait_timeout")]
        timeout_ms: u64,
    },
}

fn default_wait_timeout() -> u64 {
    DEFAULT_WAIT_TIMEOUT_MS
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Click,
    Focus,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Node(String),
    Id(String),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "on", rename_all = "snake_case")]
pub enum Condition {
    /// A control with this id is present (the default) or absent.
    Node {
        id: String,
        #[serde(default = "default_true")]
        present: bool,
    },
    /// A control with this id has a label or value containing the text.
    Text { id: String, contains: String },
    /// A JSON pointer into the snapshot's `state` equals the value.
    State { pointer: String, equals: Value },
    /// A JSON pointer into the snapshot's `state` contains the text (strings
    /// or arrays of strings).
    StateContains { pointer: String, contains: String },
}

impl Condition {
    pub fn describe(&self) -> String {
        match self {
            Condition::Node { id, present } => {
                if *present {
                    format!("node `{id}` present")
                } else {
                    format!("node `{id}` absent")
                }
            }
            Condition::Text { id, contains } => format!("node `{id}` contains {contains:?}"),
            Condition::State { pointer, equals } => {
                format!("state {pointer} == {equals}")
            }
            Condition::StateContains { pointer, contains } => {
                format!("state {pointer} contains {contains:?}")
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Error {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nearest: Option<Vec<String>>,
    /// The snapshot (or snapshot section) observed when the call failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<Value>,
    /// Evidence captured after a failed, authenticated call. Authentication
    /// errors never expose the app's state or diagnostics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle: Option<FailureBundle>,
}

impl Error {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            target: None,
            nearest: None,
            state: None,
            bundle: None,
        }
    }

    pub fn with_target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }

    pub fn with_nearest(mut self, nearest: Vec<String>) -> Self {
        self.nearest = Some(nearest);
        self
    }

    pub fn with_state(mut self, state: Value) -> Self {
        self.state = Some(state);
        self
    }
    pub fn with_bundle(mut self, bundle: FailureBundle) -> Self {
        self.state = Some(bundle.observed["state"].clone());
        self.bundle = Some(bundle);
        self
    }
}

/// One failure's evidence. Missing daemon data is explicit, and the app's
/// own counters and observed health survive a disconnected or hung daemon.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureBundle {
    pub observed: Value,
    pub tree_diff: TreeDiff,
    pub recent_events: Option<waku_client::DiagnosticLogs>,
    pub counters: waku_client::DiagnosticCounters,
    pub diagnostic_errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeDiff {
    pub baseline_available: bool,
    pub observed_available: bool,
    pub before_frame: Value,
    pub after_frame: Value,
    pub focus_before: Value,
    pub focus_after: Value,
    pub added: Vec<Value>,
    pub removed: Vec<Value>,
    pub changed: Vec<NodeChange>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeChange {
    pub before: Value,
    pub after: Value,
}

impl TreeDiff {
    /// Compare the two actual frames. Stable semantic ids take precedence
    /// over frame-local handles, and duplicate ids are retained individually.
    /// A missing frame is named, never represented as an empty observed tree.
    pub fn between(before: &Value, after: &Value) -> Self {
        let before_nodes = before["nodes"].as_array();
        let after_nodes = after["nodes"].as_array();
        let mut diff = Self {
            baseline_available: before_nodes.is_some(),
            observed_available: after_nodes.is_some(),
            before_frame: before["frame"].clone(),
            after_frame: after["frame"].clone(),
            focus_before: before["focus"].clone(),
            focus_after: after["focus"].clone(),
            added: Vec::new(),
            removed: Vec::new(),
            changed: Vec::new(),
        };
        if let (Some(before_nodes), Some(after_nodes)) = (before_nodes, after_nodes) {
            let before = indexed_nodes(before_nodes);
            let after = indexed_nodes(after_nodes);
            for (id, node) in &after {
                match before.get(id) {
                    None => diff.added.push((*node).clone()),
                    Some(previous) if *previous != *node => diff.changed.push(NodeChange {
                        before: (*previous).clone(),
                        after: (*node).clone(),
                    }),
                    _ => {}
                }
            }
            for (id, node) in &before {
                if !after.contains_key(id) {
                    diff.removed.push((*node).clone());
                }
            }
        }
        diff
    }
}

fn indexed_nodes(nodes: &[Value]) -> BTreeMap<(String, usize), &Value> {
    let mut occurrences = BTreeMap::<String, usize>::new();
    nodes
        .iter()
        .enumerate()
        .map(|(index, node)| {
            let id = node["element_id"]
                .as_str()
                .map(|id| format!("id:{id}"))
                .or_else(|| node["node"].as_str().map(|id| format!("node:{id}")))
                .unwrap_or_else(|| format!("index:{index}"));
            let occurrence = occurrences.entry(id.clone()).or_default();
            let key = (id, *occurrence);
            *occurrence += 1;
            (key, node)
        })
        .collect()
}

#[derive(Clone, Debug, Serialize)]
pub struct Reply {
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

impl Reply {
    pub fn ok(id: u64, result: Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: u64, error: Error) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tree_diff_names_added_removed_changed_and_focus() {
        let before = json!({"frame": 3, "focus": "1", "nodes": [
            {"element_id": "send", "node": "1", "label": "Send"},
            {"element_id": "gone", "node": "2"},
            {"element_id": "same", "node": "3"},
        ]});
        let after = json!({"frame": 4, "focus": "4", "nodes": [
            {"element_id": "send", "node": "1", "label": "Stop"},
            {"element_id": "new", "node": "4"},
            {"element_id": "same", "node": "3"},
        ]});
        let diff = TreeDiff::between(&before, &after);
        assert_eq!(diff.before_frame, 3);
        assert_eq!(diff.after_frame, 4);
        assert_eq!(diff.focus_before, "1");
        assert_eq!(diff.focus_after, "4");
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0]["element_id"], "new");
        assert_eq!(diff.removed.len(), 1);
        assert_eq!(diff.removed[0]["element_id"], "gone");
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0].before["label"], "Send");
        assert_eq!(diff.changed[0].after["label"], "Stop");
    }

    #[test]
    fn duplicate_ids_are_not_lost_and_missing_frames_are_explicit() {
        let before = json!({"nodes": [
            {"element_id": "duplicate", "node": "1"},
            {"element_id": "duplicate", "node": "2"},
        ]});
        let after = json!({"nodes": [
            {"element_id": "duplicate", "node": "1"},
            {"element_id": "duplicate", "node": "2", "label": "changed"},
        ]});
        assert_eq!(TreeDiff::between(&before, &after).changed.len(), 1);
        let missing = TreeDiff::between(&before, &Value::Null);
        assert!(missing.baseline_available);
        assert!(!missing.observed_available);
        assert!(
            missing.removed.is_empty(),
            "an unavailable frame is not an empty tree"
        );
    }

    #[test]
    fn failure_evidence_round_trips_without_losing_original_error_details() {
        let error = Error::new("not_found", "missing button")
            .with_target("missing")
            .with_nearest(vec!["send".into()])
            .with_bundle(FailureBundle {
                observed: json!({"state": {"composer": "hello"}, "health": {"daemon": "disconnected"}}),
                tree_diff: TreeDiff::between(&Value::Null, &Value::Null),
                recent_events: None,
                counters: waku_client::DiagnosticCounters { observed_at_ms: 42, processes: vec![] },
                diagnostic_errors: vec!["daemon disconnected".into()],
            });
        let encoded = serde_json::to_value(Reply::error(7, error)).unwrap();
        assert_eq!(encoded["id"], 7);
        assert_eq!(encoded["error"]["target"], "missing");
        assert_eq!(encoded["error"]["state"]["composer"], "hello");
        assert_eq!(encoded["error"]["bundle"]["counters"]["observedAtMs"], 42);
        let decoded: Error = serde_json::from_value(encoded["error"].clone()).unwrap();
        assert_eq!(decoded.code, "not_found");
        assert_eq!(
            decoded.bundle.unwrap().diagnostic_errors,
            ["daemon disconnected"]
        );
    }
}
