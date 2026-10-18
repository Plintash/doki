//! Daemon-owned, durable issue-stage orchestration. Drivers are the same runtime
//! adapters as manual conversations; this module owns only ordering and evidence.
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use crate::EventSink;
use crate::daemon::RuntimeExit;
use crate::driver::{self, DriverEventSender, DriverHandle, DriverStartOptions};
use crate::model::{
    AgentSession, BoardStage, DriverEvent, Message, MessageRole, ProviderKind, RuntimeMode,
    SessionStageInvocation, SessionStatus, SessionWorkspace, StageHandoff, Task, TaskState,
    TaskWorkspace, TurnStatus, unix_time,
};
use crate::persistence::{PersistedState, StateStore};
use crate::settings::DaemonSettingsStore;
use anyhow::{Context as _, anyhow, bail};
use parking_lot::{Condvar, Mutex, MutexGuard};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use waku_protocol::issue_automation::*;

const MAX_PUBLIC_OUTPUT: usize = 256 * 1024;
const MAX_HANDOFF_CHARS: usize = 16_000;
const RESULT_MARKER: &str = "```doki-stage-result\n";
type Sessions = Arc<Mutex<HashMap<Uuid, (Uuid, DriverHandle)>>>;
type Exits = Arc<Mutex<HashMap<(Uuid, Uuid), Arc<RuntimeExit>>>>;

#[derive(Clone, Serialize, Deserialize)]
struct Record {
    run: IssueRun,
    #[serde(default)]
    request_aliases: Vec<(Uuid, IssueWorkflow)>,
    project_path: PathBuf,
    #[serde(default)]
    base_branch: Option<String>,
    #[serde(default)]
    completion_stage: Option<BoardStage>,
    /// Written atomically with the report; cleared only after the task's
    /// terminal mapping is durable. Missing means legacy/already finalized.
    #[serde(default)]
    completion_pending: bool,
    workspace: TaskWorkspace,
}
impl Record {
    fn reserves_workspace(&self) -> bool {
        !self.run.status.is_terminal() || self.completion_pending
    }
}

/// Only the launch boundary is injectable. Tests exercise the real journal,
/// controls, sessions, event stream, result validation and sequencing.
pub(crate) trait IssueHarness: Send + Sync {
    fn validate(
        &self,
        stage: &IssueWorkflowStage,
        settings: &waku_protocol::DaemonSettings,
    ) -> anyhow::Result<()>;
    fn start(
        &self,
        stage: &IssueWorkflowStage,
        cwd: PathBuf,
        settings: &waku_protocol::DaemonSettings,
        events: DriverEventSender,
    ) -> anyhow::Result<DriverHandle>;
}
struct LocalHarness;
impl IssueHarness for LocalHarness {
    fn validate(
        &self,
        stage: &IssueWorkflowStage,
        settings: &waku_protocol::DaemonSettings,
    ) -> anyhow::Result<()> {
        crate::daemon::ensure_shell_environment();
        if !matches!(
            stage.provider,
            ProviderKind::Codex
                | ProviderKind::Claude
                | ProviderKind::Pi
                | ProviderKind::OhMyPi
                | ProviderKind::OpenCode
        ) {
            bail!(
                "{} does not yet support automatic issue stages; choose Codex, Claude, Pi, Oh My Pi, or OpenCode",
                stage.provider.display_name()
            );
        }
        if settings.disabled_providers.contains(&stage.provider) {
            bail!(
                "{} is disabled in provider settings",
                stage.provider.display_name()
            );
        }
        let binary_override = settings
            .provider_binary_overrides
            .get(&stage.provider)
            .map(String::as_str);
        if crate::model::provider_probe(stage.provider, binary_override)
            .path
            .is_none()
        {
            bail!(
                "{} is not installed or its configured binary is unavailable",
                stage.provider.display_name()
            );
        }
        Ok(())
    }
    fn start(
        &self,
        stage: &IssueWorkflowStage,
        cwd: PathBuf,
        settings: &waku_protocol::DaemonSettings,
        events: DriverEventSender,
    ) -> anyhow::Result<DriverHandle> {
        self.validate(stage, settings)?;
        let binary = crate::model::provider_probe(
            stage.provider,
            settings
                .provider_binary_overrides
                .get(&stage.provider)
                .map(String::as_str),
        )
        .path
        .ok_or_else(|| anyhow!("provider binary disappeared"))?;
        driver::start_local(
            stage.provider,
            DriverStartOptions {
                binary,
                cwd,
                mode: RuntimeMode::Auto,
                model: stage.model.clone(),
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
                agent_preset: None,
                computer_use_enabled: false,
                provider_cursor: None,
            },
            events,
        )
    }
}

struct Inner {
    path: PathBuf,
    load_error: Option<String>,
    records: Mutex<Vec<Record>>,
    workers: Mutex<HashSet<Uuid>>,
    workers_changed: Condvar,
    shutdown: AtomicBool,
    store: Arc<StateStore>,
    state: Arc<Mutex<PersistedState>>,
    settings: Arc<DaemonSettingsStore>,
    sessions: Sessions,
    exits: Exits,
    harness: Mutex<Arc<dyn IssueHarness>>,
}
pub(crate) struct IssueMutationGuard<'a> {
    records: MutexGuard<'a, Vec<Record>>,
}
impl IssueMutationGuard<'_> {
    pub(crate) fn owned_sessions(&self) -> HashSet<Uuid> {
        self.records
            .iter()
            .flat_map(|r| r.run.attempts.iter().map(|a| a.session_id))
            .collect()
    }
    pub(crate) fn ensure_workspace_mutation_allowed(&self, path: &Path) -> anyhow::Result<()> {
        ensure_workspace_available(&self.records, path)
    }
    pub(crate) fn validate_changes(
        &self,
        state: &PersistedState,
        tasks: &[Task],
        projects: &[crate::model::Project],
        sessions: &[AgentSession],
    ) -> anyhow::Result<()> {
        let unfinished = |task_id| {
            self.records
                .iter()
                .any(|r| r.run.task_id == task_id && r.reserves_workspace())
        };
        for task in tasks {
            if let Some(old) = state.tasks.iter().find(|old| old.id == task.id)
                && task.updated_at > old.updated_at
                && unfinished(task.id)
                && (task.state.is_done() || task.workspace != old.workspace)
            {
                bail!(
                    "Cancel the issue's automatic run before marking it Done or changing its workspace"
                );
            }
        }
        for project in projects {
            if self.records.iter().any(|r| {
                r.run.project_id == project.id
                    && r.reserves_workspace()
                    && r.project_path != project.path
            }) {
                bail!("Cancel active issue runs before changing their project directory");
            }
        }
        let owned_sessions = self.owned_sessions();
        for session in sessions {
            if session.task_id.is_some_and(unfinished)
                && session.status.is_busy()
                && !owned_sessions.contains(&session.id)
            {
                bail!(
                    "An automatic run owns this issue's workspace; a separate running conversation cannot be filed into it"
                );
            }
        }
        Ok(())
    }
}

pub(crate) struct IssueRunManager {
    inner: Arc<Inner>,
}
impl Drop for IssueRunManager {
    fn drop(&mut self) {
        self.inner.shutdown.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut workers = self.inner.workers.lock();
        while !workers.is_empty() && Instant::now() < deadline {
            self.inner
                .workers_changed
                .wait_for(&mut workers, Duration::from_millis(100));
        }
    }
}

impl IssueRunManager {
    pub(crate) fn new(
        store: Arc<StateStore>,
        state: Arc<Mutex<PersistedState>>,
        settings: Arc<DaemonSettingsStore>,
        sessions: Sessions,
        exits: Exits,
    ) -> anyhow::Result<Self> {
        let path = store.path().with_extension("issue-runs.json");
        let (mut records, mut load_error): (Vec<Record>, Option<String>) = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(records) => (records, None),
                Err(error) => (
                    Vec::new(),
                    Some(format!(
                        "Issue run journal is invalid; automation is unavailable and no runs were replayed: {error}"
                    )),
                ),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Vec::new(), None),
            Err(error) => (
                Vec::new(),
                Some(format!("Could not read the issue run journal: {error}")),
            ),
        };
        let mut recovered = false;
        for record in &mut records {
            if record.run.status.is_active()
                || record
                    .run
                    .attempts
                    .last()
                    .is_some_and(|a| a.status == IssueAttemptStatus::Running)
            {
                record.run.status = IssueRunStatus::Blocked;
                record.run.error = Some("Daemon restarted during this run. Inspect the last attempt and workspace, then explicitly Retry; no stage was replayed.".into());
                if let Some(attempt) = record
                    .run
                    .attempts
                    .last_mut()
                    .filter(|a| a.status == IssueAttemptStatus::Running)
                {
                    attempt.status = IssueAttemptStatus::Interrupted;
                    attempt.error = record.run.error.clone();
                    attempt.finished_at = Some(unix_time());
                }
                record.run.touch();
                recovered = true;
            }
        }
        if recovered && let Err(error) = save_journal(&path, &records) {
            load_error = Some(format!(
                "Could not persist interrupted-run recovery; automation is unavailable: {error:#}"
            ));
        }
        // Include previously parked attempts too: if the daemon crashed
        // between journal recovery and session recovery, this is idempotent.
        let interrupted_sessions = records
            .iter()
            .flat_map(|r| &r.run.attempts)
            .filter(|a| a.status == IssueAttemptStatus::Interrupted)
            .map(|a| a.session_id)
            .collect::<Vec<_>>();
        if load_error.is_none() && !interrupted_sessions.is_empty() {
            let recover_sessions = (|| -> anyhow::Result<()> {
                let mut state = state.lock();
                for id in interrupted_sessions {
                    if !state
                        .sessions
                        .iter()
                        .any(|session| session.id == id && session.status.is_busy())
                    {
                        continue;
                    }
                    if let Some(session) = state.session_mut(id) {
                        store.hydrate(session)?;
                        session.finish_active_turn(TurnStatus::Interrupted);
                        session.status = SessionStatus::Idle;
                        session.touch();
                    }
                }
                store.save(&mut state)?;
                Ok(())
            })();
            if let Err(error) = recover_sessions {
                load_error = Some(format!(
                    "Could not recover interrupted stage conversations: {error:#}"
                ));
            }
        }
        let manager = Self {
            inner: Arc::new(Inner {
                path,
                load_error,
                records: Mutex::new(records),
                workers: Mutex::new(HashSet::new()),
                workers_changed: Condvar::new(),
                shutdown: AtomicBool::new(false),
                store,
                state,
                settings,
                sessions,
                exits,
                harness: Mutex::new(Arc::new(LocalHarness)),
            }),
        };
        if manager.inner.load_error.is_none() {
            let pending = manager
                .inner
                .records
                .lock()
                .iter()
                .filter(|r| r.completion_pending && r.run.status == IssueRunStatus::Completed)
                .map(|r| r.run.id)
                .collect::<Vec<_>>();
            for run_id in pending {
                // This only reconciles durable lifecycle state. No provider
                // is started and the completed report is never regenerated.
                if let Err(error) = manager.inner.apply_completion_stage(run_id) {
                    let _ = manager.inner.update(run_id, |r| {
                        r.run.error = Some(format!(
                            "Report saved; final board update needs attention: {error:#}"
                        ));
                        Ok(())
                    });
                }
            }
        }
        Ok(manager)
    }

    #[cfg(test)]
    pub(crate) fn set_harness(&self, harness: Arc<dyn IssueHarness>) {
        *self.inner.harness.lock() = harness;
    }

    pub(crate) fn ensure_loaded(&self) -> anyhow::Result<()> {
        if let Some(error) = &self.inner.load_error {
            bail!("{error}");
        }
        Ok(())
    }

    pub(crate) fn list(&self, task_id: Option<Uuid>) -> anyhow::Result<Vec<IssueRun>> {
        self.ensure_loaded()?;
        let mut runs = self
            .inner
            .records
            .lock()
            .iter()
            .filter(|r| task_id.is_none_or(|id| id == r.run.task_id))
            .map(|r| r.run.clone())
            .collect::<Vec<_>>();
        runs.sort_by_key(|run| std::cmp::Reverse((run.created_at, run.updated_at, run.id)));
        Ok(runs)
    }
    pub(crate) fn owned_sessions(&self) -> HashSet<Uuid> {
        self.inner
            .records
            .lock()
            .iter()
            .flat_map(|r| r.run.attempts.iter().map(|a| a.session_id))
            .collect()
    }
    pub(crate) fn run_for_session(&self, session_id: Uuid) -> Option<Uuid> {
        self.inner
            .records
            .lock()
            .iter()
            .find(|r| r.run.attempts.iter().any(|a| a.session_id == session_id))
            .map(|r| r.run.id)
    }
    pub(crate) fn session_is_owned(&self, session_id: Uuid) -> bool {
        self.owned_sessions().contains(&session_id)
    }
    pub(crate) fn mutation_guard(&self) -> IssueMutationGuard<'_> {
        IssueMutationGuard {
            records: self.inner.records.lock(),
        }
    }
    pub(crate) fn ensure_manual_work_allowed(
        &self,
        session_id: Uuid,
    ) -> anyhow::Result<IssueMutationGuard<'_>> {
        let guard = self.mutation_guard();
        let state = self.inner.state.lock();
        if let Some(task_id) = state
            .sessions
            .iter()
            .find(|s| s.id == session_id)
            .and_then(|s| s.task_id)
            && guard
                .records
                .iter()
                .any(|r| r.run.task_id == task_id && r.reserves_workspace())
        {
            bail!(
                "This issue has an unfinished automatic run. Cancel it before starting separate manual work in the same workspace"
            );
        }
        if let Some(session) = state.sessions.iter().find(|s| s.id == session_id) {
            let cwd = session.workspace.path().or_else(|| {
                state
                    .projects
                    .iter()
                    .find(|p| p.id == session.project_id)
                    .map(|p| p.path.as_path())
            });
            if let Some(cwd) = cwd {
                guard.ensure_workspace_mutation_allowed(cwd)?;
            }
        }
        Ok(guard)
    }
    /// Path-only workspace commands have no trustworthy session/issue identity.
    /// Hold the same reservation lock as Start until the filesystem mutation ends.
    pub(crate) fn ensure_workspace_mutation_allowed(
        &self,
        path: &Path,
    ) -> anyhow::Result<IssueMutationGuard<'_>> {
        let guard = self.mutation_guard();
        guard.ensure_workspace_mutation_allowed(path)?;
        Ok(guard)
    }
    pub(crate) fn ensure_session_mutable(&self, session_id: Uuid) -> anyhow::Result<()> {
        if self.owned_sessions().contains(&session_id) {
            bail!(
                "This conversation is a recorded automatic stage. Use the run controls, or create a separate manual conversation."
            );
        }
        Ok(())
    }
    pub(crate) fn start(
        &self,
        task_id: Uuid,
        request_id: Uuid,
        workflow: IssueWorkflow,
        events: EventSink,
    ) -> anyhow::Result<IssueRun> {
        self.ensure_loaded()?;
        workflow.validate().map_err(|message| anyhow!(message))?;
        if request_id.is_nil() {
            bail!("An issue run needs a nonempty request ID for durable retry safety");
        }
        let run = {
            let mut records = self.inner.records.lock();
            if let Some(existing) = records.iter().find(|r| {
                r.run.request_id == request_id
                    || r.request_aliases.iter().any(|(id, _)| *id == request_id)
            }) {
                let requested_workflow = existing
                    .request_aliases
                    .iter()
                    .find(|(id, _)| *id == request_id)
                    .map_or(&existing.run.workflow, |(_, workflow)| workflow);
                if existing.run.task_id != task_id || *requested_workflow != workflow {
                    bail!("This request ID already belongs to a different issue run");
                }
                return Ok(existing.run.clone());
            }
            if let Some(index) = records.iter().position(|r| {
                r.run.task_id == task_id
                    && (r.reserves_workspace() || self.inner.workers.lock().contains(&r.run.id))
            }) {
                // Remember the duplicate request too: an uncertain response
                // retried after completion must not create a new run.
                if records[index].request_aliases.len() >= 64 {
                    bail!("Too many duplicate start requests; use the existing run controls");
                }
                let mut candidate = records.clone();
                candidate[index]
                    .request_aliases
                    .push((request_id, workflow));
                save_journal(&self.inner.path, &candidate)?;
                *records = candidate;
                return Ok(records[index].run.clone());
            }
            let mut state = self.inner.state.lock();
            let task = state
                .tasks
                .iter()
                .find(|task| task.id == task_id)
                .cloned()
                .ok_or_else(|| anyhow!("Issue no longer exists"))?;
            let project_id = task
                .project_id()
                .ok_or_else(|| anyhow!("Choose a project workspace for this issue first"))?;
            let project = state
                .projects
                .iter()
                .find(|p| p.id == project_id)
                .ok_or_else(|| anyhow!("Issue project no longer exists"))?;
            if state.sessions.iter().any(|s| {
                s.task_id == Some(task_id)
                    && (s.status.is_busy() || self.inner.sessions.lock().contains_key(&s.id))
            }) {
                bail!("Stop this issue's manual conversations before starting its automatic run");
            }
            if records
                .iter()
                .filter(|r| r.run.task_id == task_id)
                .flat_map(|r| &r.run.attempts)
                .any(|a| {
                    self.inner
                        .exits
                        .lock()
                        .iter()
                        .any(|((id, _), exit)| *id == a.session_id && exit.is_unconfirmed_close())
                })
            {
                bail!("A previous issue runtime has not confirmed shutdown");
            }
            if let Some(cwd) = task_workspace_path(task.workspace.as_ref().unwrap(), &project.path)
            {
                ensure_workspace_available(&records, cwd)?;
                let cwd = workspace_lease_root(cwd)?;
                for session in &state.sessions {
                    let active = session.status.is_busy()
                        || self.inner.sessions.lock().contains_key(&session.id)
                        || self.inner.exits.lock().iter().any(|((id, _), exit)| {
                            *id == session.id && exit.is_unconfirmed_close()
                        });
                    if !active {
                        continue;
                    }
                    let session_cwd = session.workspace.path().or_else(|| {
                        state
                            .projects
                            .iter()
                            .find(|p| p.id == session.project_id)
                            .map(|p| p.path.as_path())
                    });
                    if let Some(session_cwd) = session_cwd {
                        let session_cwd = workspace_lease_root(session_cwd)?;
                        if cwd.overlaps(&session_cwd) {
                            bail!(
                                "Stop the active conversation in this workspace and wait for shutdown before starting an automatic run"
                            );
                        }
                    }
                }
            }
            for stage in &workflow.stages {
                if let Some(id) = stage.board_stage_id
                    && !project.board.stages.iter().any(|s| s.id == id)
                {
                    bail!("A workflow references a stage outside this project's board");
                }
            }
            // A new run sorts after the previous one even for same-second
            // reruns and after clock skew. Progress revisions are independent.
            let prior_created = records
                .iter()
                .filter(|r| r.run.task_id == task_id)
                .map(|r| r.run.created_at)
                .max()
                .unwrap_or(0);
            let now = unix_time().max(prior_created.saturating_add(1));
            let run = IssueRun {
                id: Uuid::new_v4(),
                request_id,
                task_id,
                project_id,
                issue_title: task.title.clone(),
                issue_description: task.description.clone(),
                workflow,
                status: IssueRunStatus::Running,
                current_stage: 0,
                attempts: Vec::new(),
                review_retries: 0,
                report: None,
                error: None,
                created_at: now,
                updated_at: now,
            };
            let record = Record {
                run: run.clone(),
                request_aliases: Vec::new(),
                project_path: project.path.clone(),
                base_branch: project.worktree_base_branch.clone(),
                completion_stage: workflow_completion_stage(&run.workflow, &project.board),
                completion_pending: false,
                workspace: task.workspace.clone().unwrap(),
            };
            let mut candidate = records.clone();
            candidate.push(record);
            save_journal(&self.inner.path, &candidate)?;
            *records = candidate;
            // Explicit Start reopens Done; default report completion does not archive.
            if let Some(task) = state.tasks.iter_mut().find(|t| t.id == task_id) {
                task.set_state(TaskState::Doing);
            }
            if let Err(error) = self.inner.store.save(&mut state) {
                // The durable reservation already exists, so make its recovery
                // explicit rather than leaving an unspawned Running run.
                if let Some(saved) = records.iter_mut().find(|r| r.run.id == run.id) {
                    saved.run.status = IssueRunStatus::Blocked;
                    saved.run.error = Some(format!(
                        "Could not prepare issue state: {error}; Retry after checking storage"
                    ));
                    saved.run.touch();
                }
                save_journal(&self.inner.path, &records)?;
                return Err(error.into());
            }
            run
        };
        events.task_state_changed();
        self.spawn(run.id, events)?;
        Ok(run)
    }

    pub(crate) fn control(
        &self,
        run_id: Uuid,
        action: IssueRunAction,
        events: EventSink,
    ) -> anyhow::Result<IssueRun> {
        self.ensure_loaded()?;
        let worker_running = self.inner.workers.lock().contains(&run_id);
        let mut launch = false;
        let mut cancel_session = None;
        let run = self.inner.update(run_id, |record| {
            let run = &mut record.run;
            match action {
                IssueRunAction::Pause => {
                    if run.status == IssueRunStatus::Running || (run.status == IssueRunStatus::Blocked && worker_running) { run.status = if worker_running { IssueRunStatus::PauseRequested } else { IssueRunStatus::Paused }; }
                }
                IssueRunAction::Resume => {
                    if run.status == IssueRunStatus::PauseRequested { run.status = IssueRunStatus::Running; }
                    else if run.status == IssueRunStatus::Paused {
                        if worker_running { bail!("The previous stage is still settling; resume again when it has stopped"); }
                        run.status = IssueRunStatus::Running; run.error = None; launch = true;
                    } else if run.status != IssueRunStatus::Running { bail!("Only a paused run can resume; use Retry for a blocked or failed stage"); }
                }
                IssueRunAction::Retry => {
                    if worker_running { bail!("The current provider is still active. Answer its request or cancel and wait for shutdown before retrying"); }
                    if !matches!(run.status, IssueRunStatus::Blocked | IssueRunStatus::Failed) { bail!("Only a blocked or failed run can retry its current stage"); }
                    if run.attempts.iter().any(|a| self.inner.exits.lock().iter().any(|((id, _), exit)| *id == a.session_id && exit.is_unconfirmed_close())) { bail!("The previous provider has not confirmed shutdown; retry would create competing workers"); }
                    if run.current_stage >= run.workflow.stages.len() { bail!("This run has no stage to retry"); }
                    run.status = IssueRunStatus::Running; run.error = None; launch = true;
                }
                IssueRunAction::Cancel => {
                    if run.status.is_terminal() { return Ok(()); }
                    if !worker_running && run.attempts.iter().any(|a| self.inner.exits.lock().iter().any(|((id, _), exit)| *id == a.session_id && exit.is_unconfirmed_close())) {
                        bail!("Provider shutdown is still unconfirmed. The run remains blocked to prevent another writer in its workspace");
                    }
                    run.status = if worker_running { IssueRunStatus::CancelRequested } else { IssueRunStatus::Cancelled };
                    cancel_session = run.attempts.last().map(|a| a.session_id);
                }
            }
            Ok(())
        })?;
        if let Some(id) = cancel_session
            && let Some((_, driver)) = self.inner.sessions.lock().get(&id)
        {
            driver.cancel();
        }
        events.task_state_changed();
        if launch {
            self.spawn(run_id, events)?;
        }
        Ok(run)
    }

    fn spawn(&self, run_id: Uuid, events: EventSink) -> anyhow::Result<()> {
        if !self.inner.workers.lock().insert(run_id) {
            return Ok(());
        }
        let inner = self.inner.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("doki-issue-run-{run_id}"))
            .spawn(move || {
                let result = inner.drive(run_id, &events);
                if let Err(error) = result {
                    let _ = inner.update(run_id, |record| {
                        if !record.run.status.is_terminal() {
                            record.run.status = IssueRunStatus::Blocked;
                        }
                        record.run.error = Some(format!("{error:#}"));
                        if let Some(attempt) = record
                            .run
                            .attempts
                            .last_mut()
                            .filter(|a| a.status == IssueAttemptStatus::Running)
                            && !inner.sessions.lock().contains_key(&attempt.session_id)
                            && !inner.exits.lock().iter().any(|((id, _), exit)| {
                                *id == attempt.session_id && exit.is_unconfirmed_close()
                            })
                        {
                            attempt.status = IssueAttemptStatus::Blocked;
                            attempt.error = record.run.error.clone();
                            attempt.finished_at = Some(unix_time());
                        }
                        Ok(())
                    });
                }
                inner.workers.lock().remove(&run_id);
                inner.workers_changed.notify_all();
                events.task_state_changed();
            });
        if let Err(error) = spawned {
            self.inner.workers.lock().remove(&run_id);
            self.inner.update(run_id, |r| {
                r.run.status = IssueRunStatus::Blocked;
                r.run.error = Some(error.to_string());
                Ok(())
            })?;
            return Err(error.into());
        }
        Ok(())
    }
}

impl Inner {
    fn update(
        &self,
        run_id: Uuid,
        change: impl FnOnce(&mut Record) -> anyhow::Result<()>,
    ) -> anyhow::Result<IssueRun> {
        let mut records = self.records.lock();
        let index = records
            .iter()
            .position(|r| r.run.id == run_id)
            .ok_or_else(|| anyhow!("Issue run no longer exists"))?;
        let mut record = records[index].clone();
        change(&mut record)?;
        record.run.touch();
        let mut candidate = records.clone();
        candidate[index] = record;
        save_journal(&self.path, &candidate)?;
        *records = candidate;
        Ok(records[index].run.clone())
    }
    fn record(&self, id: Uuid) -> anyhow::Result<Record> {
        self.records
            .lock()
            .iter()
            .find(|r| r.run.id == id)
            .cloned()
            .ok_or_else(|| anyhow!("Issue run no longer exists"))
    }
    fn drive(&self, run_id: Uuid, events: &EventSink) -> anyhow::Result<()> {
        let initial = self.record(run_id)?;
        if initial.run.status == IssueRunStatus::CancelRequested {
            self.update(run_id, |r| {
                r.run.status = IssueRunStatus::Cancelled;
                Ok(())
            })?;
            return Ok(());
        }
        if matches!(
            initial.run.status,
            IssueRunStatus::Paused | IssueRunStatus::PauseRequested
        ) {
            self.update(run_id, |r| {
                r.run.status = IssueRunStatus::Paused;
                Ok(())
            })?;
            return Ok(());
        }
        if initial.run.status.is_terminal() {
            return Ok(());
        }
        let harness = self.harness.lock().clone();
        for stage in &self.record(run_id)?.run.workflow.stages {
            harness.validate(stage, &self.settings.get())?;
        }
        self.materialize_workspace(run_id)?;
        loop {
            if self.shutdown.load(Ordering::Acquire) {
                bail!("Daemon stopped; inspect the last attempt before retrying");
            }
            let record = self.record(run_id)?;
            match record.run.status {
                IssueRunStatus::PauseRequested => {
                    self.update(run_id, |r| {
                        r.run.status = IssueRunStatus::Paused;
                        Ok(())
                    })?;
                    return Ok(());
                }
                IssueRunStatus::CancelRequested => {
                    self.update(run_id, |r| {
                        r.run.status = IssueRunStatus::Cancelled;
                        Ok(())
                    })?;
                    return Ok(());
                }
                IssueRunStatus::Running => (),
                _ => return Ok(()),
            }
            let Some(stage) = record.run.current_stage().cloned() else {
                bail!("Workflow has no final report");
            };
            self.run_stage(record, stage, harness.as_ref(), events)?;
        }
    }

    fn materialize_workspace(&self, run_id: Uuid) -> anyhow::Result<()> {
        let record = self.record(run_id)?;
        if let TaskWorkspace::NewWorktree { project_id } = record.workspace {
            let created = crate::worktree::create(
                &record.project_path,
                project_id,
                run_id,
                &record.run.issue_title,
                record.base_branch.as_deref(),
            )?;
            self.update(run_id, |r| {
                r.workspace = TaskWorkspace::Worktree {
                    project_id,
                    path: created.path,
                    branch: created.branch,
                };
                Ok(())
            })?;
        }
        let record = self.record(run_id)?;
        let mut state = self.state.lock();
        let task = state
            .tasks
            .iter_mut()
            .find(|t| t.id == record.run.task_id)
            .ok_or_else(|| anyhow!("Issue was removed"))?;
        task.workspace = Some(record.workspace);
        task.set_state(TaskState::Doing);
        task.touch();
        self.store.save(&mut state)?;
        Ok(())
    }

    fn run_stage(
        &self,
        record: Record,
        stage: IssueWorkflowStage,
        harness: &dyn IssueHarness,
        root_events: &EventSink,
    ) -> anyhow::Result<()> {
        let run_id = record.run.id;
        let cwd = match &record.workspace {
            TaskWorkspace::Local { .. } => record.project_path.clone(),
            TaskWorkspace::Worktree { path, .. } => path.clone(),
            _ => bail!("Issue workspace is not ready"),
        };
        if !cwd.is_dir() {
            bail!("Issue workspace is unavailable: {}", cwd.display());
        }
        let mut session = AgentSession::new(record.run.project_id, stage.provider);
        session.title = format!("{} · {}", record.run.issue_title, stage.name);
        session.task_id = Some(record.run.task_id);
        session.model = stage.model.clone();
        session.runtime_mode = RuntimeMode::Auto;
        session.workspace = match &record.workspace {
            TaskWorkspace::Worktree { path, branch, .. } => SessionWorkspace::Worktree {
                path: path.clone(),
                branch: branch.clone(),
            },
            _ => SessionWorkspace::Local,
        };
        let attempt = IssueStageAttempt {
            id: Uuid::new_v4(),
            stage_id: stage.id,
            session_id: session.id,
            status: IssueAttemptStatus::Running,
            outcome: None,
            error: None,
            started_at: unix_time(),
            finished_at: None,
        };
        let board_revision = self
            .state
            .lock()
            .projects
            .iter()
            .find(|p| p.id == record.run.project_id)
            .map_or(0, |p| p.board.revision);
        session.stage_invocation = Some(SessionStageInvocation {
            id: attempt.id,
            board_revision,
            source: None,
            stage: BoardStage {
                id: stage.board_stage_id.unwrap_or(stage.id),
                name: stage.name.clone(),
                state: TaskState::Doing,
                provider: Some(stage.provider),
                model: stage.model.clone(),
                instructions: stage.instructions.clone(),
                handoff: StageHandoff::PriorResult,
            },
        });
        let prompt = stage_prompt(&record.run, &stage, &attempt);
        let turn_id = session.begin_turn(prompt.clone());
        session.status = SessionStatus::Connecting;
        let session_id = session.id;
        let message_id = session.messages.last().unwrap().id;
        // Reserve the attempt before starting any provider process. A crash
        // anywhere below parks this exact reservation rather than replaying it.
        let mut reserved = false;
        self.update(run_id, |r| {
            if r.run.status == IssueRunStatus::Running {
                r.run.attempts.push(attempt.clone());
                r.run.error = None;
                reserved = true;
            }
            Ok(())
        })?;
        if !reserved {
            return Ok(());
        }
        {
            let mut state = self.state.lock();
            state.push_session(session);
            self.store.save(&mut state)?;
        }
        let runtime_id = Uuid::new_v4();
        let events = root_events.for_runtime(session_id, runtime_id);
        events.task_state_changed();
        let (wake, _wakes) = smol::channel::bounded(1);
        let (sender, receiver) = driver::event_channel(wake);
        let handle = match harness.start(&stage, cwd.clone(), &self.settings.get(), sender) {
            Ok(handle) => handle,
            Err(error) => {
                self.finish_session(session_id, String::new(), TurnStatus::Failed)?;
                self.finish_attempt(
                    run_id,
                    attempt.id,
                    Err((
                        IssueRunStatus::Failed,
                        format!("Could not start {}: {error:#}", stage.name),
                    )),
                )?;
                return Ok(());
            }
        };
        let exit = Arc::new(RuntimeExit::default());
        self.exits
            .lock()
            .insert((session_id, runtime_id), exit.clone());
        self.sessions
            .lock()
            .insert(session_id, (runtime_id, handle.clone()));
        // From this point every fallible event projection is collected below;
        // it cannot bypass the runtime's mandatory shutdown path.
        let _ = events.send(waku_protocol::event_to_wire(
            DriverEvent::PromptSubmitted {
                message: prompt.clone(),
                turn_id,
                message_id,
            },
        )?);
        handle.prompt(prompt);
        let mut output = String::new();
        let mut output_too_large = false;
        let mut failure = None;
        let mut cancel_sent = false;
        let mut process_exited = false;
        let progress = (|| -> anyhow::Result<(bool, bool, Option<String>)> {
            let finished = loop {
                if self.shutdown.load(Ordering::Acquire) {
                    handle.cancel();
                    break (false, true, Some("Daemon shutting down".into()));
                }
                if self.record(run_id)?.run.status == IssueRunStatus::CancelRequested
                    && !cancel_sent
                {
                    handle.cancel();
                    cancel_sent = true;
                }
                let event = match receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(event) => event,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                    Err(_) => {
                        break (
                            false,
                            false,
                            Some("Provider event stream closed without a completed turn".into()),
                        );
                    }
                };
                let finished = match &event {
                    DriverEvent::TextDelta(text) => {
                        if output.len().saturating_add(text.len()) <= MAX_PUBLIC_OUTPUT {
                            output.push_str(text);
                        } else {
                            output_too_large = true;
                            handle.cancel();
                        }
                        None
                    }
                    DriverEvent::Connected { provider_cursor } => {
                        self.session_update(session_id, |s| {
                            s.provider_cursor = provider_cursor.clone();
                        })?;
                        None
                    }
                    DriverEvent::TurnStarted => {
                        self.session_update(session_id, |s| {
                            s.status = SessionStatus::Working;
                            s.mark_active_turn_provider_started();
                        })?;
                        None
                    }
                    DriverEvent::Permission { title, .. }
                    | DriverEvent::UserInputRequested {
                        request_id: title, ..
                    } => {
                        self.update(run_id, |r| {
                            if r.run.status == IssueRunStatus::Running {
                                r.run.status = IssueRunStatus::Blocked;
                            }
                            r.run.error = Some(format!(
                                "Waiting for your response in the {} conversation: {title}",
                                stage.name
                            ));
                            Ok(())
                        })?;
                        self.session_update(session_id, |s| s.status = SessionStatus::Waiting)?;
                        root_events.task_state_changed();
                        None
                    }
                    DriverEvent::TurnParked => {
                        self.session_update(session_id, |s| s.status = SessionStatus::Background)?;
                        None
                    }
                    DriverEvent::Error(error) => {
                        failure = Some(error.clone());
                        self.update(run_id, |r| {
                            if !matches!(r.run.status, IssueRunStatus::PauseRequested | IssueRunStatus::CancelRequested) {
                                r.run.status = IssueRunStatus::Blocked;
                            }
                            r.run.error = Some(format!("Provider reported an error in {}: {error}. The provider is still active; open the stage conversation or cancel before retrying.", stage.name));
                            Ok(())
                        })?;
                        root_events.task_state_changed();
                        None
                    }
                    DriverEvent::ProcessExited => {
                        exit.observe();
                        process_exited = true;
                        Some((
                            false,
                            false,
                            Some("Provider exited before finishing the stage".into()),
                        ))
                    }
                    DriverEvent::TurnFinished {
                        success,
                        interrupted,
                        summary,
                    } => Some((*success, *interrupted, summary.clone())),
                    _ => None,
                };
                let _ = events.send(waku_protocol::event_to_wire(event)?);
                if let Some(finished) = finished {
                    break finished;
                }
            };
            Ok(finished)
        })();
        let (success, interrupted, summary) = match progress {
            Ok(finished) => finished,
            Err(error) => {
                handle.cancel();
                (
                    false,
                    false,
                    Some(format!("Could not persist stage progress: {error:#}")),
                )
            }
        };
        // A completed turn is not proof the process has stopped writing. Close
        // the owned runtime and wait for positive exit/session-close evidence.
        exit.request_close();
        let shared_shutdown = handle.confirm_session_shutdown();
        if let Some(Ok(())) = &shared_shutdown {
            exit.observe();
            process_exited = true;
        }
        self.sessions.lock().remove(&session_id);
        drop(handle);
        let waiting_since = Instant::now();
        let mut warned = false;
        while !process_exited && !self.shutdown.load(Ordering::Acquire) {
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(DriverEvent::ProcessExited) => {
                    exit.observe();
                    process_exited = true;
                }
                Ok(_) => (),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => (),
                Err(_) => {
                    failure = Some("Provider closed its event channel without confirming shutdown. Restart the daemon only after checking the provider process.".into());
                    break;
                }
            }
            if !warned && waiting_since.elapsed() > Duration::from_secs(10) {
                warned = true;
                self.update(run_id, |r| { if !matches!(r.run.status, IssueRunStatus::CancelRequested | IssueRunStatus::PauseRequested) { r.run.status = IssueRunStatus::Blocked; } r.run.error = Some("Waiting for the provider to confirm shutdown; another stage will not start alongside it".into()); Ok(()) })?;
                root_events.task_state_changed();
            }
        }
        if let Some(Err(error)) = shared_shutdown {
            failure = Some(format!("Provider shutdown failed: {error:#}"));
        }
        if !process_exited {
            failure.get_or_insert_with(|| "Provider shutdown was not confirmed".into());
        }
        let cancelled =
            cancel_sent || self.record(run_id)?.run.status == IssueRunStatus::CancelRequested;
        let result = if self.shutdown.load(Ordering::Acquire) {
            Err((IssueRunStatus::Blocked, "Daemon stopped during this attempt. Inspect its workspace before explicitly retrying.".into()))
        } else if !process_exited {
            Err((IssueRunStatus::Blocked, failure.unwrap_or_default()))
        } else if cancelled {
            Err((IssueRunStatus::Cancelled, "Cancelled by the user".into()))
        } else if output_too_large {
            Err((
                IssueRunStatus::Blocked,
                "Stage output exceeded the 256 KiB limit".into(),
            ))
        } else if !success || interrupted {
            Err((
                IssueRunStatus::Failed,
                summary
                    .clone()
                    .or(failure)
                    .unwrap_or_else(|| "Provider did not successfully finish the stage".into()),
            ))
        } else {
            parse_outcome(&output, summary.as_deref(), run_id, attempt.id, stage.role)
                .map_err(|error| (IssueRunStatus::Blocked, error.to_string()))
        };
        let turn_status = if cancelled || interrupted {
            TurnStatus::Interrupted
        } else if success && process_exited {
            TurnStatus::Completed
        } else {
            TurnStatus::Failed
        };
        self.finish_session(session_id, output, turn_status)?;
        self.finish_attempt(run_id, attempt.id, result)?;
        self.apply_completion_stage(run_id)?;
        events.end_runtime();
        root_events.task_state_changed();
        Ok(())
    }

    fn apply_completion_stage(&self, run_id: Uuid) -> anyhow::Result<()> {
        let record = self.record(run_id)?;
        if record.run.status != IssueRunStatus::Completed || !record.completion_pending {
            return Ok(());
        }
        let mut state = self.state.lock();
        // A configuration edit cannot silently substitute another terminal
        // destination. Invalid mappings are durably abandoned so manual Done
        // and a fresh run remain possible; transient storage errors retain the
        // pending marker and are retried at startup.
        let invalid = match record.completion_stage.as_ref() {
            None => Some(
                "Report completed, but its configured terminal board stage is unavailable; move the issue manually",
            ),
            Some(stage)
                if !state.projects.iter().any(|p| {
                    p.id == record.run.project_id
                        && p.board
                            .stages
                            .iter()
                            .any(|s| s.id == stage.id && s.state.is_done())
                }) =>
            {
                Some(
                    "Report completed, but its configured terminal board stage changed; move the issue manually",
                )
            }
            Some(_) if !state.tasks.iter().any(|t| t.id == record.run.task_id) => {
                Some("Report completed, but its issue no longer exists")
            }
            _ => None,
        };
        if let Some(message) = invalid {
            drop(state);
            self.update(run_id, |r| {
                r.completion_pending = false;
                r.run.error = Some(message.into());
                Ok(())
            })?;
            bail!("{message}");
        }
        let stage = record.completion_stage.unwrap();
        let index = state
            .tasks
            .iter()
            .position(|t| t.id == record.run.task_id)
            .unwrap();
        // A replay after the DB write but before the marker clear is a no-op.
        if state.tasks[index].stage_id != Some(stage.id) || !state.tasks[index].state.is_done() {
            let previous = state.tasks[index].clone();
            let position = state.tasks[index].position;
            state.tasks[index].set_stage(&stage, position);
            if let Err(error) = self.store.save(&mut state) {
                state.tasks[index] = previous;
                return Err(error.into());
            }
        }
        drop(state);
        self.update(run_id, |r| {
            r.completion_pending = false;
            r.run.error = None;
            Ok(())
        })?;
        Ok(())
    }

    fn session_update(
        &self,
        session_id: Uuid,
        change: impl FnOnce(&mut AgentSession),
    ) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        let session = state
            .session_mut(session_id)
            .ok_or_else(|| anyhow!("Stage session was removed"))?;
        change(session);
        session.touch();
        self.store.save(&mut state)?;
        Ok(())
    }
    fn finish_session(
        &self,
        session_id: Uuid,
        output: String,
        status: TurnStatus,
    ) -> anyhow::Result<()> {
        self.session_update(session_id, |session| {
            if !output.is_empty()
                && let Some(turn) = session.turns.last()
            {
                session.messages.push(Message::new_for_turn(
                    MessageRole::Assistant,
                    output,
                    turn.id,
                ));
            }
            session.finish_active_turn(status);
            session.status = if status == TurnStatus::Failed {
                SessionStatus::Failed
            } else {
                SessionStatus::Idle
            };
        })
    }
    fn finish_attempt(
        &self,
        run_id: Uuid,
        attempt_id: Uuid,
        result: Result<IssueStageOutcome, (IssueRunStatus, String)>,
    ) -> anyhow::Result<()> {
        self.update(run_id, |record| {
            let run = &mut record.run;
            let Some(attempt) = run.attempts.last_mut().filter(|a| a.id == attempt_id && a.status == IssueAttemptStatus::Running) else { return Ok(()); };
            attempt.finished_at = Some(unix_time());
            match result {
                Err((status, error)) => {
                    attempt.status = match status { IssueRunStatus::Cancelled => IssueAttemptStatus::Cancelled, IssueRunStatus::Failed => IssueAttemptStatus::Failed, _ => IssueAttemptStatus::Blocked };
                    attempt.error = Some(error.clone()); run.status = status; run.error = Some(error);
                }
                Ok(outcome) => {
                    attempt.status = match outcome.decision { IssueStageDecision::Rejected => IssueAttemptStatus::Rejected, IssueStageDecision::Blocked => IssueAttemptStatus::Blocked, _ => IssueAttemptStatus::Completed };
                    attempt.outcome = Some(outcome.clone());
                    run.error = None;
                    if outcome.decision == IssueStageDecision::Blocked {
                        run.status = IssueRunStatus::Blocked; run.error = Some(outcome.summary); return Ok(());
                    }
                    if outcome.decision == IssueStageDecision::Rejected {
                        if run.review_retries >= run.workflow.max_review_retries {
                            run.status = IssueRunStatus::Blocked;
                            run.error = Some(format!("Review rejected after {} correction rounds. Inspect the findings before retrying or cancelling.", run.review_retries));
                            return Ok(());
                        }
                        run.review_retries += 1;
                        run.current_stage = run.workflow.stages[..run.current_stage].iter().rposition(|s| s.role == IssueStageRole::Execution)
                            .ok_or_else(|| anyhow!("Review has no preceding execution stage"))?;
                    } else {
                        if run.workflow.stages[run.current_stage].role == IssueStageRole::Report { run.report = Some(outcome); }
                        run.current_stage += 1;
                    }
                    if run.current_stage == run.workflow.stages.len() { run.status = IssueRunStatus::Completed; record.completion_pending = record.completion_stage.is_some(); }
                    else if run.status == IssueRunStatus::PauseRequested { run.status = IssueRunStatus::Paused; }
                    else if run.status == IssueRunStatus::CancelRequested { run.status = IssueRunStatus::Cancelled; }
                    else { run.status = IssueRunStatus::Running; }
                }
            }
            Ok(())
        })?;
        Ok(())
    }
}

fn task_workspace_path<'a>(
    workspace: &'a TaskWorkspace,
    project_path: &'a Path,
) -> Option<&'a Path> {
    match workspace {
        TaskWorkspace::Local { .. } => Some(project_path),
        TaskWorkspace::Worktree { path, .. } => Some(path),
        TaskWorkspace::NewWorktree { .. } => None,
    }
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn ensure_workspace_available(records: &[Record], path: &Path) -> anyhow::Result<()> {
    let mut requested = None;
    for record in records.iter().filter(|r| r.reserves_workspace()) {
        let Some(workspace) = task_workspace_path(&record.workspace, &record.project_path) else {
            continue;
        };
        if requested.is_none() {
            requested = Some(workspace_lease_root(path)?);
        }
        let requested = requested.as_ref().unwrap();
        if requested.overlaps(&workspace_lease_root(workspace)?) {
            bail!(
                "An unfinished automatic run owns this workspace. Cancel it and wait for shutdown before changing workspace files or Git state"
            );
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct WorkspaceLeaseRoot {
    path: PathBuf,
    git: bool,
}
impl WorkspaceLeaseRoot {
    fn overlaps(&self, other: &Self) -> bool {
        if self.git && other.git {
            // A worktree nested inside another checkout still has its own
            // index and branch, so it is an independent execution workspace.
            self.path == other.path
        } else {
            paths_overlap(&self.path, &other.path)
        }
    }
}

/// Sibling subprojects share a Git index/branch. Lease their checkout root,
/// never the common Git directory (distinct worktrees must stay independent).
/// This coordinates daemon agent starts and built-in workspace mutations; it
/// is not an OS sandbox. User terminals and external editors remain available.
fn workspace_lease_root(path: &Path) -> anyhow::Result<WorkspaceLeaseRoot> {
    let canonical = canonical_mutation_path(path)?;
    let directory = canonical
        .ancestors()
        .find(|p| p.is_dir())
        .ok_or_else(|| anyhow!("Workspace directory is unavailable: {}", path.display()))?;
    let output = crate::command_env::plain_command("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(directory)
        .output()
        .context("Could not inspect workspace ownership")?;
    if output.status.success() {
        let root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        return Ok(WorkspaceLeaseRoot {
            path: canonical_mutation_path(&root)?,
            git: true,
        });
    }
    Ok(WorkspaceLeaseRoot {
        path: canonical,
        git: false,
    })
}

/// Resolve symlink aliases even for a file that does not exist yet. Resolve
/// each existing parent before appending missing path components; reject paths
/// that cannot be resolved rather than bypassing an active workspace lease.
fn canonical_mutation_path(path: &Path) -> anyhow::Result<PathBuf> {
    if path.exists() {
        return fs::canonicalize(path)
            .with_context(|| format!("Could not resolve {}", path.display()));
    }
    let parent = path
        .parent()
        .filter(|p| *p != path)
        .ok_or_else(|| anyhow!("Could not resolve workspace path {}", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("Invalid workspace path {}", path.display()))?;
    Ok(canonical_mutation_path(if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    })?
    .join(name))
}

fn workflow_completion_stage(
    workflow: &IssueWorkflow,
    board: &crate::model::ProjectBoard,
) -> Option<BoardStage> {
    let final_stage = workflow.stages.last()?;
    if final_stage.role != IssueStageRole::Report {
        return None;
    }
    board
        .stages
        .iter()
        .find(|stage| Some(stage.id) == final_stage.board_stage_id && stage.state.is_done())
        .cloned()
}

fn save_journal(path: &Path, records: &[Record]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Issue journal has no directory"))?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("issue-runs-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        serde_json::to_writer(&mut file, records)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn stage_prompt(run: &IssueRun, stage: &IssueWorkflowStage, attempt: &IssueStageAttempt) -> String {
    let example = IssueStageOutcome {
        run_id: run.id,
        attempt_id: attempt.id,
        decision: if stage.role == IssueStageRole::Review {
            IssueStageDecision::Approved
        } else {
            IssueStageDecision::Completed
        },
        summary: "Specific public result and any remaining limitations".into(),
        evidence: vec![
            "Actual verification performed and its observed result, or explicitly not run".into(),
        ],
    };
    let mut prompt = format!(
        "You are executing one stage of an explicitly requested Doki issue workflow.\nIssue: {}\n\n{}\n\nStage: {} ({:?})\n{}\n\nWork only in this issue's assigned workspace. Do not merge, deploy, publish, or mark the issue Done. Do not claim tests ran without evidence. Never disclose private reasoning. If user input or permission is required, ask through the harness instead of assuming approval.\n",
        run.issue_title, run.issue_description, stage.name, stage.role, stage.instructions
    );
    if !run.attempts.is_empty() {
        prompt.push_str("\nAttempt manifest (all attempts; evidence is public, bounded excerpts may be incomplete):\n");
        for prior in &run.attempts {
            let role = run
                .workflow
                .stages
                .iter()
                .find(|s| s.id == prior.stage_id)
                .map(|s| s.role);
            prompt.push_str(&format!(
                "Stage {} ({role:?}); status {:?}; attempt {}; session {}\n",
                prior.stage_id, prior.status, prior.id, prior.session_id
            ));
            if let Some(outcome) = &prior.outcome {
                let excerpt = format!("{}\n{}", outcome.summary, outcome.evidence.join("\n"));
                prompt.extend(excerpt.chars().take(600));
                if excerpt.chars().count() > 600 {
                    prompt.push_str(
                        " [excerpt truncated; inspect the referenced attempt for full evidence]",
                    );
                }
                prompt.push('\n');
            }
        }
    }
    for prior in run
        .attempts
        .iter()
        .filter_map(|a| a.outcome.as_ref().map(|o| (a, o)))
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        let (attempt, outcome) = prior;
        prompt.push_str(&format!("\nPrior public stage result (background to verify independently):\nAttempt: {}\nSession: {}\nDecision: {:?}\n", attempt.id, attempt.session_id, outcome.decision));
        let public = format!(
            "{}\nEvidence:\n{}",
            outcome.summary,
            outcome.evidence.join("\n")
        );
        prompt.extend(public.chars().take(MAX_HANDOFF_CHARS));
        if public.chars().count() > MAX_HANDOFF_CHARS {
            prompt.push_str("\n[Public result excerpt truncated; inspect the referenced attempt for full evidence]");
        }
    }
    prompt.push_str("\nEnd your final public response with exactly this structured result fence, using your real summary and evidence. The IDs must be copied exactly. Use decision completed for ordinary stages; a review MUST use approved or rejected. Use blocked when unable to proceed. This envelope is required even if the work could not be completed. Do not put any text after the fence.\n");
    prompt.push_str(RESULT_MARKER);
    prompt.push_str(&serde_json::to_string_pretty(&example).unwrap());
    prompt.push_str("\n```");
    prompt
}

fn parse_outcome(
    output: &str,
    summary: Option<&str>,
    run_id: Uuid,
    attempt_id: Uuid,
    role: IssueStageRole,
) -> anyhow::Result<IssueStageOutcome> {
    let parse = |text: &str| -> anyhow::Result<IssueStageOutcome> {
        if text.len() > MAX_PUBLIC_OUTPUT {
            bail!("Structured stage result exceeds the output limit");
        }
        let (_, suffix) = text.rsplit_once(RESULT_MARKER).ok_or_else(|| anyhow!("Stage ended without a structured doki-stage-result. Inspect the conversation, then Retry."))?;
        let json = suffix
            .trim()
            .strip_suffix("```")
            .ok_or_else(|| anyhow!("Structured result must be the final response fence"))?
            .trim();
        let outcome: IssueStageOutcome = serde_json::from_str(json)
            .context("Stage result does not match the required schema")?;
        if outcome.run_id != run_id || outcome.attempt_id != attempt_id {
            bail!("Stage result belongs to a different run or attempt");
        }
        if outcome.summary.trim().is_empty()
            || outcome.summary.len() > 32_000
            || outcome.evidence.is_empty()
            || outcome.evidence.len() > 64
            || outcome
                .evidence
                .iter()
                .any(|e| e.trim().is_empty() || e.len() > 8_000)
        {
            bail!("Stage result needs a nonempty bounded summary and concrete evidence");
        }
        if role == IssueStageRole::Review {
            if outcome.decision == IssueStageDecision::Completed {
                bail!("Review must explicitly approve, reject, or block");
            }
        } else if matches!(
            outcome.decision,
            IssueStageDecision::Approved | IssueStageDecision::Rejected
        ) {
            bail!("Only a review stage can approve or reject");
        }
        Ok(outcome)
    };
    if let Some(summary) = summary.filter(|s| s.contains(RESULT_MARKER)) {
        parse(summary)
    } else {
        parse(output)
    }
}

#[cfg(test)]
mod tests;
