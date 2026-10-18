//! Durable issue workflows. Workflow order is independent of board columns.
use crate::model::{ProviderKind, unix_time};
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum IssueStageRole {
    Plan,
    Execution,
    Review,
    Report,
    Custom,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct IssueWorkflowStage {
    pub id: Uuid,
    /// Optional board provenance. A final Report mapped explicitly to the
    /// project's terminal stage moves the issue there after providers settle.
    pub board_stage_id: Option<Uuid>,
    pub name: String,
    pub role: IssueStageRole,
    pub provider: ProviderKind,
    pub model: Option<String>,
    pub instructions: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct IssueWorkflow {
    pub stages: Vec<IssueWorkflowStage>,
    pub max_review_retries: u32,
}

impl IssueWorkflow {
    pub fn plan_execute_review_report(provider: ProviderKind) -> Self {
        Self {
            stages: [
                ("Plan", IssueStageRole::Plan, "Inspect the issue and repository. Produce a concrete implementation and verification plan. Do not modify implementation files."),
                ("Execute", IssueStageRole::Execution, "Implement the issue in this issue's workspace. Run relevant tests and record the actual results. Address prior review findings when present."),
                ("Review", IssueStageRole::Review, "Independently review the changes and verification evidence. Do not edit implementation files. Approve only if the issue is satisfied; otherwise reject with specific actionable findings."),
                ("Report", IssueStageRole::Report, "Summarize delivered changes, verification actually run, review outcome, limitations and remaining risks. Do not merge, deploy, or mark the issue Done."),
            ].into_iter().map(|(name, role, instructions)| IssueWorkflowStage {
                id: Uuid::new_v4(), board_stage_id: None, name: name.into(), role,
                provider, model: None, instructions: instructions.into(),
            }).collect(),
            max_review_retries: 2,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.stages.is_empty() || self.stages.len() > 32 {
            return Err("Choose between 1 and 32 workflow stages".into());
        }
        if self.max_review_retries > 10 {
            return Err("Review retries must be at most 10".into());
        }
        let mut ids = std::collections::HashSet::new();
        for (index, stage) in self.stages.iter().enumerate() {
            if stage.name.len() > 256
                || stage.instructions.len() > 16_384
                || stage.model.as_ref().is_some_and(|model| model.len() > 512)
            {
                return Err(
                    "Stage names, instructions or model identifiers exceed their size limits"
                        .into(),
                );
            }
            if stage.name.trim().is_empty() || !ids.insert(stage.id) {
                return Err("Workflow stage names must be nonempty and IDs unique".into());
            }
            if stage
                .model
                .as_ref()
                .is_some_and(|model| model.trim().is_empty())
            {
                return Err("Choose provider default instead of an empty model".into());
            }
            if stage.role == IssueStageRole::Review
                && !self.stages[..index]
                    .iter()
                    .any(|prior| prior.role == IssueStageRole::Execution)
            {
                return Err("A review stage needs an earlier execution stage".into());
            }
        }
        if self
            .stages
            .last()
            .is_none_or(|stage| stage.role != IssueStageRole::Report)
        {
            return Err("The final workflow stage must produce a report".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum IssueRunStatus {
    Running,
    PauseRequested,
    CancelRequested,
    Paused,
    Blocked,
    Failed,
    Cancelled,
    Completed,
}
impl IssueRunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Cancelled | Self::Completed)
    }
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Running | Self::PauseRequested | Self::CancelRequested
        )
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum IssueRunAction {
    Pause,
    Resume,
    Cancel,
    Retry,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum IssueAttemptStatus {
    Running,
    Completed,
    Rejected,
    Blocked,
    Failed,
    Cancelled,
    Interrupted,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum IssueStageDecision {
    Completed,
    Approved,
    Rejected,
    Blocked,
}

/// A provider's explicit public result. Prose or transport success alone never
/// advances a workflow; identity, schema, decision and evidence are validated.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct IssueStageOutcome {
    pub run_id: Uuid,
    pub attempt_id: Uuid,
    pub decision: IssueStageDecision,
    pub summary: String,
    pub evidence: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct IssueStageAttempt {
    pub id: Uuid,
    pub stage_id: Uuid,
    pub session_id: Uuid,
    pub status: IssueAttemptStatus,
    pub outcome: Option<IssueStageOutcome>,
    pub error: Option<String>,
    pub started_at: u64,
    pub finished_at: Option<u64>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
pub struct IssueRun {
    pub id: Uuid,
    pub request_id: Uuid,
    pub task_id: Uuid,
    pub project_id: Uuid,
    pub issue_title: String,
    pub issue_description: String,
    pub workflow: IssueWorkflow,
    pub status: IssueRunStatus,
    pub current_stage: usize,
    pub attempts: Vec<IssueStageAttempt>,
    pub review_retries: u32,
    pub report: Option<IssueStageOutcome>,
    pub error: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}
impl IssueRun {
    pub fn current_stage(&self) -> Option<&IssueWorkflowStage> {
        self.workflow.stages.get(self.current_stage)
    }
    pub fn touch(&mut self) {
        self.updated_at = unix_time().max(self.updated_at.saturating_add(1));
    }
}
