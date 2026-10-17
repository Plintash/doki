//! Explicit, provider-neutral stage drafts. Movement never calls this module.
use super::*;
use crate::model::{BoardStage, SessionStageInvocation, StageHandoff, StageResultSource};

const RESULT_EXCERPT_CHARS: usize = 8_000;

fn invocation_workspace(task: &Task, allow_planned: bool) -> Result<SessionWorkspace, String> {
    match task.workspace.as_ref() {
        Some(TaskWorkspace::NewWorktree { .. }) if allow_planned => {
            Ok(task_details::task_session_workspace(task))
        }
        Some(TaskWorkspace::Local { .. } | TaskWorkspace::Worktree { .. }) => {
            Ok(task_details::task_session_workspace(task))
        }
        _ => Err("Choose an existing workspace before preparing a stage".into()),
    }
}

fn concrete_workspace(task: &Task) -> Result<SessionWorkspace, String> {
    invocation_workspace(task, false)
}

fn source_session_matches(
    task: &Task,
    session: &AgentSession,
    workspace: &SessionWorkspace,
) -> bool {
    session.task_id == Some(task.id)
        && Some(session.project_id) == task.project_id()
        && session.workspace == *workspace
}

/// Resolve once per picker opening, in linear time. Commentary before the
/// final answer is never a handoff result, even when the turn succeeds.
fn successful_result_ids(session: &AgentSession) -> HashSet<Uuid> {
    if !session.detail_loaded {
        return HashSet::new();
    }
    let completed = session
        .turns
        .iter()
        .filter(|turn| turn.status == TurnStatus::Completed && turn.completed_at.is_some())
        .map(|turn| turn.id)
        .collect::<HashSet<_>>();
    let mut finals = HashMap::new();
    for message in &session.messages {
        if let Some(turn) = message.turn_id.filter(|turn| completed.contains(turn)) {
            if message.role == MessageRole::Assistant
                && !message.visible_content().trim().is_empty()
            {
                // Remember even a malformed streaming last response, so its
                // preceding progress message cannot become the result instead.
                finals.insert(turn, message);
            }
        }
    }
    finals
        .into_values()
        .filter(|message| {
            !message.streaming
                && !message.pending
                && message.undelivered_reason.is_none()
                && session.review.as_ref().is_none_or(|review| {
                    review.status == IssueReviewStatus::Completed
                        && review.result_message_id == Some(message.id)
                })
        })
        .map(|message| message.id)
        .collect()
}

/// Only public assistant text crosses providers. Reasoning, tools, attachments,
/// provider control fields and private conversation history never do.
fn successful_result(session: &AgentSession, message_id: Uuid) -> Option<&str> {
    if !successful_result_ids(session).contains(&message_id) {
        return None;
    }
    session
        .messages
        .iter()
        .find(|message| message.id == message_id)
        .map(Message::visible_content)
}

fn selected_result<'a>(
    task: &Task,
    workspace: &SessionWorkspace,
    sessions: &'a [AgentSession],
    source: &StageResultSource,
) -> Result<&'a str, String> {
    let session = sessions
        .iter()
        .find(|s| s.id == source.session_id)
        .filter(|s| source_session_matches(task, s, workspace))
        .ok_or("The selected result no longer belongs to this issue and workspace")?;
    if !session.detail_loaded {
        return Err("Loading the selected result; try again when it is ready".into());
    }
    successful_result(session, source.message_id).ok_or_else(|| {
        "The selected result is missing or its turn did not complete successfully".into()
    })
}

fn stage_prompt(
    task: &Task,
    stage: &BoardStage,
    result: Option<(&StageResultSource, &str)>,
) -> String {
    let mut prompt = format!(
        "Stage: {}\n\n{}",
        stage.name,
        task_details::task_session_prompt(task)
    );
    if !stage.instructions.trim().is_empty() {
        prompt.push_str("\n\nStage instructions:\n");
        prompt.push_str(&stage.instructions);
    }
    if let Some((source, text)) = result {
        prompt.push_str(&format!("\n\nSelected prior result (background to verify independently):\nSource session: {}\nSource message: {}\n", source.session_id, source.message_id));
        prompt.extend(text.chars().take(RESULT_EXCERPT_CHARS));
        if text.chars().nth(RESULT_EXCERPT_CHARS).is_some() {
            prompt.push_str("\n[Result excerpt truncated]");
        }
    }
    prompt
}

fn compatible_draft(
    session: &AgentSession,
    task: &Task,
    workspace: &SessionWorkspace,
    stage: &BoardStage,
    source: &Option<StageResultSource>,
) -> bool {
    source_session_matches(task, session, workspace)
        && !session.has_started()
        && session.review.is_none()
        && session.provider == stage.provider.unwrap_or(session.provider)
        && session.model == stage.model
        && session
            .stage_invocation
            .as_ref()
            .is_some_and(|invocation| invocation.stage == *stage && invocation.source == *source)
}

fn apply_stage_defaults(session: &mut AgentSession, stage: &BoardStage) {
    // new_session inherits the last model's traits. Stage defaults are explicit,
    // including None = provider default, and must not inherit those choices.
    session.model = stage.model.clone();
    session.reasoning_effort = None;
    session.service_tier = None;
    session.context_window = None;
    session.agent_preset = None;
}

fn cold_stage_result_sessions(task: &Task, sessions: &[AgentSession]) -> Vec<Uuid> {
    let Ok(workspace) = concrete_workspace(task) else {
        return Vec::new();
    };
    sessions
        .iter()
        .filter(|session| {
            source_session_matches(task, session, &workspace) && !session.detail_loaded
        })
        .map(|session| session.id)
        .collect()
}

type StageResultChoices = Result<Vec<(StageResultSource, String)>, String>;

fn stage_result_choices(task: &Task, sessions: &[AgentSession]) -> StageResultChoices {
    let workspace = concrete_workspace(task)?;
    if !cold_stage_result_sessions(task, sessions).is_empty() {
        return Err("Loading prior results".into());
    }
    let mut choices = Vec::new();
    for session in sessions
        .iter()
        .filter(|s| source_session_matches(task, s, &workspace))
    {
        let result_ids = successful_result_ids(session);
        for message in &session.messages {
            if result_ids.contains(&message.id) {
                let excerpt = message
                    .visible_content()
                    .chars()
                    .take(90)
                    .collect::<String>()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                choices.push((
                    StageResultSource {
                        session_id: session.id,
                        message_id: message.id,
                    },
                    format!("{} · {}", session.display_title(), excerpt),
                ));
            }
        }
    }
    Ok(choices)
}

/// Owned by one inspector visit and stage, never shared with another issue.
/// Hydration only changes the choices; selecting a result remains explicit.
pub(super) struct StageResultPicker {
    task_id: Uuid,
    stage_id: Uuid,
    menu: ContextMenuHandle,
    choices: Option<StageResultChoices>,
    pending: HashSet<Uuid>,
}

impl StageResultPicker {
    fn new(
        task_id: Uuid,
        stage_id: Uuid,
        menu: ContextMenuHandle,
        menus: &mut HashMap<SharedString, ContextMenuHandle>,
    ) -> Self {
        // Window-wide overlays dismiss registered menus. Replace this one slot
        // on every inspector visit so registration never revives an old picker.
        menus.insert("issue-stage-result".into(), menu.clone());
        Self {
            task_id,
            stage_id,
            menu,
            choices: None,
            pending: HashSet::new(),
        }
    }

    fn matches(&self, task_id: Uuid, stage_id: Uuid) -> bool {
        self.task_id == task_id && self.stage_id == stage_id
    }

    fn open(&mut self, task: &Task, sessions: &[AgentSession]) -> Vec<Uuid> {
        let cold = cold_stage_result_sessions(task, sessions);
        self.pending = cold.iter().copied().collect();
        self.choices = Some(stage_result_choices(task, sessions));
        cold
    }

    fn close(&mut self) {
        self.pending.clear();
        self.choices = None;
    }

    fn hydrated(
        &mut self,
        session_id: Uuid,
        error: Option<String>,
        task: Option<&Task>,
        sessions: &[AgentSession],
    ) {
        if !self.menu.is_open() || !self.pending.remove(&session_id) {
            return;
        }
        if let Some(error) = error {
            self.pending.clear();
            self.choices = Some(Err(error));
        } else if self.pending.is_empty() {
            self.choices = Some(
                task.ok_or_else(|| "Issue not found".to_owned())
                    .and_then(|task| stage_result_choices(task, sessions)),
            );
        } else {
            return;
        }
        self.menu.refresh_items();
    }
}

impl Waku {
    pub(super) fn load_stage_results(
        &mut self,
        task_id: Uuid,
        cx: &mut Context<Self>,
    ) -> StageResultChoices {
        let task = self
            .state
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .ok_or("Issue not found")?;
        let cold = cold_stage_result_sessions(task, &self.state.sessions);
        let choices = stage_result_choices(task, &self.state.sessions);
        for id in cold {
            self.ensure_session_loaded(id, cx);
        }
        choices
    }

    pub(super) fn refresh_stage_result_picker(&mut self, session_id: Uuid, error: Option<String>) {
        if let Some(picker) = self
            .task_detail
            .as_mut()
            .and_then(|detail| detail.stage_result_picker.as_mut())
        {
            let task = self
                .state
                .tasks
                .iter()
                .find(|task| task.id == picker.task_id);
            picker.hydrated(session_id, error, task, &self.state.sessions);
        }
    }

    pub(super) fn prepare_stage_invocation(
        &mut self,
        task_id: Uuid,
        stage_id: Uuid,
        source: Option<StageResultSource>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Uuid, String> {
        let task = self
            .state
            .tasks
            .iter()
            .find(|t| t.id == task_id)
            .cloned()
            .ok_or("Issue not found")?;
        let project_id = task.project_id().ok_or("Choose a project first")?;
        let project = self
            .state
            .projects
            .iter()
            .find(|p| p.id == project_id)
            .ok_or("Project not found")?;
        let stage = project
            .board
            .stage_for_task(&task)
            .filter(|stage| stage.id == stage_id)
            .cloned()
            .ok_or("The issue is no longer in this stage")?;
        let revision = project.board.revision;
        if stage.state.is_done() {
            return Err("Terminal stages cannot prepare execution drafts".into());
        }
        let workspace = invocation_workspace(&task, stage.handoff == StageHandoff::BriefOnly)?;
        let provider = stage
            .provider
            .ok_or("Configure a harness for this stage first")?;
        if !self.provider_enabled(provider) {
            return Err("This stage's harness is unavailable or switched off".into());
        }
        if let Some(model) = &stage.model {
            if !self
                .provider_probe(provider)
                .is_some_and(|probe| probe.model(model).is_some())
            {
                return Err("This stage's model is unavailable; update its settings first".into());
            }
        }
        match (stage.handoff, source.as_ref()) {
            (StageHandoff::PriorResult, None) => {
                return Err("Explicitly select a successful prior result first".into());
            }
            (StageHandoff::BriefOnly, Some(_)) => {
                return Err("This stage is configured for the issue brief only".into());
            }
            _ => {}
        }
        if let Some(source) = &source {
            self.ensure_session_loaded(source.session_id, cx);
        }
        let result = source
            .as_ref()
            .map(|source| {
                selected_result(&task, &workspace, &self.state.sessions, source)
                    .map(|text| (source, text))
            })
            .transpose()?;
        let prompt = stage_prompt(&task, &stage, result);
        let existing = self
            .state
            .sessions
            .iter()
            .find(|session| compatible_draft(session, &task, &workspace, &stage, &source))
            .map(|s| s.id);
        let id = if let Some(id) = existing {
            id
        } else {
            let mut session = self.state.new_session(project_id, provider);
            apply_stage_defaults(&mut session, &stage);
            session.task_id = Some(task_id);
            session.workspace = workspace;
            session.auto_title = Some(format!("{} · {}", stage.name, task.title));
            session.stage_invocation = Some(SessionStageInvocation {
                id: Uuid::new_v4(),
                stage,
                board_revision: revision,
                source,
            });
            let id = session.id;
            self.composer_drafts.set(
                crate::persistence::ComposerDraftKey::for_session(&session),
                crate::persistence::ComposerDraft {
                    text: prompt,
                    ..Default::default()
                },
            );
            self.state.push_session(session);
            id
        };
        // Preparing a draft never moves/reopens the issue or starts a provider.
        self.select_session(id, cx);
        self.schedule_composer_draft_save(cx);
        self.save_task_changes(cx);
        window.focus(&self.composer_focus(cx), cx);
        Ok(id)
    }

    /// Recheck the source at normal Send, including content removed since the
    /// picker opened. Historical started conversations keep their provenance.
    pub(super) fn validate_stage_submission(&self, session: &AgentSession) -> Result<(), String> {
        let Some(invocation) = &session.stage_invocation else {
            return Ok(());
        };
        if session.has_started() {
            return Ok(());
        }
        if Some(session.provider) != invocation.stage.provider {
            return Err("The stage harness changed; prepare a new stage draft".into());
        }
        if !self.provider_enabled(session.provider) {
            return Err("This stage's harness is unavailable or switched off".into());
        }
        let task = self
            .state
            .tasks
            .iter()
            .find(|task| Some(task.id) == session.task_id)
            .ok_or("The stage's issue is missing")?;
        if invocation.stage.state.is_done()
            || task.state.is_done()
            || task.stage_id != Some(invocation.stage.id)
        {
            return Err(
                "The issue moved out of this active stage; prepare a draft for its current stage"
                    .into(),
            );
        }
        if invocation_workspace(task, invocation.source.is_none())? != session.workspace
            || task.project_id() != Some(session.project_id)
        {
            return Err("The issue workspace changed; prepare a new stage draft".into());
        }
        if let Some(source) = &invocation.source {
            selected_result(task, &session.workspace, &self.state.sessions, source)?;
        }
        Ok(())
    }

    pub(super) fn render_stage_invocation_controls(
        &mut self,
        task: &Task,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let stage = task
            .project_id()
            .and_then(|id| self.state.projects.iter().find(|p| p.id == id))
            .and_then(|p| p.board.stage_for_task(task))
            .cloned();
        let picker_stage = stage.as_ref().filter(|stage| {
            !stage.state.is_done()
                && stage.handoff == StageHandoff::PriorResult
                && stage
                    .provider
                    .is_some_and(|provider| self.provider_enabled(provider))
        });
        if let Some(detail) = self.task_detail.as_mut()
            && detail.stage_result_picker.as_ref().is_some_and(|picker| {
                picker_stage.is_none_or(|stage| !picker.matches(task.id, stage.id))
            })
        {
            detail.stage_result_picker = None;
        }
        let Some(stage) = stage else {
            return div().into_any_element();
        };
        if stage.state.is_done() {
            return div().into_any_element();
        }
        let theme = Theme::current(cx);
        let task_id = task.id;
        let stage_id = stage.id;
        let mut section = div()
            .px(px(11.0))
            .min_w_0()
            .max_w_full()
            .flex()
            .items_center()
            .gap(px(8.0));
        let Some(provider) = stage.provider else {
            return section
                .text_size(sp(12.0))
                .text_color(theme.text_tertiary)
                .child(format!(
                    "{}: configure a harness to prepare a stage draft",
                    stage.name
                ))
                .into_any_element();
        };
        if !self.provider_enabled(provider) {
            return section
                .text_size(sp(12.0))
                .text_color(theme.text_tertiary)
                .child(format!(
                    "{}: harness unavailable or switched off",
                    stage.name
                ))
                .into_any_element();
        }
        if stage.handoff == StageHandoff::BriefOnly {
            section = section
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(sp(12.0))
                        .text_color(theme.text_tertiary)
                        .child(format!("{} · {}", stage.name, provider.display_name())),
                )
                .child(self.task_icon_action(
                    "issue-prepare-stage",
                    format!("Prepare {} draft", stage.name),
                    "icons/pencil.svg",
                    true,
                    move |this, window, cx| {
                        if let Err(error) =
                            this.prepare_stage_invocation(task_id, stage_id, None, window, cx)
                        {
                            this.show_toast(error);
                            cx.notify();
                        }
                    },
                    cx,
                ));
        } else {
            let weak = cx.entity().downgrade();
            let toggle_weak = weak.clone();
            let menus = &self.menus;
            let detail = self.task_detail.as_mut().expect("detail initialized");
            let picker = detail.stage_result_picker.get_or_insert_with(|| {
                let menu = ContextMenuHandle::new(cx);
                let identity = menu.focus_handle().clone();
                let menu = menu.on_toggle(move |open, _, cx| {
                    let _ = toggle_weak.update(cx, |this, cx| {
                        let Some(picker) = this
                            .task_detail
                            .as_mut()
                            .and_then(|detail| detail.stage_result_picker.as_mut())
                            .filter(|picker| picker.menu.focus_handle() == &identity)
                        else {
                            return;
                        };
                        let cold = if open {
                            let Some(task) =
                                this.state.tasks.iter().find(|task| task.id == task_id)
                            else {
                                return;
                            };
                            picker.open(task, &this.state.sessions)
                        } else {
                            picker.close();
                            Vec::new()
                        };
                        for id in cold {
                            this.ensure_session_loaded(id, cx);
                        }
                        cx.notify();
                    });
                });
                StageResultPicker::new(task_id, stage_id, menu, &mut menus.borrow_mut())
            });
            let handle = picker.menu.clone();
            section = section.child(crate::ui::menu::dropdown_menu_list(
                MenuChip::new("issue-stage-result")
                    .label(format!("Prepare {} with prior result", stage.name))
                    .max_w(px(280.0))
                    .min_w_0()
                    .overflow_hidden()
                    .caret(true)
                    .selected(handle.is_open()),
                "issue-stage-result-menu",
                &handle,
                MenuAlign::BelowLeft,
                move |cx: &mut App| {
                    let result = weak.upgrade().and_then(|entity| {
                        entity
                            .read(cx)
                            .task_detail
                            .as_ref()
                            .and_then(|detail| detail.stage_result_picker.as_ref())
                            .filter(|picker| picker.matches(task_id, stage_id))
                            .and_then(|picker| picker.choices.clone())
                    });
                    match result {
                        Some(Ok(choices)) if !choices.is_empty() => choices
                            .into_iter()
                            .map(|(source, label)| {
                                let weak = weak.clone();
                                let id = format!(
                                    "stage-result-{}-{}",
                                    source.session_id, source.message_id
                                );
                                MenuItem::new(label, move |window, cx| {
                                    let _ = weak.update(cx, |this, cx| {
                                        if let Err(error) = this.prepare_stage_invocation(
                                            task_id,
                                            stage_id,
                                            Some(source.clone()),
                                            window,
                                            cx,
                                        ) {
                                            this.show_toast(error);
                                            cx.notify();
                                        }
                                    });
                                })
                                .semantic_id(id)
                            })
                            .collect(),
                        Some(Err(error)) => vec![MenuItem::new(error, |_, _| {}).disabled(true)],
                        _ => vec![
                            MenuItem::new(
                                "No successful prior results in this workspace",
                                |_, _| {},
                            )
                            .disabled(true),
                        ],
                    }
                },
            ));
        }
        section.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Task, BoardStage, AgentSession) {
        let project = Uuid::new_v4();
        let mut task = Task::new("Implement checkout", TaskState::Doing, 0);
        task.description = "Preserve existing carts".into();
        task.workspace = Some(TaskWorkspace::Local {
            project_id: project,
        });
        let stage = BoardStage {
            id: Uuid::new_v4(),
            name: "Review".into(),
            state: TaskState::Doing,
            provider: Some(ProviderKind::Pi),
            model: None,
            instructions: "Inspect changes and report evidence".into(),
            handoff: StageHandoff::BriefOnly,
        };
        task.stage_id = Some(stage.id);
        let mut session = AgentSession::new(project, ProviderKind::Pi);
        session.task_id = Some(task.id);
        session.stage_invocation = Some(SessionStageInvocation {
            id: Uuid::new_v4(),
            stage: stage.clone(),
            board_revision: 4,
            source: None,
        });
        (task, stage, session)
    }

    fn completed_source(task: &Task) -> AgentSession {
        let mut session = AgentSession::new(task.project_id().unwrap(), ProviderKind::Pi);
        session.task_id = Some(task.id);
        let turn = session.begin_turn("Work");
        session.messages.push(Message::new_for_turn(
            MessageRole::Assistant,
            "Finished",
            turn,
        ));
        session.finish_active_turn(TurnStatus::Completed);
        session
    }

    #[gpui::test]
    fn prior_result_picker_refreshes_only_after_its_hydrations_finish(
        cx: &mut gpui::TestAppContext,
    ) {
        let (task, stage, _) = fixture();
        let mut sessions = vec![completed_source(&task), completed_source(&task)];
        for session in &mut sessions {
            session.detail_loaded = false;
        }
        let menu = cx.update(ContextMenuHandle::new);
        let mut menus = HashMap::new();
        let mut picker = StageResultPicker::new(task.id, stage.id, menu.clone(), &mut menus);
        let cx = cx.add_empty_window();
        cx.update(|window, cx| menu.open_context_menu(window, cx));
        assert_eq!(picker.open(&task, &sessions).len(), 2);
        assert!(picker.choices.as_ref().unwrap().is_err());
        picker.hydrated(Uuid::new_v4(), None, Some(&task), &sessions);
        assert_eq!(
            picker.pending.len(),
            2,
            "unrelated hydration must not refresh the picker"
        );
        sessions[1].detail_loaded = true;
        picker.hydrated(sessions[1].id, None, Some(&task), &sessions);
        assert!(picker.choices.as_ref().unwrap().is_err());
        sessions[0].detail_loaded = true;
        picker.hydrated(sessions[0].id, None, Some(&task), &sessions);
        let choices = picker.choices.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(choices.len(), 2);
        assert_eq!(choices[0].0.session_id, sessions[0].id);
        assert_eq!(choices[1].0.session_id, sessions[1].id);
        assert!(menu.is_open(), "hydration must preserve the open picker");
        assert!(sessions.iter().all(|session| session.turns.len() == 1));
        assert_eq!(task.stage_id, Some(stage.id));
    }

    #[gpui::test]
    fn prior_result_picker_dismissal_and_issue_navigation_ignore_old_hydration(
        cx: &mut gpui::TestAppContext,
    ) {
        let (task, stage, _) = fixture();
        let mut source = completed_source(&task);
        source.detail_loaded = false;
        let menu = cx.update(ContextMenuHandle::new);
        let mut menus = HashMap::new();
        let mut picker = StageResultPicker::new(task.id, stage.id, menu.clone(), &mut menus);
        let cx = cx.add_empty_window();
        cx.update(|window, cx| menu.open_context_menu(window, cx));
        picker.open(&task, std::slice::from_ref(&source));
        // Command palette/task switcher collect this registry before their
        // deferred close. Hydration arriving after that close must stay inert.
        let open_menus = menus
            .values()
            .filter(|menu| menu.is_open())
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(open_menus.len(), 1);
        cx.update(|window, cx| {
            for menu in open_menus {
                menu.close(window, cx);
            }
        });
        picker.close();
        source.detail_loaded = true;
        picker.hydrated(source.id, None, Some(&task), std::slice::from_ref(&source));
        assert!(picker.choices.is_none());
        assert!(!menu.is_open());

        // A new inspector visit owns a new picker, including when navigating
        // Back to the same issue. Old session completions cannot repopulate it.
        let (other, other_stage, _) = fixture();
        let other_menu = cx.update(|_, cx| ContextMenuHandle::new(cx));
        picker = StageResultPicker::new(other.id, other_stage.id, other_menu.clone(), &mut menus);
        assert_eq!(menus.len(), 1, "a visit replaces the registered handle");
        assert_ne!(picker.menu.focus_handle(), menu.focus_handle());
        assert_eq!(
            menus["issue-stage-result"].focus_handle(),
            other_menu.focus_handle()
        );
        cx.update(|window, cx| other_menu.open_context_menu(window, cx));
        picker.open(&other, std::slice::from_ref(&source));
        picker.hydrated(source.id, None, Some(&other), std::slice::from_ref(&source));
        assert!(
            picker
                .choices
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap()
                .is_empty()
        );
        assert!(!picker.matches(task.id, stage.id));
        assert!(
            !picker.matches(other.id, stage.id),
            "stage changes need a fresh picker too"
        );

        let fresh_menu = cx.update(|_, cx| ContextMenuHandle::new(cx));
        picker = StageResultPicker::new(task.id, stage.id, fresh_menu.clone(), &mut menus);
        picker.hydrated(source.id, None, Some(&task), std::slice::from_ref(&source));
        assert!(picker.choices.is_none());
        assert!(!fresh_menu.is_open());
        cx.update(|window, cx| fresh_menu.open_context_menu(window, cx));
        assert!(picker.open(&task, std::slice::from_ref(&source)).is_empty());
        assert_eq!(picker.choices.as_ref().unwrap().as_ref().unwrap().len(), 1);
    }

    #[gpui::test]
    fn prior_result_picker_failure_waits_for_explicit_retry_and_rechecks_workspace(
        cx: &mut gpui::TestAppContext,
    ) {
        let (mut task, stage, _) = fixture();
        let mut source = completed_source(&task);
        source.detail_loaded = false;
        let menu = cx.update(ContextMenuHandle::new);
        let mut menus = HashMap::new();
        let mut picker = StageResultPicker::new(task.id, stage.id, menu.clone(), &mut menus);
        let cx = cx.add_empty_window();
        cx.update(|window, cx| menu.open_context_menu(window, cx));
        picker.open(&task, std::slice::from_ref(&source));
        picker.hydrated(
            source.id,
            Some("load failed".into()),
            Some(&task),
            std::slice::from_ref(&source),
        );
        assert_eq!(picker.choices, Some(Err("load failed".into())));
        assert!(
            picker.pending.is_empty(),
            "failure must not start a retry loop"
        );
        picker.hydrated(source.id, None, Some(&task), std::slice::from_ref(&source));
        assert_eq!(picker.choices, Some(Err("load failed".into())));
        cx.update(|window, cx| menu.close(window, cx));
        picker.close();
        cx.update(|window, cx| menu.open_context_menu(window, cx));
        assert_eq!(
            picker.open(&task, std::slice::from_ref(&source)),
            vec![source.id]
        );
        source.detail_loaded = true;
        task.workspace = Some(TaskWorkspace::Worktree {
            project_id: task.project_id().unwrap(),
            path: "/changed".into(),
            branch: "changed".into(),
        });
        picker.hydrated(source.id, None, Some(&task), std::slice::from_ref(&source));
        assert!(
            picker
                .choices
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn stage_reuse_requires_exact_issue_workspace_config_and_source() {
        let (task, stage, session) = fixture();
        let workspace = concrete_workspace(&task).unwrap();
        assert!(compatible_draft(&session, &task, &workspace, &stage, &None));
        let mut changed = stage.clone();
        changed.instructions.push_str(" Carefully.");
        assert!(!compatible_draft(
            &session, &task, &workspace, &changed, &None
        ));
        changed = stage.clone();
        changed.provider = Some(ProviderKind::Codex);
        assert!(!compatible_draft(
            &session, &task, &workspace, &changed, &None
        ));
        let mut other = task.clone();
        other.id = Uuid::new_v4();
        assert!(!compatible_draft(
            &session, &other, &workspace, &stage, &None
        ));
        let source = Some(StageResultSource {
            session_id: Uuid::new_v4(),
            message_id: Uuid::new_v4(),
        });
        assert!(!compatible_draft(
            &session, &task, &workspace, &stage, &source
        ));
        let different = SessionWorkspace::Worktree {
            path: "/other".into(),
            branch: "other".into(),
        };
        assert!(!compatible_draft(
            &session, &task, &different, &stage, &None
        ));
        let mut started = session;
        started.begin_turn("Already sent");
        assert!(!compatible_draft(
            &started, &task, &workspace, &stage, &None
        ));
    }

    #[test]
    fn provider_default_clears_inherited_model_traits_and_preserves_ordinary_session() {
        let (_, stage, mut session) = fixture();
        session.model = Some("last-model".into());
        session.reasoning_effort = Some("high".into());
        session.service_tier = Some("fast".into());
        session.context_window = Some("large".into());
        session.agent_preset = Some("custom".into());
        apply_stage_defaults(&mut session, &stage);
        assert!(session.model.is_none());
        assert!(session.reasoning_effort.is_none());
        assert!(session.service_tier.is_none());
        assert!(session.context_window.is_none());
        assert!(session.agent_preset.is_none());
        assert!(session.review.is_none());
        assert!(session.provider_cursor.is_none());
        assert!(session.turns.is_empty());
    }

    #[test]
    fn handoff_requires_loaded_successful_assistant_turn_not_idle_or_failure() {
        let (task, _, mut session) = fixture();
        let turn = session.begin_turn("Work");
        let message = Message::new_for_turn(MessageRole::Assistant, "Result", turn);
        let id = message.id;
        session.messages.push(message);
        assert!(successful_result(&session, id).is_none());
        session.finish_active_turn(TurnStatus::Failed);
        assert!(successful_result(&session, id).is_none());
        session.turns[0].status = TurnStatus::Interrupted;
        assert!(successful_result(&session, id).is_none());
        session.turns[0].status = TurnStatus::Completed;
        assert_eq!(successful_result(&session, id), Some("Result"));
        session.detail_loaded = false;
        assert!(successful_result(&session, id).is_none());
        session.detail_loaded = true;
        let source = StageResultSource {
            session_id: session.id,
            message_id: id,
        };
        let workspace = concrete_workspace(&task).unwrap();
        assert!(selected_result(&task, &workspace, &[session.clone()], &source).is_ok());
        session.task_id = Some(Uuid::new_v4());
        assert!(selected_result(&task, &workspace, &[session], &source).is_err());
    }

    #[test]
    fn only_final_response_is_a_result_and_native_review_requires_explicit_success() {
        let (_, _, mut session) = fixture();
        let turn = session.begin_turn("Work");
        let commentary = Message::new_for_turn(MessageRole::Assistant, "Investigating", turn);
        let commentary_id = commentary.id;
        let final_message = Message::new_for_turn(MessageRole::Assistant, "Finished", turn);
        let final_id = final_message.id;
        session.messages.extend([commentary, final_message]);
        session.finish_active_turn(TurnStatus::Completed);
        assert!(successful_result(&session, commentary_id).is_none());
        assert_eq!(successful_result(&session, final_id), Some("Finished"));
        session.review = Some(SessionReview {
            target: IssueReviewTarget::UncommittedChanges,
            source_session_id: None,
            status: IssueReviewStatus::Failed,
            summary: None,
            result_message_id: Some(final_id),
            fix_session_id: None,
        });
        assert!(successful_result(&session, final_id).is_none());
        session.review.as_mut().unwrap().status = IssueReviewStatus::Completed;
        assert_eq!(successful_result(&session, final_id), Some("Finished"));
        session.review.as_mut().unwrap().result_message_id = Some(commentary_id);
        assert!(successful_result(&session, final_id).is_none());
    }

    #[test]
    fn brief_only_can_prepare_planned_workspace_but_handoff_cannot() {
        let (mut task, _, _) = fixture();
        let project_id = task.project_id().unwrap();
        task.workspace = Some(TaskWorkspace::NewWorktree { project_id });
        assert!(invocation_workspace(&task, true).is_ok());
        assert!(concrete_workspace(&task).is_err());
    }

    #[test]
    fn prompt_is_bounded_visible_source_only_and_review_name_has_no_native_semantics() {
        let (task, stage, mut session) = fixture();
        let turn = session.begin_turn("Private user history");
        let mut message =
            Message::new_for_turn(MessageRole::Assistant, "provider-only-control", turn);
        message.display_content = Some("界".repeat(RESULT_EXCERPT_CHARS + 10));
        let source = StageResultSource {
            session_id: session.id,
            message_id: message.id,
        };
        session.messages.push(message);
        session.finish_active_turn(TurnStatus::Completed);
        let text = selected_result(
            &task,
            &SessionWorkspace::Local,
            std::slice::from_ref(&session),
            &source,
        )
        .unwrap();
        let prompt = stage_prompt(&task, &stage, Some((&source, text)));
        assert_eq!(prompt.matches('界').count(), RESULT_EXCERPT_CHARS);
        assert!(prompt.contains(&source.session_id.to_string()));
        assert!(prompt.contains(&source.message_id.to_string()));
        assert!(prompt.contains("[Result excerpt truncated]"));
        assert!(!prompt.contains("provider-only-control"));
        assert!(!prompt.contains("Private user history"));
        assert!(!prompt.contains("approved"));
        assert!(session.review.is_none());
        session
            .messages
            .retain(|message| message.id != source.message_id);
        assert!(selected_result(&task, &SessionWorkspace::Local, &[session], &source).is_err());
    }

    #[test]
    fn stage_provenance_survives_stale_catalog_and_hydration() {
        let (_, _, mut local) = fixture();
        local.updated_at = 20;
        let invocation = local.stage_invocation.clone();
        let mut stale = local.clone();
        stale.updated_at = 10;
        stale.stage_invocation = None;
        let mut sessions = vec![local];
        super::super::runtime::merge_remote_session_catalog(
            &mut sessions,
            vec![stale.clone()],
            |_| false,
        );
        assert_eq!(sessions[0].stage_invocation, invocation);
        super::super::runtime::replace_hydrated_session(&mut sessions[0], stale);
        assert_eq!(sessions[0].stage_invocation, invocation);
    }
}
