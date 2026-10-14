//! Daemon-owned suite lifecycle. Full, bounded program results remain available
//! independently of the client connection that started a job.
use crate::{AutomationError, AutomationRun, AutomationStatus};
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

pub const MAX_AUTOMATION_JOB_RUNS: usize = 16;
pub const MAX_AUTOMATION_JOB_RESULT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationJobRun {
    pub run_id: Uuid,
    pub name: String,
    pub status: AutomationStatus,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub programs: Vec<AutomationRun>,
    pub error: Option<AutomationError>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_job_has_a_stable_run_id_without_a_finished_timestamp() {
        let run = AutomationJobRun {
            run_id: Uuid::new_v4(),
            name: "suite".into(),
            status: AutomationStatus::Pending,
            started_at_ms: 1234,
            finished_at_ms: None,
            programs: vec![],
            error: None,
        };
        let wire = serde_json::to_value(&run).unwrap();
        assert_eq!(wire["status"], "pending");
        assert_eq!(wire["startedAtMs"], 1234);
        assert!(wire["finishedAtMs"].is_null());
        let decoded: AutomationJobRun = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), wire);
    }
}
