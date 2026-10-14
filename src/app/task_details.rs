//! The board's issue inspector. Editing an issue never starts an agent: a new
//! conversation opens a reviewable composer draft with the issue's context.
//! All displayed facts come from the in-memory catalog; hydration is requested
//! on entry, not from a row render.

use super::*;
use crate::ui::SemanticControlExt;
use crate::ui::menu::dropdown_menu_list;

mod attach_picker;

pub(super) fn init_attach_picker_keys(cx: &mut App) {
    attach_picker::init(cx);
}

pub(super) struct TaskDetail {
    id: Uuid,
    title: Entity<TextInput>,
    description: Entity<TextInput>,
    list: ListState,
    editor_focus: FocusHandle,
    session_ids: Vec<Uuid>,
    session_indices: Vec<usize>,
    session_focuses: HashMap<Uuid, FocusHandle>,
    editor_layout: Option<(u64, bool, bool, bool, bool, Option<(Uuid, u64)>)>,
    attach_picker: Entity<attach_picker::TaskAttachPicker>,
    project_menu: ContextMenuHandle,
    workspace_menu: ContextMenuHandle,
    pub(super) review_scope: issue_review::ReviewScope,
    pub(super) review_view: issue_review::IssueReviewView,
    pub(super) review_reference: Entity<TextInput>,
}

impl TaskDetail {
    pub(super) fn invalidate_review_layout(&mut self) {
        self.editor_layout = None;
    }
}

/// An edited issue may have a debounced write waiting after its editor closes.
/// Navigation consumes that write; it must never invent one or let its old
/// timer save the same edit again.
#[derive(Default)]
pub(super) struct TaskDetailSave {
    generation: u64,
    pending: bool,
}

impl TaskDetailSave {
    fn schedule(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.pending = true;
        self.generation
    }

    fn is_current(&self, generation: u64) -> bool {
        self.pending && self.generation == generation
    }

    fn take_pending(&mut self) -> bool {
        std::mem::take(&mut self.pending)
    }
}

pub(super) fn task_project_id(task: &Task) -> Option<Uuid> {
    task.workspace.as_ref().map(|workspace| match workspace {
        TaskWorkspace::Local { project_id }
        | TaskWorkspace::NewWorktree { project_id }
        | TaskWorkspace::Worktree { project_id, .. } => *project_id,
    })
}

pub(super) fn task_session_workspace(task: &Task) -> SessionWorkspace {
    match &task.workspace {
        Some(TaskWorkspace::Worktree { path, branch, .. }) => SessionWorkspace::Worktree {
            path: path.clone(),
            branch: branch.clone(),
        },
        Some(TaskWorkspace::NewWorktree { .. }) => {
            SessionWorkspace::NewWorktree { base_branch: None }
        }
        _ => SessionWorkspace::Local,
    }
}

fn task_session_uses_default_workspace(task: &Task, session: &AgentSession) -> bool {
    task_project_id(task) == Some(session.project_id)
        && task_session_workspace(task) == session.workspace
}

pub(super) fn task_session_prompt(task: &Task) -> String {
    let description = task.description.trim();
    if description.is_empty() {
        task.title.clone()
    } else {
        format!("{}\n\n{}", task.title.trim(), description)
    }
}

fn task_development_draft(task: &Task, sessions: &[AgentSession]) -> Option<Uuid> {
    let project_id = task_project_id(task)?;
    let workspace = task_session_workspace(task);
    sessions
        .iter()
        .find(|session| {
            task_draft_follows_workspace(task.id, session)
                && session.project_id == project_id
                && session.workspace == workspace
                && session.provider == ProviderKind::Pi
        })
        .map(|session| session.id)
}

fn task_draft_follows_workspace(task_id: Uuid, session: &AgentSession) -> bool {
    session.task_id == Some(task_id)
        && !session.has_started()
        && !session.is_busy()
        && session.review.is_none()
        // The correction owns its reviewed workspace even after the source
        // review reruns, is rolled back, or leaves the issue/catalog.
        && session.review_correction_source_id.is_none()
}

fn task_session_navigation_index(index: usize, count: usize, key: &str) -> Option<usize> {
    if count == 0 {
        return None;
    }
    match key {
        "up" => index.checked_sub(1),
        "down" => (index + 1 < count).then_some(index + 1),
        "home" => Some(0),
        "end" => Some(count - 1),
        _ => None,
    }
}

impl Waku {
    pub(super) fn task_detail_values(&self, id: Uuid, cx: &App) -> Option<(String, String)> {
        let detail = self.task_detail.as_ref().filter(|detail| detail.id == id)?;
        Some((
            detail.title.read(cx).content().to_owned(),
            detail.description.read(cx).content().to_owned(),
        ))
    }

    pub(super) fn set_task_draft_title(&mut self, title: String, cx: &mut Context<Self>) {
        if let Some(task) = self.new_task_draft.as_mut() {
            task.title = title.clone();
            if let Some(detail) = self
                .task_detail
                .as_ref()
                .filter(|detail| detail.id == task.id)
            {
                detail
                    .title
                    .update(cx, |input, cx| input.set_content(title, cx));
            }
        }
    }

    /// Focus a concrete control after revealing its virtualized row. Ordinary
    /// Tab order only knows rendered elements, so crossing a row boundary must
    /// also be able to bring an off-screen conversation into the viewport.
    fn focus_task_detail_session(
        &mut self,
        index: usize,
        menu: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(detail) = self.task_detail.as_ref() else {
            return false;
        };
        let Some((&id, &session_index)) = detail
            .session_ids
            .get(index)
            .zip(detail.session_indices.get(index))
        else {
            return false;
        };
        if !self
            .state
            .sessions
            .get(session_index)
            .is_some_and(|session| session.id == id)
        {
            return false;
        }
        let row_focus = detail.session_focuses[&id].clone().tab_stop(true);
        detail.list.scroll_to_reveal_item(index + 1);
        let focus = if menu {
            self.transcript_control_focuses
                .borrow_mut()
                .entry(format!("issue-session-detach-{id}"))
                .or_insert_with(|| cx.focus_handle().tab_stop(true))
                .clone()
        } else {
            row_focus
        };
        window.focus(&focus, cx);
        cx.notify();
        true
    }

    fn focus_task_detail_editor_tail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(detail) = self.task_detail.as_ref() else {
            return;
        };
        let can_start = self
            .state
            .tasks
            .iter()
            .find(|task| task.id == detail.id)
            .and_then(task_project_id)
            .is_some_and(|id| self.state.projects.iter().any(|project| project.id == id))
            && !detail.title.read(cx).content().trim().is_empty();
        let focus = if can_start {
            self.transcript_control_focuses
                .borrow()
                .get("task-detail-new-session")
                .cloned()
        } else {
            Some(detail.title.read(cx).focus())
        };
        let height = detail
            .list
            .bounds_for_item(0)
            .map(|bounds| bounds.size.height)
            .unwrap_or_default();
        detail.list.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: if can_start {
                (height - detail.list.viewport_bounds().size.height).max(px(0.0))
            } else {
                px(0.0)
            },
        });
        if let Some(focus) = focus {
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    pub(super) fn ensure_task_detail(
        &mut self,
        task: &Task,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(detail) = self
            .task_detail
            .as_ref()
            .filter(|detail| detail.id == task.id)
        {
            if !detail.title.read(cx).focus().is_focused(window)
                && detail.title.read(cx).content().trim() != task.title
            {
                detail
                    .title
                    .update(cx, |input, cx| input.set_content(task.title.clone(), cx));
            }
            if !detail.description.read(cx).focus().is_focused(window)
                && detail.description.read(cx).content() != task.description
            {
                detail.description.update(cx, |input, cx| {
                    input.set_content(task.description.clone(), cx)
                });
            }
            return;
        }
        let title = cx.new(|cx| {
            TextInput::new(window, cx)
                .semantic_id(format!("issue-title-{}", task.id))
                .tab_stop(true)
                .placeholder(tr!("board.new_task_placeholder"))
        });
        title.update(cx, |input, cx| input.set_content(task.title.clone(), cx));
        let description = cx.new(|cx| {
            TextInput::new(window, cx)
                .semantic_id(format!("issue-description-{}", task.id))
                .tab_stop(true)
                .multi_line()
                .placeholder(tr!("board.description_placeholder"))
        });
        description.update(cx, |input, cx| {
            input.set_content(task.description.clone(), cx)
        });
        let review_reference = cx.new(|cx| {
            TextInput::new(window, cx)
                .semantic_id("issue-review-reference")
                .tab_stop(true)
                .placeholder(tr!("review.reference_placeholder"))
        });
        cx.subscribe(&review_reference, |_: &mut Self, _, _: &InputEvent, cx| {
            cx.notify()
        })
        .detach();
        let task_id = task.id;
        cx.subscribe(&title, move |this, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Edited) {
                let title = input.read(cx).content().trim().to_owned();
                if let Some(draft) = this
                    .new_task_draft
                    .as_mut()
                    .filter(|task| task.id == task_id)
                {
                    draft.title = title;
                    draft.touch();
                } else if !title.is_empty()
                    && let Some(task) = this.state.tasks.iter_mut().find(|task| task.id == task_id)
                    && task.title != title
                {
                    task.title = title;
                    task.touch();
                    this.schedule_task_detail_save(cx);
                }
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(&description, move |this, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Edited) {
                let description = input.read(cx).content().to_owned();
                if let Some(draft) = this
                    .new_task_draft
                    .as_mut()
                    .filter(|task| task.id == task_id)
                {
                    draft.description = description;
                    draft.touch();
                } else if let Some(task) =
                    this.state.tasks.iter_mut().find(|task| task.id == task_id)
                    && task.description != description
                {
                    task.description = description;
                    task.touch();
                    this.schedule_task_detail_save(cx);
                }
                cx.notify();
            }
        })
        .detach();
        let title_focus = title.read(cx).focus();
        let editor_focus = cx.focus_handle();
        let list = ListState::new(0, ListAlignment::Top, px(256.0));
        list.splice_focusable(0..0, [Some(editor_focus.clone())]);
        let owner = cx.entity().downgrade();
        let attach_picker =
            cx.new(|cx| attach_picker::TaskAttachPicker::new(task_id, owner, window, cx));
        self.task_detail = Some(TaskDetail {
            id: task.id,
            title,
            description,
            list,
            editor_focus,
            session_ids: Vec::new(),
            session_indices: Vec::new(),
            session_focuses: HashMap::new(),
            editor_layout: None,
            attach_picker,
            project_menu: ContextMenuHandle::new(cx),
            workspace_menu: ContextMenuHandle::new(cx),
            review_scope: issue_review::ReviewScope::Uncommitted,
            review_view: issue_review::IssueReviewView::default(),
            review_reference,
        });
        let weak = cx.entity().downgrade();
        window.on_next_frame(move |window, cx| {
            let still_open = weak
                .update(cx, |this, _| {
                    this.board_open && this.selected_task == Some(task_id)
                })
                .unwrap_or(false);
            if still_open {
                window.focus(&title_focus, cx);
            }
        });
    }

    /// Explicit issue mutations wait for the daemon's database acknowledgment.
    /// Coalescing preserves ordering without blocking a frame or streaming saves.
    pub(super) fn save_task_changes(&mut self, cx: &mut Context<Self>) {
        super::rpa::daemon::model_changed();
        // This save includes the current issue catalog, including any edit
        // whose debounce timer has not fired yet.
        self.task_detail_save.take_pending();
        if self.task_save_in_flight {
            self.task_save_pending = true;
            return;
        }
        self.state.right_panel_descriptors = self.persisted_right_panel_descriptors(cx);
        let completion_attempts = self.task_completion_save_attempts();
        let prepared = self.store.prepare_confirmed_save(&mut self.state);
        let (dirty_ids, save) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.settle_task_completion_save(&completion_attempts, false);
                self.task_save_error = Some(error.to_string());
                self.show_toast(tr!("errors.save_local_state", error = error));
                cx.notify();
                return;
            }
        };
        self.task_save_in_flight = true;
        self.task_save_pending = false;
        self.task_save_error = None;
        cx.spawn(async move |waku, cx| {
            let result = cx.background_executor().spawn(async move { save() }).await;
            let _ = waku.update(cx, |this, cx| {
                this.task_save_in_flight = false;
                this.settle_task_completion_save(&completion_attempts, result.is_ok());
                if let Err(error) = result {
                    for id in dirty_ids {
                        this.state.mark_session_dirty(id);
                    }
                    this.task_save_error = Some(error.to_string());
                    this.show_toast(tr!("errors.save_local_state", error = error));
                }
                if std::mem::take(&mut this.task_save_pending) {
                    this.save_task_changes(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn render_task_save_status(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        // A save acknowledgment must not move filters or issue actions. Keep
        // the same small slot in both header modes, including while idle.
        let slot = div()
            .id("board-save-status")
            .w(px(80.0))
            .h(px(30.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_end();
        if !self.task_save_in_flight && !self.task_save_pending && self.task_save_error.is_none() {
            return slot.into_any_element();
        }
        if self.task_save_error.is_some() {
            return slot
                .child(
                    self.task_action_button(
                        "task-save-retry",
                        tr!("common.retry"),
                        false,
                        true,
                        |this, _, cx| this.save_task_changes(cx),
                        cx,
                    )
                    .aria_label(tr!("board.retry_save"))
                    .tooltip(Tooltip::text(tr!("board.retry_save")))
                    .text_color(theme.warning),
                )
                .into_any_element();
        }
        slot.text_size(sp(11.0))
            .text_color(theme.text_ghost)
            .child(tr!("board.saving"))
            .into_any_element()
    }

    fn schedule_task_detail_save(&mut self, cx: &mut Context<Self>) {
        super::rpa::daemon::model_changed();
        self.sidebar_rows_fingerprint.set(None);
        let generation = self.task_detail_save.schedule();
        cx.spawn(async move |waku, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            let _ = waku.update(cx, |this, cx| {
                if this.task_detail_save.is_current(generation) {
                    this.save_task_changes(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn show_task_board(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_task = None;
        self.task_detail = None;
        self.board_open = true;
        if self.task_detail_save.take_pending() {
            self.save_task_changes(cx);
        }
        window.focus(&self.board_dialog_focus(cx), cx);
        cx.notify();
    }

    /// A single reusable native action control, with a stable focus handle,
    /// mouse activation, and the conventional Enter/Space keyboard behavior.
    pub(super) fn task_action_button(
        &self,
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        primary: bool,
        enabled: bool,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let id = id.into();
        let label = label.into();
        let focus = self
            .transcript_control_focuses
            .borrow_mut()
            .entry(id.to_string())
            .or_insert_with(|| cx.focus_handle().tab_stop(true))
            .clone()
            .tab_stop(enabled);
        let theme = Theme::current(cx);
        let action = Rc::new(action);
        let click = Rc::clone(&action);
        div()
            .id(id)
            .semantic_control(gpui::Role::Button, !enabled)
            .aria_label(label.clone())
            .track_focus(&focus)
            .tab_index(0)
            .tab_stop(enabled)
            .h(px(30.0))
            .px(px(10.0))
            .rounded(px(7.0))
            .flex()
            .items_center()
            .justify_center()
            .flex_none()
            .border_1()
            .border_color(if primary {
                gpui::transparent_black()
            } else {
                theme.border_strong
            })
            .bg(if primary && enabled {
                theme.inverse
            } else {
                gpui::transparent_black()
            })
            .text_color(if !enabled {
                theme.text_ghost
            } else if primary {
                theme.on_inverse
            } else {
                theme.text_secondary
            })
            .text_size(sp(12.5))
            .cursor_default()
            .focus_visible(|style| style.border_color(theme.accent))
            .when(enabled, |element| {
                if primary {
                    element
                        .hover(|style| style.opacity(0.9))
                        .active(|style| style.opacity(0.8))
                } else {
                    element
                        .hover(|style| style.bg(theme.overlay))
                        .active(|style| style.bg(theme.overlay_strong))
                }
            })
            .child(label)
            .on_click(cx.listener(move |this, _, window, cx| {
                if enabled {
                    click(this, window, cx);
                }
                cx.stop_propagation();
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if enabled
                    && !event.keystroke.modifiers.modified()
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                {
                    action(this, window, cx);
                    cx.stop_propagation();
                }
            }))
    }

    /// Shell-sized actions retain the same semantic IDs and keyboard behavior
    /// as labeled task buttons. Their names live in tooltips, not page chrome.
    pub(super) fn task_icon_action(
        &self,
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        path: &'static str,
        enabled: bool,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let label = label.into();
        let id = id.into();
        let theme = Theme::current(cx);
        let button = self.task_action_button(id.clone(), "", false, enabled, action, cx);
        let focus = self.transcript_control_focuses.borrow()[id.as_ref()].clone();
        button
            .relative()
            .child(crate::ui::tooltip::FocusHint::new(focus, label.clone()))
            .aria_label(label.clone())
            .size(px(28.0))
            .px_0()
            .border_color(gpui::transparent_black())
            .tooltip(Tooltip::text(label))
            .child(icon(
                path,
                14.0,
                if enabled {
                    theme.text_secondary
                } else {
                    theme.text_ghost
                },
            ))
    }

    pub(super) fn start_task_session(
        &mut self,
        task_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.task_completion_stops.remove(&task_id);
        let Some(task) = self
            .state
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .cloned()
        else {
            return;
        };
        let Some(project_id) = task_project_id(&task)
            .filter(|id| self.state.projects.iter().any(|project| project.id == *id))
        else {
            self.show_toast(tr!("board.choose_project_first"));
            cx.notify();
            return;
        };
        let draft = task_development_draft(&task, &self.state.sessions);
        self.reopen_task_for_continued_work(task_id, cx);
        let id = if let Some(id) = draft {
            // Repeated clicks reopen this issue's unsent draft, never a draft
            // that happens to share the same project with a different issue.
            id
        } else {
            let mut session = self.state.new_session(project_id, ProviderKind::Pi);
            session.task_id = Some(task_id);
            session.workspace = task_session_workspace(&task);
            let id = session.id;
            self.composer_drafts.set(
                crate::persistence::ComposerDraftKey::for_session(&session),
                crate::persistence::ComposerDraft {
                    text: task_session_prompt(&task),
                    ..Default::default()
                },
            );
            self.state.push_session(session);
            id
        };
        if let Some(task) = self.state.tasks.iter_mut().find(|task| task.id == task_id)
            && task.state == TaskState::Backlog
        {
            task.set_state(TaskState::Doing);
        }
        self.sidebar_rows_fingerprint.set(None);
        self.select_session(id, cx);
        self.schedule_composer_draft_save(cx);
        self.save_task_changes(cx);
        window.focus(&self.composer_focus(cx), cx);
    }

    pub(super) fn set_task_workspace(
        &mut self,
        task_id: Uuid,
        workspace: TaskWorkspace,
        cx: &mut Context<Self>,
    ) {
        let project_id = match &workspace {
            TaskWorkspace::Local { project_id }
            | TaskWorkspace::NewWorktree { project_id }
            | TaskWorkspace::Worktree { project_id, .. } => *project_id,
        };
        if let Some(draft) = self
            .new_task_draft
            .as_mut()
            .filter(|task| task.id == task_id)
        {
            draft.workspace = Some(workspace);
            draft.touch();
            cx.notify();
            return;
        }
        let Some(task) = self.state.tasks.iter_mut().find(|task| task.id == task_id) else {
            return;
        };
        if task.workspace.as_ref() == Some(&workspace) {
            return;
        }
        task.workspace = Some(workspace);
        task.touch();
        let session_workspace = task_session_workspace(task);
        let drafts = self
            .state
            .sessions
            .iter()
            .filter(|session| task_draft_follows_workspace(task_id, session))
            .map(|session| session.id)
            .collect::<Vec<_>>();
        for id in drafts {
            if let Some(session) = self.state.session_mut(id) {
                session.project_id = project_id;
                session.workspace = session_workspace.clone();
                session.updated_at = session.updated_at.max(unix_time()).saturating_add(1);
            }
        }
        self.sidebar_rows_fingerprint.set(None);
        self.save_task_changes(cx);
        cx.notify();
    }

    fn add_project_for_task(&mut self, task_id: Uuid, cx: &mut Context<Self>) {
        if self.daemon.is_remote() {
            self.show_toast(tr!("errors.remote_project_picker"));
            cx.notify();
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(tr!("project.add_project").into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = this.update(cx, |this, cx| {
                    if this.task_for_editor(task_id).is_none() {
                        return;
                    }
                    let preferred = this.task_for_editor(task_id).and_then(task_project_id);
                    let project_id =
                        this.register_project(path, preferred, projects::ProjectPathHost::Local);
                    if preferred == Some(project_id) {
                        return;
                    }
                    this.set_task_workspace(task_id, TaskWorkspace::NewWorktree { project_id }, cx);
                });
            }
        })
        .detach();
    }

    /// Once the first issue-owned worktree materializes, make it the default
    /// for follow-up conversations. Existing concrete bindings are deliberate
    /// user choices and are not replaced by some other conversation finishing.
    pub(super) fn sync_task_workspace_from_session(
        &mut self,
        session_id: Uuid,
        materialized: bool,
    ) -> bool {
        let Some(session) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        else {
            return false;
        };
        let Some(task_id) = session.task_id else {
            return false;
        };
        let workspace = match &session.workspace {
            SessionWorkspace::Local => TaskWorkspace::Local {
                project_id: session.project_id,
            },
            SessionWorkspace::Worktree { path, branch } => TaskWorkspace::Worktree {
                project_id: session.project_id,
                path: path.clone(),
                branch: branch.clone(),
            },
            SessionWorkspace::NewWorktree { .. } => return false,
        };
        if let Some(task) = self.state.tasks.iter_mut().find(|task| task.id == task_id)
            && (task.workspace.is_none()
                || materialized
                    && matches!(task.workspace, Some(TaskWorkspace::NewWorktree { .. })))
            && task_project_id(task).is_none_or(|id| id == session.project_id)
        {
            task.workspace = Some(workspace);
            task.touch();
            return true;
        }
        false
    }

    fn render_task_project_picker(&mut self, task: &Task, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let task_id = task.id;
        let project_id = task_project_id(task);
        let label = self
            .state
            .projects
            .iter()
            .find(|project| Some(project.id) == project_id)
            .map(Project::display_name)
            .unwrap_or_else(|| tr!("board.choose_project"));
        let Some(detail) = self.task_detail.as_ref() else {
            return div().into_any_element();
        };
        let handle = detail.project_menu.clone();
        let weak = cx.entity().downgrade();
        dropdown_menu_list(
            MenuChip::new("task-detail-project")
                .icon("icons/folder.svg", theme.text_tertiary)
                .label(label)
                .max_w(px(300.0)),
            "task-detail-project-menu",
            &handle,
            MenuAlign::BelowLeft,
            move |cx| {
                let Some(entity) = weak.upgrade() else {
                    return Vec::new();
                };
                let this = entity.read(cx);
                let projects = projects::picker_projects(&this.state.projects, project_id)
                    .into_iter()
                    .map(|project| (project.id, project.display_name()))
                    .collect::<Vec<_>>();
                let has_sessions = this
                    .state
                    .sessions
                    .iter()
                    .any(|session| session.task_id == Some(task_id) && session.has_started());
                let mut items = projects
                    .iter()
                    .map(|(id, name)| {
                        let weak = weak.clone();
                        let id = *id;
                        MenuItem::new(name.clone(), move |_, cx| {
                            if Some(id) == project_id {
                                return;
                            }
                            let _ = weak.update(cx, |this, cx| {
                                this.set_task_workspace(
                                    task_id,
                                    TaskWorkspace::NewWorktree { project_id: id },
                                    cx,
                                );
                            });
                        })
                        .semantic_id(format!("task-project-{id}"))
                        .selected(Some(id) == project_id)
                        .disabled(has_sessions && Some(id) != project_id)
                    })
                    .collect::<Vec<_>>();
                if has_sessions {
                    items.push(MenuItem::Header(tr!("board.project_locked").into()));
                } else {
                    items.push(MenuItem::Separator);
                    let weak = weak.clone();
                    items.push(
                        MenuItem::new(tr!("project.new_project"), move |_, cx| {
                            let _ =
                                weak.update(cx, |this, cx| this.add_project_for_task(task_id, cx));
                        })
                        .icon("icons/folder-new.svg"),
                    );
                }
                items
            },
        )
    }

    fn render_task_workspace_picker(&mut self, task: &Task, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let task_id = task.id;
        let Some(project_id) = task_project_id(task) else {
            return div().into_any_element();
        };
        let label = match &task.workspace {
            Some(TaskWorkspace::Worktree { branch, .. }) => branch.clone(),
            Some(TaskWorkspace::NewWorktree { .. }) => tr!("workspace.new_worktree"),
            _ => tr!("workspace.local"),
        };
        let workspace_hint = match &task.workspace {
            Some(TaskWorkspace::Worktree { branch, path, .. }) => {
                format!("{branch} · {}", path.display())
            }
            Some(TaskWorkspace::Local { project_id }) => self
                .state
                .projects
                .iter()
                .find(|project| project.id == *project_id)
                .map(|project| project.path.display().to_string())
                .unwrap_or_else(|| label.clone()),
            _ => label.clone(),
        };
        let projectless = self
            .state
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .is_some_and(Project::is_projectless);
        let selected = task.workspace.clone();
        let Some(detail) = self.task_detail.as_ref() else {
            return div().into_any_element();
        };
        let handle = detail.workspace_menu.clone();
        let weak = cx.entity().downgrade();
        dropdown_menu_list(
            MenuChip::new("task-detail-workspace")
                .icon("icons/git-branch.svg", theme.text_tertiary)
                .label(label)
                .max_w(px(300.0))
                .tooltip(Tooltip::text(workspace_hint)),
            "task-detail-workspace-menu",
            &handle,
            MenuAlign::BelowLeft,
            move |cx| {
                let Some(entity) = weak.upgrade() else {
                    return Vec::new();
                };
                let this = entity.read(cx);
                let mut seen = HashSet::new();
                let worktrees = this
                    .state
                    .sessions
                    .iter()
                    .filter(|session| session.project_id == project_id)
                    .filter_map(|session| match &session.workspace {
                        SessionWorkspace::Worktree { path, branch }
                            if seen.insert(path.clone()) =>
                        {
                            Some((path.clone(), branch.clone()))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                let local = weak.clone();
                let mut items = vec![
                    MenuItem::new(tr!("workspace.local"), move |_, cx| {
                        let _ = local.update(cx, |this, cx| {
                            this.set_task_workspace(
                                task_id,
                                TaskWorkspace::Local { project_id },
                                cx,
                            )
                        });
                    })
                    .semantic_id("task-workspace-local")
                    .selected(matches!(selected, Some(TaskWorkspace::Local { .. }))),
                ];
                for (path, branch) in &worktrees {
                    let target = TaskWorkspace::Worktree {
                        project_id,
                        path: path.clone(),
                        branch: branch.clone(),
                    };
                    let is_selected = selected.as_ref() == Some(&target);
                    let weak = weak.clone();
                    items.push(
                        MenuItem::new(branch.clone(), move |_, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.set_task_workspace(task_id, target.clone(), cx)
                            });
                        })
                        .semantic_id(format!("task-workspace-{}", path.display()))
                        .icon("icons/git-branch.svg")
                        .selected(is_selected),
                    );
                }
                items.push(MenuItem::Separator);
                let weak = weak.clone();
                items.push(
                    MenuItem::new(tr!("workspace.new_worktree"), move |_, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.set_task_workspace(
                                task_id,
                                TaskWorkspace::NewWorktree { project_id },
                                cx,
                            )
                        });
                    })
                    .semantic_id("task-workspace-new")
                    .icon("icons/fork.svg")
                    .selected(matches!(selected, Some(TaskWorkspace::NewWorktree { .. })))
                    .disabled(projectless),
                );
                items
            },
        )
    }

    pub(super) fn render_task_detail(
        &mut self,
        task_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(task) = self.task_for_editor(task_id).cloned() else {
            self.selected_task = None;
            self.task_detail = None;
            return self.render_board(window, cx);
        };
        self.ensure_task_detail(&task, window, cx);
        // Collect only identity/index/order once per frame. Card text, workspace
        // labels, live status, and controls are built only for visible rows.
        let mut sessions = self
            .state
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| session.task_id == Some(task_id))
            .map(|(index, session)| (session.id, index, session.updated_at))
            .collect::<Vec<_>>();
        sessions.sort_by_key(|(_, _, updated)| std::cmp::Reverse(*updated));
        let review_view = issue_review::issue_review_view(&task, &sessions, &self.state.sessions);
        let review_revision = review_view
            .latest
            .map(|(id, index)| (id, self.state.sessions[index].updated_at));
        let is_draft = self.is_task_draft(task_id);
        let detail = self
            .task_detail
            .as_mut()
            .expect("task detail was initialized");
        detail.review_view = review_view;
        let ids = sessions.iter().map(|(id, _, _)| *id).collect::<Vec<_>>();
        if detail.session_ids != ids {
            let prefix = detail
                .session_ids
                .iter()
                .zip(&ids)
                .take_while(|(old, new)| old == new)
                .count();
            let old_count = detail.session_ids.len();
            let retained_ids = ids.iter().copied().collect::<HashSet<_>>();
            detail
                .session_focuses
                .retain(|id, _| retained_ids.contains(id));
            let focuses = ids[prefix..]
                .iter()
                .map(|id| {
                    Some(
                        detail
                            .session_focuses
                            .entry(*id)
                            .or_insert_with(|| cx.focus_handle())
                            .clone(),
                    )
                })
                .collect::<Vec<_>>();
            // Row zero is the editor. Keep its measurements and focus scope,
            // and splice only the changed session suffix to preserve scrolling.
            detail
                .list
                .splice_focusable(prefix + 1..old_count + 1, focuses);
            detail.list.clone().with_uniform_item_height(px(44.0));
            detail.session_ids = ids;
        }
        detail.session_indices = sessions.into_iter().map(|(_, index, _)| index).collect();
        let editor_layout = (
            task.updated_at,
            // Create can keep the same timestamp and editor entity, but adds
            // review and conversation controls that change the row height.
            is_draft,
            self.task_completion_stops.contains_key(&task_id),
            detail.title.read(cx).content().trim().is_empty(),
            detail.session_ids.is_empty(),
            review_revision,
        );
        if detail.editor_layout != Some(editor_layout) {
            detail.list.remeasure_items(0..1);
            detail.editor_layout = Some(editor_layout);
        }
        let list_state = detail.list.clone();
        let weak = cx.entity().downgrade();
        let theme = Theme::current(cx);
        div()
            .id("task-detail")
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.surface)
            .child(
                div().size_full().flex().justify_center().child(
                    div().w_full().h_full().max_w(px(840.0)).px(px(24.0)).child(
                        list(list_state, move |index, _window, cx| {
                            weak.update(cx, |this, cx| {
                                if index == 0 {
                                    this.render_task_detail_editor(&task, cx)
                                } else {
                                    this.render_task_detail_session(index - 1, cx)
                                }
                            })
                            .unwrap_or_else(|_| div().into_any_element())
                        })
                        .size_full(),
                    ),
                ),
            )
            .into_any_element()
    }

    /// Both the form and the session cards share this list, so the editor
    /// scrolls out naturally instead of leaving a nested conversation scroller.
    fn render_task_detail_editor(&mut self, task: &Task, cx: &mut Context<Self>) -> AnyElement {
        let task_id = task.id;
        let is_draft = self.is_task_draft(task_id);
        let detail = self
            .task_detail
            .as_ref()
            .expect("task detail was initialized");
        let title = detail.title.clone();
        let description = detail.description.clone();
        let editor_focus = detail.editor_focus.clone();
        let session_count = detail.session_ids.len();
        let title_empty = title.read(cx).content().trim().is_empty();
        let theme = Theme::current(cx);
        let project = task_project_id(&task)
            .and_then(|id| self.state.projects.iter().find(|project| project.id == id));
        let can_start = project.is_some();
        let completing = self.task_completion_stops.contains_key(&task_id);
        let project_picker = self.render_task_project_picker(&task, cx);
        let workspace_picker = self.render_task_workspace_picker(&task, cx);
        let attach = div()
            .child(self.render_task_attach_picker(&task, cx))
            .on_action(cx.listener(move |this, _: &FocusNext, window, cx| {
                if can_start && !title_empty
                    || !this.focus_task_detail_session(0, false, window, cx)
                {
                    cx.propagate();
                }
            }));
        let start = self
            .task_icon_action(
                "task-detail-new-session",
                if task.state.is_done() {
                    tr!("review.reopen_develop_pi")
                } else {
                    tr!("review.develop_pi")
                },
                "icons/compose.svg",
                can_start && !title_empty,
                move |this, window, cx| this.start_task_session(task_id, window, cx),
                cx,
            )
            .on_action(cx.listener(|this, _: &FocusNext, window, cx| {
                if !this.focus_task_detail_session(0, false, window, cx) {
                    cx.propagate();
                }
            }));

        let review = (!is_draft).then(|| self.render_issue_review(task, cx));
        let editor = div()
            .track_focus(&editor_focus)
            .w_full()
            .min_w_0()
            .pt(px(24.0))
            .pb(px(18.0))
            .flex()
            .flex_col()
            .gap(px(18.0))
            .when(completing, |element| {
                element.child(
                    div()
                        .p(px(12.0))
                        .rounded(px(8.0))
                        .bg(theme.overlay)
                        .text_size(sp(12.0))
                        .text_color(theme.text_secondary)
                        .child(tr!("board.completion_stopping")),
                )
            })
            .when(task.state.is_done(), |element| {
                element.child(
                    div()
                        .p(px(12.0))
                        .rounded(px(8.0))
                        .bg(theme.overlay)
                        .text_size(sp(12.0))
                        .text_color(theme.text_secondary)
                        .child(tr!("board.archived_hint")),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div()
                            .h(px(40.0))
                            .px(px(10.0))
                            .flex()
                            .items_center()
                            .rounded(px(7.0))
                            .border_1()
                            .border_color(if title_empty && !is_draft {
                                theme.warning
                            } else {
                                gpui::transparent_black()
                            })
                            .hover(|style| style.border_color(theme.border))
                            .text_size(sp(20.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(title),
                    )
                    .when(title_empty && !is_draft, |element| {
                        element.child(
                            div()
                                .pl(px(11.0))
                                .text_size(sp(11.0))
                                .text_color(theme.warning)
                                .child(tr!("board.title_required")),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            // MenuChip contributes 7px; match the fields' 11px text inset.
                            .pl(px(4.0))
                            .gap(px(8.0))
                            .child(project_picker)
                            .child(workspace_picker),
                    )
                    .when(!can_start, |element| {
                        element.child(
                            div()
                                .pl(px(11.0))
                                .text_size(sp(12.0))
                                .text_color(theme.warning)
                                .child(tr!("board.choose_project_first")),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div().pl(px(11.0)).flex().items_center().child(
                            div()
                                .text_size(sp(12.0))
                                .text_color(theme.text_tertiary)
                                .child(tr!("board.description")),
                        ),
                    )
                    .child(
                        div()
                            .h(px(156.0))
                            .p(px(10.0))
                            .rounded(px(8.0))
                            .border_1()
                            .border_color(theme.border)
                            .bg(gpui::transparent_black())
                            .text_size(sp(13.0))
                            .line_height(sp(21.0))
                            .text_color(theme.text)
                            .child(description),
                    ),
            )
            .when_some(review, |element, review| element.child(review))
            .when(!is_draft, |element| {
                element
                    .child(div().mx(px(11.0)).h(px(1.0)).bg(theme.border))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(8.0))
                            .px(px(11.0))
                            .child(
                                div()
                                    .text_size(sp(14.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child(board::conversations_label(session_count)),
                            )
                            .child(div().flex_1())
                            .child(attach)
                            .child(start),
                    )
                    .when(session_count == 0, |element| {
                        element.child(
                            div()
                                .py(px(30.0))
                                .px(px(18.0))
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap(px(8.0))
                                .child(
                                    div()
                                        .text_size(sp(13.0))
                                        .text_color(theme.text_secondary)
                                        .child(tr!("board.no_sessions")),
                                ),
                        )
                    })
            })
            .into_any_element();
        editor
    }

    fn detach_task_detail_session(
        &mut self,
        id: Uuid,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.unbind_session_from_task(id, cx);
        // A draft cannot be detached. Keep its control focused when the
        // operation was refused, so the user can act on the explanation.
        if self
            .state
            .sessions
            .iter()
            .any(|session| session.id == id && session.task_id.is_some())
        {
            return;
        }
        // The focused unlink control disappears with its row. Move to the
        // nearest remaining conversation (or the editor).
        if !self.focus_task_detail_session(index + 1, false, window, cx)
            && !(index > 0 && self.focus_task_detail_session(index - 1, false, window, cx))
        {
            self.focus_task_detail_editor_tail(window, cx);
        }
    }

    fn render_task_detail_session(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(detail) = self.task_detail.as_ref() else {
            return div().into_any_element();
        };
        let Some((&id, &session_index)) = detail
            .session_ids
            .get(index)
            .zip(detail.session_indices.get(index))
        else {
            return div().into_any_element();
        };
        let Some(session) = self
            .state
            .sessions
            .get(session_index)
            .filter(|session| session.id == id)
        else {
            return div().into_any_element();
        };
        let focus = detail.session_focuses[&id].clone().tab_stop(true);
        let count = detail.session_ids.len();
        let title = if session.has_started() || session.review.is_some() {
            session.display_title().to_owned()
        } else {
            tr!("board.draft_session")
        };
        let provider = session.provider;
        let review_status = session.review.as_ref().map(|review| {
            if session.status == SessionStatus::Waiting {
                tr!("board.needs_you_short")
            } else {
                issue_review::review_status_label(review)
            }
        });
        let status =
            if session.status == SessionStatus::Idle && self.session_has_live_background_work(id) {
                SessionStatus::Background
            } else {
                session.status
            };
        let show_workspace = self
            .task_for_editor(detail.id)
            .is_none_or(|task| !task_session_uses_default_workspace(task, session));
        let workspace_label = match &session.workspace {
            SessionWorkspace::Local => tr!("workspace.local"),
            SessionWorkspace::NewWorktree { .. } => tr!("workspace.new_worktree"),
            SessionWorkspace::Worktree { branch, path } => {
                format!("{} · {}", branch, compact_path(path))
            }
        };
        let theme = Theme::current(cx);
        let menu = self.menu_handle(&format!("task-session-menu-{id}"), cx);
        let keyboard_menu = menu.clone();
        let weak = cx.entity().downgrade();
        let menu_items = move |_: &mut App| {
            let weak = weak.clone();
            vec![
                MenuItem::new(tr!("board.detach_session"), move |window, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.detach_task_detail_session(id, index, window, cx)
                    });
                })
                .semantic_id(format!("issue-session-detach-menu-{id}")),
            ]
        };
        let detach = self.task_icon_action(
            format!("issue-session-detach-{id}"),
            tr!("board.detach_session"),
            "icons/x.svg",
            true,
            move |this, window, cx| this.detach_task_detail_session(id, index, window, cx),
            cx,
        );
        let next_focus = focus.clone();
        let previous_focus = focus.clone();
        let row = div()
            .id(SharedString::from(format!("task-session-{id}")))
            .semantic_control(gpui::Role::Button, false)
            .aria_label(title.clone())
            .track_focus(&focus)
            .tab_index(0)
            .tab_stop(true)
            .w_full()
            // 10px padding + 1px focus border shares the editor's content baseline.
            .px(px(10.0))
            .py(px(5.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(gpui::transparent_black())
            .flex()
            .flex_col()
            .gap(px(4.0))
            .cursor_pointer()
            .hover(|style| style.bg(theme.overlay))
            .active(|style| style.bg(theme.overlay_strong))
            .focus_visible(|style| style.border_color(theme.accent))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_session(id, cx);
                window.focus(&this.composer_focus(cx), cx);
            }))
            .on_action(cx.listener(move |this, _: &FocusNext, window, cx| {
                if next_focus.is_focused(window)
                    || !this.focus_task_detail_session(index + 1, false, window, cx)
                {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(move |this, _: &FocusPrev, window, cx| {
                if !previous_focus.is_focused(window) {
                    cx.propagate();
                } else if index == 0 {
                    this.focus_task_detail_editor_tail(window, cx);
                } else if !this.focus_task_detail_session(index - 1, true, window, cx) {
                    cx.propagate();
                }
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                let key = event.keystroke.key.as_str();
                if key == "f10" && event.keystroke.modifiers.shift {
                    keyboard_menu.open_context_menu(window, cx);
                    cx.stop_propagation();
                } else if !event.keystroke.modifiers.modified() {
                    if matches!(key, "enter" | "space") {
                        this.select_session(id, cx);
                        window.focus(&this.composer_focus(cx), cx);
                        cx.stop_propagation();
                    } else if let Some(next) = task_session_navigation_index(index, count, key) {
                        this.focus_task_detail_session(next, false, window, cx);
                        cx.stop_propagation();
                    }
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(provider_mark(provider, 14.0, theme.text_tertiary))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(sp(13.0))
                            .line_height(sp(18.0))
                            .text_color(theme.text)
                            .child(single_line_label(&title)),
                    )
                    .when(
                        status != SessionStatus::Idle || review_status.is_some(),
                        |element| {
                            element.child(
                                div()
                                    .flex_none()
                                    .text_size(sp(11.0))
                                    .text_color(status_color(&theme, status))
                                    .child(review_status.unwrap_or_else(|| match status {
                                        SessionStatus::Working
                                        | SessionStatus::Connecting
                                        | SessionStatus::Background => tr!("board.running"),
                                        SessionStatus::Waiting => tr!("board.needs_you_short"),
                                        SessionStatus::Failed => tr!("board.session_failed"),
                                        SessionStatus::Idle => tr!("board.session_idle"),
                                    })),
                            )
                        },
                    )
                    .child(detach),
            )
            .when(show_workspace, |element| {
                element.child(
                    div()
                        .pl(px(22.0))
                        .text_size(sp(11.5))
                        .line_height(sp(16.0))
                        .text_color(theme.text_tertiary)
                        .truncate()
                        .child(single_line_label(&workspace_label)),
                )
            });
        let row = context_menu(
            div().w_full().child(row),
            SharedString::from(format!("task-session-context-{id}")),
            &menu,
            menu_items,
        );
        div().w_full().pb(px(2.0)).child(row).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::SemanticControlExt;

    #[test]
    fn leaving_an_unchanged_issue_does_not_create_a_save() {
        let mut save = TaskDetailSave::default();
        assert!(!save.take_pending());
        assert!(
            !save.take_pending(),
            "repeated Back is still navigation only"
        );
    }

    #[test]
    fn leaving_after_an_edit_flushes_once_and_retires_its_timer() {
        let mut save = TaskDetailSave::default();
        let generation = save.schedule();
        assert!(save.is_current(generation));
        assert!(save.take_pending(), "the real edit must be flushed");
        assert!(
            !save.is_current(generation),
            "the old timer cannot save twice"
        );
        assert!(!save.take_pending(), "reopening and leaving adds no write");
    }

    #[test]
    fn consuming_a_pending_save_does_not_invalidate_a_newer_edit() {
        let mut save = TaskDetailSave::default();
        let first = save.schedule();
        let second = save.schedule();
        assert!(!save.is_current(first));
        assert!(save.is_current(second));

        // An explicit save, including one queued behind an in-flight save,
        // consumes the pending catalog edit before leaving the inspector.
        assert!(save.take_pending());
        let third = save.schedule();
        assert!(!save.is_current(first));
        assert!(!save.is_current(second));
        assert!(save.is_current(third));
        assert!(save.take_pending());
    }

    #[test]
    fn development_draft_reuse_matches_workspace_and_preserves_review_corrections() {
        let project_id = Uuid::new_v4();
        let mut task = Task::new("Fix checkout", TaskState::Doing, 0);
        task.workspace = Some(TaskWorkspace::Local { project_id });
        let mut fix = AgentSession::new(project_id, ProviderKind::Pi);
        fix.task_id = Some(task.id);
        let mut review = AgentSession::new(project_id, ProviderKind::Codex);
        review.task_id = Some(task.id);
        fix.review_correction_source_id = Some(review.id);
        review.review = Some(SessionReview {
            target: IssueReviewTarget::UncommittedChanges,
            source_session_id: None,
            status: IssueReviewStatus::Completed,
            summary: None,
            result_message_id: Some(Uuid::new_v4()),
            fix_session_id: Some(fix.id),
        });
        let mut ordinary = AgentSession::new(project_id, ProviderKind::Pi);
        ordinary.task_id = Some(task.id);
        let ordinary_id = ordinary.id;
        let mut sessions = vec![fix, review, ordinary];
        assert_eq!(task_development_draft(&task, &sessions), Some(ordinary_id));
        sessions.pop();
        assert_eq!(task_development_draft(&task, &sessions), None);
        assert!(!task_draft_follows_workspace(task.id, &sessions[0]));
        issue_review::begin_review(&mut sessions[1]);
        assert!(
            sessions[1]
                .review
                .as_ref()
                .unwrap()
                .fix_session_id
                .is_none()
        );
        assert_eq!(task_development_draft(&task, &sessions), None);
        assert!(!task_draft_follows_workspace(task.id, &sessions[0]));
        sessions.remove(1);
        assert_eq!(task_development_draft(&task, &sessions), None);
        assert!(!task_draft_follows_workspace(task.id, &sessions[0]));

        task.workspace = Some(TaskWorkspace::Worktree {
            project_id,
            path: PathBuf::from("/fixture/review-b"),
            branch: "review-b".into(),
        });
        assert_eq!(task_development_draft(&task, &sessions), None);
        let mut next = AgentSession::new(project_id, ProviderKind::Pi);
        next.task_id = Some(task.id);
        next.workspace = task_session_workspace(&task);
        let next_id = next.id;
        sessions.push(next);
        assert_eq!(task_development_draft(&task, &sessions), Some(next_id));
        sessions.last_mut().unwrap().project_id = Uuid::new_v4();
        assert_eq!(task_development_draft(&task, &sessions), None);
    }

    #[test]
    fn planned_isolation_and_explicit_local_remain_distinct() {
        let project_id = Uuid::new_v4();
        let mut task = Task::new("Fix checkout", TaskState::Backlog, 0);
        task.workspace = Some(TaskWorkspace::NewWorktree { project_id });
        assert_eq!(task_project_id(&task), Some(project_id));
        assert_eq!(
            task_session_workspace(&task),
            SessionWorkspace::NewWorktree { base_branch: None }
        );

        task.workspace = Some(TaskWorkspace::Local { project_id });
        assert_eq!(task_session_workspace(&task), SessionWorkspace::Local);
    }

    #[test]
    fn session_metadata_is_hidden_only_for_the_same_concrete_default() {
        let project_id = Uuid::new_v4();
        let mut task = Task::new("Keep the user's title", TaskState::Doing, 0);
        task.workspace = Some(TaskWorkspace::Local { project_id });
        let mut session = AgentSession::new(project_id, ProviderKind::Claude);
        assert!(task_session_uses_default_workspace(&task, &session));
        session.project_id = Uuid::new_v4();
        assert!(!task_session_uses_default_workspace(&task, &session));
        session.project_id = project_id;
        let path = std::path::PathBuf::from("/project/worktree");
        session.workspace = SessionWorkspace::Worktree {
            path: path.clone(),
            branch: "feature".into(),
        };
        assert!(!task_session_uses_default_workspace(&task, &session));
        task.workspace = Some(TaskWorkspace::Worktree {
            project_id,
            path,
            branch: "feature".into(),
        });
        assert!(task_session_uses_default_workspace(&task, &session));
        session.workspace = SessionWorkspace::Worktree {
            path: "/other/worktree".into(),
            branch: "feature".into(),
        };
        assert!(!task_session_uses_default_workspace(&task, &session));
        task.workspace = Some(TaskWorkspace::NewWorktree { project_id });
        assert!(!task_session_uses_default_workspace(&task, &session));
    }

    #[test]
    fn session_keyboard_navigation_reaches_long_list_edges_without_wrapping() {
        assert_eq!(task_session_navigation_index(0, 2_000, "up"), None);
        assert_eq!(task_session_navigation_index(0, 2_000, "down"), Some(1));
        assert_eq!(task_session_navigation_index(5, 2_000, "home"), Some(0));
        assert_eq!(task_session_navigation_index(5, 2_000, "end"), Some(1_999));
        assert_eq!(task_session_navigation_index(1_999, 2_000, "down"), None);
        assert_eq!(task_session_navigation_index(0, 0, "end"), None);
        assert_eq!(task_session_navigation_index(5, 2_000, "enter"), None);
    }

    struct DetailListHarness {
        rows: ListState,
        rendered: Rc<RefCell<Vec<usize>>>,
    }

    impl Render for DetailListHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let rendered = self.rendered.clone();
            div().w(px(800.0)).h(px(600.0)).child(
                list(self.rows.clone(), move |index, _, _| {
                    rendered.borrow_mut().push(index);
                    div()
                        .w_full()
                        .h(px(if index == 0 { 480.0 } else { 70.0 }))
                        .into_any_element()
                })
                .size_full(),
            )
        }
    }

    /// The editor is a taller first item in the same scroll surface. Neither
    /// measuring it nor jumping to a distant session may build the full tail.
    #[gpui::test]
    fn detail_body_list_keeps_long_histories_virtualized(cx: &mut gpui::TestAppContext) {
        let rows =
            ListState::new(2_001, ListAlignment::Top, px(256.0)).with_uniform_item_height(px(70.0));
        let rendered = Rc::new(RefCell::new(Vec::new()));
        let (_view, cx) = cx.add_window_view(|_, _| DetailListHarness {
            rows: rows.clone(),
            rendered: rendered.clone(),
        });
        cx.run_until_parked();
        assert!(rendered.borrow().contains(&0));
        assert!(rendered.borrow().iter().all(|index| *index < 20));

        rendered.borrow_mut().clear();
        rows.scroll_to(ListOffset {
            item_ix: 1_500,
            offset_in_item: px(0.0),
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let rendered = rendered.borrow();
        assert!(rendered.contains(&1_500));
        assert!(
            rendered.len() < 64,
            "only a viewport and overdraw should render: {}",
            rendered.len()
        );
        assert!(rendered.iter().all(|index| (1_480..1_520).contains(index)));
    }

    #[test]
    fn new_session_inherits_issue_context_and_concrete_workspace() {
        let project_id = Uuid::new_v4();
        let mut task = Task::new("Fix checkout", TaskState::Backlog, 0);
        task.description = "Keep existing carts intact".into();
        task.workspace = Some(TaskWorkspace::Worktree {
            project_id,
            path: PathBuf::from("/work/checkout"),
            branch: "fix-checkout".into(),
        });
        assert_eq!(task_project_id(&task), Some(project_id));
        assert_eq!(
            task_session_prompt(&task),
            "Fix checkout\n\nKeep existing carts intact"
        );
        assert_eq!(
            task_session_workspace(&task),
            SessionWorkspace::Worktree {
                path: PathBuf::from("/work/checkout"),
                branch: "fix-checkout".into()
            }
        );
    }

    #[test]
    fn empty_description_does_not_add_empty_prompt_sections() {
        let task = Task::new("Fix checkout", TaskState::Backlog, 0);
        assert_eq!(task_session_prompt(&task), "Fix checkout");
        assert_eq!(task_project_id(&task), None);
        assert_eq!(task_session_workspace(&task), SessionWorkspace::Local);
    }
}
