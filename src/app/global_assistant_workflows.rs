//! Explicit controls for daemon-owned workflows. Rendering reads snapshots only;
//! neither board movement nor assistant output can launch or advance a run.
use super::global_assistant_context::{self as context, AssistantCommand, IssueReference};
use super::*;
use crate::model::BoardStage;
use crate::ui::menu::dropdown_menu_list;
use waku_protocol::{
    IssueAttemptStatus, IssueRun, IssueRunAction, IssueRunStatus, IssueStageRole, IssueWorkflow,
};

#[derive(Default)]
pub(super) struct AssistantWorkflows {
    runs: HashMap<Uuid, IssueRun>,
    setups: HashMap<Uuid, WorkflowSetup>,
    visible_setup: Option<Uuid>,
    refresh_in_flight: Option<u64>,
    generation: u64,
    poll_armed: bool,
    poll_generation: u64,
    action_in_flight: Option<Uuid>,
    refresh_error: Option<String>,
    loaded: bool,
}

struct WorkflowSetup {
    project_id: Uuid,
    workflow: IssueWorkflow,
    request_id: Uuid,
    // Once a request might have reached the daemon, its identity and payload
    // must survive dismissal, navigation, and transport errors unchanged.
    submitted_task: Option<Task>,
    error: Option<String>,
}

impl AssistantWorkflows {
    pub(super) fn new() -> Self {
        Self::default()
    }

    fn accept_run(&mut self, run: IssueRun) -> bool {
        let task_id = run.task_id;
        let mut changed = false;
        if self
            .runs
            .get(&task_id)
            .is_none_or(|old| run_order(&run) >= run_order(old) && *old != run)
        {
            self.runs.insert(task_id, run.clone());
            changed = true;
        }
        if self
            .setups
            .get(&task_id)
            .is_some_and(|setup| setup.request_id == run.request_id)
        {
            self.setups.remove(&task_id);
            changed = true;
            if self.visible_setup == Some(task_id) {
                self.visible_setup = None;
            }
        }
        changed
    }
}

fn run_order(run: &IssueRun) -> (u64, u64, Uuid) {
    (run.created_at, run.updated_at, run.id)
}

fn stage_control_id(task_id: Uuid, stage_id: Uuid, control: &str) -> String {
    format!("assistant-workflow-{task_id}-{stage_id}-{control}")
}

fn report_session_id(run: &IssueRun) -> Option<Uuid> {
    let report = run
        .report
        .as_ref()
        .filter(|report| report.run_id == run.id)?;
    run.attempts
        .iter()
        .find(|attempt| attempt.id == report.attempt_id)
        .map(|attempt| attempt.session_id)
}

fn role_for_name(name: &str) -> Option<IssueStageRole> {
    match name.trim().to_ascii_lowercase().as_str() {
        "plan" | "planning" => Some(IssueStageRole::Plan),
        "execute" | "execution" | "implement" | "implementation" => Some(IssueStageRole::Execution),
        "review" | "code review" => Some(IssueStageRole::Review),
        "report" | "reporting" => Some(IssueStageRole::Report),
        _ => None,
    }
}

fn suggested_workflow(stages: &[BoardStage], fallback: ProviderKind) -> IssueWorkflow {
    let mut workflow = IssueWorkflow::plan_execute_review_report(fallback);
    for row in &mut workflow.stages {
        let mut matches = stages
            .iter()
            .filter(|stage| role_for_name(&stage.name) == Some(row.role));
        let Some(stage) = matches.next() else {
            continue;
        };
        // Ambiguous names and lifecycle columns are not execution defaults.
        if matches.next().is_some() {
            continue;
        }
        // An existing explicit Report → Done mapping is honored by the daemon
        // only after the report provider has closed. New presets never infer it.
        row.board_stage_id =
            (row.role == IssueStageRole::Report && stage.state.is_done()).then_some(stage.id);
        if let Some(provider) = stage.provider {
            row.provider = provider;
            row.model = stage.model.clone();
        }
        if !stage.instructions.trim().is_empty() {
            row.instructions
                .push_str("\n\nProject stage instructions:\n");
            row.instructions.push_str(&stage.instructions);
        }
    }
    workflow
}

fn target_issue(
    explicit: Option<&IssueReference>,
    selected_task: Option<Uuid>,
    session_task: Option<Uuid>,
    tasks: &[Task],
) -> Result<Uuid, String> {
    if let Some(reference) = explicit {
        return context::resolve_issue(reference, tasks);
    }
    selected_task
        .or(session_task)
        .ok_or_else(|| "Select an issue, or use /start <issue ID>".into())
}

fn allowed_action(status: IssueRunStatus, action: IssueRunAction) -> bool {
    match action {
        IssueRunAction::Pause => status == IssueRunStatus::Running,
        IssueRunAction::Resume => status == IssueRunStatus::Paused,
        IssueRunAction::Retry => matches!(status, IssueRunStatus::Blocked | IssueRunStatus::Failed),
        IssueRunAction::Cancel => {
            !status.is_terminal() && status != IssueRunStatus::CancelRequested
        }
    }
}

fn blocked_live_session(run: &IssueRun) -> Option<Uuid> {
    let attempt = run.attempts.last()?;
    (run.status == IssueRunStatus::Blocked
        && attempt.finished_at.is_none()
        && matches!(
            attempt.status,
            IssueAttemptStatus::Running | IssueAttemptStatus::Blocked
        ))
    .then_some(attempt.session_id)
}

fn status_label(status: IssueRunStatus, has_report: bool) -> &'static str {
    match status {
        IssueRunStatus::Running => "Running",
        IssueRunStatus::PauseRequested => "Pausing",
        IssueRunStatus::CancelRequested => "Cancelling",
        IssueRunStatus::Paused => "Paused",
        IssueRunStatus::Blocked => "Blocked",
        IssueRunStatus::Failed => "Failed",
        IssueRunStatus::Cancelled => "Cancelled",
        IssueRunStatus::Completed if has_report => "Report ready",
        IssueRunStatus::Completed => "Completed · report unavailable",
    }
}

fn run_label(run: &IssueRun) -> String {
    let status = status_label(run.status, run.report.is_some());
    if run.status.is_terminal() {
        status.to_owned()
    } else if let Some(stage) = run.current_stage() {
        format!("{} · {status}", stage.name)
    } else {
        status.to_owned()
    }
}

fn bounded_text(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let mut result: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}

fn workflow_context(runs: &HashMap<Uuid, IssueRun>, selected: Option<Uuid>) -> String {
    const LIMIT: usize = 2_000;
    let mut runs = runs.values().collect::<Vec<_>>();
    runs.sort_by_key(|run| {
        (
            Some(run.task_id) != selected,
            std::cmp::Reverse(run.updated_at),
        )
    });
    let mut result = String::from("Cached issue workflows (report ready is not merge evidence):\n");
    for run in runs.into_iter().take(12) {
        let mut line = format!(
            "issue={} status={} stage={}\n",
            run.task_id,
            status_label(run.status, run.report.is_some()),
            run.current_stage()
                .map(|stage| bounded_text(&stage.name, 60))
                .unwrap_or_else(|| "none".into())
        );
        if let Some(error) = &run.error {
            line.push_str(&format!("error={:?}\n", bounded_text(error, 180)));
        }
        if let Some(report) = &run.report {
            line.push_str(&format!(
                "report={:?}\n",
                bounded_text(&report.summary, 220)
            ));
        }
        if result.chars().count() + line.chars().count() > LIMIT {
            result.push_str("[more workflows omitted]\n");
            break;
        }
        result.push_str(&line);
    }
    result.chars().take(LIMIT).collect()
}

impl Waku {
    pub(super) fn assistant_workflow_context(&self) -> String {
        let selected = self.assistant_workflow_issue(None).ok();
        let mut context = if let Some(error) = &self.assistant_workflows.refresh_error {
            format!(
                "Workflow refresh failed; cached statuses may be stale: {:?}\n",
                bounded_text(error, 180)
            )
        } else if !self.assistant_workflows.loaded {
            "Workflow status has not loaded yet.\n".into()
        } else {
            String::new()
        };
        context.push_str(&workflow_context(&self.assistant_workflows.runs, selected));
        context.chars().take(2_000).collect()
    }

    pub(super) fn refresh_assistant_workflows(&mut self, cx: &mut Context<Self>) {
        if self.assistant_workflows.refresh_in_flight.is_some() {
            return;
        }
        self.assistant_workflows.generation = self.assistant_workflows.generation.wrapping_add(1);
        let generation = self.assistant_workflows.generation;
        self.assistant_workflows.refresh_in_flight = Some(generation);
        let client = waku_client::IssueRunClient::new(self.daemon.client());
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { client.list(None) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.assistant_workflows.refresh_in_flight != Some(generation) {
                    return;
                }
                this.assistant_workflows.refresh_in_flight = None;
                if this.assistant_workflows.generation != generation {
                    if this.assistant.open || this.board_open {
                        this.refresh_assistant_workflows(cx);
                    }
                    return;
                }
                let changed = match result {
                    Ok(runs) => {
                        let mut changed = !this.assistant_workflows.loaded
                            || this.assistant_workflows.refresh_error.is_some();
                        this.assistant_workflows.loaded = true;
                        this.assistant_workflows.refresh_error = None;
                        // Do not erase a newer command response with a list
                        // snapshot taken before that command settled.
                        for run in runs {
                            changed |= this.assistant_workflows.accept_run(run);
                        }
                        changed
                    }
                    Err(error) => {
                        let error = error.to_string();
                        let changed =
                            this.assistant_workflows.refresh_error.as_ref() != Some(&error);
                        this.assistant_workflows.refresh_error = Some(error);
                        changed
                    }
                };
                if changed {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Call on initialization and opening either surface, never from render.
    /// The sole timer exits when both surfaces close; opening restarts it.
    pub(super) fn start_assistant_workflow_poll(&mut self, cx: &mut Context<Self>) {
        if !(self.assistant.open || self.board_open) || self.assistant_workflows.poll_armed {
            return;
        }
        self.assistant_workflows.poll_armed = true;
        self.assistant_workflows.poll_generation =
            self.assistant_workflows.poll_generation.wrapping_add(1);
        let generation = self.assistant_workflows.poll_generation;
        self.refresh_assistant_workflows(cx);
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                let keep_polling = this
                    .update(cx, |this, cx| {
                        if this.assistant_workflows.poll_generation != generation {
                            return false;
                        }
                        if !(this.assistant.open || this.board_open) {
                            this.assistant_workflows.poll_armed = false;
                            return false;
                        }
                        this.refresh_assistant_workflows(cx);
                        true
                    })
                    .unwrap_or(false);
                if !keep_polling {
                    break;
                }
            }
        })
        .detach();
    }

    fn assistant_workflow_issue(&self, explicit: Option<&IssueReference>) -> Result<Uuid, String> {
        let (_, session_id, task_id) = self.assistant_context_selection();
        target_issue(
            explicit,
            task_id,
            session_id
                .and_then(|id| self.session(id))
                .and_then(|session| session.task_id),
            &self.state.tasks,
        )
    }

    pub(super) fn run_assistant_workflow_command(
        &mut self,
        command: AssistantCommand,
        cx: &mut Context<Self>,
    ) {
        let result = match command {
            AssistantCommand::Start(reference) => self
                .assistant_workflow_issue(reference.as_ref())
                .and_then(|id| self.open_assistant_workflow_setup(id, cx)),
            AssistantCommand::Pause
            | AssistantCommand::Resume
            | AssistantCommand::Cancel
            | AssistantCommand::Retry => {
                let action = match command {
                    AssistantCommand::Pause => IssueRunAction::Pause,
                    AssistantCommand::Resume => IssueRunAction::Resume,
                    AssistantCommand::Cancel => IssueRunAction::Cancel,
                    _ => IssueRunAction::Retry,
                };
                self.assistant_workflow_issue(None).and_then(|id| {
                    let run =
                        self.assistant_workflows.runs.get(&id).cloned().ok_or(
                            "No workflow is loaded for this issue; refresh its status first",
                        )?;
                    self.control_assistant_workflow(id, run.id, action, cx)
                })
            }
            // Non-workflow commands remain owned by the assistant dispatcher.
            _ => return,
        };
        if let Err(error) = result {
            self.assistant.notice = Some(error);
        }
        cx.notify();
    }

    fn validate_assistant_workflow_issue(
        &self,
        task_id: Uuid,
        project_id: Option<Uuid>,
        cx: &App,
    ) -> Result<Task, String> {
        if self.is_task_draft(task_id) {
            return Err("Create and save this issue before starting a workflow".into());
        }
        let task = self
            .state
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .ok_or("Issue not found")?;
        if task.title.trim().is_empty()
            || self
                .task_detail_values(task_id, cx)
                .is_some_and(|(title, _)| title.trim().is_empty())
        {
            return Err("Enter an issue title before starting a workflow".into());
        }
        if task.state.is_done() || self.task_completion_stops.contains_key(&task_id) {
            return Err("Reopen this issue before starting a workflow".into());
        }
        let actual_project = task
            .project_id()
            .ok_or("Choose this issue's project and workspace first")?;
        if project_id.is_some_and(|expected| expected != actual_project) {
            return Err(
                "The issue's project changed; close setup and review its workflow again".into(),
            );
        }
        if !self
            .state
            .projects
            .iter()
            .any(|project| project.id == actual_project)
        {
            return Err("This issue's project is unavailable".into());
        }
        Ok(task.clone())
    }

    fn open_assistant_workflow_setup(
        &mut self,
        task_id: Uuid,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let task = self.validate_assistant_workflow_issue(task_id, None, cx)?;
        if self
            .assistant_workflows
            .runs
            .get(&task_id)
            .is_some_and(|run| !run.status.is_terminal())
        {
            return Err("This issue already has a workflow; use its pause, resume, retry, or cancel control".into());
        }
        let project_id = task.project_id().ok_or("Choose a project first")?;
        // An unsubmitted suggestion can be replaced after a project change;
        // an uncertain submitted request must retain its original identity.
        if self
            .assistant_workflows
            .setups
            .get(&task_id)
            .is_some_and(|setup| setup.project_id != project_id && setup.submitted_task.is_none())
        {
            self.assistant_workflows.setups.remove(&task_id);
        }
        if !self.assistant_workflows.setups.contains_key(&task_id) {
            let project = self
                .state
                .projects
                .iter()
                .find(|project| project.id == project_id)
                .ok_or("Project not found")?;
            let provider = if self.provider_enabled(self.state.last_provider) {
                self.state.last_provider
            } else {
                ProviderKind::ALL
                    .iter()
                    .copied()
                    .find(|provider| self.provider_enabled(*provider))
                    .unwrap_or(self.state.last_provider)
            };
            let workflow = suggested_workflow(&project.board.stages, provider);
            self.assistant_workflows.setups.insert(
                task_id,
                WorkflowSetup {
                    project_id,
                    workflow,
                    request_id: Uuid::new_v4(),
                    submitted_task: None,
                    error: None,
                },
            );
        }
        self.assistant_workflows.visible_setup = Some(task_id);
        self.assistant.notice = None;
        self.start_assistant_workflow_poll(cx);
        cx.notify();
        Ok(())
    }

    fn validate_assistant_workflow(&self, workflow: &IssueWorkflow) -> Result<(), String> {
        workflow.validate()?;
        for stage in &workflow.stages {
            if !self.provider_enabled(stage.provider) {
                return Err(format!("{}: choose an enabled harness", stage.name));
            }
            if let Some(model) = &stage.model {
                if !self
                    .provider_probe(stage.provider)
                    .is_some_and(|probe| probe.model(model).is_some())
                {
                    return Err(format!(
                        "{}: choose an available model or provider default",
                        stage.name
                    ));
                }
            }
        }
        Ok(())
    }

    fn start_assistant_workflow(&mut self, task_id: Uuid, cx: &mut Context<Self>) {
        if self.assistant_workflows.action_in_flight.is_some() {
            return;
        }
        let result = (|| {
            let setup = self
                .assistant_workflows
                .setups
                .get(&task_id)
                .ok_or("Open workflow setup first")?;
            let task =
                self.validate_assistant_workflow_issue(task_id, Some(setup.project_id), cx)?;
            self.validate_assistant_workflow(&setup.workflow)?;
            if setup
                .submitted_task
                .as_ref()
                .is_some_and(|submitted| *submitted != task)
            {
                return Err("The issue changed after an uncertain start. Refresh status before retrying the original request".to_owned());
            }
            if self
                .assistant_workflows
                .runs
                .get(&task_id)
                .is_some_and(|run| !run.status.is_terminal())
            {
                return Err("This issue already has a workflow".to_owned());
            }
            Ok((task, setup.request_id))
        })();
        let (task, request_id) = match result {
            Ok(value) => value,
            Err(error) => {
                self.assistant_workflow_error(task_id, error, cx);
                return;
            }
        };
        self.assistant_workflows.action_in_flight = Some(task_id);
        if let Some(setup) = self.assistant_workflows.setups.get_mut(&task_id) {
            setup.error = None;
        }
        // This path consumes debounced edits and waits for the daemon's actual
        // persistence acknowledgment; a transient timer never implies a save.
        self.save_task_changes(cx);
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                let wait = this
                    .update(cx, |this, cx| {
                        if this.assistant_workflows.action_in_flight != Some(task_id) {
                            return false;
                        }
                        if this.task_save_in_flight || this.task_save_pending {
                            return true;
                        }
                        if let Some(error) = this.task_save_error.clone() {
                            this.assistant_workflows.action_in_flight = None;
                            this.assistant_workflow_error(
                                task_id,
                                format!("Save the issue before starting: {error}"),
                                cx,
                            );
                            return false;
                        }
                        let valid =
                            this.validate_assistant_workflow_issue(task_id, task.project_id(), cx);
                        if !matches!(valid, Ok(ref current) if *current == task) {
                            this.assistant_workflows.action_in_flight = None;
                            this.assistant_workflow_error(
                                task_id,
                                "The issue changed while saving; review setup and start again"
                                    .into(),
                                cx,
                            );
                            return false;
                        }
                        this.send_assistant_workflow_start(task.clone(), request_id, cx);
                        false
                    })
                    .unwrap_or(false);
                if !wait {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn send_assistant_workflow_start(
        &mut self,
        task: Task,
        request_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        let task_id = task.id;
        let Some(setup) = self
            .assistant_workflows
            .setups
            .get_mut(&task_id)
            .filter(|setup| setup.request_id == request_id)
        else {
            self.assistant_workflows.action_in_flight = None;
            return;
        };
        setup.submitted_task = Some(task);
        let workflow = setup.workflow.clone();
        self.assistant_workflows.generation = self.assistant_workflows.generation.wrapping_add(1);
        let client = waku_client::IssueRunClient::new(self.daemon.client());
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { client.start(task_id, request_id, workflow) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.assistant_workflows.action_in_flight = None;
                this.assistant_workflows.generation =
                    this.assistant_workflows.generation.wrapping_add(1);
                match result {
                    Ok(run) if run.task_id == task_id && run.request_id == request_id => {
                        this.assistant_workflows.accept_run(run);
                        this.assistant.notice = None;
                    }
                    Ok(_) => this.assistant_workflow_error(
                        task_id,
                        "The daemon returned a different workflow. Refresh status before retrying the original request".into(),
                        cx,
                    ),
                    Err(error) => this.assistant_workflow_error(
                        task_id,
                        format!("Start not confirmed: {error}. Retry uses the same request"),
                        cx,
                    ),
                }
                this.refresh_assistant_workflows(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn assistant_workflow_error(&mut self, task_id: Uuid, error: String, cx: &mut Context<Self>) {
        if let Some(setup) = self.assistant_workflows.setups.get_mut(&task_id) {
            setup.error = Some(error);
        } else {
            self.assistant.notice = Some(error);
        }
        cx.notify();
    }

    fn control_assistant_workflow(
        &mut self,
        task_id: Uuid,
        run_id: Uuid,
        action: IssueRunAction,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.assistant_workflows.action_in_flight.is_some() {
            return Err("Wait for the current workflow request".into());
        }
        let run = self
            .assistant_workflows
            .runs
            .get(&task_id)
            .filter(|run| run.id == run_id)
            .ok_or("The workflow changed; refresh its status first")?;
        if !allowed_action(run.status, action) {
            return Err("That control is unavailable for the workflow's current state".into());
        }
        if action == IssueRunAction::Retry && blocked_live_session(run).is_some() {
            return Err("Open the stage conversation and resolve its pending permission or input before retrying".into());
        }
        self.assistant_workflows.action_in_flight = Some(task_id);
        self.assistant_workflows.generation = self.assistant_workflows.generation.wrapping_add(1);
        self.assistant.notice = None;
        let client = waku_client::IssueRunClient::new(self.daemon.client());
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { client.control(run_id, action) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.assistant_workflows.action_in_flight = None;
                this.assistant_workflows.generation =
                    this.assistant_workflows.generation.wrapping_add(1);
                match result {
                    Ok(run) if run.id == run_id && run.task_id == task_id => {
                        this.assistant_workflows.accept_run(run);
                    }
                    Ok(_) => {
                        this.assistant.notice =
                            Some("The daemon returned a different workflow; refresh status".into())
                    }
                    Err(error) => {
                        this.assistant.notice =
                            Some(format!("Workflow request not confirmed: {error}"))
                    }
                }
                this.refresh_assistant_workflows(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
        Ok(())
    }

    fn render_assistant_workflow_picker(
        &self,
        task_id: Uuid,
        index: usize,
        provider_picker: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let setup = &self.assistant_workflows.setups[&task_id];
        let stage = &setup.workflow.stages[index];
        let request_id = setup.request_id;
        let stage_id = stage.id;
        let provider = stage.provider;
        let selected_model = stage.model.clone();
        let locked =
            setup.submitted_task.is_some() || self.assistant_workflows.action_in_flight.is_some();
        let id: SharedString = stage_control_id(
            task_id,
            stage_id,
            if provider_picker { "provider" } else { "model" },
        )
        .into();
        let handle = self.menu_handle(id.clone(), cx);
        let label = if provider_picker {
            provider.display_name().to_owned()
        } else {
            selected_model
                .as_ref()
                .and_then(|id| {
                    self.provider_probe(provider)
                        .and_then(|probe| probe.model(id))
                })
                .map(|model| model.name.clone())
                .or(selected_model.clone())
                .unwrap_or_else(|| "Provider default".into())
        };
        let weak = cx.entity().downgrade();
        let providers = if handle.is_open() && provider_picker {
            ProviderKind::ALL
                .iter()
                .copied()
                .map(|provider| (provider, self.provider_enabled(provider)))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let models = if handle.is_open() && !provider_picker {
            self.provider_probe(provider)
                .map(|probe| {
                    probe
                        .models
                        .iter()
                        .map(|model| (model.id.clone(), model.name.clone()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        dropdown_menu_list(
            MenuChip::new(id.clone())
                .label(label)
                .caret(true)
                .height(px(26.0))
                .min_w_0()
                .max_w(px(if provider_picker { 132.0 } else { 180.0 }))
                .overflow_hidden()
                .disabled(locked)
                .selected(handle.is_open()),
            SharedString::from(format!("{id}-menu")),
            &handle,
            MenuAlign::BelowLeft,
            move |_| {
                if provider_picker {
                    providers
                        .iter()
                        .map(|(choice, enabled)| {
                            let choice = *choice;
                            let weak = weak.clone();
                            MenuItem::new(
                                if *enabled {
                                    choice.display_name().to_owned()
                                } else {
                                    format!("{} (unavailable)", choice.display_name())
                                },
                                move |_, cx| {
                                    let _ = weak.update(cx, |this, cx| {
                                        if this.assistant_workflows.action_in_flight.is_some() {
                                            return;
                                        }
                                        if let Some(setup) = this
                                            .assistant_workflows
                                            .setups
                                            .get_mut(&task_id)
                                            .filter(|setup| {
                                                setup.request_id == request_id
                                                    && setup.submitted_task.is_none()
                                            })
                                        {
                                            if let Some(stage) = setup
                                                .workflow
                                                .stages
                                                .iter_mut()
                                                .find(|stage| stage.id == stage_id)
                                            {
                                                if stage.provider != choice {
                                                    stage.provider = choice;
                                                    stage.model = None;
                                                }
                                                setup.error = None;
                                            }
                                        }
                                        cx.notify();
                                    });
                                },
                            )
                            .selected(choice == provider)
                            .disabled(!*enabled)
                            .semantic_id(stage_control_id(
                                task_id,
                                stage_id,
                                &format!("provider-{choice:?}"),
                            ))
                        })
                        .collect()
                } else {
                    std::iter::once((None, "Provider default".to_owned()))
                        .chain(
                            models
                                .iter()
                                .map(|(id, name)| (Some(id.clone()), name.clone())),
                        )
                        .map(|(choice, label)| {
                            let weak = weak.clone();
                            let selected = choice == selected_model;
                            let semantic = stage_control_id(
                                task_id,
                                stage_id,
                                &format!("model-{}", choice.as_deref().unwrap_or("default")),
                            );
                            MenuItem::new(label, move |_, cx| {
                                let _ = weak.update(cx, |this, cx| {
                                    if this.assistant_workflows.action_in_flight.is_some() {
                                        return;
                                    }
                                    if let Some(setup) =
                                        this.assistant_workflows.setups.get_mut(&task_id).filter(
                                            |setup| {
                                                setup.request_id == request_id
                                                    && setup.submitted_task.is_none()
                                            },
                                        )
                                    {
                                        if let Some(stage) =
                                            setup.workflow.stages.iter_mut().find(|stage| {
                                                stage.id == stage_id && stage.provider == provider
                                            })
                                        {
                                            stage.model = choice.clone();
                                            setup.error = None;
                                        }
                                    }
                                    cx.notify();
                                });
                            })
                            .selected(selected)
                            .semantic_id(semantic)
                        })
                        .collect()
                }
            },
        )
    }

    pub(super) fn render_assistant_workflow_controls(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let task_id = self
            .assistant_workflows
            .visible_setup
            .or_else(|| self.assistant_workflow_issue(None).ok())?;
        let task = self.task_for_editor(task_id)?;
        let theme = Theme::current(cx);
        let busy = self.assistant_workflows.action_in_flight.is_some();
        let run = self.assistant_workflows.runs.get(&task_id);
        let mut panel = div()
            .id("assistant-workflow-controls")
            .flex_none()
            .min_w_0()
            .min_h_0()
            .max_h((window.bounds().size.height * 0.4).min(px(260.0)))
            .overflow_y_scroll()
            .px(px(12.0))
            .py(px(6.0))
            .flex()
            .flex_col()
            .gap(px(4.0));
        let mut header = div().min_w_0().flex().items_center().gap(px(4.0)).child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(sp(11.0))
                .text_color(theme.text_secondary)
                .child(run.map(run_label).unwrap_or_else(|| {
                    if self.assistant_workflows.loaded {
                        "Workflow".into()
                    } else {
                        "Workflow · checking status".into()
                    }
                })),
        );
        header = header.child(self.task_icon_action(
            "assistant-workflow-refresh",
            "Refresh workflow status",
            "icons/rotate-cw.svg",
            true,
            |this, _, cx| this.refresh_assistant_workflows(cx),
            cx,
        ));
        if let Some(run) = &run {
            for (action, label, path) in [
                (
                    IssueRunAction::Pause,
                    "Pause after this stage",
                    "icons/hourglass.svg",
                ),
                (
                    IssueRunAction::Resume,
                    "Resume workflow",
                    "icons/arrow-right.svg",
                ),
                (
                    IssueRunAction::Retry,
                    "Retry workflow stage",
                    "icons/rotate-cw.svg",
                ),
                (IssueRunAction::Cancel, "Cancel workflow", "icons/stop.svg"),
            ] {
                if !allowed_action(run.status, action) {
                    continue;
                }
                if action == IssueRunAction::Retry && blocked_live_session(run).is_some() {
                    continue;
                }
                let run_id = run.id;
                header = header.child(self.task_icon_action(
                    format!("assistant-workflow-{task_id}-{run_id}-{action:?}").to_lowercase(),
                    label,
                    path,
                    !busy,
                    move |this, _, cx| {
                        if let Err(error) =
                            this.control_assistant_workflow(task_id, run_id, action, cx)
                        {
                            this.assistant.notice = Some(error);
                            cx.notify();
                        }
                    },
                    cx,
                ));
            }
            if run.status == IssueRunStatus::Blocked {
                if let Some(session_id) = run.attempts.last().map(|attempt| attempt.session_id) {
                    header = header.child(self.task_icon_action(
                        format!("assistant-workflow-{task_id}-open-stage"),
                        "Open stage conversation to resolve the blocker",
                        "icons/arrow-up-right.svg",
                        self.session(session_id).is_some(),
                        move |this, _, cx| this.select_session(session_id, cx),
                        cx,
                    ));
                }
            }
            if run.status == IssueRunStatus::Completed && run.report.is_some() {
                let report_session = report_session_id(run);
                if let Some(session_id) = report_session {
                    header = header.child(self.task_icon_action(
                        format!("assistant-workflow-{task_id}-report"),
                        "Open workflow report",
                        "icons/file.svg",
                        self.session(session_id).is_some(),
                        move |this, _, cx| this.select_session(session_id, cx),
                        cx,
                    ));
                }
                if !task.state.is_done() {
                    header = header.child(self.task_icon_action(
                        format!("assistant-workflow-{task_id}-done"),
                        "Mark issue Done",
                        "icons/check.svg",
                        !busy && !self.task_completion_stops.contains_key(&task_id),
                        move |this, window, cx| {
                            this.move_task(task_id, TaskState::Done, window, cx)
                        },
                        cx,
                    ));
                }
            }
        }
        if run.as_ref().is_none_or(|run| run.status.is_terminal())
            && self.assistant_workflows.visible_setup != Some(task_id)
        {
            header = header.child(self.task_icon_action(
                format!("assistant-workflow-{task_id}-setup"),
                "Set up workflow",
                "icons/arrow-right.svg",
                !busy && !self.is_task_draft(task_id) && !task.state.is_done(),
                move |this, _, cx| {
                    if let Err(error) = this.open_assistant_workflow_setup(task_id, cx) {
                        this.assistant.notice = Some(error);
                        cx.notify();
                    }
                },
                cx,
            ));
        }
        panel = panel.child(header);
        if let Some(error) = run
            .as_ref()
            .and_then(|run| {
                run.error.as_ref().or_else(|| {
                    run.attempts
                        .last()
                        .and_then(|attempt| attempt.error.as_ref())
                })
            })
            .or(self.assistant_workflows.refresh_error.as_ref())
        {
            panel = panel.child(
                div()
                    .text_size(sp(11.0))
                    .text_color(theme.warning)
                    .child(bounded_text(error, 360)),
            );
        }
        if let Some(report) = run
            .as_ref()
            .filter(|run| run.status == IssueRunStatus::Completed)
            .and_then(|run| run.report.as_ref())
        {
            panel = panel.child(
                div()
                    .text_size(sp(11.0))
                    .text_color(theme.text_secondary)
                    .child(bounded_text(&report.summary, 240)),
            );
        }
        if self.assistant_workflows.visible_setup == Some(task_id) {
            if let Some(setup) = self.assistant_workflows.setups.get(&task_id) {
                let retry = setup.submitted_task.is_some();
                let error = setup.error.clone();
                let stage_names = setup
                    .workflow
                    .stages
                    .iter()
                    .map(|stage| stage.name.clone())
                    .collect::<Vec<_>>();
                panel = panel.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(sp(11.0))
                        .text_color(theme.text_tertiary)
                        .child(format!("{} · {}", task.title, &task.id.to_string()[..8])),
                );
                if setup
                    .workflow
                    .stages
                    .last()
                    .is_some_and(|stage| stage.board_stage_id.is_some())
                {
                    panel = panel.child(
                        div()
                            .text_size(sp(11.0))
                            .text_color(theme.text_secondary)
                            .child("Project setting: mark Done after the report finishes"),
                    );
                }
                for (index, name) in stage_names.into_iter().enumerate() {
                    panel = panel.child(
                        div()
                            .flex()
                            .items_center()
                            .min_w_0()
                            .gap(px(4.0))
                            .child(
                                div()
                                    .w(px(48.0))
                                    .flex_none()
                                    .text_size(sp(11.0))
                                    .text_color(theme.text_secondary)
                                    .child(name),
                            )
                            .child(self.render_assistant_workflow_picker(task_id, index, true, cx))
                            .child(
                                self.render_assistant_workflow_picker(task_id, index, false, cx),
                            ),
                    );
                }
                if let Some(error) = error {
                    panel = panel.child(
                        div()
                            .text_size(sp(11.0))
                            .text_color(theme.warning)
                            .child(bounded_text(&error, 360)),
                    );
                }
                panel = panel.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_end()
                        .gap(px(4.0))
                        .child(self.task_icon_action(
                            format!("assistant-workflow-{task_id}-dismiss"),
                            "Close workflow setup",
                            "icons/x.svg",
                            !busy,
                            |this, _, cx| {
                                this.assistant_workflows.visible_setup = None;
                                cx.notify();
                            },
                            cx,
                        ))
                        .child(self.task_action_button(
                            format!("assistant-workflow-{task_id}-start"),
                            if busy {
                                "Starting…"
                            } else if retry {
                                "Retry start"
                            } else {
                                "Start workflow"
                            },
                            true,
                            !busy,
                            move |this, _, cx| this.start_assistant_workflow(task_id, cx),
                            cx,
                        )),
                );
            }
        }
        Some(panel.into_any_element())
    }

    pub(super) fn render_issue_workflow_status(
        &self,
        task_id: Uuid,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let run = self.assistant_workflows.runs.get(&task_id)?;
        let theme = Theme::current(cx);
        let warning = matches!(run.status, IssueRunStatus::Blocked | IssueRunStatus::Failed);
        let path = if run.status == IssueRunStatus::Completed {
            "icons/file.svg"
        } else if warning {
            "icons/circle-alert.svg"
        } else {
            "icons/queue.svg"
        };
        Some(
            div()
                .flex()
                .items_center()
                .min_w_0()
                .gap(px(4.0))
                .text_size(sp(10.0))
                .text_color(if warning {
                    theme.warning
                } else {
                    theme.text_tertiary
                })
                .child(icon(
                    path,
                    11.0,
                    if warning {
                        theme.warning
                    } else {
                        theme.text_tertiary
                    },
                ))
                .child(div().min_w_0().truncate().child(run_label(run)))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::StageHandoff;

    fn stage(name: &str, state: TaskState, provider: ProviderKind) -> BoardStage {
        BoardStage {
            id: Uuid::new_v4(),
            name: name.into(),
            state,
            provider: Some(provider),
            model: Some("configured-model".into()),
            instructions: "Project guidance".into(),
            handoff: StageHandoff::BriefOnly,
        }
    }

    #[test]
    fn stage_control_identity_tracks_domain_objects_not_position() {
        let task = Uuid::new_v4();
        let stage = Uuid::new_v4();
        let identity = stage_control_id(task, stage, "model");
        assert!(identity.contains(&task.to_string()));
        assert!(identity.contains(&stage.to_string()));
        assert_eq!(identity, stage_control_id(task, stage, "model"));
        assert_ne!(identity, stage_control_id(Uuid::new_v4(), stage, "model"));
        assert_ne!(identity, stage_control_id(task, Uuid::new_v4(), "model"));
        assert_ne!(identity, stage_control_id(task, stage, "provider"));
    }

    #[test]
    fn defaults_match_explicit_roles_not_lifecycle_or_order() {
        let stages = vec![
            stage("Done", TaskState::Done, ProviderKind::Claude),
            stage("Review", TaskState::Backlog, ProviderKind::Codex),
            stage("Report", TaskState::Done, ProviderKind::Claude),
            stage("Doing", TaskState::Doing, ProviderKind::Claude),
        ];
        let workflow = suggested_workflow(&stages, ProviderKind::Pi);
        assert_eq!(
            workflow
                .stages
                .iter()
                .map(|stage| stage.name.as_str())
                .collect::<Vec<_>>(),
            ["Plan", "Execute", "Review", "Report"]
        );
        assert_eq!(workflow.stages[0].provider, ProviderKind::Pi);
        assert_eq!(workflow.stages[1].provider, ProviderKind::Pi);
        assert_eq!(workflow.stages[2].provider, ProviderKind::Codex);
        assert_eq!(workflow.stages[3].provider, ProviderKind::Claude);
        assert!(
            workflow.stages[..3]
                .iter()
                .all(|stage| stage.board_stage_id.is_none())
        );
        assert_eq!(workflow.stages[3].board_stage_id, Some(stages[2].id));
        assert!(
            suggested_workflow(&[], ProviderKind::Pi)
                .stages
                .iter()
                .all(|stage| stage.board_stage_id.is_none())
        );
        assert!(workflow.stages[3].instructions.contains("Do not merge"));
        assert!(workflow.validate().is_ok());
    }

    #[test]
    fn ambiguous_or_substring_role_names_do_not_silently_choose_defaults() {
        let stages = vec![
            stage("Plan", TaskState::Backlog, ProviderKind::Claude),
            stage("Planning", TaskState::Doing, ProviderKind::Codex),
            stage("Waiting for review", TaskState::Doing, ProviderKind::Claude),
        ];
        let workflow = suggested_workflow(&stages, ProviderKind::Pi);
        assert!(
            workflow
                .stages
                .iter()
                .all(|stage| stage.provider == ProviderKind::Pi && stage.board_stage_id.is_none())
        );
    }

    #[test]
    fn explicit_target_never_falls_back_and_selected_issue_precedes_session() {
        let selected = Task::new("Selected", TaskState::Doing, 0);
        let session_task = Task::new("Session", TaskState::Doing, 1);
        let tasks = vec![selected.clone(), session_task.clone()];
        assert_eq!(
            target_issue(None, Some(selected.id), Some(session_task.id), &tasks),
            Ok(selected.id)
        );
        assert_eq!(
            target_issue(None, None, Some(session_task.id), &tasks),
            Ok(session_task.id)
        );
        assert_eq!(
            target_issue(
                Some(&IssueReference::Id(session_task.id)),
                Some(selected.id),
                None,
                &tasks
            ),
            Ok(session_task.id)
        );
        assert!(
            target_issue(
                Some(&IssueReference::Id(Uuid::new_v4())),
                Some(selected.id),
                None,
                &tasks
            )
            .is_err()
        );
        assert!(target_issue(None, None, None, &tasks).is_err());
    }

    #[test]
    fn workflow_completion_is_report_ready_never_issue_done() {
        assert_eq!(
            status_label(IssueRunStatus::Completed, true),
            "Report ready"
        );
        assert_ne!(
            status_label(IssueRunStatus::Completed, false),
            "Report ready"
        );
        for status in [
            IssueRunStatus::Running,
            IssueRunStatus::PauseRequested,
            IssueRunStatus::CancelRequested,
            IssueRunStatus::Paused,
            IssueRunStatus::Blocked,
            IssueRunStatus::Failed,
            IssueRunStatus::Cancelled,
            IssueRunStatus::Completed,
        ] {
            assert!(!status_label(status, true).contains("Done"));
        }
    }

    #[test]
    fn controls_are_bound_to_valid_states() {
        assert!(allowed_action(
            IssueRunStatus::Running,
            IssueRunAction::Pause
        ));
        assert!(!allowed_action(
            IssueRunStatus::Paused,
            IssueRunAction::Pause
        ));
        assert!(allowed_action(
            IssueRunStatus::Paused,
            IssueRunAction::Resume
        ));
        assert!(allowed_action(
            IssueRunStatus::Failed,
            IssueRunAction::Retry
        ));
        assert!(allowed_action(
            IssueRunStatus::Blocked,
            IssueRunAction::Retry
        ));
        assert!(!allowed_action(
            IssueRunStatus::Running,
            IssueRunAction::Retry
        ));
        for action in [
            IssueRunAction::Pause,
            IssueRunAction::Resume,
            IssueRunAction::Retry,
            IssueRunAction::Cancel,
        ] {
            assert!(!allowed_action(IssueRunStatus::Completed, action));
            assert!(!allowed_action(IssueRunStatus::Cancelled, action));
        }
    }

    fn run(task_id: Uuid, status: IssueRunStatus) -> IssueRun {
        IssueRun {
            id: Uuid::new_v4(),
            request_id: Uuid::new_v4(),
            task_id,
            project_id: Uuid::new_v4(),
            issue_title: "Issue".into(),
            issue_description: String::new(),
            workflow: IssueWorkflow::plan_execute_review_report(ProviderKind::Pi),
            status,
            current_stage: 0,
            attempts: Vec::new(),
            review_retries: 0,
            report: None,
            error: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn report_link_uses_the_exact_persisted_report_attempt() {
        use waku_protocol::{IssueStageAttempt, IssueStageDecision, IssueStageOutcome};
        let mut run = run(Uuid::new_v4(), IssueRunStatus::Completed);
        let first_id = Uuid::new_v4();
        let final_id = Uuid::new_v4();
        let final_session = Uuid::new_v4();
        for (id, session_id) in [(first_id, Uuid::new_v4()), (final_id, final_session)] {
            run.attempts.push(IssueStageAttempt {
                id,
                stage_id: Uuid::new_v4(),
                session_id,
                status: IssueAttemptStatus::Completed,
                outcome: None,
                error: None,
                started_at: 1,
                finished_at: Some(2),
            });
        }
        run.report = Some(IssueStageOutcome {
            run_id: run.id,
            attempt_id: final_id,
            decision: IssueStageDecision::Completed,
            summary: "Final report".into(),
            evidence: Vec::new(),
        });
        assert_eq!(report_session_id(&run), Some(final_session));
        run.report.as_mut().unwrap().attempt_id = Uuid::new_v4();
        assert_eq!(report_session_id(&run), None);
    }

    #[test]
    fn cache_does_not_regress_and_confirmed_request_clears_only_matching_setup() {
        let task_id = Uuid::new_v4();
        let mut current = run(task_id, IssueRunStatus::Paused);
        current.updated_at = 5;
        let mut state = AssistantWorkflows::default();
        let workflow = current.workflow.clone();
        let request_id = current.request_id;
        state.setups.insert(
            task_id,
            WorkflowSetup {
                project_id: current.project_id,
                workflow,
                request_id,
                submitted_task: None,
                error: Some("Uncertain start".into()),
            },
        );
        state.visible_setup = Some(task_id);
        assert!(state.accept_run(current.clone()));
        assert!(state.setups.is_empty());
        assert!(state.visible_setup.is_none());
        let mut stale = current.clone();
        stale.updated_at = 3;
        stale.status = IssueRunStatus::Running;
        assert!(!state.accept_run(stale));
        assert!(!state.accept_run(current.clone()));
        assert_eq!(state.runs[&task_id], current);
    }

    #[test]
    fn live_blocker_requires_its_conversation_before_retry() {
        let mut blocked = run(Uuid::new_v4(), IssueRunStatus::Blocked);
        let session_id = Uuid::new_v4();
        blocked.attempts.push(waku_protocol::IssueStageAttempt {
            id: Uuid::new_v4(),
            stage_id: blocked.workflow.stages[0].id,
            session_id,
            status: IssueAttemptStatus::Running,
            outcome: None,
            error: None,
            started_at: 1,
            finished_at: None,
        });
        assert_eq!(blocked_live_session(&blocked), Some(session_id));
        blocked.attempts[0].status = IssueAttemptStatus::Blocked;
        assert_eq!(blocked_live_session(&blocked), Some(session_id));
        blocked.attempts[0].finished_at = Some(2);
        assert_eq!(blocked_live_session(&blocked), None);
    }

    #[test]
    fn workflow_context_is_bounded_and_prioritizes_selected_issue() {
        let mut runs = HashMap::new();
        let selected = Uuid::new_v4();
        for index in 0..30 {
            let mut run = run(
                if index == 0 { selected } else { Uuid::new_v4() },
                IssueRunStatus::Blocked,
            );
            run.updated_at = index;
            run.error = Some("界".repeat(5_000));
            runs.insert(run.task_id, run);
        }
        let snapshot = workflow_context(&runs, Some(selected));
        assert!(snapshot.chars().count() <= 2_000);
        assert!(
            snapshot
                .lines()
                .nth(1)
                .unwrap()
                .contains(&selected.to_string())
        );
        assert!(snapshot.contains("status=Blocked"));
    }
}
