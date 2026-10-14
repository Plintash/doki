//! Daemon-owned local schedules. Each program runs in a fresh owned scratch
//! instance; a job stops at its first failing program.
use crate::{
    AutomationError, AutomationReplay, AutomationScenario, AutomationScratchSeed, AutomationStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use ts_rs::TS;
use uuid::Uuid;

pub const MAX_AUTOMATION_SCHEDULES: usize = 32;
pub const MAX_AUTOMATION_JOB_PROGRAMS: usize = 8;
pub const MAX_AUTOMATION_JOB_BYTES: usize = 1024 * 1024;
pub const MAX_AUTOMATION_SCHEDULE_RESULT_BYTES: usize = 16 * 1024;
pub const MIN_AUTOMATION_SCHEDULE_INTERVAL_MS: u64 = 1000;
pub const DEFAULT_AUTOMATION_JOB_TIMEOUT_MS: u64 = 300_000;
pub const MAX_AUTOMATION_JOB_TIMEOUT_MS: u64 = 1_800_000;

fn default_job_timeout_ms() -> u64 {
    DEFAULT_AUTOMATION_JOB_TIMEOUT_MS
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationJob {
    pub name: String,
    pub programs: Vec<AutomationProgram>,
    #[serde(default)]
    pub seed: AutomationScratchSeed,
    #[serde(default)]
    pub headless: bool,
    #[serde(default = "default_job_timeout_ms")]
    pub timeout_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum AutomationProgram {
    Scenario {
        scenario: AutomationScenario,
    },
    Replay {
        replay: AutomationReplay,
        #[serde(default)]
        parameters: BTreeMap<String, Value>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationScheduleDefinition {
    pub name: String,
    pub job: AutomationJob,
    pub first_run_at_ms: u64,
    /// Omit for a one-shot. Recurrences are anchored to firstRunAtMs, with
    /// missed intervals coalesced rather than enqueued.
    pub interval_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationSchedule {
    pub id: Uuid,
    pub definition: AutomationScheduleDefinition,
    pub enabled: bool,
    /// None means that there are no unclaimed runs (including a consumed
    /// one-shot). An active run can still finish after the schedule is disabled.
    pub next_run_at_ms: Option<u64>,
    pub active_run_id: Option<Uuid>,
    pub last_result: Option<AutomationScheduleResult>,
}

/// Bounded durable summary, not a second copy of scenario observations. The
/// store retains only the error code/message; full evidence stays with the job.
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationScheduleResult {
    pub run_id: Uuid,
    pub status: AutomationStatus,
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
    pub error: Option<AutomationError>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AutomationCall, AutomationStep, AutomationStepOperation};
    use serde_json::json;

    #[test]
    fn schedule_wire_roundtrip_retains_explicit_program_type_and_cadence() {
        let definition = AutomationScheduleDefinition {
            name: "morning smoke".into(),
            job: AutomationJob {
                name: "smoke".into(),
                programs: vec![AutomationProgram::Scenario {
                    scenario: AutomationScenario {
                        name: "describe".into(),
                        steps: vec![AutomationStep {
                            name: "describe".into(),
                            operation: AutomationStepOperation::Call {
                                call: AutomationCall::Describe,
                            },
                        }],
                    },
                }],
                seed: AutomationScratchSeed::default(),
                headless: true,
                timeout_ms: DEFAULT_AUTOMATION_JOB_TIMEOUT_MS,
            },
            first_run_at_ms: 1234,
            interval_ms: Some(5000),
        };
        let wire = serde_json::to_value(&definition).unwrap();
        assert_eq!(wire["job"]["programs"][0]["type"], "scenario");
        assert_eq!(wire["firstRunAtMs"], 1234);
        assert_eq!(wire["intervalMs"], 5000);
        let decoded: AutomationScheduleDefinition = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), wire);
    }

    #[test]
    fn omitted_job_timeout_uses_bounded_five_minute_default() {
        let decoded: AutomationJob =
            serde_json::from_value(json!({"name":"smoke", "programs":[]})).unwrap();
        assert_eq!(decoded.timeout_ms, DEFAULT_AUTOMATION_JOB_TIMEOUT_MS);
        assert!(!decoded.headless);
    }

    #[test]
    fn job_does_not_accept_unrecognized_execution_options() {
        let wire = json!({"name":"smoke", "programs":[], "enableProviders":true});
        assert!(serde_json::from_value::<AutomationJob>(wire).is_err());
    }
}
