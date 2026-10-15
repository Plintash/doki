//! Durable project-owned Board columns. Editing and movement never start a provider.
use super::board::{BoardColumnList, TaskCardDrag};
use super::*;
use crate::model::{BoardStage, ProjectBoard, StageHandoff};
use crate::ui::SemanticControlExt;
use gpui::{KeyBinding, KeyDownEvent, actions};

actions!(board_stage_settings, [DismissStageSettings]);
const SETTINGS_CONTEXT: &str = "BoardStageSettings";
pub(super) fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new(
        "escape",
        DismissStageSettings,
        Some(SETTINGS_CONTEXT),
    )]);
}

#[derive(Clone)]
struct Stage {
    id: Uuid,
    name: String,
    provider: Option<ProviderKind>,
    model: String,
    instructions: String,
    prior_result: bool,
    state: TaskState,
    terminal: bool,
}
impl From<&BoardStage> for Stage {
    fn from(stage: &BoardStage) -> Self {
        Self {
            id: stage.id,
            name: stage.name.clone(),
            provider: stage.provider,
            model: stage.model.clone().unwrap_or_default(),
            instructions: stage.instructions.clone(),
            prior_result: stage.handoff == StageHandoff::PriorResult,
            state: stage.state,
            terminal: stage.state.is_done(),
        }
    }
}
impl Stage {
    fn durable(&self) -> BoardStage {
        BoardStage {
            id: self.id,
            name: self.name.clone(),
            state: self.state,
            provider: self.provider,
            model: (!self.model.is_empty()).then(|| self.model.clone()),
            instructions: self.instructions.clone(),
            handoff: if self.prior_result {
                StageHandoff::PriorResult
            } else {
                StageHandoff::BriefOnly
            },
        }
    }
}
struct ProjectStages {
    stages: Vec<Stage>,
    revision: u64,
    lists: HashMap<Uuid, BoardColumnList>,
}
impl ProjectStages {
    fn new(board: &ProjectBoard) -> Self {
        Self {
            stages: board.stages.iter().map(Stage::from).collect(),
            revision: board.revision,
            lists: HashMap::new(),
        }
    }
    fn refresh(&mut self, board: &ProjectBoard) {
        self.stages = board.stages.iter().map(Stage::from).collect();
        self.revision = board.revision;
    }
    fn stage_for(&self, project: Uuid, task: &Task) -> Uuid {
        // Legacy same-path project IDs remain visible, but never inherit this
        // project's configuration merely because the path or name matches.
        task.stage_id
            .filter(|id| {
                task_details::task_project_id(task) == Some(project)
                    && self
                        .stages
                        .iter()
                        .any(|s| s.id == *id && s.state == task.state)
            })
            .or_else(|| {
                self.stages
                    .iter()
                    .find(|s| s.id == crate::model::canonical_stage_id(project, task.state))
                    .map(|s| s.id)
            })
            .or_else(|| {
                self.stages
                    .iter()
                    .find(|s| s.state == task.state)
                    .map(|s| s.id)
            })
            .expect("validated board contains every lifecycle")
    }
    fn reorder(&mut self, id: Uuid, right: bool) {
        let Some(index) = self.stages.iter().position(|s| s.id == id && !s.terminal) else {
            return;
        };
        let target = if right {
            index + 1
        } else {
            index.saturating_sub(1)
        };
        if target < self.stages.len() - 1 {
            self.stages.swap(index, target);
        }
    }
}
struct StageEditor {
    project: Uuid,
    stage: Stage,
    name: Entity<TextInput>,
    model: Entity<TextInput>,
    instructions: Entity<TextInput>,
    error: bool,
    newly_added: bool,
}
pub(super) struct StageBoard {
    pending_saves: HashMap<Uuid, u64>,
    projects: HashMap<Uuid, ProjectStages>,
    editor: Option<StageEditor>,
}
impl StageBoard {
    pub(super) fn new() -> Self {
        Self {
            pending_saves: HashMap::new(),
            projects: HashMap::new(),
            editor: None,
        }
    }
}
#[derive(Clone, Copy)]
enum Action {
    Add,
    Edit(Uuid),
    Shift(Uuid, bool),
    Handoff,
    Save,
    Cancel,
}
impl Waku {
    fn stage_button(
        &self,
        id: String,
        label: String,
        action: Action,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let focus = self
            .transcript_control_focuses
            .borrow_mut()
            .entry(id.clone())
            .or_insert_with(|| cx.focus_handle().tab_stop(true))
            .clone();
        let accessible_label = if let Action::Edit(stage) = action {
            self.board_project_filter
                .and_then(|p| self.board_stages.projects.get(&p))
                .and_then(|b| b.stages.iter().find(|s| s.id == stage))
                .map(|s| format!("Configure {}", s.name))
                .unwrap_or(label.clone())
        } else {
            label.clone()
        };
        div()
            .id(SharedString::from(id))
            .semantic_control(gpui::Role::Button, false)
            .aria_label(accessible_label.clone())
            .relative()
            .child(crate::ui::tooltip::FocusHint::new(
                focus.clone(),
                accessible_label.clone(),
            ))
            .tooltip(Tooltip::text(accessible_label))
            .track_focus(&focus)
            .tab_index(0)
            .tab_stop(true)
            .px(px(7.0))
            .py(px(5.0))
            .rounded(px(5.0))
            .text_size(sp(12.0))
            .text_color(theme.text_secondary)
            .hover(|s| s.bg(theme.overlay_strong))
            .focus_visible(|s| s.border_1().border_color(theme.accent))
            .child(label)
            .on_click(cx.listener(move |this, _, window, cx| this.stage_action(action, window, cx)))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.stage_action(action, window, cx);
                    cx.stop_propagation();
                }
            }))
            .into_any_element()
    }
    fn stage_action(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.board_project_filter else {
            return;
        };
        match action {
            Action::Add => {
                self.close_stage_editor(true, window, cx);
                let board = self
                    .board_stages
                    .projects
                    .get_mut(&project)
                    .expect("rendered project board");
                let stage = Stage {
                    id: Uuid::new_v4(),
                    name: "New stage".into(),
                    provider: None,
                    model: String::new(),
                    instructions: String::new(),
                    prior_result: false,
                    terminal: false,
                    state: TaskState::Doing,
                };
                let id = stage.id;
                board.stages.insert(board.stages.len() - 1, stage);
                self.stage_action(Action::Edit(id), window, cx);
                if let Some(editor) = self.board_stages.editor.as_mut() {
                    editor.newly_added = true;
                }
            }
            Action::Edit(id) => {
                self.close_stage_editor(true, window, cx);
                let Some(stage) = self
                    .board_stages
                    .projects
                    .get(&project)
                    .and_then(|b| b.stages.iter().find(|s| s.id == id))
                    .cloned()
                else {
                    return;
                };
                let name = cx.new(|cx| {
                    TextInput::new(window, cx)
                        .semantic_id("stage-name")
                        .tab_stop(true)
                });
                name.update(cx, |input, cx| input.set_content(stage.name.clone(), cx));
                let model = cx.new(|cx| {
                    TextInput::new(window, cx)
                        .semantic_id("stage-model")
                        .tab_stop(true)
                        .placeholder("Provider default")
                });
                model.update(cx, |input, cx| input.set_content(stage.model.clone(), cx));
                let instructions = cx.new(|cx| {
                    TextInput::new(window, cx)
                        .semantic_id("stage-instructions")
                        .tab_stop(true)
                        .multi_line()
                        .placeholder("What should this stage do?")
                });
                instructions.update(cx, |input, cx| {
                    input.set_content(stage.instructions.clone(), cx)
                });
                name.read(cx).focus().focus(window, cx);
                self.board_stages.editor = Some(StageEditor {
                    project,
                    stage,
                    name,
                    model,
                    instructions,
                    error: false,
                    newly_added: false,
                });
            }
            Action::Shift(id, right) => {
                if let Some(board) = self.board_stages.projects.get_mut(&project) {
                    board.reorder(id, right);
                }
                if let Some(editor) = self.board_stages.editor.as_ref() {
                    editor.name.read(cx).focus().focus(window, cx);
                }
            }
            Action::Handoff => {
                if let Some(editor) = self.board_stages.editor.as_mut() {
                    editor.stage.prior_result = !editor.stage.prior_result;
                }
            }
            Action::Cancel => self.close_stage_editor(true, window, cx),
            Action::Save => {
                if self.board_stages.pending_saves.contains_key(&project) {
                    return;
                }
                if let Some(editor) = self.board_stages.editor.as_mut() {
                    let name = editor.name.read(cx).content().trim().to_owned();
                    if name.is_empty() {
                        editor.error = true;
                        cx.notify();
                        return;
                    }
                    editor.stage.name = name;
                    editor.stage.model = editor.model.read(cx).content().trim().to_owned();
                    editor.stage.instructions = editor.instructions.read(cx).content().to_owned();
                    if let Some(stage) = self
                        .board_stages
                        .projects
                        .get_mut(&editor.project)
                        .and_then(|b| b.stages.iter_mut().find(|s| s.id == editor.stage.id))
                    {
                        *stage = editor.stage.clone();
                    }
                }
                self.save_stage_board(project, window, cx);
            }
        }
        cx.notify();
    }
    fn close_stage_editor(&mut self, cancel: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.board_stages.editor.take() {
            if cancel
                && let Some(owner) = self.state.projects.iter().find(|p| p.id == editor.project)
                && let Some(board) = self.board_stages.projects.get_mut(&editor.project)
            {
                board.refresh(&owner.board);
            }
            let focus_id = if cancel
                && editor.newly_added
                && !self
                    .state
                    .projects
                    .iter()
                    .find(|p| p.id == editor.project)
                    .is_some_and(|p| p.board.stages.iter().any(|s| s.id == editor.stage.id))
            {
                if let Some(board) = self.board_stages.projects.get_mut(&editor.project) {
                    board.lists.remove(&editor.stage.id);
                }
                "stage-add".to_owned()
            } else {
                format!("stage-settings-{}", editor.stage.id)
            };
            if let Some(focus) = self.transcript_control_focuses.borrow().get(&focus_id) {
                focus.focus(window, cx);
            }
        }
        cx.notify();
    }
    fn save_stage_board(&mut self, project: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(owner) = self.state.projects.iter().find(|p| p.id == project) else {
            return;
        };
        let draft = &self.board_stages.projects[&project];
        if owner.board.revision != draft.revision {
            self.show_toast("Columns changed elsewhere. Your draft is still here; cancel and reopen settings to use the latest version.");
            cx.notify();
            return;
        }
        let saved_editor_id = self.board_stages.editor.as_ref().map(|e| e.stage.id);
        let expected_revision = draft.revision;
        let board = ProjectBoard {
            revision: expected_revision + 1,
            stages: draft.stages.iter().map(Stage::durable).collect(),
        };
        if let Err(error) = board.validate(project) {
            self.show_toast(error);
            cx.notify();
            return;
        }
        self.board_stages
            .pending_saves
            .insert(project, board.revision);
        let daemon = self.daemon.client();
        let window_handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let result = match daemon.request(
                        Uuid::nil(),
                        Uuid::nil(),
                        waku_client::Command::SaveProjectBoard {
                            project_id: project,
                            expected_revision,
                            board,
                        },
                    ) {
                        Ok(waku_client::ResponsePayload::ProjectBoardSaved { project }) => {
                            Ok(project.board)
                        }
                        Ok(_) => {
                            Err("The daemon returned an invalid column save response".to_owned())
                        }
                        Err(error) => Err(error.to_string()),
                    };
                    let refreshed = if result.is_err() {
                        match daemon.request(
                            Uuid::nil(),
                            Uuid::nil(),
                            waku_client::Command::LoadTaskState,
                        ) {
                            Ok(waku_client::ResponsePayload::TaskState { projects, .. }) => {
                                projects
                                    .into_iter()
                                    .find(|p| p.id == project)
                                    .map(|p| p.board)
                            }
                            _ => None,
                        }
                    } else {
                        None
                    };
                    (result, refreshed)
                })
                .await;
            let focus = this.update(cx, |this, cx| {
                let mut restore_focus = None;
                this.board_stages.pending_saves.remove(&project);
                let (result, refreshed) = result;
                let board = match result {
                    Ok(board) => {
                        let unchanged = this
                            .board_stages
                            .editor
                            .as_ref()
                            .filter(|e| e.project == project && Some(e.stage.id) == saved_editor_id)
                            .is_some_and(|editor| {
                                board
                                    .stages
                                    .iter()
                                    .find(|s| s.id == editor.stage.id)
                                    .is_some_and(|saved| {
                                        saved.name == editor.name.read(cx).content().trim()
                                            && saved.model.as_deref().unwrap_or_default()
                                                == editor.model.read(cx).content().trim()
                                            && saved.instructions
                                                == editor.instructions.read(cx).content()
                                            && saved.provider == editor.stage.provider
                                            && (saved.handoff == StageHandoff::PriorResult)
                                                == editor.stage.prior_result
                                    })
                                    && this.board_stages.projects[&project]
                                        .stages
                                        .iter()
                                        .map(|s| s.id)
                                        .eq(board.stages.iter().map(|s| s.id))
                            });
                        if unchanged {
                            this.board_stages.editor = None;
                            if this.board_project_filter == Some(project) {
                                restore_focus = saved_editor_id.and_then(|id| {
                                    this.transcript_control_focuses
                                        .borrow()
                                        .get(&format!("stage-settings-{id}"))
                                        .cloned()
                                });
                            }
                        }
                        if let Some(cache) = this.board_stages.projects.get_mut(&project) {
                            cache.revision = board.revision;
                        }
                        Some(board)
                    }
                    Err(error) => {
                        this.show_toast(format!(
                            "Couldn't save columns: {error}. Your draft is still here."
                        ));
                        refreshed
                    }
                };
                if let Some(board) = board
                    && let Some(owner) = this.state.projects.iter_mut().find(|p| p.id == project)
                    && owner.board.revision <= board.revision
                {
                    owner.board = board;
                }
                super::rpa::daemon::model_changed();
                cx.notify();
                restore_focus
            });
            if let Ok(Some(focus)) = focus {
                let _ = window_handle.update(cx, |_, window, cx| window.focus(&focus, cx));
            }
        })
        .detach();
    }
    pub(super) fn stage_board_snapshot(&self) -> serde_json::Value {
        let Some(project) = self.board_project_filter else {
            return serde_json::Value::Null;
        };
        let Some(owner) = self.state.projects.iter().find(|p| p.id == project) else {
            return serde_json::Value::Null;
        };
        let board = ProjectStages::new(&owner.board);
        serde_json::json!({"persistent": true, "revision": board.revision, "manualStart": true, "draftPreparationEnabled": true, "automaticExecution": false,
            "stages": board.stages.iter().map(|s| serde_json::json!({"id": s.id, "name": s.name, "provider": s.provider, "model": s.model, "instructions": s.instructions, "handoff": if s.prior_result { "selectedResult" } else { "brief" }, "terminal": s.terminal})).collect::<Vec<_>>(),
            "placements": self.state.tasks.iter().filter(|t| self.task_matches_board_scope(t)).map(|t| serde_json::json!({"taskId":t.id,"stageId":board.stage_for(project, t)})).collect::<Vec<_>>()})
    }
    pub(super) fn move_stage_card(
        &mut self,
        task_id: Uuid,
        stage_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.board_project_filter else {
            return;
        };
        let Some(board) = self.board_stages.projects.get(&project) else {
            return;
        };
        let Some(stage) = board.stages.iter().find(|s| s.id == stage_id) else {
            return;
        };
        if !self
            .state
            .projects
            .iter()
            .find(|p| p.id == project)
            .is_some_and(|p| p.board.stages.iter().any(|s| s.id == stage_id))
        {
            self.show_toast("Save this column before moving issues into it.");
            cx.notify();
            return;
        }
        let Some(task) = self.state.tasks.iter().find(|t| t.id == task_id) else {
            return;
        };
        if !self.task_matches_board_scope(task) {
            return;
        }
        if task.project_id() != Some(project)
            && stage.id != crate::model::canonical_stage_id(project, stage.state)
        {
            self.show_toast("This issue belongs to another project record. Choose its project to use custom columns.");
            cx.notify();
            return;
        }
        // Foreign aliases can move by lifecycle only; never install a stage ID
        // owned by the selected alias onto an issue with a different owner.
        let destination =
            (task_details::task_project_id(task) == Some(project)).then(|| stage.durable());
        let state = stage.state;
        self.move_task_to_stage(task_id, state, destination, window, cx);
        cx.notify();
    }
    pub(super) fn move_stage_card_adjacent(
        &mut self,
        task_id: Uuid,
        right: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.board_project_filter else {
            return;
        };
        let Some(board) = self.board_stages.projects.get(&project) else {
            return;
        };
        let Some(task) = self.state.tasks.iter().find(|t| t.id == task_id) else {
            return;
        };
        let index = board
            .stages
            .iter()
            .position(|s| s.id == board.stage_for(project, task))
            .unwrap();
        let next = if right {
            (index + 1).min(board.stages.len() - 1)
        } else {
            index.saturating_sub(1)
        };
        self.move_stage_card(task_id, board.stages[next].id, window, cx);
    }
    pub(super) fn render_stage_board(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let project = self.board_project_filter.unwrap();
        let theme = Theme::current(cx);
        let stats = self.task_stats();
        let task_ids: Vec<_> = [TaskState::Backlog, TaskState::Doing, TaskState::Done]
            .into_iter()
            .flat_map(|s| self.tasks_in_state(s))
            .collect();
        let Some(owner) = self.state.projects.iter().find(|p| p.id == project) else {
            return div().into_any_element();
        };
        let editing = self
            .board_stages
            .editor
            .as_ref()
            .is_some_and(|e| e.project == project);
        let board = self
            .board_stages
            .projects
            .entry(project)
            .or_insert_with(|| ProjectStages::new(&owner.board));
        if !editing {
            board.refresh(&owner.board);
        }
        let stages = board.stages.clone();
        let scoped_ids: HashSet<_> = task_ids.iter().copied().collect();
        let placements: HashMap<_, _> = self
            .state
            .tasks
            .iter()
            .filter(|t| scoped_ids.contains(&t.id))
            .map(|t| (t.id, board.stage_for(project, t)))
            .collect();
        let mut columns = Vec::new();
        for (index, stage) in stages.iter().enumerate() {
            let id = stage.id;
            let ids: Vec<_> = task_ids
                .iter()
                .copied()
                .filter(|task| placements.get(task) == Some(&id))
                .collect();
            let count = ids.len();
            let slot = self
                .board_stages
                .projects
                .get_mut(&project)
                .unwrap()
                .lists
                .entry(id)
                .or_insert_with(BoardColumnList::new);
            sync_virtual_ids(&slot.list, &mut slot.ids, &ids, px(92.0));
            let list_state = slot.list.clone();
            let entity = cx.entity().downgrade();
            let card_stats = Rc::clone(&stats);
            let mut header = div()
                .flex_none()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(4.0))
                .px(px(8.0))
                .h(px(36.0))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(sp(12.5))
                        .text_color(theme.text)
                        .child(stage.name.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(sp(11.0))
                        .text_color(theme.text_ghost)
                        .child(count.to_string()),
                )
                .child(div().flex_1());
            {
                header = header.child(self.task_icon_action(
                    format!("stage-settings-{id}"),
                    format!("Configure {}", stage.name),
                    "icons/ellipsis.svg",
                    true,
                    move |this, window, cx| this.stage_action(Action::Edit(id), window, cx),
                    cx,
                ));
            }
            let mut column = div()
                .id(SharedString::from(format!("stage-column-{id}")))
                .flex_1()
                .min_w(px(236.0))
                .h_full()
                .flex()
                .flex_col()
                .rounded(px(10.0))
                .bg(theme.overlay)
                .border_1()
                .border_color(theme.border)
                .drag_over::<TaskCardDrag>(move |s, _, _, _| s.border_color(theme.accent))
                .on_drop(cx.listener(move |this, drag: &TaskCardDrag, window, cx| {
                    this.move_stage_card(drag.task_id, id, window, cx)
                }))
                .child(header);
            if !stage.terminal {
                column = column.child(
                    div()
                        .px(px(12.0))
                        .pb(px(8.0))
                        .text_size(sp(11.0))
                        .text_color(theme.text_tertiary)
                        .child(format!(
                            "{} · Manual",
                            stage
                                .provider
                                .map(|p| p.display_name())
                                .unwrap_or("Not configured")
                        )),
                );
            }
            if self
                .board_stages
                .editor
                .as_ref()
                .is_some_and(|e| e.project == project && e.stage.id == id)
            {
                column = column.child(self.render_stage_editor(index, stages.len(), cx));
            }
            column = column.child(
                div().flex_1().min_h_0().p(px(8.0)).child(
                    list(list_state, move |index, _, cx| {
                        let Some(task_id) = ids.get(index).copied() else {
                            return div().into_any_element();
                        };
                        entity
                            .upgrade()
                            .map(|entity| {
                                div()
                                    .pb(px(8.0))
                                    .child(entity.update(cx, |this, cx| {
                                        this.render_board_card(task_id, &card_stats, cx)
                                    }))
                                    .into_any_element()
                            })
                            .unwrap_or_else(|| div().into_any_element())
                    })
                    .size_full(),
                ),
            );
            columns.push(column.into_any_element());
        }
        div()
            .id("task-board")
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.surface)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .px(px(14.0))
                    .pt(px(4.0))
                    .child(self.stage_button(
                        "stage-add".into(),
                        "+ Add column".into(),
                        Action::Add,
                        cx,
                    )),
            )
            .child(
                div()
                    .id("board-stage-columns")
                    .flex_1()
                    .min_h_0()
                    .overflow_x_scroll()
                    .flex()
                    .gap(px(12.0))
                    .p(px(14.0))
                    .children(columns),
            )
            .into_any_element()
    }
    fn stage_harness_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let editor = self.board_stages.editor.as_ref().unwrap();
        let stage_id = editor.stage.id;
        let handle = self.menu_handle("stage-harness", cx);
        let weak = cx.entity().downgrade();
        let choices: Vec<_> = std::iter::once(None)
            .chain(ProviderKind::ALL.iter().copied().map(Some))
            .map(|provider| {
                (
                    provider,
                    provider
                        .map(|p| {
                            if self.provider_enabled(p) {
                                p.display_name().to_owned()
                            } else {
                                format!("{} (not enabled)", p.display_name())
                            }
                        })
                        .unwrap_or_else(|| "Not configured".into()),
                )
            })
            .collect();
        dropdown_menu(
            MenuChip::new("stage-harness")
                .label(
                    editor
                        .stage
                        .provider
                        .map(|p| p.display_name())
                        .unwrap_or("Not configured"),
                )
                .caret(true)
                .selected(handle.is_open()),
            "stage-harness-menu",
            &handle,
            MenuAlign::BelowLeft,
            move |_| {
                choices
                    .iter()
                    .map(|(provider, label)| {
                        let weak = weak.clone();
                        let provider = *provider;
                        MenuItem::new(label.clone(), move |_, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                if let Some(editor) = this
                                    .board_stages
                                    .editor
                                    .as_mut()
                                    .filter(|e| e.stage.id == stage_id)
                                {
                                    if editor.stage.provider != provider {
                                        editor.stage.provider = provider;
                                        editor.model.update(cx, |input, cx| {
                                            input.set_content(String::new(), cx)
                                        });
                                    }
                                    cx.notify();
                                }
                            });
                        })
                        .semantic_id(format!(
                            "stage-harness-{}",
                            provider
                                .map(|p| format!("{p:?}"))
                                .unwrap_or_else(|| "none".into())
                        ))
                    })
                    .collect()
            },
        )
    }
    fn render_stage_editor(
        &self,
        index: usize,
        count: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let editor = self.board_stages.editor.as_ref().unwrap();
        let theme = Theme::current(cx);
        let label = |text: &str| {
            div()
                .text_size(sp(11.0))
                .text_color(theme.text_tertiary)
                .child(text.to_owned())
        };
        let panel = div()
            .id("stage-settings")
            .key_context(SETTINGS_CONTEXT)
            .on_action(cx.listener(|this, _: &DismissStageSettings, window, cx| {
                this.close_stage_editor(true, window, cx);
                cx.stop_propagation();
            }))
            .mx(px(8.0))
            .mb(px(8.0))
            .p(px(10.0))
            .rounded(px(7.0))
            .bg(theme.surface)
            .border_1()
            .border_color(theme.border)
            .flex()
            .flex_col()
            .gap(px(7.0))
            .child(label("Column name"))
            .child(editor.name.clone())
            .when(editor.error, |s| s.child(label("Enter a column name")));
        let panel = if !editor.stage.terminal {
            panel
                .child(label("Harness"))
                .child(self.stage_harness_picker(cx))
                .when(
                    editor
                        .stage
                        .provider
                        .is_some_and(|p| !self.provider_enabled(p)),
                    |s| s.child(label("Not enabled on this machine")),
                )
                .child(label("Model"))
                .child(editor.model.clone())
                .child(label("Instructions"))
                .child(editor.instructions.clone())
                .child(label("Handoff"))
                .child(
                    self.stage_button(
                        "stage-handoff".into(),
                        if editor.stage.prior_result {
                            "Brief + selected result  ›"
                        } else {
                            "Issue brief only  ›"
                        }
                        .into(),
                        Action::Handoff,
                        cx,
                    ),
                )
                .child(label("Start manually · Moving does not run agents"))
                .child(
                    div()
                        .flex()
                        .gap(px(4.0))
                        .when(index > 0, |s| {
                            s.child(self.stage_button(
                                "stage-move-left".into(),
                                "← Move".into(),
                                Action::Shift(editor.stage.id, false),
                                cx,
                            ))
                        })
                        .when(index + 2 < count, |s| {
                            s.child(self.stage_button(
                                "stage-move-right".into(),
                                "Move →".into(),
                                Action::Shift(editor.stage.id, true),
                                cx,
                            ))
                        }),
                )
        } else {
            panel.child(label("Completed issues · Moving here stops active work"))
        };
        panel
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(4.0))
                    .child(self.stage_button(
                        "stage-cancel".into(),
                        "Cancel".into(),
                        Action::Cancel,
                        cx,
                    ))
                    .child(
                        self.stage_button(
                            "stage-save".into(),
                            if self
                                .board_stages
                                .pending_saves
                                .contains_key(&editor.project)
                            {
                                "Saving…"
                            } else {
                                "Save"
                            }
                            .into(),
                            Action::Save,
                            cx,
                        ),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(project: Uuid, state: TaskState) -> Task {
        let mut task = Task::new("Issue", state, 0);
        task.workspace = Some(TaskWorkspace::Local {
            project_id: project,
        });
        task.stage_id = Some(crate::model::canonical_stage_id(project, state));
        task
    }

    #[test]
    fn reordered_columns_preserve_canonical_identity() {
        let project = Uuid::new_v4();
        let mut board = ProjectStages::new(&ProjectBoard::canonical(project));
        let task = issue(project, TaskState::Backlog);
        let original = board.stage_for(project, &task);
        let terminal = board.stages.last().unwrap().id;
        board.reorder(original, true);
        assert_eq!(board.stage_for(project, &task), original);
        board.reorder(terminal, false);
        assert_eq!(board.stages.last().unwrap().id, terminal);
    }

    #[test]
    fn missing_foreign_and_lifecycle_mismatched_stages_remain_visible() {
        let project = Uuid::new_v4();
        let mut board = ProjectStages::new(&ProjectBoard::canonical(project));
        let mut review = board.stages[1].clone();
        review.id = Uuid::new_v4();
        review.name = "Review".into();
        board.stages.insert(2, review.clone());
        let active = crate::model::canonical_stage_id(project, TaskState::Doing);
        let mut task = issue(project, TaskState::Doing);
        task.stage_id = Some(review.id);
        assert_eq!(board.stage_for(project, &task), review.id);
        task.workspace = Some(TaskWorkspace::Local {
            project_id: Uuid::new_v4(),
        });
        assert_eq!(board.stage_for(project, &task), active);
        task.workspace = Some(TaskWorkspace::Local {
            project_id: project,
        });
        task.stage_id = Some(Uuid::new_v4());
        assert_eq!(board.stage_for(project, &task), active);
        task.stage_id = Some(board.stages.last().unwrap().id);
        assert_eq!(board.stage_for(project, &task), active);
    }

    #[test]
    fn cancelling_after_failed_save_resets_all_draft_fields_before_next_editor() {
        let project = Uuid::new_v4();
        let authoritative = ProjectBoard::canonical(project);
        let mut draft = ProjectStages::new(&authoritative);
        draft.stages[0].name = "Rejected edit".into();
        draft.stages[0].provider = Some(ProviderKind::Codex);
        draft.reorder(draft.stages[0].id, true);
        draft.refresh(&authoritative);
        assert_eq!(
            draft.stages.iter().map(Stage::durable).collect::<Vec<_>>(),
            authoritative.stages
        );
        assert_eq!(draft.revision, authoritative.revision);
    }

    #[test]
    fn stage_settings_round_trip_without_substituting_a_provider() {
        let project = Uuid::new_v4();
        let board = ProjectBoard::canonical(project);
        for stage in &board.stages {
            assert_eq!(&Stage::from(stage).durable(), stage);
            assert_eq!(stage.provider, None);
        }
    }
}
