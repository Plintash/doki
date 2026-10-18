//! Typed controls for daemon-owned issue workflows. Call off the UI thread.
use crate::DaemonClient;
use uuid::Uuid;
use waku_protocol::{Command, IssueRun, IssueRunAction, IssueWorkflow, ResponsePayload};

#[derive(Clone)]
pub struct IssueRunClient {
    client: DaemonClient,
}
impl IssueRunClient {
    pub fn new(client: DaemonClient) -> Self {
        Self { client }
    }
    pub fn list(&self, task_id: Option<Uuid>) -> anyhow::Result<Vec<IssueRun>> {
        match self
            .client
            .request(Uuid::nil(), Uuid::nil(), Command::ListIssueRuns { task_id })?
        {
            ResponsePayload::IssueRuns { runs } => Ok(runs),
            _ => anyhow::bail!("Daemon returned an invalid issue-run list"),
        }
    }
    /// Reuse request_id when retrying an uncertain transport result.
    pub fn start(
        &self,
        task_id: Uuid,
        request_id: Uuid,
        workflow: IssueWorkflow,
    ) -> anyhow::Result<IssueRun> {
        self.request(Command::StartIssueRun {
            task_id,
            request_id,
            workflow,
        })
    }
    pub fn control(&self, run_id: Uuid, action: IssueRunAction) -> anyhow::Result<IssueRun> {
        self.request(Command::ControlIssueRun { run_id, action })
    }
    fn request(&self, command: Command) -> anyhow::Result<IssueRun> {
        match self.client.request(Uuid::nil(), Uuid::nil(), command)? {
            ResponsePayload::IssueRun { run } => Ok(run),
            _ => anyhow::bail!("Daemon returned an invalid issue-run response"),
        }
    }
}
