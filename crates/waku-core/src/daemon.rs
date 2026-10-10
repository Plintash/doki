//! Provider backend and driver-event wire translation for `waku-daemon`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use crate::{
    Backend, Command, EventSink, Request, ResponsePayload, WireDriverEvent, WorkspaceOperation,
    WorkspaceResult,
};
use anyhow::{Context as _, anyhow, bail};
use crossbeam_channel::RecvTimeoutError;
use parking_lot::Mutex;
use serde_json::Value;
use uuid::Uuid;

use crate::attachments::AttachmentStore;
use crate::driver::{self, DriverHandle, DriverStartOptions, SessionOptions};
use crate::model::{
    AgentSession, Checkpoint, CheckpointStatus, DriverEvent, Project, ProviderKind,
    ProviderResumeCursor, SessionStatus,
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

pub struct WakuBackend {
    sessions: Mutex<HashMap<Uuid, (Uuid, DriverHandle)>>,
    terminals: Mutex<HashMap<Uuid, OwnedTerminal>>,
    #[cfg(all(test, unix))]
    terminal_shell: Option<alacritty_terminal::tty::Shell>,
    settings: DaemonSettingsStore,
    /// The task store and the state it holds are shared with the per-session
    /// event threads: a generated objective is written by the daemon itself,
    /// with no client save behind it, and the thread that reads the session's
    /// driver events is the one that has it.
    task_store: Arc<StateStore>,
    task_state: Arc<Mutex<PersistedState>>,
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
        Ok(Self {
            sessions: Mutex::new(HashMap::new()),
            terminals: Mutex::new(HashMap::new()),
            #[cfg(all(test, unix))]
            terminal_shell: None,
            settings,
            task_store: Arc::new(task_store),
            task_state: Arc::new(Mutex::new(task_state)),
            removed_session_ids: Mutex::new(HashSet::new()),
            composer_drafts,
            attachments,
            usage_scan_cache: Mutex::new(HashMap::new()),
            checkpoint_capture_locks: Mutex::new(HashMap::new()),
            usage_rates_dir,
            default_cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        })
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
            }
            record_settled_turn(&mut state.sessions[index], turn_count, &checkpoint);
            state.mark_session_dirty(session_id);
            self.task_store.save(&mut state)?;
        }
        Ok(checkpoint)
    }
}

/// Record what a settled turn did on the task itself.
///
/// The row card's facts line reads a turn count and a changed-file count, and
/// neither can be derived when it is drawn: the list projection carries no
/// transcript, so `turns` is empty there, and the file count lives in the
/// ending checkpoint. Capturing that checkpoint is the one moment the daemon
/// holds both the settled turn and its file list, so the counts are recorded
/// here and preserved against client saves like the rest of the daemon-owned
/// triage state.
///
/// `settled_turn` counts itself: a client captures an ending checkpoint only
/// after finishing the turn, while the save that carries the settlement to the
/// daemon can land after this capture does. Counting the turns up to and
/// including it therefore reads a turn that just ended as settled, where
/// reading a stored status would read it as still running.
fn record_settled_turn(session: &mut AgentSession, settled_turn: usize, checkpoint: &Checkpoint) {
    session.turn_count = Some(
        session
            .turns
            .iter()
            .filter(|turn| turn.turn_count <= settled_turn)
            .count() as u32,
    );
    // An unavailable checkpoint says nothing about how many files the turn
    // touched, which is not the same as touching none: keep the last count a
    // real capture reported rather than reporting a zero nobody measured.
    if matches!(checkpoint.status, CheckpointStatus::Ready) {
        session.changed_files = Some(checkpoint.files.len() as u32);
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
                    default_cwd: self.default_cwd.clone(),
                    projectless_root: crate::projectless::workspace_root(),
                })
            }
            Command::SaveTaskState {
                projects,
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
                            preserve_daemon_triage(existing, &mut session);
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
                self.task_store.save(&mut state)?;
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
            Command::SetTaskArchived { archived } => {
                {
                    let mut state = self.task_state.lock();
                    if let Some(session) = state
                        .sessions
                        .iter_mut()
                        .find(|session| session.id == session_id)
                    {
                        session.archived_at = archived.then(crate::model::unix_time);
                        state.mark_session_dirty(session_id);
                    }
                    self.task_store.save(&mut state)?;
                }
                Ok(ResponsePayload::Ack)
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
                    // A client renders the objective and never ranks it
                    // against a goal, so the hand-off carries the resolved
                    // value. The session keeps the stored pair: its `objective`
                    // is the generated value a cleared goal falls back to, and
                    // a client saving this copy back must not overwrite it.
                    let mut session = session.clone();
                    let resolved = session.resolved_objective().map(str::to_owned);
                    session.objective = resolved;
                    Some(session)
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
                let (event_sender, event_receiver) = driver::event_channel(wake);
                let handle = driver::start_local(provider, options, event_sender)?;
                let supports_steer = handle.supports_steer();
                let digest = TaskDigestThread {
                    digest: TaskDigest {
                        session_id,
                        driver: handle.clone(),
                        // Only Pi carries the extension that answers a
                        // trigger. Every other provider's sessions forward
                        // their events as they always have: no schedule, no
                        // generation, and no catalog revision from one.
                        writer: (provider == ProviderKind::Pi).then(|| ObjectiveStore {
                            task_state: self.task_state.clone(),
                            task_store: self.task_store.clone(),
                        }),
                        schedule: crate::task_digest::DigestSchedule::default(),
                        turn_open: false,
                    },
                    events: events.clone(),
                };
                std::thread::Builder::new()
                    .name(format!("waku-daemon-events-{session_id}"))
                    .spawn(move || digest.forward(event_receiver))
                    .context("could not start daemon event forwarding thread")?;
                self.sessions
                    .lock()
                    .insert(session_id, (runtime_id, handle));
                Ok(ResponsePayload::Started { supports_steer })
            }
            Command::CloseSession => {
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
                if let Command::Prompt {
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
        let sessions = std::mem::take(&mut *self.sessions.lock());
        drop(sessions);
        let terminals = std::mem::take(&mut *self.terminals.lock());
        drop(terminals);
    }
}

/// One session's generated objective.
///
/// The daemon owns the objective — no client's save carries it — so the
/// per-session event thread is what stores a generated one and publishes the
/// catalog revision every client reloads on.
///
/// Only a turn that announced itself can settle: a refusal, and a command an
/// extension consumed, answer a prompt with no run behind them. So a task that
/// was merely opened or resumed generates nothing, and no pass over stored
/// history exists — this only ever sees what happens while the session runs.
struct TaskDigest {
    session_id: Uuid,
    driver: DriverHandle,
    /// `None` on a provider whose sessions carry no extension to answer a
    /// trigger. Such a session generates nothing, and its events are forwarded
    /// exactly as they always were.
    writer: Option<ObjectiveStore>,
    schedule: crate::task_digest::DigestSchedule,
    /// Whether the provider's turn announced itself and has not settled yet.
    turn_open: bool,
}

impl TaskDigest {
    /// What one driver event means for the task's objective.
    ///
    /// The objective a published result replaced is what the caller publishes;
    /// every other event changes nothing that reaches anyone.
    fn observe(&mut self, event: &DriverEvent, now: Instant) -> Option<String> {
        match event {
            DriverEvent::TurnStarted => {
                self.turn_open = true;
                None
            }
            DriverEvent::TurnFinished { .. } => {
                if !self.turn_open {
                    return None;
                }
                self.turn_open = false;
                if self.writer.is_some() {
                    self.schedule.settled(now);
                }
                None
            }
            DriverEvent::ExtensionMessage {
                custom_type, text, ..
            } if custom_type == crate::task_digest::DIGEST_SURFACE => {
                let result = crate::task_digest::parse_result(text)?;
                // A result counts only for the dispatch the daemon is waiting
                // on: one that arrives after its timeout, or that answers a
                // generation this task never asked for, changes nothing.
                if !self.schedule.resolve(result.dispatch) {
                    return None;
                }
                self.writer
                    .as_ref()?
                    .store(self.session_id, &result.objective)
            }
            _ => None,
        }
    }

    /// The dispatch to trigger, when a generation is due.
    fn due(&mut self, now: Instant) -> Option<Uuid> {
        self.writer.as_ref()?;
        match self.schedule.advance(now) {
            // The driver is what refuses to send this to a session whose
            // provider never reported the command: on such a session the
            // trigger would be an ordinary prompt, and the task would gain a
            // real turn.
            crate::task_digest::DigestStep::Dispatch(dispatch) => Some(dispatch),
            // A generation that published nothing is the silent failure the
            // objective is allowed to have, and a quiet task has nothing due.
            crate::task_digest::DigestStep::Timeout(_) | crate::task_digest::DigestStep::Idle => {
                None
            }
        }
    }
}

/// The state one session's events are forwarded to, and the objective work the
/// forwarding notices on the way.
struct TaskDigestThread {
    digest: TaskDigest,
    events: EventSink,
}

impl TaskDigestThread {
    /// Forwards this session's driver events, and acts on the two that decide
    /// the task's objective: a settlement schedules a generation, and a
    /// published result becomes the objective and one catalog revision.
    fn forward(mut self, events: crossbeam_channel::Receiver<DriverEvent>) {
        loop {
            let event = match self.digest.schedule.next_wake() {
                // Sleeping until the next deadline is what makes the quiet
                // period and the generation timeout real without a timer thread
                // per task: the wake is this session's own event stream.
                Some(deadline) => match events.recv_deadline(deadline) {
                    Ok(event) => Some(event),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                },
                None => match events.recv() {
                    Ok(event) => Some(event),
                    Err(_) => break,
                },
            };
            if let Some(event) = event {
                let now = Instant::now();
                if let Some(objective) = self.digest.observe(&event, now) {
                    self.events.task_objective_published(objective);
                }
                let wire = event_to_wire(event).unwrap_or_else(|error| {
                    WireDriverEvent::new(
                        "error",
                        Value::String(format!("could not encode daemon event: {error}")),
                    )
                });
                if self.events.send(wire).is_err() {
                    break;
                }
            }
            if let Some(dispatch) = self.digest.due(Instant::now()) {
                let trigger = crate::task_digest::trigger_prompt(dispatch);
                self.digest.driver.prompt(trigger);
            }
        }
    }
}

/// The daemon-owned state a generated objective is written to, shared with the
/// per-session event threads.
#[derive(Clone)]
struct ObjectiveStore {
    task_state: Arc<Mutex<PersistedState>>,
    task_store: Arc<StateStore>,
}

impl ObjectiveStore {
    /// Stores one generated objective for `session_id`.
    ///
    /// The returned objective is the one clients now render, or `None` when the
    /// store — and every client holding it — is untouched: a task the daemon
    /// does not know, an objective the parser rejects, a rewrite that says what
    /// the stored text already says, and a task whose goal owns the field all
    /// end here with nothing written and no revision emitted.
    fn store(&self, session_id: Uuid, candidate: &str) -> Option<String> {
        let mut state = self.task_state.lock();
        let objective =
            crate::task_digest::store_generated_objective(&mut state, session_id, candidate)?;
        // A store that cannot be written is not news for the user, and the
        // objective is in the daemon's memory either way.
        let _ = self.task_store.save(&mut state);
        Some(objective)
    }
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

/// The daemon generates the objective, counts what a settled turn did, and
/// applies an archive action, so a client's copy of those can never be newer:
/// a projection that never loaded the transcript must not erase them. Blocked
/// state is different — the client that noticed the transition owns it — so a
/// projection is allowed to report one, and the client that leaves the blocked
/// status is the authority on the blockage having ended.
fn preserve_daemon_triage(existing: &AgentSession, incoming: &mut AgentSession) {
    // The goal is what the list resolves a task's objective from, and after a
    // restart the narrow row is the only copy of it: the detail that also
    // holds it is not loaded until the task is opened. A skeleton save lacks
    // the goal because the client never loaded it, which is not the same as
    // the provider having cleared it, so it fills the gap rather than taking
    // the stored goal away. A hydrated client's save is the authority on the
    // goal being gone.
    if !incoming.detail_loaded {
        incoming.thread_goal = incoming
            .thread_goal
            .clone()
            .or_else(|| existing.thread_goal.clone());
    }
    incoming.objective = existing.objective.clone();
    incoming.turn_count = existing.turn_count;
    incoming.changed_files = existing.changed_files;
    incoming.archived_at = existing.archived_at;
    if matches!(
        incoming.status,
        SessionStatus::Waiting | SessionStatus::Failed
    ) {
        incoming.blocked_since = incoming.blocked_since.or(existing.blocked_since);
        incoming.blocked_reason = incoming
            .blocked_reason
            .clone()
            .or_else(|| existing.blocked_reason.clone());
    } else {
        incoming.blocked_since = None;
        incoming.blocked_reason = None;
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
        rewound.set_status(SessionStatus::Idle);

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
        | Command::SetTaskArchived { .. }
        | Command::HydrateSession { .. }
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
        | Command::CloseSession => {
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
    use crate::driver::DriverControl;
    use crate::task_digest::{
        DIGEST_SURFACE, DigestSchedule, DigestStep, GENERATION_TIMEOUT, QUIET_PERIOD,
        trigger_dispatch, trigger_prompt,
    };
    use serde_json::json;
    use std::time::Duration;
    use waku_protocol::event_from_wire;

    /// A goal the way a provider reports one.
    fn thread_goal(objective: &str) -> crate::model::ThreadGoal {
        crate::model::ThreadGoal {
            objective: objective.to_owned(),
            status: crate::model::ThreadGoalStatus::Active,
            token_budget: None,
            tokens_used: 0,
            time_used_seconds: 0,
        }
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

    fn checkpoint_with_files(turn_count: usize, files: &[&str]) -> Checkpoint {
        Checkpoint {
            turn_count,
            git_ref: format!("refs/waku/checkpoint-{turn_count}"),
            status: CheckpointStatus::Ready,
            files: files
                .iter()
                .map(|path| crate::model::CheckpointFile {
                    path: (*path).to_owned(),
                    additions: 1,
                    deletions: 0,
                })
                .collect(),
            additions: files.len() as u64,
            deletions: 0,
            created_at: 1,
        }
    }

    /// The card's facts line reads a turn count and a changed-file count, and
    /// nothing wrote either one: every value the row could show was preserved
    /// and re-published but never produced, so the line could only ever show
    /// recency. Settlement is where the daemon holds both numbers.
    #[test]
    fn a_settled_turn_records_the_counts_the_row_card_reports() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Pi);
        session.begin_turn("Do the work");
        session.finish_active_turn(crate::model::TurnStatus::Completed);

        record_settled_turn(
            &mut session,
            1,
            &checkpoint_with_files(1, &["a.rs", "b.rs"]),
        );
        assert_eq!(session.turn_count, Some(1), "one turn has settled");
        assert_eq!(session.changed_files, Some(2));

        // A second turn settles before the save that says so reaches the
        // daemon: the turn's own number is what counts it, and the newest
        // checkpoint's file list is what the card reports.
        session.begin_turn("Do more");
        session.finish_active_turn(crate::model::TurnStatus::Completed);
        record_settled_turn(&mut session, 2, &checkpoint_with_files(2, &["c.rs"]));
        assert_eq!(session.turn_count, Some(2));
        assert_eq!(session.changed_files, Some(1));

        // A checkpoint the daemon could not take measures nothing. Reporting
        // it as zero files would claim the turn touched none, so the count a
        // real capture reported stands.
        session.begin_turn("Do a third");
        record_settled_turn(
            &mut session,
            3,
            &Checkpoint {
                status: CheckpointStatus::Unavailable,
                files: Vec::new(),
                ..checkpoint_with_files(3, &[])
            },
        );
        assert_eq!(session.turn_count, Some(3));
        assert_eq!(
            session.changed_files,
            Some(1),
            "the last measured count stands"
        );
    }

    #[test]
    fn a_client_projection_neither_sets_nor_clears_daemon_owned_triage_state() {
        let mut existing = AgentSession::new(Uuid::new_v4(), ProviderKind::Pi);
        existing.objective = Some("Sidebar schedules tasks by state".into());
        existing.turn_count = Some(12);
        existing.changed_files = Some(6);
        existing.archived_at = Some(1_700_000_000);
        existing.status = SessionStatus::Waiting;
        existing.blocked_since = Some(1_700_000_100);
        existing.blocked_reason = Some("Waiting for the npm test decision".into());

        // What an older client sends: the projection has none of it.
        let mut incoming = existing.clone();
        incoming.objective = None;
        incoming.turn_count = None;
        incoming.changed_files = None;
        incoming.archived_at = None;
        incoming.blocked_since = None;
        incoming.blocked_reason = None;

        preserve_daemon_triage(&existing, &mut incoming);

        assert_eq!(incoming.objective.as_deref(), existing.objective.as_deref());
        assert_eq!(incoming.turn_count, Some(12));
        assert_eq!(incoming.changed_files, Some(6));
        assert_eq!(incoming.archived_at, existing.archived_at);
        assert_eq!(incoming.blocked_since, existing.blocked_since);
        assert_eq!(incoming.blocked_reason, existing.blocked_reason);

        // A client reads the list projection, so its copy of the objective is
        // the resolved one. Saving that must not overwrite the generated
        // value: clearing the goal has to fall back to the generated text, not
        // to the goal the clear removed.
        existing.thread_goal = Some(thread_goal("The sidebar groups tasks by what they need"));
        let mut resolved = existing.clone();
        resolved.objective = existing.resolved_objective().map(str::to_owned);

        preserve_daemon_triage(&existing, &mut resolved);

        assert_eq!(
            resolved.objective.as_deref(),
            Some("Sidebar schedules tasks by state")
        );
        assert_eq!(
            resolved.resolved_objective(),
            Some("The sidebar groups tasks by what they need")
        );
        assert_eq!(
            existing.objective.as_deref(),
            Some("Sidebar schedules tasks by state")
        );

        // The provider reports the goal cleared. The client's copy still shows
        // the goal text; the daemon resolves the save to the stored value.
        let mut cleared = existing.clone();
        cleared.thread_goal = None;
        cleared.objective = Some("The sidebar groups tasks by what they need".into());

        preserve_daemon_triage(&existing, &mut cleared);

        assert_eq!(
            cleared.resolved_objective(),
            Some("Sidebar schedules tasks by state")
        );
    }

    #[test]
    fn a_skeleton_save_cannot_clear_the_goal_the_narrow_row_holds() {
        let mut existing = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        existing.thread_goal = Some(thread_goal("The sidebar groups tasks by what they need"));

        // A client that never hydrated the task: its projection carries no
        // goal, and "not loaded here" is not "the provider cleared it".
        let mut incoming = existing.clone();
        incoming.detail_loaded = false;
        incoming.thread_goal = None;
        preserve_daemon_triage(&existing, &mut incoming);
        assert_eq!(incoming.thread_goal, existing.thread_goal);

        // A hydrated client clearing the goal is the authority on it being
        // gone, and its save carries the transcript that says so.
        let mut cleared = existing.clone();
        cleared.thread_goal = None;
        preserve_daemon_triage(&existing, &mut cleared);
        assert!(cleared.thread_goal.is_none());
    }

    #[test]
    fn leaving_the_blocked_status_clears_the_blockage() {
        let mut existing = AgentSession::new(Uuid::new_v4(), ProviderKind::Pi);
        existing.status = SessionStatus::Waiting;
        existing.blocked_since = Some(1_700_000_100);
        existing.blocked_reason = Some("Waiting for the npm test decision".into());

        let mut incoming = existing.clone();
        incoming.status = SessionStatus::Working;

        preserve_daemon_triage(&existing, &mut incoming);

        assert!(
            incoming.blocked_since.is_none(),
            "the client that left the status owns the blockage having ended"
        );
        assert!(incoming.blocked_reason.is_none());
    }

    #[test]
    fn a_stale_projection_keeps_the_objective_and_the_archive() {
        let runtime_id = Uuid::new_v4();
        let epoch = Uuid::new_v4();
        let mut existing = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        existing.objective = Some("The parser survives malformed rows".into());
        existing.archived_at = Some(1_700_000_000);
        existing.turn_count = Some(4);
        existing.runtime_event_cursor = Some(crate::model::RuntimeEventCursor {
            runtime_id,
            epoch,
            sequence: 10,
        });

        let mut stale = existing.clone();
        stale.objective = None;
        stale.archived_at = None;
        stale.turn_count = None;
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

        assert_eq!(
            existing.objective.as_deref(),
            Some("The parser survives malformed rows")
        );
        assert_eq!(existing.archived_at, Some(1_700_000_000));
        assert_eq!(existing.turn_count, Some(4));
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

    /// A driver that records what the daemon asks it to do.
    #[derive(Default)]
    struct RecordingDriver(Mutex<Vec<String>>);

    impl RecordingDriver {
        fn prompts(&self) -> Vec<String> {
            self.0.lock().clone()
        }
    }

    impl DriverControl for RecordingDriver {
        fn prompt(&self, prompt: String) {
            self.0.lock().push(prompt);
        }

        fn cancel(&self) {}

        fn respond(&self, _request_id: String, _option_id: String) {}

        fn rollback(&self, _turns: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
            bail!("rolling back is not part of this test")
        }
    }

    /// One task's objective generation, the driver it triggers, and the store
    /// it writes to — the three things the daemon's event thread holds.
    struct DigestUnderTest {
        digest: TaskDigest,
        driver: Arc<RecordingDriver>,
        task_state: Arc<Mutex<PersistedState>>,
        task_store: Arc<StateStore>,
    }

    impl DigestUnderTest {
        /// A Pi task whose store lives in a directory of its own. `objective`
        /// is what the task already carries.
        fn new(name: &str, objective: Option<&str>) -> Self {
            let root = std::env::temp_dir().join(format!("waku-{name}-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let task_store = StateStore::daemon(root.join("app.db"));
            let mut state = task_store.load().expect("a fresh task store");
            let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Pi);
            session.title = "Sidebar groups tasks by what they need".into();
            session.objective = objective.map(str::to_owned);
            // A task with a settled turn behind it: a draft that never started
            // owns no row, and the objective is a column of that row.
            session.begin_turn("describe what this is for");
            session.finish_active_turn(crate::model::TurnStatus::Completed);
            let session_id = session.id;
            state.push_session(session);

            let driver = Arc::new(RecordingDriver::default());
            let task_state = Arc::new(Mutex::new(state));
            let task_store = Arc::new(task_store);
            Self {
                digest: TaskDigest {
                    session_id,
                    driver: DriverHandle::from_control(driver.clone()),
                    writer: Some(ObjectiveStore {
                        task_state: task_state.clone(),
                        task_store: task_store.clone(),
                    }),
                    schedule: DigestSchedule::default(),
                    turn_open: false,
                },
                driver,
                task_state,
                task_store,
            }
        }

        /// Asks for a generation and returns the dispatch it sent.
        fn dispatch(&mut self, now: Instant) -> Uuid {
            let dispatch = self
                .digest
                .due(now)
                .expect("a settled task's generation is due");
            let trigger = trigger_prompt(dispatch);
            self.digest.driver.prompt(trigger.clone());
            assert_eq!(
                trigger_dispatch(&trigger),
                Some(dispatch),
                "the trigger names the dispatch its result must carry"
            );
            assert_eq!(
                self.driver.prompts().last().map(String::as_str),
                Some(trigger.as_str()),
                "the trigger reaches the driver"
            );
            dispatch
        }

        fn objective(&self) -> Option<String> {
            self.task_state
                .lock()
                .sessions
                .first()
                .and_then(|session| session.objective.clone())
        }

        fn turn(&mut self, at: Instant) {
            self.digest.observe(&DriverEvent::TurnStarted, at);
            self.settle(at);
        }

        fn settle(&mut self, at: Instant) {
            self.digest.observe(
                &DriverEvent::TurnFinished {
                    interrupted: false,
                    success: true,
                    summary: None,
                },
                at,
            );
        }
    }

    /// One published result, as the provider reports it.
    fn digest_result(dispatch: Uuid, objective: &str) -> DriverEvent {
        DriverEvent::ExtensionMessage {
            custom_type: DIGEST_SURFACE.to_owned(),
            text: json!({"v": 1, "dispatch": dispatch, "objective": objective}).to_string(),
            display: false,
        }
    }

    #[test]
    fn a_settled_turn_generates_once_after_the_task_goes_quiet() {
        let mut task = DigestUnderTest::new("digest-settles", None);
        let base = Instant::now();
        // A rapid back-and-forth: four turns settle inside one quiet period.
        for offset in 0..4 {
            task.turn(base + Duration::from_secs(offset));
        }

        assert_eq!(task.digest.due(base + Duration::from_secs(3)), None);
        let dispatch = task.dispatch(base + QUIET_PERIOD + Duration::from_secs(3));
        assert_eq!(
            task.digest
                .due(base + QUIET_PERIOD + Duration::from_secs(4)),
            None,
            "one generation runs at a time"
        );

        // The result answers the dispatch, and the task carries the objective.
        let now = base + QUIET_PERIOD + Duration::from_secs(5);
        assert_eq!(
            task.digest.observe(
                &digest_result(dispatch, "The task list says why each task exists"),
                now
            ),
            Some("The task list says why each task exists".to_owned())
        );
        assert_eq!(
            task.objective().as_deref(),
            Some("The task list says why each task exists")
        );
        assert_eq!(
            task.digest.due(base + Duration::from_secs(120)),
            None,
            "four settled turns are one generation, and it already answered"
        );
        assert_eq!(task.driver.prompts().len(), 1);
    }

    #[test]
    fn an_answer_with_no_run_behind_it_generates_nothing() {
        // A refusal, and a command the provider itself handled, both end a
        // prompt without a turn ever starting. Neither is work to describe, and
        // generating from one would describe the user's message rather than
        // what the task is for.
        let mut task = DigestUnderTest::new("digest-no-run", None);
        let base = Instant::now();
        for offset in 0..3 {
            let at = base + Duration::from_secs(offset);
            task.digest.observe(
                &DriverEvent::TurnFinished {
                    interrupted: false,
                    success: offset != 1,
                    summary: (offset == 1).then(|| "the prompt was refused".to_owned()),
                },
                at,
            );
        }

        assert_eq!(task.digest.due(base + Duration::from_secs(3600)), None);
        assert!(task.driver.prompts().is_empty());
    }

    #[test]
    fn a_generation_only_the_daemon_asked_for_becomes_the_objective() {
        let mut task = DigestUnderTest::new("digest-dispatch", Some("The list says why"));
        let base = Instant::now();
        task.turn(base);
        let dispatch = task.dispatch(base + QUIET_PERIOD);

        // A result for a generation this task never asked for — one from an
        // earlier session, or one answering after its timeout — is stale.
        assert_eq!(
            task.digest.observe(
                &digest_result(Uuid::new_v4(), "Something else entirely"),
                base + QUIET_PERIOD
            ),
            None
        );
        assert_eq!(task.objective().as_deref(), Some("The list says why"));

        // A result the daemon is not waiting on, because the generation timed
        // out, is stale too: silence keeps the previous objective.
        let after_timeout = base + QUIET_PERIOD + GENERATION_TIMEOUT;
        assert_eq!(task.digest.due(after_timeout), None, "the dispatch gave up");
        assert_eq!(
            task.digest.observe(
                &digest_result(dispatch, "A late answer nobody is waiting for"),
                after_timeout
            ),
            None
        );
        assert_eq!(task.objective().as_deref(), Some("The list says why"));
    }

    #[test]
    fn a_hostile_or_reworded_result_leaves_the_stored_objective_alone() {
        let mut task = DigestUnderTest::new("digest-hostile", Some("The list says why"));
        let base = Instant::now();
        task.turn(base);
        let dispatch = task.dispatch(base + QUIET_PERIOD);
        let now = base + QUIET_PERIOD;

        for (candidate, why) in [
            ("Update src/app/sidebar.rs", "a path"),
            ("Extend the schema in schema.ts", "a file"),
            ("Rename render_sidebar_session_item", "a symbol"),
            ("Sidebar groups tasks by what they need", "the title again"),
            ("The list says why", "what is stored already"),
        ] {
            assert_eq!(
                task.digest
                    .observe(&digest_result(dispatch, candidate), now),
                None,
                "{candidate:?} ({why}) must not reach a client"
            );
            assert_eq!(task.objective().as_deref(), Some("The list says why"));
        }
    }

    #[test]
    fn a_goal_the_user_set_owns_the_objective() {
        let mut task = DigestUnderTest::new("digest-goal", Some("The list says why"));
        task.task_state
            .lock()
            .sessions
            .first_mut()
            .unwrap()
            .thread_goal = Some(thread_goal("The sidebar groups tasks by what they need"));
        let base = Instant::now();
        task.turn(base);
        let dispatch = task.dispatch(base + QUIET_PERIOD);

        assert_eq!(
            task.digest.observe(
                &digest_result(dispatch, "A different outcome entirely"),
                base + QUIET_PERIOD
            ),
            None,
            "a goal the user set outranks a generated objective"
        );
        assert_eq!(task.objective().as_deref(), Some("The list says why"));
    }

    #[test]
    fn a_session_without_the_extension_generates_nothing() {
        // Only Pi carries the extension that answers a trigger, so every other
        // session forwards its events exactly as it always did.
        let mut task = DigestUnderTest::new("digest-no-extension", None);
        task.digest.writer = None;
        let base = Instant::now();
        task.turn(base);

        assert_eq!(task.digest.due(base + QUIET_PERIOD), None);
        assert!(task.driver.prompts().is_empty());
    }

    #[test]
    fn a_stored_objective_survives_a_reload() {
        // The daemon writes the objective itself, so it is also the daemon that
        // has to record the task as changed: a client that never saved anything
        // would otherwise be looking at a revision the store does not back.
        let mut task = DigestUnderTest::new("digest-persisted", None);
        let base = Instant::now();
        task.turn(base);
        let dispatch = task.dispatch(base + QUIET_PERIOD);
        task.digest.observe(
            &digest_result(dispatch, "The task list says why each task exists"),
            base + QUIET_PERIOD,
        );

        let reloaded = task.task_store.load().expect("the task store reloads");
        assert_eq!(
            reloaded
                .sessions
                .first()
                .and_then(|session| session.objective.as_deref()),
            Some("The task list says why each task exists")
        );
    }

    #[test]
    fn an_objective_for_a_task_the_daemon_does_not_know_is_dropped() {
        // The event thread is the only writer, so a result for a task that is
        // not in the store — one removed while its runtime lived on — is not
        // news, and publishing a revision for it would tell every client to
        // reload for nothing.
        let mut task = DigestUnderTest::new("digest-unknown", None);
        task.task_state.lock().sessions.clear();
        let base = Instant::now();
        task.turn(base);
        let dispatch = task.dispatch(base + QUIET_PERIOD);

        assert_eq!(
            task.digest.observe(
                &digest_result(dispatch, "The task list says why each task exists"),
                base + QUIET_PERIOD
            ),
            None
        );
        assert!(task.task_state.lock().sessions.is_empty());
    }

    #[test]
    fn the_schedule_times_out_a_generation_nobody_answered() {
        // The timeout is the daemon's own: the generation is forgotten and the
        // next settled turn may ask for another.
        let mut task = DigestUnderTest::new("digest-timeout", None);
        let base = Instant::now();
        task.turn(base);
        let dispatch = task.dispatch(base + QUIET_PERIOD);

        let deadline = base + QUIET_PERIOD + GENERATION_TIMEOUT;
        assert_eq!(task.digest.due(deadline - Duration::from_secs(1)), None);
        assert_eq!(task.digest.due(deadline), None);
        assert_eq!(
            task.digest.schedule.advance(deadline),
            DigestStep::Idle,
            "the timed-out generation is forgotten"
        );

        task.turn(deadline + Duration::from_secs(1));
        task.dispatch(deadline + Duration::from_secs(1) + QUIET_PERIOD);
        assert_eq!(
            task.driver.prompts().len(),
            2,
            "a task may generate again after a failure"
        );
        assert_ne!(task.driver.prompts()[0], task.driver.prompts()[1]);
        let _ = dispatch;
    }
}
