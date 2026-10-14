//! Portable semantic recordings. Bindings address exact JSON values; they are
//! never string templates and cannot interpolate substrings into operations.
use crate::{AutomationError, AutomationStep};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const AUTOMATION_REPLAY_FORMAT_VERSION: u32 = 1;
pub const MAX_AUTOMATION_REPLAY_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationReplay {
    pub format_version: u32,
    pub name: String,
    pub steps: Vec<AutomationReplayStep>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationReplayStep {
    #[serde(flatten)]
    pub step: AutomationStep,
    #[serde(default)]
    pub bindings: Vec<AutomationBinding>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationBinding {
    /// JSON pointer relative to the serialized AutomationStepOperation, not
    /// to the replay document. Example: /call/text or /condition/equals.
    pub pointer: String,
    pub source: AutomationBindingSource,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum AutomationBindingSource {
    Parameter {
        name: String,
    },
    /// Zero-based earlier step index and a JSON pointer into its result value.
    /// Existing call results expose the invoked result under /value.
    StepResult {
        step: usize,
        pointer: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AutomationReplayInputKind {
    String,
    Uuid,
    Json,
}

/// Required caller input only. No original value or secret default is saved.
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationReplayInput {
    pub name: String,
    pub kind: AutomationReplayInputKind,
    pub uses: Vec<AutomationReplayInputUse>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationReplayInputUse {
    /// Zero-based step index and exact destination within its operation.
    pub step: usize,
    pub pointer: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AutomationRecordingStatus {
    Recording,
    Stopped,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationRecording {
    pub status: AutomationRecordingStatus,
    pub completed_steps: usize,
    pub pending: bool,
    pub required_inputs: Vec<AutomationReplayInput>,
    /// Sanitized bounded failure evidence. The original call/wait response
    /// remains the source of detailed runtime evidence.
    pub error: Option<AutomationError>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AutomationCall, AutomationStepOperation};
    use serde_json::json;

    #[test]
    fn replay_wire_shape_is_versioned_and_explicit() {
        let replay = AutomationReplay {
            format_version: AUTOMATION_REPLAY_FORMAT_VERSION,
            name: "draft".into(),
            steps: vec![AutomationReplayStep {
                step: AutomationStep {
                    name: "type".into(),
                    operation: AutomationStepOperation::Call {
                        call: AutomationCall::Type {
                            text: String::new(),
                            target: None,
                        },
                    },
                },
                bindings: vec![AutomationBinding {
                    pointer: "/call/text".into(),
                    source: AutomationBindingSource::Parameter {
                        name: "text1".into(),
                    },
                }],
            }],
        };
        let wire = serde_json::to_value(&replay).unwrap();
        assert_eq!(wire["formatVersion"], 1);
        assert_eq!(wire["steps"][0]["type"], "call");
        assert_eq!(wire["steps"][0]["call"]["text"], "");
        assert_eq!(
            wire["steps"][0]["bindings"][0],
            json!({"pointer":"/call/text","source":{"type":"parameter","name":"text1"}})
        );
        let decoded: AutomationReplay = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), wire);
    }

    #[test]
    fn binding_source_rejects_unknown_fields_and_uses_zero_based_step() {
        let source = json!({"type":"stepResult","step":0,"pointer":"/value/opened"});
        let decoded: AutomationBindingSource = serde_json::from_value(source.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), source);
        assert!(
            serde_json::from_value::<AutomationBindingSource>(
                json!({"type":"parameter","name":"text1","default":"secret"})
            )
            .is_err()
        );
    }
}
