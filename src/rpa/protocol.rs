//! The debug RPA wire protocol.
//!
//! One JSON object per line over a loopback TCP connection. Every call
//! carries the protocol version and the instance token the app published;
//! every reply is correlated by the call's id. The surface is deliberately
//! small — observation, semantic action, waiting, and ordered batches — so a
//! client can close a whole interaction in one call without guessing
//! coordinates or sleeping.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped whenever the shape of [`Request`] or the error contract changes.
/// Both ends refuse a mismatch instead of guessing.
pub const PROTOCOL_VERSION: u32 = 1;

pub const DEFAULT_WAIT_TIMEOUT_MS: u64 = 10_000;
/// How often a wait re-reads the app. One display frame, so a condition that
/// just became true is observed on the next poll.
pub const WAIT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

#[derive(Clone, Debug, Deserialize)]
pub struct Call {
    pub id: u64,
    pub version: u32,
    pub token: String,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Instance identity: which checkout, database, build and process answered.
    Describe,
    /// The current frame's semantic tree plus the app's observable state.
    Snapshot,
    /// One semantic action on one control.
    Act { action: Action, target: Target },
    /// Focus a target (when given) and type text one character at a time.
    Type {
        text: String,
        #[serde(default)]
        target: Option<Target>,
    },
    /// Send a chord like `secondary-l` or `cmd-shift-p`, space separated for
    /// a sequence.
    Key { chord: String },
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
    /// An ordered batch of steps, stopping at the first failure.
    Run {
        steps: Vec<Step>,
        /// Include a final snapshot in the result. Defaults to true because
        /// the caller almost always wants to assert on where it ended up.
        #[serde(default = "default_true")]
        final_snapshot: bool,
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
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Step {
    Click {
        target: Target,
    },
    Focus {
        target: Target,
    },
    Type {
        text: String,
        #[serde(default)]
        target: Option<Target>,
    },
    Key {
        chord: String,
    },
    Invoke {
        verb: String,
        #[serde(default)]
        args: Value,
    },
    Wait {
        condition: Condition,
        #[serde(default = "default_wait_timeout")]
        timeout_ms: u64,
    },
}

impl Step {
    pub fn label(&self) -> &'static str {
        match self {
            Step::Click { .. } => "click",
            Step::Focus { .. } => "focus",
            Step::Type { .. } => "type",
            Step::Key { .. } => "key",
            Step::Invoke { .. } => "invoke",
            Step::Wait { .. } => "wait",
        }
    }
}

/// How to name the control an action applies to. `node` is the handle from a
/// snapshot; `id` is the stable semantic id; `label` matches the accessible
/// label, for callers that have nothing better.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Node(String),
    Id(String),
    Label(String),
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
}

impl Error {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            target: None,
            nearest: None,
            state: None,
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
