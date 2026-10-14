//! Provider backend and driver-event wire translation for `waku-daemon`.

mod automation_headless;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use crate::{
    Backend, Command, EventSink, Request, ResponsePayload, WireDriverEvent, WorkspaceOperation,
    WorkspaceResult,
};
use anyhow::{Context as _, anyhow, bail};
use parking_lot::{Condvar, Mutex};
use serde_json::Value;
use uuid::Uuid;

use crate::attachments::AttachmentStore;
use crate::driver::{self, DriverHandle, DriverStartOptions, SessionOptions};
use crate::model::{
    AgentSession, Checkpoint, CheckpointStatus, DriverEvent, GoalOperation, Project, ProviderKind,
    ProviderResumeCursor, SessionStatus, Task, TaskState, ThreadGoalStatus,
};
use crate::persistence::{ComposerDraftStore, PersistedState, StateStore};
use crate::settings::DaemonSettingsStore;
use waku_protocol::provider_session::{ProviderSessionFork, ProviderSessionForkRequest};
// The driver-event wire translation lives with the wire types it speaks, so the
// daemon and every client encode and decode one implementation.
use waku_protocol::{TerminalSnapshot, decode_enum, event_to_wire};

/// How many fully hydrated transcripts the daemon keeps resident.
///
/// Hydration is a cache: consumers reload a released session from the store on
/// demand. Without a cap, a daemon that lives for days adopts the transcript
/// of every session its clients have touched — SaveTaskState pushes, hydrate
/// requests, forks, checkpoints — and resident memory grows without bound.
const RESIDENT_TRANSCRIPT_WINDOW: usize = 24;

/// Releases resident transcripts beyond the recency window after a save.
/// `pinned` names sessions with live runtimes; dirty sessions are skipped
/// inside [`PersistedState::trim_idle_transcripts`] because they hold unsaved
/// work.
fn trim_resident_transcripts(state: &mut PersistedState, pinned: &HashSet<Uuid>) {
    state.trim_idle_transcripts(pinned, RESIDENT_TRANSCRIPT_WINDOW);
}

/// A daemon-owned terminal together with the ids that scope it.
///
/// The envelope's session id is the terminal itself, the runtime id rejects
/// writes from a client bound to a superseded one, and the owning task is what
/// task removal disposes.
struct OwnedTerminal {
    task_id: Uuid,
    runtime_id: Uuid,
    terminal: crate::terminal::DaemonTerminal,
}

/// Records positive process-exit or owned-session shutdown evidence independently
/// of client subscriptions. Shared services retain their user-owned process.
/// A timed-out close keeps this latch, so a retry cannot mistake absence for exit.
#[derive(Default)]
struct RuntimeExit {
    close_requested: AtomicBool,
    exited: Mutex<bool>,
    changed: Condvar,
}

impl RuntimeExit {
    fn request_close(&self) {
        self.close_requested.store(true, Ordering::Release);
    }

    fn is_unconfirmed_close(&self) -> bool {
        self.close_requested.load(Ordering::Acquire) && !*self.exited.lock()
    }

    fn observe(&self) {
        *self.exited.lock() = true;
        self.changed.notify_all();
    }

    fn wait(&self, timeout: std::time::Duration) -> anyhow::Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        let mut exited = self.exited.lock();
        while !*exited {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                bail!("the provider has not confirmed shutdown; the issue remains active");
            }
            self.changed.wait_for(&mut exited, remaining);
        }
        Ok(())
    }
}

pub struct WakuBackend {
    headless_identity: OnceLock<waku_protocol::AutomationInstance>,
    scratch: Arc<crate::automation_scratch::ScratchManager>,
    automation_jobs: Option<
        Result<crate::automation_jobs::AutomationJobManager, waku_protocol::AutomationError>,
    >,
    sessions: Mutex<HashMap<Uuid, (Uuid, DriverHandle)>>,
    runtime_exits: Mutex<HashMap<(Uuid, Uuid), Arc<RuntimeExit>>>,
    terminals: Mutex<HashMap<Uuid, OwnedTerminal>>,
    #[cfg(all(test, unix))]
    terminal_shell: Option<alacritty_terminal::tty::Shell>,
    settings: DaemonSettingsStore,
    task_store: StateStore,
    task_state: Mutex<PersistedState>,
    removed_session_ids: Mutex<HashSet<Uuid>>,
    composer_drafts: ComposerDraftStore,
    attachments: AttachmentStore,
    usage_scan_cache: Mutex<crate::usage_history::ScanCache>,
    checkpoint_capture_locks: Mutex<HashMap<(PathBuf, Uuid, usize), Arc<Mutex<()>>>>,
    usage_rates_dir: std::path::PathBuf,
    default_cwd: std::path::PathBuf,
}

impl WakuBackend {
    pub fn new(settings: DaemonSettingsStore, task_store: StateStore) -> anyhow::Result<Self> {
        let mut task_state = task_store
            .load()
            .context("could not load Waku task database")?;
        migrate_projectless_state(&task_store, &mut task_state)?;
        let composer_drafts = ComposerDraftStore::for_state_path(task_store.path());
        let attachments = AttachmentStore::new(
            task_store
                .path()
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("attachments"),
        );
        let usage_rates_dir = task_store
            .path()
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_owned();
        let scratch = Arc::new(crate::automation_scratch::ScratchManager::new(
            &usage_rates_dir,
        ));
        let automation_jobs = cfg!(debug_assertions).then(|| {
            crate::automation_jobs::AutomationJobManager::new(scratch.clone(), &usage_rates_dir)
        });
        Ok(Self {
            headless_identity: OnceLock::new(),
            automation_jobs,
            scratch,
            sessions: Mutex::new(HashMap::new()),
            runtime_exits: Mutex::new(HashMap::new()),
            terminals: Mutex::new(HashMap::new()),
            #[cfg(all(test, unix))]
            terminal_shell: None,
            settings,
            task_store,
            task_state: Mutex::new(task_state),
            removed_session_ids: Mutex::new(HashSet::new()),
            composer_drafts,
            attachments,
            usage_scan_cache: Mutex::new(HashMap::new()),
            checkpoint_capture_locks: Mutex::new(HashMap::new()),
            usage_rates_dir,
            default_cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        })
    }

    fn close_session_and_wait(
        &self,
        session_id: Uuid,
        runtime_id: Uuid,
        timeout: std::time::Duration,
    ) -> anyhow::Result<()> {
        let exit = self
            .runtime_exits
            .lock()
            .get(&(session_id, runtime_id))
            .cloned()
            .ok_or_else(|| anyhow!("shutdown evidence for runtime {runtime_id} is unavailable"))?;
        exit.request_close();
        self.confirm_shared_session_shutdown(session_id, runtime_id)?;
        let removed = {
            let mut sessions = self.sessions.lock();
            // A retry can confirm its old runtime independently, but must
            // never shut down a replacement started by explicit continuation.
            if sessions
                .get(&session_id)
                .is_some_and(|(id, _)| *id == runtime_id)
            {
                sessions.remove(&session_id)
            } else {
                None
            }
        };
        // Driver Drop requests shutdown; only its ProcessExited event
        // confirms that the asynchronous process owner has settled.
        drop(removed);
        exit.wait(timeout)?;
        Ok(())
    }

    fn confirm_shared_session_shutdown(
        &self,
        session_id: Uuid,
        runtime_id: Uuid,
    ) -> anyhow::Result<()> {
        let driver = self
            .sessions
            .lock()
            .get(&session_id)
            .filter(|(id, _)| *id == runtime_id)
            .map(|(_, driver)| driver.clone());
        if let Some(driver) = driver
            && let Some(result) = driver.confirm_session_shutdown()
        {
            // OpenCode owns a native session on an adopted service, not that
            // process. Its explicit session settlement is the close boundary.
            result?;
            if let Some(exit) = self.runtime_exits.lock().get(&(session_id, runtime_id)) {
                exit.observe();
            }
        }
        Ok(())
    }

    fn note_runtime_close_requested(&self, session_id: Uuid, runtime_id: Uuid) {
        if let Some(exit) = self.runtime_exits.lock().get(&(session_id, runtime_id)) {
            exit.request_close();
        }
    }

    fn unconfirmed_shutdown_sessions(&self) -> HashSet<Uuid> {
        self.runtime_exits
            .lock()
            .iter()
            .filter_map(|((session_id, _), exit)| {
                exit.is_unconfirmed_close().then_some(*session_id)
            })
            .collect()
    }

    #[cfg(all(test, unix))]
    pub(crate) fn with_terminal_shell(mut self, shell: alacritty_terminal::tty::Shell) -> Self {
        self.terminal_shell = Some(shell);
        self
    }

    fn open_terminal(
        &self,
        cwd: &Path,
        cols: u16,
        rows: u16,
        events: EventSink,
    ) -> anyhow::Result<crate::terminal::DaemonTerminal> {
        #[cfg(all(test, unix))]
        if let Some(shell) = &self.terminal_shell {
            return crate::terminal::DaemonTerminal::open_with_shell(
                cwd,
                cols,
                rows,
                events,
                shell.clone(),
            );
        }
        ensure_shell_environment();
        crate::terminal::DaemonTerminal::open(cwd, cols, rows, events)
    }

    /// Reopening is part of explicit work submission, never a consequence of
    /// late stop/completion events or simply viewing an archived conversation.
    fn reopen_task_for_work(&self, session_id: Uuid) -> anyhow::Result<bool> {
        let mut state = self.task_state.lock();
        let task_id = state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .and_then(|session| session.task_id);
        let Some(index) = state
            .tasks
            .iter()
            .position(|task| Some(task.id) == task_id && task.state.is_done())
        else {
            return Ok(false);
        };
        let previous = state.tasks[index].clone();
        state.tasks[index].set_state(TaskState::Doing);
        if let Err(error) = self.task_store.save(&mut state) {
            state.tasks[index] = previous;
            return Err(error).context("could not reopen the task before submitting work");
        }
        Ok(true)
    }

    /// Capture and persist one ending checkpoint exactly once per daemon.
    /// Desktop and Web may observe the same turn completion concurrently; a
    /// per-turn lock prevents both clients from running the expensive Git
    /// snapshot while leaving unrelated tasks independent.
    fn capture_turn_checkpoint(
        &self,
        cwd: PathBuf,
        session_id: Uuid,
        turn_count: usize,
    ) -> anyhow::Result<Checkpoint> {
        let key = (cwd.clone(), session_id, turn_count);
        let capture_lock = self
            .checkpoint_capture_locks
            .lock()
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _capture = capture_lock.lock();

        {
            let mut state = self.task_state.lock();
            if let Some(index) = state
                .sessions
                .iter()
                .position(|session| session.id == session_id)
            {
                self.task_store.hydrate(&mut state.sessions[index])?;
                if let Some(checkpoint) = state.sessions[index]
                    .turns
                    .iter()
                    .find(|turn| turn.turn_count == turn_count)
                    .and_then(|turn| turn.checkpoint.as_ref())
                    .filter(|checkpoint| {
                        matches!(
                            checkpoint.status,
                            CheckpointStatus::Ready | CheckpointStatus::Unavailable
                        )
                    })
                {
                    return Ok(checkpoint.clone());
                }
            }
        }

        let checkpoint = crate::checkpoint::capture_turn(&cwd, session_id, turn_count)?;
        let mut state = self.task_state.lock();
        if let Some(index) = state
            .sessions
            .iter()
            .position(|session| session.id == session_id)
        {
            self.task_store.hydrate(&mut state.sessions[index])?;
            if let Some(turn) = state.sessions[index]
                .turns
                .iter_mut()
                .find(|turn| turn.turn_count == turn_count)
            {
                turn.checkpoint = Some(checkpoint.clone());
                state.mark_session_dirty(session_id);
                self.task_store.save(&mut state)?;
            }
        }
        Ok(checkpoint)
    }
}

/// Storage-layout migrations belong to the daemon because both the database
/// rows and the directories name paths on its host. Persist after each move
/// so a later failure cannot leave an earlier project pointing at its old
/// location in SQLite.
fn migrate_projectless_state(
    task_store: &StateStore,
    task_state: &mut PersistedState,
) -> anyhow::Result<()> {
    let indices = task_state
        .projects
        .iter()
        .enumerate()
        .filter_map(|(index, project)| {
            crate::projectless::needs_migration(&project.path).then_some(index)
        })
        .collect::<Vec<_>>();
    for index in indices {
        let old_path = task_state.projects[index].path.clone();
        let workspace = crate::projectless::migrate_workspace(&old_path).with_context(|| {
            format!(
                "could not move projectless workspace {} under ~/.waku/projects",
                old_path.display()
            )
        })?;
        task_state.projects[index].name = crate::model::Project::PROJECTLESS_NAME.to_owned();
        task_state.projects[index].path = workspace.cwd;
        task_store
            .save(task_state)
            .context("could not persist migrated projectless workspace")?;
    }
    Ok(())
}

impl Backend for WakuBackend {
    fn handle_automation_jobs(
        &self,
        command: &waku_protocol::AutomationCommand,
    ) -> waku_protocol::AutomationResult {
        match &self.automation_jobs {
            Some(Ok(jobs)) => jobs
                .command(command)
                .unwrap_or_else(|error| waku_protocol::AutomationResult::Error { error }),
            Some(Err(error)) => waku_protocol::AutomationResult::Error {
                error: error.clone(),
            },
            None => waku_protocol::AutomationResult::Error {
                error: waku_protocol::AutomationError::new(
                    "unavailable",
                    "automation jobs require a debug build",
                ),
            },
        }
    }

    fn handle_automation_scratch(
        &self,
        command: &waku_protocol::AutomationCommand,
    ) -> anyhow::Result<waku_protocol::AutomationResult> {
        self.scratch.handle(command)
    }

    fn forward_automation(
        &self,
        command: &waku_protocol::AutomationCommand,
        outgoing: crossbeam_channel::Sender<crate::ServerMessage>,
        subscriber_id: u64,
    ) -> Option<anyhow::Result<waku_protocol::AutomationResult>> {
        self.scratch.forward(command, outgoing, subscriber_id)
    }

    fn automation_subscriber_disconnected(&self, subscriber_id: u64) {
        self.scratch.disconnected(subscriber_id);
    }

    fn handle(&self, request: Request, events: EventSink) -> anyhow::Result<ResponsePayload> {
        let session_id = request.session_id;
        let runtime_id = request.runtime_id;
        match request.command {
            Command::AttachSession => {
                let sessions = self.sessions.lock();
                let Some((runtime_id, driver)) = sessions.get(&session_id) else {
                    return Ok(ResponsePayload::SessionRuntime {
                        runtime_id: None,
                        supports_steer: false,
                    });
                };
                Ok(ResponsePayload::SessionRuntime {
                    runtime_id: Some(*runtime_id),
                    supports_steer: driver.supports_steer(),
                })
            }
            Command::GetSettings => Ok(ResponsePayload::Settings {
                settings: self.settings.get(),
            }),
            Command::UpdateSettings { settings } => {
                self.settings.replace(settings)?;
                Ok(ResponsePayload::Ack)
            }
            Command::ProbeProvider {
                provider,
                binary_override,
                discover_models,
                probe_version,
            } => {
                ensure_shell_environment();
                let mut probe = match binary_override.as_deref() {
                    override_value if discover_models || probe_version => {
                        crate::model::provider_probe(provider, override_value)
                    }
                    override_value => crate::model::cached_provider_probe(provider, override_value),
                };
                let version = probe_version
                    .then(|| {
                        probe
                            .path
                            .as_deref()
                            .and_then(crate::model::probe_provider_version)
                    })
                    .flatten();
                if discover_models {
                    probe = crate::model::discover_provider_models(probe);
                }
                Ok(ResponsePayload::ProviderProbe { probe, version })
            }
            Command::FetchPlanUsage {
                provider,
                binary_override,
                cli_version,
            } => {
                let usage = match provider {
                    crate::model::ProviderKind::Claude => Some(
                        crate::usage::fetch_claude_plan_usage(cli_version.as_deref())?,
                    ),
                    crate::model::ProviderKind::Codex => {
                        Some(crate::usage::fetch_codex_plan_usage()?)
                    }
                    crate::model::ProviderKind::OpenCode => {
                        crate::usage::fetch_opencode_go_plan_usage()?
                    }
                    crate::model::ProviderKind::Grok => {
                        ensure_shell_environment();
                        let probe = match binary_override.as_deref() {
                            override_value => {
                                crate::model::provider_probe(provider, override_value)
                            }
                        };
                        let binary = probe.path.ok_or_else(|| anyhow!("grok is not installed"))?;
                        Some(crate::usage::fetch_grok_plan_usage(&binary)?)
                    }
                    _ => bail!("provider has no plan usage fetcher"),
                };
                Ok(ResponsePayload::PlanUsage { usage })
            }
            Command::ProbeComputerPermissions { prompt } => {
                Ok(ResponsePayload::ComputerPermissions {
                    permissions: crate::computer_use::probe_permissions(prompt)?,
                })
            }
            Command::LoadUsageHistory {
                window,
                project_roots,
            } => {
                let rates = crate::usage_history::load_rate_table(&self.usage_rates_dir);
                let history = crate::usage_history::scan(
                    &mut self.usage_scan_cache.lock(),
                    &rates,
                    window,
                    &project_roots,
                );
                Ok(ResponsePayload::UsageHistory { history })
            }
            Command::LoadSkills { projects } => {
                let locations = crate::skills::skill_locations(&projects);
                Ok(ResponsePayload::SkillsCatalog {
                    catalog: crate::skills::scan_skills(&locations),
                })
            }
            Command::SetSkillsEnabled { dirs, enabled } => {
                for dir in dirs {
                    crate::skills::set_skill_enabled(&dir, enabled)
                        .map_err(|error| anyhow!(error))?;
                }
                Ok(ResponsePayload::Ack)
            }
            Command::TrashSkills { dirs } => {
                crate::skills::trash_skills(&dirs).map_err(|error| anyhow!(error))?;
                Ok(ResponsePayload::Ack)
            }
            Command::LoadTaskState => {
                let state = self.task_state.lock();
                Ok(ResponsePayload::TaskState {
                    projects: state.projects.clone(),
                    sessions: state
                        .sessions
                        .iter()
                        .map(AgentSession::list_projection)
                        .collect(),
                    tasks: state.tasks.clone(),
                    default_cwd: self.default_cwd.clone(),
                    projectless_root: crate::projectless::workspace_root(),
                })
            }
            Command::SaveTaskState {
                projects,
                tasks,
                live_session_ids: _,
                skeleton_session_ids,
                sessions,
            } => {
                let active_runtimes = self
                    .sessions
                    .lock()
                    .iter()
                    .map(|(session_id, (runtime_id, _))| (*session_id, *runtime_id))
                    .collect::<HashMap<_, _>>();
                let mut state = self.task_state.lock();
                let removed_session_ids = self.removed_session_ids.lock();
                validate_task_completions(
                    &state,
                    &tasks,
                    &sessions,
                    &active_runtimes,
                    &self.unconfirmed_shutdown_sessions(),
                )?;
                for project in projects {
                    if let Some(existing) = state
                        .projects
                        .iter_mut()
                        .find(|existing| existing.id == project.id)
                    {
                        *existing = project;
                    } else {
                        state.projects.push(project);
                    }
                }
                // Tasks merge the way projects do: a save is additive, and an
                // older snapshot never overwrites a newer one. Deleting a task
                // is an explicit client action, not a side effect of saving.
                let previous_task_count = state.tasks.len();
                let mut replaced_tasks = Vec::new();
                for task in tasks {
                    if let Some(index) = state
                        .tasks
                        .iter()
                        .position(|existing| existing.id == task.id)
                    {
                        if task.updated_at > state.tasks[index].updated_at {
                            let previous = std::mem::replace(&mut state.tasks[index], task);
                            replaced_tasks.push((index, previous));
                        }
                    } else {
                        state.tasks.push(task);
                    }
                }
                let sessions = sessions
                    .into_iter()
                    .filter(|session| !removed_session_ids.contains(&session.id))
                    .collect::<Vec<_>>();
                drop(removed_session_ids);
                // `detail_loaded` is process-local and does not survive the
                // wire, so the client names the skeletons it is sending.
                // Restore the marker before merging: without it the empty
                // projection looks like a fully loaded session, and the save
                // below overwrites the stored transcript and deletes the
                // task's rows as if the client had removed it.
                let skeleton_session_ids = skeleton_session_ids.into_iter().collect::<HashSet<_>>();
                let sessions = sessions
                    .into_iter()
                    .map(|mut session| {
                        if skeleton_session_ids.contains(&session.id) {
                            session.detail_loaded = false;
                        }
                        session
                    })
                    .collect::<Vec<_>>();
                let saved_ids = sessions
                    .iter()
                    .map(|session| session.id)
                    .collect::<Vec<_>>();
                for mut session in sessions {
                    if let Some(existing) = state
                        .sessions
                        .iter_mut()
                        .find(|existing| existing.id == session.id)
                    {
                        if session_projection_precedes(
                            existing,
                            &session,
                            active_runtimes.get(&session.id).copied(),
                        ) {
                            merge_stale_session_metadata(existing, session);
                        } else {
                            preserve_daemon_checkpoints(existing, &mut session);
                            *existing = session;
                        }
                    } else {
                        state.sessions.push(session);
                    }
                }
                let used_project_ids = state
                    .sessions
                    .iter()
                    .map(|session| session.project_id)
                    .collect::<std::collections::HashSet<_>>();
                state.projects.retain(|project| {
                    !project.is_projectless() || used_project_ids.contains(&project.id)
                });
                for session_id in &saved_ids {
                    state.mark_session_dirty(*session_id);
                }
                if let Err(error) = self.task_store.save(&mut state) {
                    // A failed acknowledged edit must not become the daemon's
                    // authoritative issue state when the client refreshes.
                    // Keep the replaced values, not cloned transcripts, for
                    // this rollback. Dirty session data remains retryable.
                    for (index, previous) in replaced_tasks.into_iter().rev() {
                        state.tasks[index] = previous;
                    }
                    state.tasks.truncate(previous_task_count);
                    return Err(error.into());
                }
                let sessions = saved_ids
                    .into_iter()
                    .filter_map(|session_id| {
                        state
                            .sessions
                            .iter()
                            .find(|session| session.id == session_id)
                            .cloned()
                    })
                    .collect();
                // The save above can adopt full transcripts for every session
                // the client touched. Keep only the recent window resident;
                // the echoed clones above still carry the saved detail.
                trim_resident_transcripts(&mut state, &active_runtimes.keys().copied().collect());
                Ok(ResponsePayload::TaskStateSaved { sessions })
            }
            Command::RemoveSession => {
                {
                    let mut state = self.task_state.lock();
                    self.removed_session_ids.lock().insert(session_id);
                    let project_id = state
                        .sessions
                        .iter()
                        .find(|session| session.id == session_id)
                        .map(|session| session.project_id);
                    state.sessions.retain(|session| session.id != session_id);
                    if let Some(project_id) = project_id {
                        let remove_project = state
                            .projects
                            .iter()
                            .find(|project| project.id == project_id)
                            .is_some_and(Project::is_projectless)
                            && !state
                                .sessions
                                .iter()
                                .any(|session| session.project_id == project_id);
                        if remove_project {
                            state.projects.retain(|project| project.id != project_id);
                        }
                    }
                    self.task_store.save(&mut state)?;
                }
                let removed = self.sessions.lock().remove(&session_id);
                drop(removed);
                self.runtime_exits
                    .lock()
                    .retain(|(id, _), _| *id != session_id);
                // The task owns its terminals: its shells end with it. A client
                // closing only its surface never reaches this, which is what
                // keeps a detached terminal running.
                let disposed = {
                    let mut terminals = self.terminals.lock();
                    terminals
                        .extract_if(|_, owned| owned.task_id == session_id)
                        .map(|(_, owned)| owned)
                        .collect::<Vec<_>>()
                };
                drop(disposed);
                Ok(ResponsePayload::Ack)
            }
            Command::ReadTranscript {
                conversation_id,
                query,
            } => {
                if !cfg!(debug_assertions) {
                    bail!("transcript inspection is only available in debug builds");
                }
                Ok(ResponsePayload::Transcript {
                    result: crate::transcript::read_transcript(
                        self.task_store.path(),
                        conversation_id,
                        &query,
                    )
                    .map_err(|error| {
                        error
                            .downcast::<waku_protocol::TranscriptError>()
                            .unwrap_or_else(|error| {
                                waku_protocol::TranscriptError::new(
                                    "transcript_read_failed",
                                    &error.to_string(),
                                )
                            })
                    }),
                })
            }
            Command::HydrateSession { session_id } => {
                // Live runtimes stay resident; everything else is trimmed to
                // the recency window once the response is built.
                let pinned = self.sessions.lock().keys().copied().collect();
                let mut state = self.task_state.lock();
                let session = if let Some(session) = state
                    .sessions
                    .iter_mut()
                    .find(|session| session.id == session_id)
                {
                    self.task_store.hydrate(session)?;
                    Some(session.clone())
                } else {
                    None
                };
                trim_resident_transcripts(&mut state, &pinned);
                Ok(ResponsePayload::Session { session })
            }
            Command::SearchSessionMessages { query, limit } => {
                let matches = self.task_store.session_message_search(query, limit)()?;
                Ok(ResponsePayload::SessionMessageMatches { matches })
            }
            Command::ListProviderSessions { provider, limit } => {
                const MAX_PROVIDER_SESSIONS: usize = 500;
                let limit = limit.min(MAX_PROVIDER_SESSIONS);
                if limit == 0 {
                    return Ok(ResponsePayload::ProviderSessions {
                        sessions: Vec::new(),
                    });
                }
                ensure_shell_environment();
                let settings = self.settings.get();
                if settings.disabled_providers.contains(&provider) {
                    return Ok(ResponsePayload::ProviderSessions {
                        sessions: Vec::new(),
                    });
                }
                let binary_override = settings
                    .provider_binary_overrides
                    .get(&provider)
                    .map(String::as_str);
                let Some(binary) = crate::model::provider_probe(provider, binary_override).path
                else {
                    return Ok(ResponsePayload::ProviderSessions {
                        sessions: Vec::new(),
                    });
                };
                // Discovery is deliberately provider-scoped. Opening Resume
                // must not start every installed agent CLI, and another
                // provider is queried only after the user explicitly picks it.
                let mut sessions = match provider {
                    ProviderKind::Amp => {
                        crate::amp_session::list_provider_sessions(&binary, limit)?
                    }
                    ProviderKind::Claude => crate::claude_session::list_provider_sessions(limit)?,
                    ProviderKind::Codex => {
                        crate::codex_session::list_provider_sessions(&binary, limit)?
                    }
                    ProviderKind::Cursor | ProviderKind::Fx => {
                        crate::acp_session::list_provider_sessions(provider, &binary, &[], limit)?
                    }
                    ProviderKind::OpenCode => {
                        crate::opencode_session::list_provider_sessions(&binary, limit)?
                    }
                    ProviderKind::DeepSeek => {
                        crate::deepseek_session::list_provider_sessions(&binary, limit)?
                    }
                    ProviderKind::Grok => crate::grok_session::list_provider_sessions(limit)?,
                    ProviderKind::Kimi => crate::kimi_session::list_provider_sessions(limit)?,
                    ProviderKind::OhMyPi | ProviderKind::Pi => {
                        crate::pi_session::list_provider_sessions(provider, limit)?
                    }
                };
                sessions.sort_by(|a, b| {
                    b.updated_at
                        .cmp(&a.updated_at)
                        .then_with(|| a.title.cmp(&b.title))
                });
                let imported = {
                    let state = self.task_state.lock();
                    state
                        .sessions
                        .iter()
                        .filter_map(|session| session.provider_cursor.as_ref())
                        .map(|cursor| (cursor.provider(), cursor.native_id().to_owned()))
                        .collect::<HashSet<_>>()
                };
                sessions.retain(|session| {
                    !imported.contains(&(session.provider(), session.cursor.native_id().to_owned()))
                });
                sessions.truncate(limit);
                Ok(ResponsePayload::ProviderSessions { sessions })
            }
            Command::LoadProviderSession { cursor, cwd } => {
                // Preserve every native turn shell for exact provider turn
                // numbering, but bound imported display text to recent turns.
                const VISIBLE_TURN_LIMIT: usize = 100;
                let history = match &cursor {
                    ProviderResumeCursor::Amp { thread_id, .. } => {
                        let binary = self.provider_binary(ProviderKind::Amp)?;
                        crate::amp_session::provider_session_history(
                            &binary,
                            &cwd,
                            thread_id,
                            VISIBLE_TURN_LIMIT,
                        )?
                    }
                    ProviderResumeCursor::Claude { session_id, .. } => {
                        self.provider_binary(ProviderKind::Claude)?;
                        crate::claude_session::provider_session_history(
                            session_id,
                            VISIBLE_TURN_LIMIT,
                        )?
                    }
                    ProviderResumeCursor::Codex { thread_id } => {
                        let binary = self.provider_binary(ProviderKind::Codex)?;
                        crate::codex_session::provider_session_history(
                            &binary,
                            thread_id,
                            VISIBLE_TURN_LIMIT,
                        )?
                    }
                    // OpenCode is not an ACP provider: its history comes
                    // from the adopted v2 service's own export route.
                    ProviderResumeCursor::OpenCode { session_id, .. } => {
                        let binary = self.provider_binary(ProviderKind::OpenCode)?;
                        crate::opencode_session::provider_session_history(
                            &binary,
                            session_id,
                            VISIBLE_TURN_LIMIT,
                        )?
                    }
                    ProviderResumeCursor::Cursor { session_id, .. }
                    | ProviderResumeCursor::Fx { session_id }
                    | ProviderResumeCursor::Grok { session_id }
                    | ProviderResumeCursor::Kimi { session_id } => {
                        let provider = cursor.provider();
                        let binary = self.provider_binary(provider)?;
                        crate::acp_session::provider_session_history(
                            provider,
                            &binary,
                            &cwd,
                            session_id,
                            VISIBLE_TURN_LIMIT,
                        )?
                    }
                    ProviderResumeCursor::DeepSeek { session_id } => {
                        let binary = self.provider_binary(ProviderKind::DeepSeek)?;
                        crate::deepseek_session::provider_session_history(
                            &binary,
                            session_id,
                            VISIBLE_TURN_LIMIT,
                        )?
                    }
                    ProviderResumeCursor::OhMyPi {
                        session_id,
                        session_file,
                    }
                    | ProviderResumeCursor::Pi {
                        session_id,
                        session_file,
                    } => {
                        self.provider_binary(cursor.provider())?;
                        let session_file = session_file.as_deref().ok_or_else(|| {
                            anyhow!(
                                "{} did not report its native session file",
                                cursor.provider().display_name()
                            )
                        })?;
                        crate::pi_session::provider_session_history(
                            cursor.provider(),
                            session_id,
                            session_file,
                            VISIBLE_TURN_LIMIT,
                        )?
                    }
                };
                Ok(ResponsePayload::ProviderSessionHistory { history })
            }
            Command::LoadComposerDrafts => Ok(ResponsePayload::ComposerDrafts {
                drafts: self.composer_drafts.load()?,
            }),
            Command::SaveComposerDrafts { drafts, generation } => {
                self.composer_drafts.save(drafts, generation)?;
                Ok(ResponsePayload::Ack)
            }
            Command::ApplyComposerDraftChanges { changes } => {
                self.composer_drafts.apply_changes(changes)?;
                Ok(ResponsePayload::Ack)
            }
            Command::StoreBlob { mime_type, bytes } => {
                let reference = self
                    .task_store
                    .blobs()
                    .store_image_bytes(&mime_type, &bytes)?;
                let path = self
                    .task_store
                    .blobs()
                    .path_for(&reference)
                    .ok_or_else(|| anyhow!("stored blob has no daemon path"))?;
                Ok(ResponsePayload::BlobStored { reference, path })
            }
            Command::ImportAttachment { name, upload } => Ok(ResponsePayload::AttachmentStored {
                attachment: self.attachments.import(&name, upload)?,
            }),
            Command::ImportPathAttachment { path } => Ok(ResponsePayload::AttachmentStored {
                attachment: self.attachments.import_path(&path)?,
            }),
            Command::ReadBlob { reference } => {
                let path = self
                    .task_store
                    .blobs()
                    .path_for(&reference)
                    .ok_or_else(|| anyhow!("invalid blob reference"))?;
                Ok(ResponsePayload::BlobData {
                    bytes: std::fs::read(path)?,
                })
            }
            Command::ReadAttachment { reference, path } => Ok(ResponsePayload::BlobData {
                bytes: self.attachments.read_file(&reference, &path)?,
            }),
            Command::SweepBlobs => {
                self.task_store.blob_sweep()();
                Ok(ResponsePayload::Ack)
            }
            Command::ForkSessionFromResponse { turn_count } => {
                let (session, checkpoint_warning) =
                    self.fork_session_from_response(session_id, turn_count)?;
                Ok(ResponsePayload::SessionForked {
                    session,
                    checkpoint_warning,
                })
            }
            Command::RewindSessionToMessage { turn_count } => {
                let (session, cleanup_warning) =
                    self.rewind_session_to_message(session_id, turn_count)?;
                Ok(ResponsePayload::SessionRewound {
                    session,
                    cleanup_warning,
                })
            }
            Command::ForkProviderSession { request } => {
                Ok(ResponsePayload::ProviderSessionForked {
                    result: fork_provider_session(request)?,
                })
            }
            Command::Workspace {
                operation:
                    WorkspaceOperation::CaptureTurn {
                        cwd,
                        session_id,
                        turn_count,
                    },
            } => Ok(ResponsePayload::Workspace {
                result: WorkspaceResult::Checkpoint {
                    checkpoint: self.capture_turn_checkpoint(cwd, session_id, turn_count)?,
                },
            }),
            Command::Workspace { operation } => Ok(ResponsePayload::Workspace {
                result: crate::workspace::execute(operation)?,
            }),
            Command::OpenTerminal {
                task_id,
                cwd,
                cols,
                rows,
            } => {
                let terminal = self.open_terminal(&cwd, cols, rows, events)?;
                let previous = self.terminals.lock().insert(
                    session_id,
                    OwnedTerminal {
                        task_id,
                        runtime_id,
                        terminal,
                    },
                );
                drop(previous);
                Ok(ResponsePayload::Ack)
            }
            Command::AttachTerminal { task_id } => {
                let terminals = self.terminals.lock();
                let Some(owned) = terminals.get(&session_id) else {
                    // Nothing is running under this id: report absence rather
                    // than an error so the client can open one without
                    // mistaking a transport failure for the same answer.
                    return Ok(ResponsePayload::TerminalAbsent);
                };
                if owned.runtime_id != runtime_id {
                    bail!(
                        "daemon terminal {session_id} belongs to runtime {}, not {runtime_id}",
                        owned.runtime_id
                    );
                }
                if owned.task_id != task_id {
                    bail!(
                        "daemon terminal {session_id} belongs to task {}, not {task_id}",
                        owned.task_id
                    );
                }
                let (cols, rows) = owned.terminal.size();
                let (data, sequence) = owned.terminal.retained_snapshot();
                Ok(ResponsePayload::TerminalSnapshot(TerminalSnapshot {
                    data,
                    sequence,
                    cols,
                    rows,
                }))
            }
            Command::WriteTerminal { data } => {
                let terminals = self.terminals.lock();
                let owned = terminals
                    .get(&session_id)
                    .ok_or_else(|| anyhow!("daemon terminal {session_id} is not running"))?;
                if owned.runtime_id != runtime_id {
                    bail!(
                        "daemon terminal {session_id} belongs to runtime {}, not {runtime_id}",
                        owned.runtime_id
                    );
                }
                owned.terminal.write(data)?;
                Ok(ResponsePayload::Ack)
            }
            Command::ResizeTerminal { cols, rows } => {
                let terminals = self.terminals.lock();
                let owned = terminals
                    .get(&session_id)
                    .ok_or_else(|| anyhow!("daemon terminal {session_id} is not running"))?;
                if owned.runtime_id != runtime_id {
                    bail!(
                        "daemon terminal {session_id} belongs to runtime {}, not {runtime_id}",
                        owned.runtime_id
                    );
                }
                owned.terminal.resize(cols, rows);
                Ok(ResponsePayload::Ack)
            }
            Command::CloseTerminal => {
                let removed = {
                    let mut terminals = self.terminals.lock();
                    if let Some(owned) = terminals.get(&session_id)
                        && owned.runtime_id != runtime_id
                    {
                        bail!(
                            "daemon terminal {session_id} belongs to runtime {}, not {runtime_id}",
                            owned.runtime_id
                        );
                    }
                    terminals.remove(&session_id)
                };
                drop(removed);
                Ok(ResponsePayload::Ack)
            }
            Command::Start { options } => {
                let previous_id = self.sessions.lock().get(&session_id).map(|(id, _)| *id);
                if let Some(previous_id) = previous_id {
                    self.note_runtime_close_requested(session_id, previous_id);
                    self.confirm_shared_session_shutdown(session_id, previous_id)?;
                }
                let previous = self.sessions.lock().remove(&session_id);
                drop(previous);
                let provider = decode_enum(&options.provider)?;
                let options = DriverStartOptions {
                    binary: options.binary,
                    cwd: options.cwd,
                    mode: decode_enum(&options.mode)?,
                    model: options.model,
                    reasoning_effort: options.reasoning_effort,
                    service_tier: options.service_tier,
                    context_window: options.context_window,
                    agent_preset: options.agent_preset,
                    computer_use_enabled: options.computer_use_enabled,
                    provider_cursor: options
                        .provider_cursor
                        .map(serde_json::from_value)
                        .transpose()
                        .context("daemon received an invalid provider cursor")?,
                };
                let (wake, _wake_events) = smol::channel::bounded(1);
                let runtime = crate::diagnostics::ProviderRuntime {
                    provider,
                    session_id,
                    runtime_id,
                };
                let diagnostics = crate::diagnostics::ProviderDiagnostics::new(runtime, {
                    let events = events.clone();
                    move |record| events.record(|| Some(record))
                });
                let tracker = diagnostics.clone();
                let (event_sender, event_receiver) =
                    driver::event_channel_with_diagnostics(wake, Some(diagnostics));
                let handle = driver::start_local(provider, options, event_sender)?;
                let supports_steer = handle.supports_steer();
                let exit = Arc::new(RuntimeExit::default());
                let forwarding_exit = exit.clone();
                std::thread::Builder::new()
                    .name(format!("waku-daemon-events-{session_id}"))
                    .spawn(move || {
                        let mut active_turn = crate::diagnostics::ActiveTurn::default();
                        while let Ok(event) = event_receiver.recv() {
                            let process_exited = matches!(&event, DriverEvent::ProcessExited);
                            if process_exited {
                                forwarding_exit.observe();
                            }
                            events.record(|| {
                                let turn_id = active_turn.turn_for(&event);
                                tracker.set_turn(turn_id);
                                crate::diagnostics::driver_record(runtime, turn_id, &event)
                            });
                            let wire = event_to_wire(event).unwrap_or_else(|error| {
                                WireDriverEvent::new(
                                    "error",
                                    Value::String(format!(
                                        "could not encode daemon event: {error}"
                                    )),
                                )
                            });
                            // Shutdown evidence belongs to the daemon, not
                            // to a still-connected client or event subscriber.
                            let _ = events.send(wire);
                            if process_exited {
                                break;
                            }
                        }
                    })
                    .context("could not start daemon event forwarding thread")?;
                self.runtime_exits
                    .lock()
                    .insert((session_id, runtime_id), exit);
                self.sessions
                    .lock()
                    .insert(session_id, (runtime_id, handle));
                Ok(ResponsePayload::Started { supports_steer })
            }
            Command::CloseSessionAndWait => {
                self.close_session_and_wait(
                    session_id,
                    runtime_id,
                    std::time::Duration::from_secs(10),
                )?;
                Ok(ResponsePayload::Ack)
            }
            Command::CloseSession => {
                self.note_runtime_close_requested(session_id, runtime_id);
                self.confirm_shared_session_shutdown(session_id, runtime_id)?;
                let removed = {
                    let mut sessions = self.sessions.lock();
                    sessions
                        .get(&session_id)
                        .is_some_and(|(active_runtime_id, _)| *active_runtime_id == runtime_id)
                        .then(|| sessions.remove(&session_id))
                        .flatten()
                };
                drop(removed);
                Ok(ResponsePayload::Ack)
            }
            command => {
                let driver = {
                    let sessions = self.sessions.lock();
                    let (active_runtime_id, driver) = sessions
                        .get(&session_id)
                        .ok_or_else(|| anyhow!("daemon session {session_id} is not running"))?;
                    if *active_runtime_id != runtime_id {
                        bail!(
                            "daemon session {session_id} belongs to runtime {active_runtime_id}, not {runtime_id}"
                        );
                    }
                    driver.clone()
                };
                if command_starts_issue_work(&command) && self.reopen_task_for_work(session_id)? {
                    events.task_state_changed();
                }
                if let Command::Prompt {
                    prompt,
                    turn_id,
                    message_id,
                }
                | Command::Review {
                    prompt,
                    turn_id,
                    message_id,
                } = &command
                {
                    // Publish the submission into the runtime's event stream
                    // before the provider can start the turn. Every attached
                    // client mirrors the user message and its turn from this
                    // event, so the submitting client's own save is no longer
                    // the only record of the prompt — a follower that only
                    // knew the provider's `turnStarted` used to persist a
                    // projection without it, erasing the message for everyone.
                    events.send(event_to_wire(DriverEvent::PromptSubmitted {
                        message: prompt.clone(),
                        turn_id: turn_id.unwrap_or_else(Uuid::new_v4),
                        message_id: message_id.unwrap_or_else(Uuid::new_v4),
                    })?)?;
                }
                handle_driver_command(&driver, command)
            }
        }
    }

    fn shutdown(&self) {
        if let Some(Ok(jobs)) = &self.automation_jobs {
            jobs.shutdown();
        }
        self.scratch.shutdown();
        let sessions = std::mem::take(&mut *self.sessions.lock());
        drop(sessions);
        let terminals = std::mem::take(&mut *self.terminals.lock());
        drop(terminals);
    }
}

/// Only explicit starts reopen an issue; refresh, pause, and provider events do not.
fn command_starts_issue_work(command: &Command) -> bool {
    matches!(
        command,
        Command::Prompt { .. }
            | Command::Review { .. }
            | Command::Steer { .. }
            | Command::Goal {
                operation: GoalOperation::Set {
                    status: Some(ThreadGoalStatus::Active),
                    ..
                }
            }
    )
}

/// A client may mark Done only after all conversation snapshots in the same
/// save have settled. Stale clients cannot archive work that the daemon still
/// knows is busy. Requested shutdowns also require positive process-exit
/// evidence, even after a client restarts with an optimistic idle projection.
/// Detached controls remain the confirming client's responsibility.
fn validate_task_completions(
    state: &PersistedState,
    tasks: &[Task],
    incoming_sessions: &[AgentSession],
    active_runtimes: &HashMap<Uuid, Uuid>,
    unconfirmed_shutdowns: &HashSet<Uuid>,
) -> anyhow::Result<()> {
    let completing = tasks
        .iter()
        .filter(|task| {
            task.state.is_done()
                && state
                    .tasks
                    .iter()
                    .find(|old| old.id == task.id)
                    .is_none_or(|old| !old.state.is_done() && task.updated_at > old.updated_at)
        })
        .map(|task| task.id)
        .collect::<HashSet<_>>();
    if completing.is_empty() {
        return Ok(());
    }
    let is_unsettled = |session_id: Uuid, task_id: Option<Uuid>, status: SessionStatus| {
        (status.is_busy()
            || active_runtimes.contains_key(&session_id)
            || unconfirmed_shutdowns.contains(&session_id))
            && task_id.is_some_and(|id| completing.contains(&id))
    };
    for existing in &state.sessions {
        let incoming = incoming_sessions
            .iter()
            .find(|session| session.id == existing.id);
        let (task_id, status) = match incoming {
            Some(incoming)
                if !session_projection_precedes(
                    existing,
                    incoming,
                    active_runtimes.get(&existing.id).copied(),
                ) =>
            {
                (incoming.task_id, incoming.status)
            }
            Some(incoming) if incoming.updated_at >= existing.updated_at => {
                (incoming.task_id, existing.status)
            }
            _ => (existing.task_id, existing.status),
        };
        if is_unsettled(existing.id, task_id, status) {
            bail!("stop the task's running conversations before marking it done");
        }
    }
    for incoming in incoming_sessions {
        if !state
            .sessions
            .iter()
            .any(|session| session.id == incoming.id)
            && is_unsettled(incoming.id, incoming.task_id, incoming.status)
        {
            bail!("stop the task's running conversations before marking it done");
        }
    }
    Ok(())
}

fn session_projection_precedes(
    existing: &AgentSession,
    incoming: &AgentSession,
    active_runtime_id: Option<Uuid>,
) -> bool {
    let existing_cursor = existing.runtime_event_cursor;
    let incoming_cursor = incoming.runtime_event_cursor;
    if let Some(active_runtime_id) = active_runtime_id {
        let existing_is_active =
            existing_cursor.is_some_and(|cursor| cursor.runtime_id == active_runtime_id);
        let incoming_is_active =
            incoming_cursor.is_some_and(|cursor| cursor.runtime_id == active_runtime_id);
        if existing_is_active != incoming_is_active {
            return existing_is_active;
        }
    }
    match (existing_cursor, incoming_cursor) {
        (Some(existing), Some(incoming))
            if existing.runtime_id == incoming.runtime_id && existing.epoch == incoming.epoch =>
        {
            incoming.sequence < existing.sequence
        }
        (Some(_), None) if existing.status.is_busy() => true,
        _ => incoming.updated_at < existing.updated_at,
    }
}

fn merge_stale_session_metadata(existing: &mut AgentSession, incoming: AgentSession) {
    if incoming.updated_at >= existing.updated_at {
        existing.title = incoming.title;
        existing.project_id = incoming.project_id;
        existing.task_id = incoming.task_id;
        existing.workspace = incoming.workspace;
        existing.provider = incoming.provider;
        existing.model = incoming.model;
        existing.runtime_mode = incoming.runtime_mode;
        existing.reasoning_effort = incoming.reasoning_effort;
        existing.service_tier = incoming.service_tier;
        existing.context_window = incoming.context_window;
        existing.agent_preset = incoming.agent_preset;
        existing.updated_at = incoming.updated_at;
        existing.last_reply_at = incoming.last_reply_at.or(existing.last_reply_at);
    }
    for queued in incoming.queued_messages {
        if !existing
            .queued_messages
            .iter()
            .any(|candidate| candidate.id == queued.id)
        {
            existing.queued_messages.push(queued);
        }
    }
}

/// Ending checkpoints are produced and stored by the daemon. A second client
/// may still save a projection created just before capture completed; never
/// let that stale projection erase the canonical Git snapshot.
fn preserve_daemon_checkpoints(existing: &AgentSession, incoming: &mut AgentSession) {
    for turn in &mut incoming.turns {
        let Some(checkpoint) = existing
            .turns
            .iter()
            .find(|candidate| candidate.turn_count == turn.turn_count)
            .and_then(|candidate| candidate.checkpoint.as_ref())
            .filter(|checkpoint| {
                matches!(
                    checkpoint.status,
                    CheckpointStatus::Ready | CheckpointStatus::Unavailable
                )
            })
        else {
            continue;
        };
        turn.checkpoint = Some(checkpoint.clone());
    }
}

impl WakuBackend {
    /// Fork a response using only daemon-host state.
    ///
    /// A browser must never reconstruct or persist this operation itself:
    /// provider-native sessions, checkpoint refs, and the task database all
    /// belong to the daemon and may be on another machine.
    fn fork_session_from_response(
        &self,
        session_id: Uuid,
        turn_count: usize,
    ) -> anyhow::Result<(AgentSession, Option<String>)> {
        let (source, cwd, fork_title) = {
            let mut state = self.task_state.lock();
            let source_index = state
                .sessions
                .iter()
                .position(|session| session.id == session_id)
                .ok_or_else(|| anyhow!("the source task is unavailable"))?;
            self.task_store
                .hydrate(&mut state.sessions[source_index])
                .context("could not load the source task")?;
            let source = state.sessions[source_index].clone();
            let project = state
                .projects
                .iter()
                .find(|project| project.id == source.project_id)
                .ok_or_else(|| anyhow!("the source task project is unavailable"))?;
            let cwd = source.workspace.path().unwrap_or(&project.path).to_owned();
            let fork_title = next_response_fork_title(
                source.display_title(),
                state
                    .sessions
                    .iter()
                    .filter(|session| session.project_id == source.project_id)
                    .map(AgentSession::display_title),
            );
            (source, cwd, fork_title)
        };

        validate_response_fork(&source, turn_count)?;
        let provider_turn_count = source
            .turns
            .iter()
            .take(turn_count)
            .filter(|turn| turn.provider_turn_started)
            .count();
        let turns_to_remove = source.provider_turns_after(turn_count);
        let (provider_cursor, message_ids) = self.fork_provider_response(
            &source,
            &cwd,
            &fork_title,
            turn_count,
            provider_turn_count,
            turns_to_remove,
        )?;
        let mut forked = source
            .fork_through_turn(turn_count, provider_cursor, &fork_title)
            .ok_or_else(|| anyhow!("the selected response cannot be copied"))?;
        if !message_ids.is_empty() {
            for turn in &mut forked.turns {
                if let Some(message_id) = turn.provider_resume_at.as_mut()
                    && let Some(remapped) = message_ids.get(message_id)
                {
                    *message_id = remapped.clone();
                }
            }
        }

        let fork_id = forked.id;
        for turn in &mut forked.turns {
            if let Some(checkpoint) = turn.checkpoint.as_mut() {
                checkpoint.git_ref =
                    crate::checkpoint::checkpoint_ref(fork_id, checkpoint.turn_count);
            }
        }
        let checkpoint_warning =
            crate::checkpoint::copy_session_refs(&cwd, source.id, fork_id, turn_count)
                .err()
                .map(|error| error.to_string());

        let pinned = self.sessions.lock().keys().copied().collect();
        let mut state = self.task_state.lock();
        state.push_session(forked.clone());
        if let Err(error) = self.task_store.save(&mut state) {
            state.sessions.retain(|session| session.id != fork_id);
            let _ = crate::checkpoint::delete_all_session_refs(&cwd, fork_id);
            return Err(error).context("could not save the forked task");
        }
        trim_resident_transcripts(&mut state, &pinned);
        Ok((forked, checkpoint_warning))
    }

    /// Restore the daemon-host worktree, provider conversation, and stored
    /// transcript to immediately before one user turn.
    fn rewind_session_to_message(
        &self,
        session_id: Uuid,
        turn_count: usize,
    ) -> anyhow::Result<(AgentSession, Option<String>)> {
        let (source, cwd) = {
            let mut state = self.task_state.lock();
            let source_index = state
                .sessions
                .iter()
                .position(|session| session.id == session_id)
                .ok_or_else(|| anyhow!("the task is unavailable"))?;
            self.task_store
                .hydrate(&mut state.sessions[source_index])
                .context("could not load the task")?;
            let source = state.sessions[source_index].clone();
            let project = state
                .projects
                .iter()
                .find(|project| project.id == source.project_id)
                .ok_or_else(|| anyhow!("the task project is unavailable"))?;
            let cwd = source.workspace.path().unwrap_or(&project.path).to_owned();
            (source, cwd)
        };
        validate_message_rewind(&source, turn_count)?;

        // Resolve the executable before touching the worktree. Even native
        // transcript operations are immediately followed by a replacement
        // prompt, so accepting a rewind that cannot resume would strand the
        // user at a provider state the UI cannot continue.
        let binary = self.provider_binary(source.provider)?;
        let retained_turn_count = turn_count.saturating_sub(1);
        let previous_turn_count = source.turns.len();
        let rollback_turns = source.provider_turns_after(retained_turn_count);
        let provider_turn_count = source
            .turns
            .iter()
            .take(retained_turn_count)
            .filter(|turn| turn.provider_turn_started)
            .count();
        let provider_resume_at = retained_turn_count
            .checked_sub(1)
            .and_then(|index| source.turns.get(index))
            .and_then(|turn| turn.provider_resume_at.clone());

        let turn_start_ref = crate::checkpoint::turn_start_ref(session_id, turn_count);
        let retained_ref = crate::checkpoint::checkpoint_ref(session_id, retained_turn_count);
        let restore_ref = if crate::checkpoint::has_ref(&cwd, &turn_start_ref) {
            turn_start_ref
        } else {
            retained_ref
        };
        if !crate::checkpoint::has_ref(&cwd, &restore_ref) {
            bail!("the checkpoint before this message is unavailable");
        }

        let safety_ref = format!("refs/waku/revert-backup-{session_id}-{}", Uuid::new_v4());
        crate::checkpoint::capture_ref(&cwd, &safety_ref)
            .context("could not create a rewind safety snapshot")?;
        if let Err(error) = crate::checkpoint::restore_ref(&cwd, &restore_ref) {
            return Err(restore_rewind_safety(
                &cwd,
                &safety_ref,
                "could not restore the selected checkpoint",
                error,
            ));
        }

        let provider_rewind = self.rewind_provider_response(
            &source,
            &cwd,
            &binary,
            retained_turn_count,
            rollback_turns,
            provider_turn_count,
            provider_resume_at,
        );
        let (provider_cursor, message_ids, reset_native_session) = match provider_rewind {
            Ok(result) => result,
            Err(error) => {
                return Err(restore_rewind_safety(
                    &cwd,
                    &safety_ref,
                    "the provider rejected the rewind",
                    error,
                ));
            }
        };

        let runtime_id = self.sessions.lock().get(&session_id).map(|(id, _)| *id);
        if let Some(runtime_id) = runtime_id {
            self.note_runtime_close_requested(session_id, runtime_id);
            if let Err(error) = self.confirm_shared_session_shutdown(session_id, runtime_id) {
                return Err(restore_rewind_safety(
                    &cwd,
                    &safety_ref,
                    "the provider did not confirm session shutdown",
                    error,
                ));
            }
        }
        let _ = crate::checkpoint::delete_ref(&cwd, &safety_ref);
        let cleanup_warning = crate::checkpoint::delete_turn_refs_after(
            &cwd,
            session_id,
            retained_turn_count,
            previous_turn_count,
        )
        .err()
        .map(|error| error.to_string());

        // Every provider resumes from the newly stored cursor on the next
        // prompt. Dropping a resident source driver also prevents its late
        // events from racing the rewound transcript.
        let removed = self.sessions.lock().remove(&session_id);
        drop(removed);

        let mut rewound = source.clone();
        if !message_ids.is_empty() {
            for turn in rewound.turns.iter_mut().take(retained_turn_count) {
                if let Some(remapped) = turn
                    .provider_resume_at
                    .as_ref()
                    .and_then(|message_id| message_ids.get(message_id))
                    .cloned()
                {
                    turn.provider_resume_at = Some(remapped);
                }
            }
        }
        if reset_native_session {
            rewound.provider_cursor = None;
        } else if let Some(cursor) = provider_cursor {
            rewound.provider_cursor = Some(cursor);
        }
        rewound.truncate_after_turn(retained_turn_count);
        rewound.status = SessionStatus::Idle;

        let pinned = self.sessions.lock().keys().copied().collect();
        let mut state = self.task_state.lock();
        let existing = state
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)
            .ok_or_else(|| anyhow!("the task was removed while it was being rewound"))?;
        *existing = rewound.clone();
        state.mark_session_dirty(session_id);
        self.task_store
            .save(&mut state)
            .context("could not save the rewound task")?;
        trim_resident_transcripts(&mut state, &pinned);
        Ok((rewound, cleanup_warning))
    }

    fn fork_provider_response(
        &self,
        source: &AgentSession,
        cwd: &Path,
        fork_title: &str,
        turn_count: usize,
        provider_turn_count: usize,
        turns_to_remove: usize,
    ) -> anyhow::Result<(ProviderResumeCursor, HashMap<String, String>)> {
        match source.provider {
            ProviderKind::Claude => {
                let Some(ProviderResumeCursor::Claude { session_id, .. }) =
                    source.provider_cursor.as_ref()
                else {
                    bail!("Claude's native session is unavailable");
                };
                let resume_at = source
                    .turns
                    .get(turn_count.saturating_sub(1))
                    .and_then(|turn| turn.provider_resume_at.clone());
                let fork = fork_provider_session(ProviderSessionForkRequest::Claude {
                    session_id: session_id.clone(),
                    resume_at,
                    turn_count: provider_turn_count,
                    title: fork_title.to_owned(),
                })?;
                Ok((fork.cursor, fork.message_ids))
            }
            ProviderKind::Codex
            | ProviderKind::DeepSeek
            | ProviderKind::OhMyPi
            | ProviderKind::Pi => Ok((
                self.fork_response_with_driver(source, cwd, turns_to_remove)?,
                HashMap::new(),
            )),
            ProviderKind::Cursor => {
                let fork = fork_provider_session(ProviderSessionForkRequest::Cursor {
                    source: source.clone(),
                    turn_count,
                })?;
                Ok((fork.cursor, HashMap::new()))
            }
            ProviderKind::Amp => {
                let Some(ProviderResumeCursor::Amp {
                    thread_id,
                    fork_context,
                }) = source.provider_cursor.as_ref()
                else {
                    bail!("Amp's native thread is unavailable");
                };
                let fork = fork_provider_session(ProviderSessionForkRequest::Amp {
                    binary: self.provider_binary(ProviderKind::Amp)?,
                    cwd: cwd.to_owned(),
                    thread_id: thread_id.clone(),
                    fork_context: fork_context.clone(),
                    turn_count: provider_turn_count,
                })?;
                Ok((fork.cursor, HashMap::new()))
            }
            ProviderKind::OpenCode => {
                let Some(ProviderResumeCursor::OpenCode { session_id, .. }) =
                    source.provider_cursor.as_ref()
                else {
                    bail!("OpenCode's native session is unavailable");
                };
                // No cwd: a v2 session carries its own `location`, so there is
                // no server working directory to fork against.
                let fork = fork_provider_session(ProviderSessionForkRequest::OpenCode {
                    binary: self.provider_binary(ProviderKind::OpenCode)?,
                    session_id: session_id.clone(),
                    turn_count: provider_turn_count,
                })?;
                Ok((fork.cursor, HashMap::new()))
            }
            ProviderKind::Grok => {
                let Some(ProviderResumeCursor::Grok { session_id }) =
                    source.provider_cursor.as_ref()
                else {
                    bail!("Grok Build's native session is unavailable");
                };
                let fork = fork_provider_session(ProviderSessionForkRequest::Grok {
                    binary: self.provider_binary(ProviderKind::Grok)?,
                    cwd: cwd.to_owned(),
                    session_id: session_id.clone(),
                    turn_count: provider_turn_count,
                })?;
                Ok((fork.cursor, HashMap::new()))
            }
            // Unreachable through the UI, which hides branching for providers
            // that answer `supports_conversation_fork` with false.
            ProviderKind::Fx | ProviderKind::Kimi => {
                bail!(
                    "{} cannot branch a conversation at a turn",
                    source.provider.display_name()
                )
            }
        }
    }

    fn fork_response_with_driver(
        &self,
        source: &AgentSession,
        cwd: &Path,
        turns_to_remove: usize,
    ) -> anyhow::Result<ProviderResumeCursor> {
        if let Some(driver) = self
            .sessions
            .lock()
            .get(&source.id)
            .map(|(_, driver)| driver.clone())
        {
            return driver.fork(turns_to_remove);
        }

        match source.provider {
            ProviderKind::Codex
                if !matches!(
                    source.provider_cursor.as_ref(),
                    Some(ProviderResumeCursor::Codex { .. })
                ) =>
            {
                bail!("Codex's native thread is unavailable");
            }
            ProviderKind::DeepSeek
                if !matches!(
                    source.provider_cursor.as_ref(),
                    Some(ProviderResumeCursor::DeepSeek { .. })
                ) =>
            {
                bail!("DeepSeek Harness's native session is unavailable");
            }
            ProviderKind::Pi
                if !matches!(
                    source.provider_cursor.as_ref(),
                    Some(ProviderResumeCursor::Pi {
                        session_file: Some(_),
                        ..
                    })
                ) =>
            {
                bail!("Pi's native session file is unavailable");
            }
            ProviderKind::OhMyPi
                if !matches!(
                    source.provider_cursor.as_ref(),
                    Some(ProviderResumeCursor::OhMyPi {
                        session_file: Some(_),
                        ..
                    })
                ) =>
            {
                bail!("Oh My Pi's native session file is unavailable");
            }
            _ => {}
        }

        let (wake, _wake_events) = smol::channel::bounded(1);
        let (event_sender, _event_receiver) = driver::event_channel(wake);
        let driver = driver::start_local(
            source.provider,
            DriverStartOptions {
                binary: self.provider_binary(source.provider)?,
                cwd: cwd.to_owned(),
                mode: source.runtime_mode,
                model: source.model.clone(),
                reasoning_effort: source.reasoning_effort.clone(),
                service_tier: source.service_tier.clone(),
                context_window: source.context_window.clone(),
                agent_preset: source.agent_preset.clone(),
                computer_use_enabled: false,
                provider_cursor: source.provider_cursor.clone(),
            },
            event_sender,
        )?;
        driver.fork(turns_to_remove)
    }

    #[allow(clippy::too_many_arguments)]
    fn rewind_provider_response(
        &self,
        source: &AgentSession,
        cwd: &Path,
        binary: &Path,
        retained_turn_count: usize,
        rollback_turns: usize,
        provider_turn_count: usize,
        provider_resume_at: Option<String>,
    ) -> anyhow::Result<(Option<ProviderResumeCursor>, HashMap<String, String>, bool)> {
        if rollback_turns == 0 {
            return Ok((None, HashMap::new(), false));
        }
        let reset_native_session = retained_turn_count == 0
            && matches!(
                source.provider,
                ProviderKind::Claude | ProviderKind::Cursor | ProviderKind::Grok
            );
        if reset_native_session {
            return Ok((None, HashMap::new(), true));
        }

        match source.provider {
            ProviderKind::Claude => {
                let Some(ProviderResumeCursor::Claude { session_id, .. }) =
                    source.provider_cursor.as_ref()
                else {
                    bail!("Claude's native session is unavailable");
                };
                let fork = fork_provider_session(ProviderSessionForkRequest::Claude {
                    session_id: session_id.clone(),
                    resume_at: provider_resume_at,
                    turn_count: provider_turn_count,
                    title: format!("{} (rewind)", source.display_title()),
                })?;
                Ok((Some(fork.cursor), fork.message_ids, false))
            }
            ProviderKind::OpenCode => {
                let cursor = if let Some(driver) = self
                    .sessions
                    .lock()
                    .get(&source.id)
                    .map(|(_, driver)| driver.clone())
                {
                    driver
                        .rollback(rollback_turns)?
                        .ok_or_else(|| anyhow!("OpenCode returned no rewound-session cursor"))?
                } else {
                    let Some(ProviderResumeCursor::OpenCode { session_id, .. }) =
                        source.provider_cursor.as_ref()
                    else {
                        bail!("OpenCode's native session is unavailable");
                    };
                    fork_provider_session(ProviderSessionForkRequest::OpenCode {
                        binary: binary.to_owned(),
                        session_id: session_id.clone(),
                        turn_count: provider_turn_count,
                    })?
                    .cursor
                };
                Ok((Some(cursor), HashMap::new(), false))
            }
            ProviderKind::Amp => {
                let Some(ProviderResumeCursor::Amp {
                    thread_id,
                    fork_context,
                }) = source.provider_cursor.as_ref()
                else {
                    bail!("Amp's native thread is unavailable");
                };
                let cursor = fork_provider_session(ProviderSessionForkRequest::Amp {
                    binary: binary.to_owned(),
                    cwd: cwd.to_owned(),
                    thread_id: thread_id.clone(),
                    fork_context: fork_context.clone(),
                    turn_count: provider_turn_count,
                })?
                .cursor;
                Ok((Some(cursor), HashMap::new(), false))
            }
            ProviderKind::Cursor => {
                let cursor = fork_provider_session(ProviderSessionForkRequest::Cursor {
                    source: source.clone(),
                    turn_count: retained_turn_count,
                })?
                .cursor;
                Ok((Some(cursor), HashMap::new(), false))
            }
            ProviderKind::Grok => {
                let Some(ProviderResumeCursor::Grok { session_id }) =
                    source.provider_cursor.as_ref()
                else {
                    bail!("Grok Build's native session is unavailable");
                };
                let cursor = fork_provider_session(ProviderSessionForkRequest::Grok {
                    binary: binary.to_owned(),
                    cwd: cwd.to_owned(),
                    session_id: session_id.clone(),
                    turn_count: provider_turn_count,
                })?
                .cursor;
                Ok((Some(cursor), HashMap::new(), false))
            }
            ProviderKind::Codex
            | ProviderKind::DeepSeek
            | ProviderKind::OhMyPi
            | ProviderKind::Pi => Ok((
                self.rollback_response_with_driver(source, cwd, binary, rollback_turns)?,
                HashMap::new(),
                false,
            )),
            // Unreachable through the UI, which hides rewinding for providers
            // that answer `supports_conversation_rollback` with false.
            ProviderKind::Fx | ProviderKind::Kimi => {
                bail!(
                    "{} cannot rewind a conversation to a turn",
                    source.provider.display_name()
                )
            }
        }
    }

    fn rollback_response_with_driver(
        &self,
        source: &AgentSession,
        cwd: &Path,
        binary: &Path,
        rollback_turns: usize,
    ) -> anyhow::Result<Option<ProviderResumeCursor>> {
        if let Some(driver) = self
            .sessions
            .lock()
            .get(&source.id)
            .map(|(_, driver)| driver.clone())
        {
            return driver.rollback(rollback_turns);
        }

        let (wake, _wake_events) = smol::channel::bounded(1);
        let (event_sender, _event_receiver) = driver::event_channel(wake);
        let driver = driver::start_local(
            source.provider,
            DriverStartOptions {
                binary: binary.to_owned(),
                cwd: cwd.to_owned(),
                mode: source.runtime_mode,
                model: source.model.clone(),
                reasoning_effort: source.reasoning_effort.clone(),
                service_tier: source.service_tier.clone(),
                context_window: source.context_window.clone(),
                agent_preset: source.agent_preset.clone(),
                computer_use_enabled: false,
                provider_cursor: source.provider_cursor.clone(),
            },
            event_sender,
        )?;
        driver.rollback(rollback_turns)
    }

    fn provider_binary(&self, provider: ProviderKind) -> anyhow::Result<PathBuf> {
        ensure_shell_environment();
        let settings = self.settings.get();
        let binary_override = settings
            .provider_binary_overrides
            .get(&provider)
            .map(String::as_str);
        crate::model::provider_probe(provider, binary_override)
            .path
            .ok_or_else(|| anyhow!("{} is not installed on the daemon", provider.display_name()))
    }
}

fn validate_message_rewind(source: &AgentSession, turn_count: usize) -> anyhow::Result<()> {
    if !matches!(source.status, SessionStatus::Idle | SessionStatus::Failed) {
        bail!("stop the task before editing a prior message");
    }
    let Some(turn) = source
        .turns
        .iter()
        .find(|turn| turn.turn_count == turn_count)
    else {
        bail!("the selected message is unavailable");
    };
    if !source.messages.iter().any(|message| {
        message.turn_id == Some(turn.id) && message.role == crate::model::MessageRole::User
    }) {
        bail!("the selected user message is unavailable");
    }
    let rollback_turns = source.provider_turns_after(turn_count.saturating_sub(1));
    if rollback_turns > 0 && source.provider_cursor.is_none() {
        bail!("the provider conversation is unavailable");
    }
    Ok(())
}

fn restore_rewind_safety(
    cwd: &Path,
    safety_ref: &str,
    context: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    match crate::checkpoint::restore_ref(cwd, safety_ref) {
        Ok(()) => {
            let _ = crate::checkpoint::delete_ref(cwd, safety_ref);
            anyhow!("{context}: {error}; the original worktree was restored")
        }
        Err(restore_error) => anyhow!(
            "{context}: {error}; restoring the safety snapshot also failed: {restore_error}; snapshot: {safety_ref}"
        ),
    }
}

fn validate_response_fork(source: &AgentSession, turn_count: usize) -> anyhow::Result<()> {
    if !matches!(source.status, SessionStatus::Idle | SessionStatus::Failed) {
        bail!("stop the task before forking a response");
    }
    let cursor = source
        .provider_cursor
        .as_ref()
        .ok_or_else(|| anyhow!("the provider conversation is unavailable"))?;
    if cursor.provider() != source.provider {
        bail!("the provider conversation does not match this task");
    }
    if source
        .turns
        .get(turn_count.saturating_sub(1))
        .is_none_or(|turn| turn.turn_count != turn_count || !turn.provider_turn_started)
    {
        bail!("the selected response cannot be forked");
    }
    Ok(())
}

fn numbered_title_suffix(title: &str) -> Option<(&str, usize)> {
    let (base, suffix) = title.rsplit_once(" (")?;
    let number = suffix.strip_suffix(')')?.parse().ok()?;
    (!base.is_empty() && number >= 2).then_some((base, number))
}

fn next_response_fork_title<'a>(
    source_title: &str,
    existing_titles: impl IntoIterator<Item = &'a str>,
) -> String {
    let existing_titles = existing_titles.into_iter().collect::<Vec<_>>();
    let base = numbered_title_suffix(source_title)
        .filter(|(base, _)| existing_titles.iter().any(|title| title == base))
        .map_or(source_title, |(base, _)| base);
    let highest_number = existing_titles
        .iter()
        .filter_map(|title| {
            if *title == base {
                Some(1)
            } else {
                numbered_title_suffix(title)
                    .filter(|(candidate_base, _)| *candidate_base == base)
                    .map(|(_, number)| number)
            }
        })
        .max()
        .unwrap_or(1);
    format!("{base} ({})", highest_number.saturating_add(1).max(2))
}

fn fork_provider_session(
    request: ProviderSessionForkRequest,
) -> anyhow::Result<ProviderSessionFork> {
    use crate::model::ProviderResumeCursor;

    let (cursor, message_ids, source_resume_at) = match request {
        ProviderSessionForkRequest::Claude {
            session_id,
            resume_at,
            turn_count,
            title,
        } => {
            let source_resume_at = resume_at.map(Ok).unwrap_or_else(|| {
                crate::claude_session::message_id_for_turn(&session_id, turn_count)
            })?;
            let fork =
                crate::claude_session::fork_session_at(&session_id, &source_resume_at, &title)?;
            let fork_resume_at = fork
                .message_ids
                .get(&source_resume_at)
                .cloned()
                .ok_or_else(|| anyhow!("Claude fork did not include its target message"))?;
            (
                ProviderResumeCursor::Claude {
                    session_id: fork.session_id,
                    resume_at: Some(fork_resume_at),
                },
                fork.message_ids,
                Some(source_resume_at),
            )
        }
        ProviderSessionForkRequest::Amp {
            binary,
            cwd,
            thread_id,
            fork_context,
            turn_count,
        } => (
            crate::amp_session::fork_session_at_turn(
                &binary,
                &cwd,
                &thread_id,
                fork_context.as_deref(),
                turn_count,
            )?,
            HashMap::new(),
            None,
        ),
        ProviderSessionForkRequest::Cursor { source, turn_count } => (
            crate::cursor_session::fork_session_at_turn(&source, turn_count)?,
            HashMap::new(),
            None,
        ),
        ProviderSessionForkRequest::OpenCode {
            binary,
            session_id,
            turn_count,
        } => (
            crate::opencode_session::fork_session_at_turn(&binary, &session_id, turn_count)?,
            HashMap::new(),
            None,
        ),
        ProviderSessionForkRequest::Grok {
            binary,
            cwd,
            session_id,
            turn_count,
        } => (
            crate::grok_session::fork_session_at_turn(&binary, &cwd, &session_id, turn_count)?,
            HashMap::new(),
            None,
        ),
    };
    Ok(ProviderSessionFork {
        cursor,
        message_ids,
        source_resume_at,
    })
}

fn handle_driver_command(
    driver: &DriverHandle,
    command: Command,
) -> anyhow::Result<ResponsePayload> {
    match command {
        Command::Prompt { prompt, .. } => driver.prompt(prompt),
        Command::Review { prompt, .. } => driver.review(prompt)?,
        Command::Steer { prompt } => driver.steer(prompt),
        Command::Cancel => driver.cancel(),
        Command::CancelComputerUse => driver.cancel_computer_use(),
        Command::RefreshBackgroundWork => driver.refresh_background_work(),
        Command::StopBackgroundWork { key, control_id } => {
            driver.stop_background_work(
                serde_json::from_value(key).context("invalid background-work key")?,
                control_id,
            );
        }
        Command::Respond {
            request_id,
            option_id,
            message,
        } => driver.respond_with_message(request_id, option_id, message),
        Command::RespondUserInput {
            request_id,
            answers,
        } => driver.respond_user_input(request_id, answers),
        Command::Goal { operation } => driver.goal(operation),
        Command::RunComputerTool { request } => {
            driver.run_computer_tool(crate::computer_use::ComputerToolRequest {
                call_id: request.call_id,
                tool: request.tool,
                arguments: request.arguments,
            });
        }
        Command::RejectComputerTool { request, reason } => {
            driver.reject_computer_tool(
                crate::computer_use::ComputerToolRequest {
                    call_id: request.call_id,
                    tool: request.tool,
                    arguments: request.arguments,
                },
                reason,
            );
        }
        Command::ApplyOptions { options } => {
            return Ok(ResponsePayload::OptionsApplied {
                applied: driver.apply_options(SessionOptions {
                    mode: decode_enum(&options.mode)?,
                    model: options.model,
                    reasoning_effort: options.reasoning_effort,
                    service_tier: options.service_tier,
                    context_window: options.context_window,
                }),
            });
        }
        Command::Rollback { turns } => {
            let cursor = driver
                .rollback(turns)?
                .map(serde_json::to_value)
                .transpose()?;
            return Ok(ResponsePayload::Cursor { cursor });
        }
        Command::Fork { turns_to_remove } => {
            let cursor = Some(serde_json::to_value(driver.fork(turns_to_remove)?)?);
            return Ok(ResponsePayload::Cursor { cursor });
        }
        Command::AttachSession
        | Command::Start { .. }
        | Command::Automation { .. }
        | Command::DiagnosticLogs { .. }
        | Command::DiagnosticTimeline { .. }
        | Command::DiagnosticCounters
        | Command::DiagnosticVerbosity { .. }
        | Command::GetSettings
        | Command::UpdateSettings { .. }
        | Command::ProbeProvider { .. }
        | Command::FetchPlanUsage { .. }
        | Command::ProbeComputerPermissions { .. }
        | Command::LoadUsageHistory { .. }
        | Command::LoadSkills { .. }
        | Command::SetSkillsEnabled { .. }
        | Command::TrashSkills { .. }
        | Command::LoadTaskState
        | Command::SaveTaskState { .. }
        | Command::RemoveSession
        | Command::HydrateSession { .. }
        | Command::ReadTranscript { .. }
        | Command::SearchSessionMessages { .. }
        | Command::ListProviderSessions { .. }
        | Command::LoadProviderSession { .. }
        | Command::LoadComposerDrafts
        | Command::SaveComposerDrafts { .. }
        | Command::ApplyComposerDraftChanges { .. }
        | Command::StoreBlob { .. }
        | Command::ImportAttachment { .. }
        | Command::ImportPathAttachment { .. }
        | Command::ReadBlob { .. }
        | Command::ReadAttachment { .. }
        | Command::SweepBlobs
        | Command::ForkSessionFromResponse { .. }
        | Command::RewindSessionToMessage { .. }
        | Command::ForkProviderSession { .. }
        | Command::Workspace { .. }
        | Command::OpenTerminal { .. }
        | Command::AttachTerminal { .. }
        | Command::WriteTerminal { .. }
        | Command::ResizeTerminal { .. }
        | Command::CloseTerminal
        | Command::CloseSession
        | Command::CloseSessionAndWait => {
            bail!("daemon received a command in the wrong dispatch path")
        }
    }
    Ok(ResponsePayload::Ack)
}

fn ensure_shell_environment() {
    static REFRESHED: OnceLock<()> = OnceLock::new();
    REFRESHED.get_or_init(|| {
        crate::command_env::refresh_from_default_shell();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use waku_protocol::event_from_wire;

    struct ShutdownTestDriver(crossbeam_channel::Sender<()>);

    impl crate::driver::DriverControl for ShutdownTestDriver {
        fn prompt(&self, _: String) {}
        fn cancel(&self) {}
        fn respond(&self, _: String, _: String) {}
        fn rollback(&self, _: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
            Ok(None)
        }
    }

    impl Drop for ShutdownTestDriver {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    struct SharedShutdownTestDriver {
        settled: Arc<AtomicBool>,
        dropped: crossbeam_channel::Sender<()>,
    }

    impl crate::driver::DriverControl for SharedShutdownTestDriver {
        fn prompt(&self, _: String) {}
        fn cancel(&self) {}
        fn respond(&self, _: String, _: String) {}
        fn rollback(&self, _: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
            Ok(None)
        }
        fn confirm_session_shutdown(&self) -> Option<anyhow::Result<()>> {
            Some(if self.settled.load(Ordering::Acquire) {
                Ok(())
            } else {
                Err(anyhow!("session still running"))
            })
        }
    }

    impl Drop for SharedShutdownTestDriver {
        fn drop(&mut self) {
            let _ = self.dropped.send(());
        }
    }

    #[test]
    fn confirmed_shutdown_retains_shared_service_adapter_until_session_acknowledges() {
        let (root, backend) = shutdown_test_backend();
        let session = Uuid::new_v4();
        let runtime = Uuid::new_v4();
        let settled = Arc::new(AtomicBool::new(false));
        let exit = Arc::new(RuntimeExit::default());
        let (dropped, dropped_rx) = crossbeam_channel::bounded(1);
        backend
            .runtime_exits
            .lock()
            .insert((session, runtime), exit);
        backend.sessions.lock().insert(
            session,
            (
                runtime,
                DriverHandle::from_control(Arc::new(SharedShutdownTestDriver {
                    settled: settled.clone(),
                    dropped,
                })),
            ),
        );
        assert!(
            backend
                .close_session_and_wait(session, runtime, std::time::Duration::ZERO)
                .is_err()
        );
        assert!(backend.sessions.lock().contains_key(&session));
        assert!(dropped_rx.try_recv().is_err());
        assert!(backend.unconfirmed_shutdown_sessions().contains(&session));
        settled.store(true, Ordering::Release);
        assert!(
            backend
                .close_session_and_wait(session, runtime, std::time::Duration::ZERO)
                .is_ok()
        );
        dropped_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(!backend.sessions.lock().contains_key(&session));
        assert!(!backend.unconfirmed_shutdown_sessions().contains(&session));
        drop(backend);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn shutdown_test_backend() -> (std::path::PathBuf, Arc<WakuBackend>) {
        let root = std::env::temp_dir().join(format!("waku-confirmed-shutdown-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(root.join("app.db")),
        )
        .unwrap();
        (root, Arc::new(backend))
    }

    #[test]
    fn confirmed_shutdown_waits_for_process_exit_after_driver_drop() {
        let (root, backend) = shutdown_test_backend();
        let session_id = Uuid::new_v4();
        let runtime_id = Uuid::new_v4();
        let exit = Arc::new(RuntimeExit::default());
        backend
            .runtime_exits
            .lock()
            .insert((session_id, runtime_id), exit.clone());
        let (dropped, dropped_rx) = crossbeam_channel::bounded(1);
        backend.sessions.lock().insert(
            session_id,
            (
                runtime_id,
                DriverHandle::from_control(Arc::new(ShutdownTestDriver(dropped))),
            ),
        );
        let (finished, finished_rx) = crossbeam_channel::bounded(1);
        let closing = backend.clone();
        let worker = std::thread::spawn(move || {
            finished
                .send(closing.close_session_and_wait(
                    session_id,
                    runtime_id,
                    std::time::Duration::from_secs(2),
                ))
                .unwrap();
        });
        dropped_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(backend.sessions.lock().is_empty());
        assert!(
            finished_rx.try_recv().is_err(),
            "runtime removal is not process-exit evidence"
        );
        exit.observe();
        assert!(
            finished_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap()
                .is_ok()
        );
        worker.join().unwrap();
        drop(backend);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn confirmed_shutdown_timeout_and_absence_do_not_acknowledge_stop() {
        let (root, backend) = shutdown_test_backend();
        let session_id = Uuid::new_v4();
        let runtime_id = Uuid::new_v4();
        let timeout = std::time::Duration::ZERO;
        assert!(
            backend
                .close_session_and_wait(session_id, runtime_id, timeout)
                .is_err()
        );
        let exit = Arc::new(RuntimeExit::default());
        backend
            .runtime_exits
            .lock()
            .insert((session_id, runtime_id), exit.clone());
        assert!(
            backend
                .close_session_and_wait(session_id, runtime_id, timeout)
                .is_err()
        );
        assert!(
            backend
                .close_session_and_wait(session_id, runtime_id, timeout)
                .is_err(),
            "retry retains uncertainty after runtime removal"
        );
        assert!(
            backend
                .close_session_and_wait(session_id, Uuid::new_v4(), timeout)
                .is_err()
        );
        exit.observe();
        assert!(
            backend
                .close_session_and_wait(session_id, runtime_id, timeout)
                .is_ok()
        );
        drop(backend);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn confirmed_shutdown_retry_keeps_old_exit_evidence_after_replacement() {
        let (root, backend) = shutdown_test_backend();
        let session = Uuid::new_v4();
        let old = Uuid::new_v4();
        let replacement = Uuid::new_v4();
        let old_exit = Arc::new(RuntimeExit::default());
        let new_exit = Arc::new(RuntimeExit::default());
        backend
            .runtime_exits
            .lock()
            .insert((session, old), old_exit.clone());
        assert!(
            backend
                .close_session_and_wait(session, old, std::time::Duration::ZERO)
                .is_err()
        );
        let (dropped, dropped_rx) = crossbeam_channel::bounded(1);
        backend.sessions.lock().insert(
            session,
            (
                replacement,
                DriverHandle::from_control(Arc::new(ShutdownTestDriver(dropped))),
            ),
        );
        backend
            .runtime_exits
            .lock()
            .insert((session, replacement), new_exit.clone());
        assert!(
            backend
                .close_session_and_wait(session, old, std::time::Duration::ZERO)
                .is_err()
        );
        assert!(
            dropped_rx.try_recv().is_err(),
            "an old close cannot stop the replacement"
        );
        old_exit.observe();
        assert!(
            backend
                .close_session_and_wait(session, old, std::time::Duration::ZERO)
                .is_ok()
        );
        assert_eq!(
            backend.sessions.lock().get(&session).unwrap().0,
            replacement
        );
        assert!(
            backend
                .close_session_and_wait(session, replacement, std::time::Duration::ZERO)
                .is_err()
        );
        dropped_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        new_exit.observe();
        assert!(
            backend
                .close_session_and_wait(session, replacement, std::time::Duration::ZERO)
                .is_ok()
        );
        assert!(
            backend
                .close_session_and_wait(session, old, std::time::Duration::ZERO)
                .is_ok(),
            "retry can confirm both controls after the replacement stops"
        );
        drop(backend);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn confirmed_shutdown_blocks_done_after_client_restart_with_saved_idle_session() {
        let (root, backend) = shutdown_test_backend();
        let project = Project::from_path(root.join("project"));
        let mut task = Task::new("Stop before archiving", TaskState::Doing, 0);
        let mut session = AgentSession::new(project.id, ProviderKind::Pi);
        session.task_id = Some(task.id);
        session.status = SessionStatus::Idle;
        {
            let mut state = backend.task_state.lock();
            state.projects.push(project);
            state.tasks.push(task.clone());
            state.push_session(session.clone());
            backend.task_store.save(&mut state).unwrap();
        }
        let runtime_id = Uuid::new_v4();
        let exit = Arc::new(RuntimeExit::default());
        backend
            .runtime_exits
            .lock()
            .insert((session.id, runtime_id), exit.clone());
        // Ordinary fire-and-forget Stop and confirmed-close timeout share the
        // same evidence barrier, even once the desktop loses its in-memory map.
        backend.note_runtime_close_requested(session.id, runtime_id);
        let restarted = backend.task_store.load().unwrap();
        assert_eq!(
            restarted
                .sessions
                .iter()
                .find(|stored| stored.id == session.id)
                .unwrap()
                .status,
            SessionStatus::Idle
        );
        task.set_state(TaskState::Done);
        assert!(
            validate_task_completions(
                &restarted,
                &[task.clone()],
                &[session.clone()],
                &HashMap::new(),
                &backend.unconfirmed_shutdown_sessions()
            )
            .is_err()
        );
        assert!(
            backend
                .close_session_and_wait(session.id, runtime_id, std::time::Duration::ZERO)
                .is_err()
        );
        exit.observe();
        assert!(
            validate_task_completions(
                &restarted,
                &[task],
                &[session],
                &HashMap::new(),
                &backend.unconfirmed_shutdown_sessions()
            )
            .is_ok()
        );
        drop(backend);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn completing_an_issue_requires_its_conversations_to_settle() {
        let mut state = PersistedState::fresh(PathBuf::from("/tmp/project"));
        let task = Task::new("Repair parser", TaskState::Doing, 0);
        state.sessions[0].task_id = Some(task.id);
        state.sessions[0].begin_turn("work");
        state.sessions[0].status = SessionStatus::Working;
        state.tasks.push(task.clone());
        let mut done = task;
        done.set_state(TaskState::Done);
        assert!(
            validate_task_completions(
                &state,
                &[done.clone()],
                &[],
                &HashMap::new(),
                &HashSet::new()
            )
            .is_err()
        );
        let mut settled = state.sessions[0].clone();
        settled.status = SessionStatus::Idle;
        settled.updated_at = state.sessions[0].updated_at + 1;
        assert!(
            validate_task_completions(
                &state,
                &[done],
                &[settled],
                &HashMap::new(),
                &HashSet::new()
            )
            .is_ok()
        );
    }

    #[test]
    fn fresh_idle_projection_cannot_complete_a_linked_live_runtime() {
        let mut state = PersistedState::fresh(PathBuf::from("/tmp/project"));
        let task = Task::new("Stop the actual runtime", TaskState::Doing, 0);
        state.sessions[0].task_id = Some(task.id);
        state.sessions[0].status = SessionStatus::Working;
        let runtime = Uuid::new_v4();
        state.sessions[0].runtime_event_cursor = Some(crate::model::RuntimeEventCursor {
            runtime_id: runtime,
            epoch: Uuid::new_v4(),
            sequence: 12,
        });
        let mut fresh = state.sessions[0].clone();
        fresh.status = SessionStatus::Idle;
        fresh.updated_at = fresh.updated_at.saturating_add(1);
        state.tasks.push(task.clone());
        let mut done = task;
        done.set_state(TaskState::Done);
        let active = HashMap::from([(fresh.id, runtime)]);
        assert!(
            validate_task_completions(
                &state,
                &[done.clone()],
                &[fresh.clone()],
                &active,
                &HashSet::new()
            )
            .is_err()
        );
        // Even a daemon catalog already showing Idle cannot override the
        // authoritative fact that its provider runtime remains resident.
        state.sessions[0].status = SessionStatus::Idle;
        assert!(
            validate_task_completions(
                &state,
                &[done.clone()],
                &[fresh.clone()],
                &active,
                &HashSet::new()
            )
            .is_err()
        );
        assert!(
            validate_task_completions(&state, &[done], &[fresh], &HashMap::new(), &HashSet::new())
                .is_ok()
        );
    }

    #[test]
    fn stale_settlement_cannot_archive_a_newer_running_turn() {
        let mut state = PersistedState::fresh(PathBuf::from("/tmp/project"));
        let task = Task::new("Repair parser", TaskState::Doing, 0);
        state.sessions[0].task_id = Some(task.id);
        state.sessions[0].status = SessionStatus::Working;
        let mut stale = state.sessions[0].clone();
        stale.status = SessionStatus::Idle;
        stale.updated_at = stale.updated_at.saturating_sub(1);
        state.tasks.push(task.clone());
        let mut done = task;
        done.set_state(TaskState::Done);
        assert!(
            validate_task_completions(&state, &[done], &[stale], &HashMap::new(), &HashSet::new())
                .is_err()
        );
    }

    #[test]
    fn native_review_starts_issue_work() {
        assert!(command_starts_issue_work(&Command::Review {
            prompt: "Review the implementation".into(),
            turn_id: None,
            message_id: None,
        }));
    }

    #[test]
    fn unsupported_review_returns_a_validation_error_instead_of_a_prompt() {
        let (dropped, _dropped_rx) = crossbeam_channel::bounded(1);
        let driver = DriverHandle::from_control(Arc::new(ShutdownTestDriver(dropped)));
        let result = handle_driver_command(
            &driver,
            Command::Review {
                prompt: "Review the implementation".into(),
                turn_id: None,
                message_id: None,
            },
        );
        assert!(
            result.is_err(),
            "a transport without native review must reject it"
        );
    }

    #[test]
    fn activating_a_goal_is_explicit_work_but_refreshing_or_pausing_is_not() {
        for (status, expected) in [
            (ThreadGoalStatus::Active, true),
            (ThreadGoalStatus::Paused, false),
            (ThreadGoalStatus::Complete, false),
        ] {
            assert_eq!(
                command_starts_issue_work(&Command::Goal {
                    operation: GoalOperation::Set {
                        objective: None,
                        status: Some(status),
                        replace: false,
                    }
                }),
                expected
            );
        }
        assert!(!command_starts_issue_work(&Command::Goal {
            operation: GoalOperation::Refresh
        }));
    }

    #[test]
    fn explicit_work_reopens_a_done_issue_and_persists_it() {
        let root = std::env::temp_dir().join(format!("waku-issue-reopen-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let database = root.join("app.db");
        let backend = WakuBackend::new(
            DaemonSettingsStore::open(root.join("settings.json")).unwrap(),
            StateStore::daemon(database.clone()),
        )
        .unwrap();
        let id = {
            let mut state = backend.task_state.lock();
            let project = Project::from_path(root.join("repo"));
            let task = Task::new("Repair parser", TaskState::Done, 0);
            let mut session = AgentSession::new(project.id, ProviderKind::Codex);
            session.task_id = Some(task.id);
            let id = session.id;
            state.projects.push(project);
            state.tasks.push(task);
            state.push_session(session);
            backend.task_store.save(&mut state).unwrap();
            id
        };
        assert!(backend.reopen_task_for_work(id).unwrap());
        assert!(!backend.reopen_task_for_work(id).unwrap());
        let restored = StateStore::daemon(database).load().unwrap();
        assert_eq!(restored.tasks[0].state, TaskState::Doing);
        assert!(restored.tasks[0].completed_at.is_none());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn stale_runtime_projection_keeps_newer_transcript_cursor() {
        let runtime_id = Uuid::new_v4();
        let epoch = Uuid::new_v4();
        let mut existing = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        existing.status = SessionStatus::Working;
        existing.runtime_event_cursor = Some(crate::model::RuntimeEventCursor {
            runtime_id,
            epoch,
            sequence: 10,
        });
        existing.push_message(crate::model::MessageRole::Assistant, "complete so far");

        let mut stale = existing.clone();
        stale.title = "Renamed elsewhere".into();
        stale.messages.clear();
        stale.runtime_event_cursor = Some(crate::model::RuntimeEventCursor {
            runtime_id,
            epoch,
            sequence: 7,
        });

        assert!(session_projection_precedes(
            &existing,
            &stale,
            Some(runtime_id)
        ));
        merge_stale_session_metadata(&mut existing, stale);
        assert_eq!(existing.title, "Renamed elsewhere");
        assert_eq!(existing.messages.len(), 1);
        assert_eq!(existing.runtime_event_cursor.unwrap().sequence, 10);
    }

    #[test]
    fn client_projection_cannot_replace_a_daemon_checkpoint() {
        let mut existing = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        existing.begin_turn("change it");
        existing.finish_active_turn(crate::model::TurnStatus::Completed);
        let checkpoint = Checkpoint {
            turn_count: 1,
            git_ref: "refs/waku/canonical".into(),
            status: CheckpointStatus::Ready,
            files: Vec::new(),
            additions: 0,
            deletions: 0,
            created_at: 1,
        };
        existing.turns[0].checkpoint = Some(checkpoint.clone());

        let mut incoming = existing.clone();
        incoming.turns[0].checkpoint = Some(Checkpoint {
            git_ref: "refs/waku/stale-client".into(),
            ..checkpoint.clone()
        });
        preserve_daemon_checkpoints(&existing, &mut incoming);

        assert_eq!(incoming.turns[0].checkpoint.as_ref(), Some(&checkpoint));
    }

    #[test]
    fn response_fork_titles_follow_one_numbered_sequence() {
        assert_eq!(
            next_response_fork_title("Fix the bug", ["Fix the bug"]),
            "Fix the bug (2)"
        );
        assert_eq!(
            next_response_fork_title(
                "Fix the bug (2)",
                ["Fix the bug", "Fix the bug (2)", "Fix the bug (4)"]
            ),
            "Fix the bug (5)"
        );
        assert_eq!(
            next_response_fork_title("Plan (2026)", ["Plan (2026)"]),
            "Plan (2026) (2)"
        );
    }

    #[test]
    fn message_rewind_requires_a_settled_user_turn_and_provider_cursor() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        session.begin_turn("change it");
        session.mark_active_turn_provider_started();
        session.provider_cursor = Some(ProviderResumeCursor::Codex {
            thread_id: "thread".into(),
        });
        session.finish_active_turn(crate::model::TurnStatus::Completed);

        assert!(validate_message_rewind(&session, 1).is_ok());

        let mut busy = session.clone();
        busy.status = SessionStatus::Working;
        assert!(validate_message_rewind(&busy, 1).is_err());

        let mut missing_cursor = session.clone();
        missing_cursor.provider_cursor = None;
        assert!(validate_message_rewind(&missing_cursor, 1).is_err());

        let mut missing_message = session;
        missing_message.messages.clear();
        assert!(validate_message_rewind(&missing_message, 1).is_err());
    }

    #[test]
    fn wire_event_round_trip_preserves_ordered_delta_payload() {
        let wire = event_to_wire(DriverEvent::TextDelta("hello".into())).unwrap();
        assert_eq!(wire.kind, "textDelta");
        assert!(matches!(
            event_from_wire(wire).unwrap(),
            DriverEvent::TextDelta(text) if text == "hello"
        ));
    }

    #[test]
    fn wire_event_round_trip_preserves_prompt_submission_identity() {
        let turn_id = Uuid::new_v4();
        let message_id = Uuid::new_v4();
        let wire = event_to_wire(DriverEvent::PromptSubmitted {
            message: "ship it".into(),
            turn_id,
            message_id,
        })
        .unwrap();
        assert_eq!(wire.kind, "promptSubmitted");
        assert_eq!(wire.payload["message"], "ship it");
        assert_eq!(wire.payload["turnId"], turn_id.to_string());
        assert_eq!(wire.payload["messageId"], message_id.to_string());
        assert!(matches!(
            event_from_wire(wire).unwrap(),
            DriverEvent::PromptSubmitted { message, turn_id: decoded_turn, message_id: decoded_message }
                if message == "ship it" && decoded_turn == turn_id && decoded_message == message_id
        ));
    }
}
