//! Debug automation on the authenticated daemon connection. The app owns UI
//! observations and diffs; the daemon owns sessions, replay, waits and runs.
use crate::{DiagnosticCounters, DiagnosticLogs, DiagnosticQuery};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;
use uuid::Uuid;

pub const MAX_AUTOMATION_OBSERVATION_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_AUTOMATION_EVENTS: usize = 256;
pub const MAX_AUTOMATION_SESSIONS: usize = 64;
pub const MAX_AUTOMATION_STEPS: usize = 128;

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationInstance {
    pub instance_id: Uuid,
    pub checkout: String,
    pub database: String,
    pub pid: u32,
    pub build: String,
    pub version: String,
    #[serde(default)]
    pub executor_connected: bool,
    #[serde(default)]
    pub scratch: bool,
    /// True only for the daemon-only executor; UI operations are unavailable.
    #[serde(default)]
    pub headless: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationScratchSeed {
    #[serde(default)]
    pub projects: Vec<crate::model::Project>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<crate::model::Task>,
    #[serde(default)]
    pub sessions: Vec<crate::model::AgentSession>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AutomationCursor {
    pub epoch: Uuid,
    pub sequence: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AutomationStream {
    Ui,
    Model,
    Diagnostic,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationSession {
    pub session_id: Uuid,
    pub instance: AutomationInstance,
    pub cursor: AutomationCursor,
    pub streams: Vec<AutomationStream>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AutomationCommand {
    RegisterExecutor {
        instance: AutomationInstance,
    },
    Instances,
    ScratchCreate {
        name: Option<String>,
        seed: AutomationScratchSeed,
        #[serde(default)]
        headless: bool,
    },
    ScratchRemove {
        instance_id: Uuid,
    },
    Attach {
        instance_id: Uuid,
    },
    Resume {
        session_id: Uuid,
        after: Option<AutomationCursor>,
    },
    Detach {
        session_id: Uuid,
    },
    Call {
        session_id: Uuid,
        call: AutomationCall,
    },
    Diff {
        session_id: Uuid,
        after: AutomationCursor,
        streams: Vec<AutomationStream>,
    },
    Subscribe {
        session_id: Uuid,
        streams: Vec<AutomationStream>,
    },
    Wait {
        session_id: Uuid,
        condition: AutomationCondition,
        timeout_ms: u64,
    },
    WaitResult {
        session_id: Uuid,
        wait_id: Uuid,
    },
    RecordingStart {
        session_id: Uuid,
        name: String,
    },
    RecordingStatus {
        session_id: Uuid,
    },
    RecordingStop {
        session_id: Uuid,
    },
    RecordingCancel {
        session_id: Uuid,
    },
    ReplayStart {
        session_id: Uuid,
        replay: crate::AutomationReplay,
        parameters: std::collections::BTreeMap<String, Value>,
    },
    JobStart {
        job: crate::AutomationJob,
    },
    JobResult {
        run_id: Uuid,
    },
    JobAbort {
        run_id: Uuid,
    },
    ScheduleCreate {
        definition: crate::AutomationScheduleDefinition,
    },
    ScheduleList,
    ScheduleGet {
        schedule_id: Uuid,
    },
    ScheduleSetEnabled {
        schedule_id: Uuid,
        enabled: bool,
    },
    ScheduleCancel {
        schedule_id: Uuid,
    },
    ScheduleRemove {
        schedule_id: Uuid,
    },
    ScenarioStart {
        session_id: Uuid,
        scenario: AutomationScenario,
    },
    ScenarioResult {
        session_id: Uuid,
        run_id: Uuid,
    },
    ScenarioAbort {
        session_id: Uuid,
        run_id: Uuid,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AutomationCall {
    Describe,
    Transcript {
        conversation_id: Uuid,
        query: crate::TranscriptQuery,
    },
    Snapshot {
        #[serde(default)]
        sections: Vec<String>,
    },
    Act {
        action: AutomationAction,
        target: AutomationTarget,
    },
    Type {
        text: String,
        target: Option<AutomationTarget>,
    },
    Key {
        chord: String,
    },
    Invoke {
        verb: String,
        #[serde(default)]
        args: Value,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum AutomationAction {
    Click,
    Focus,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum AutomationTarget {
    Node(String),
    Id(String),
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(tag = "on", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum AutomationCondition {
    Node { id: String, present: bool },
    Text { id: String, contains: String },
    State { pointer: String, equals: Value },
    StateContains { pointer: String, contains: String },
    Frame { after: u64 },
    Diagnostic { query: DiagnosticQuery },
}

/// App-produced evidence. Diff values preserve the native tree's fields and
/// explicit missing-frame flags; stateDiff is a JSON-pointer/value change list.
#[derive(Clone, Debug, Default, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationObservation {
    pub observed: Value,
    pub tree_diff: Value,
    pub state_diff: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationExecution {
    pub value: Value,
    #[serde(flatten)]
    pub observation: AutomationObservation,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationCallResult {
    pub cursor: AutomationCursor,
    #[serde(flatten)]
    pub execution: AutomationExecution,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationFailureBundle {
    pub observed: Value,
    pub tree_diff: Value,
    pub recent_events: Option<DiagnosticLogs>,
    pub counters: DiagnosticCounters,
    pub diagnostic_errors: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nearest: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<AutomationFailureBundle>,
}
impl AutomationError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            target: None,
            nearest: None,
            bundle: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationExecutorRequest {
    pub request_id: Uuid,
    pub session_id: Uuid,
    pub instance_id: Uuid,
    pub call: AutomationCall,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AutomationExecutorOutcome {
    Ok { execution: AutomationExecution },
    Error { error: AutomationError },
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationExecutorReply {
    pub request_id: Uuid,
    pub outcome: AutomationExecutorOutcome,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationExecutorEvent {
    pub instance_id: Uuid,
    pub stream: AutomationStream,
    #[serde(flatten)]
    pub observation: AutomationObservation,
}

/// Only deltas are retained. A full frame is returned by calls or captured as
/// failure evidence, never mirrored in the daemon's instance registry.
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationEvent {
    pub session_id: Uuid,
    pub cursor: AutomationCursor,
    pub stream: AutomationStream,
    pub payload: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationExpiry {
    pub requested: AutomationCursor,
    pub oldest_sequence: u64,
    pub cursor: AutomationCursor,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationDiff {
    pub events: Vec<AutomationEvent>,
    pub cursor: AutomationCursor,
    pub expiry: Option<AutomationExpiry>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum AutomationStatus {
    Pending,
    Succeeded,
    Failed,
    Aborted,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationWait {
    pub wait_id: Uuid,
    pub status: AutomationStatus,
    pub cursor: AutomationCursor,
    pub result: Option<Value>,
    pub error: Option<AutomationError>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationScenario {
    pub name: String,
    pub steps: Vec<AutomationStep>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationStep {
    pub name: String,
    #[serde(flatten)]
    pub operation: AutomationStepOperation,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AutomationStepOperation {
    Call {
        call: AutomationCall,
    },
    Wait {
        condition: AutomationCondition,
        timeout_ms: u64,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationStepResult {
    pub name: String,
    pub status: AutomationStatus,
    pub result: Option<Value>,
    pub error: Option<AutomationError>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationRun {
    pub run_id: Uuid,
    pub name: String,
    pub status: AutomationStatus,
    pub steps: Vec<AutomationStepResult>,
    pub cursor: AutomationCursor,
    pub error: Option<AutomationError>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AutomationResult {
    Registered {
        instance: AutomationInstance,
    },
    Instances {
        instances: Vec<AutomationInstance>,
    },
    Attached {
        session: AutomationSession,
        replay: AutomationDiff,
    },
    Detached,
    ScratchCreated {
        instance: AutomationInstance,
    },
    ScratchRemoved {
        instance_id: Uuid,
    },
    Called {
        result: AutomationCallResult,
    },
    Diff {
        diff: AutomationDiff,
    },
    Subscribed {
        session: AutomationSession,
    },
    Wait {
        wait: AutomationWait,
    },
    Scenario {
        run: AutomationRun,
    },
    Recording {
        recording: crate::AutomationRecording,
    },
    Job {
        run: crate::AutomationJobRun,
    },
    Schedule {
        schedule: crate::AutomationSchedule,
    },
    Schedules {
        schedules: Vec<crate::AutomationSchedule>,
    },
    ScheduleRemoved {
        schedule_id: Uuid,
    },
    Replay {
        replay: crate::AutomationReplay,
    },
    Error {
        error: AutomationError,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientMessage, Command, Request, ServerMessage};
    use serde_json::json;

    fn roundtrip<T: Serialize + serde::de::DeserializeOwned>(value: T) -> Value {
        let wire = serde_json::to_value(value).unwrap();
        let decoded: T = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), wire);
        wire
    }

    #[test]
    fn automation_commands_share_the_versioned_request_envelope() {
        let session_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let cursor = AutomationCursor {
            epoch: Uuid::new_v4(),
            sequence: 7,
        };
        let instance = AutomationInstance {
            instance_id,
            checkout: "checkout".into(),
            database: "temp/app.db".into(),
            pid: 7,
            build: "build-id".into(),
            version: "test".into(),
            executor_connected: true,
            scratch: false,
            headless: false,
        };
        let commands = vec![
            AutomationCommand::RegisterExecutor { instance },
            AutomationCommand::Instances,
            AutomationCommand::ScratchCreate {
                name: Some("isolated".into()),
                seed: AutomationScratchSeed::default(),
                headless: false,
            },
            AutomationCommand::ScratchRemove { instance_id },
            AutomationCommand::Attach { instance_id },
            AutomationCommand::Resume {
                session_id,
                after: Some(cursor),
            },
            AutomationCommand::Detach { session_id },
            AutomationCommand::Call {
                session_id,
                call: AutomationCall::Act {
                    action: AutomationAction::Click,
                    target: AutomationTarget::Id("composer.send".into()),
                },
            },
            AutomationCommand::Diff {
                session_id,
                after: cursor,
                streams: vec![
                    AutomationStream::Ui,
                    AutomationStream::Model,
                    AutomationStream::Diagnostic,
                ],
            },
            AutomationCommand::Subscribe {
                session_id,
                streams: vec![],
            },
            AutomationCommand::Wait {
                session_id,
                condition: AutomationCondition::State {
                    pointer: "/model/newField".into(),
                    equals: json!(true),
                },
                timeout_ms: 5000,
            },
            AutomationCommand::WaitResult {
                session_id,
                wait_id: Uuid::new_v4(),
            },
            AutomationCommand::ScenarioStart {
                session_id,
                scenario: AutomationScenario {
                    name: "smoke".into(),
                    steps: vec![AutomationStep {
                        name: "snapshot".into(),
                        operation: AutomationStepOperation::Call {
                            call: AutomationCall::Snapshot {
                                sections: vec!["tree".into()],
                            },
                        },
                    }],
                },
            },
            AutomationCommand::ScenarioResult {
                session_id,
                run_id: Uuid::new_v4(),
            },
            AutomationCommand::ScenarioAbort {
                session_id,
                run_id: Uuid::new_v4(),
            },
        ];
        for command in commands {
            let wire = roundtrip(ClientMessage::Request(Request {
                request_id: Uuid::new_v4(),
                session_id: Uuid::nil(),
                runtime_id: Uuid::nil(),
                command: Command::Automation { command },
            }));
            assert_eq!(wire["type"], "request");
            assert_eq!(wire["command"]["type"], "automation");
            assert!(wire["command"]["command"]["type"].is_string());
            assert!(wire.to_string().find("token").is_none());
        }
    }

    #[test]
    fn executor_requests_replies_and_events_preserve_correlation_and_deltas() {
        let request_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let request = roundtrip(ServerMessage::AutomationExecutorRequest {
            request: AutomationExecutorRequest {
                request_id,
                session_id,
                instance_id,
                call: AutomationCall::Key {
                    chord: "enter".into(),
                },
            },
        });
        assert_eq!(request["request"]["requestId"], request_id.to_string());
        assert_eq!(request["request"]["sessionId"], session_id.to_string());
        let observation = AutomationObservation {
            observed: json!({"tree":{"frame":42},"state":{"model":{"newField":true}}}),
            tree_diff: json!({"added":[{"element_id":"task.new"}]}),
            state_diff: json!([{"pointer":"/route","value":"task"}]),
        };
        let reply = roundtrip(ClientMessage::AutomationExecutorReply {
            reply: AutomationExecutorReply {
                request_id,
                outcome: AutomationExecutorOutcome::Ok {
                    execution: AutomationExecution {
                        value: json!(true),
                        observation: observation.clone(),
                    },
                },
            },
        });
        assert_eq!(
            reply["reply"]["outcome"]["execution"]["observed"]["tree"]["frame"],
            42
        );
        assert!(reply["reply"]["outcome"]["execution"]["stateDiff"].is_array());
        let event = roundtrip(ClientMessage::AutomationExecutorEvent {
            event: AutomationExecutorEvent {
                instance_id,
                stream: AutomationStream::Model,
                observation,
            },
        });
        assert_eq!(event["event"]["stream"], "model");
        assert_eq!(
            event["event"]["observed"]["state"]["model"]["newField"],
            true
        );
        let interest = roundtrip(ServerMessage::AutomationInterest {
            instance_id,
            attached: true,
            streams: vec![],
            model_sections: vec!["session".into()],
        });
        assert_eq!(interest["attached"], true);
        assert_eq!(interest["streams"], json!([]));
        assert_eq!(interest["modelSections"], json!(["session"]));
    }

    #[test]
    fn failed_automation_roundtrips_its_structured_evidence() {
        let error = AutomationError {
            code: "unknown_target".into(),
            message: "missing composer.send".into(),
            target: Some("composer.send".into()),
            nearest: Some(vec!["composer.input".into()]),
            bundle: Some(AutomationFailureBundle {
                observed: json!({"state":{"composer":{"text":"hello"}}}),
                tree_diff: json!({"baselineAvailable":false,"observedAvailable":true}),
                recent_events: None,
                counters: DiagnosticCounters {
                    observed_at_ms: 123,
                    processes: vec![],
                },
                diagnostic_errors: vec!["no baseline".into()],
            }),
        };
        let wire = roundtrip(AutomationResult::Error { error });
        assert_eq!(wire["error"]["target"], "composer.send");
        assert_eq!(
            wire["error"]["bundle"]["observed"]["state"]["composer"]["text"],
            "hello"
        );
        assert_eq!(wire["error"]["bundle"]["counters"]["observedAtMs"], 123);
    }
}
