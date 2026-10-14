//! The task board: user-declared tasks arranged in three fixed columns.
//!
//! A task is the unit the user declares; conversations are filed under it and
//! archived out of the sidebar when the task reaches Done. Completion is not
//! deletion: moving a task to Done stops whatever is still running, and its
//! conversations stay reachable from the task. The daemon owns every row, so
//! the board renders only what the store handed over — no board path touches
//! disk or the network during a frame.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui::{BoxShadow, KeyBinding, KeyDownEvent, actions};

use super::sidebar::{CancelTaskRename, TASK_RENAME_PARENT_CONTEXT};
use super::*;
use crate::ui::SemanticControlExt;
use crate::ui::menu::dropdown_menu_list;

actions!(waku_board, [DismissTaskCompletion]);

const TASK_COMPLETION_DIALOG_CONTEXT: &str = "TaskCompletionDialog";

/// Escape closes the completion dialog through a key binding scoped to it — the
/// mechanism the commit and goal dialogs use. (A raw key listener on the
/// deferred overlay would depend on the binding path not consuming the key.)
pub fn init(cx: &mut App) {
    super::task_details::init_attach_picker_keys(cx);
    cx.bind_keys([KeyBinding::new(
        "escape",
        DismissTaskCompletion,
        Some(TASK_COMPLETION_DIALOG_CONTEXT),
    )]);
}

/// A card being dragged. The payload carries everything the drag preview
/// needs, so the preview view never reads app state.
struct TaskCardDrag {
    task_id: Uuid,
    title: SharedString,
}

struct TaskCardDragPreview {
    title: SharedString,
}

impl Render for TaskCardDragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::current(cx);
        div()
            .w(px(232.0))
            .px(px(10.0))
            .py(px(8.0))
            .rounded(px(9.0))
            .bg(theme.raised)
            .border_1()
            .border_color(theme.accent)
            .shadow(vec![
                BoxShadow::new(px(0.0), px(10.0), gpui::black().opacity(0.32))
                    .blur_radius(px(24.0)),
            ])
            .text_size(sp(13.5))
            .text_color(theme.text)
            .child(self.title.clone())
    }
}

/// A move to Done that must confirm closing connected or running conversations.
pub(super) struct TaskCompletionConfirm {
    pub task_id: Uuid,
    pub confirm_focus: FocusHandle,
    pub cancel_focus: FocusHandle,
}

/// Task facts gathered in one pass per frame. Cards, the board header, and the
/// sidebar's task rows read these instead of each scanning the session list
/// (docs/performance.md: a row builder does not rebuild whole-session state).
#[derive(Default)]
pub(super) struct TaskStats {
    pub(super) attention: HashMap<Uuid, usize>,
    pub(super) busy: HashSet<Uuid>,
    pub(super) conversations: HashMap<Uuid, usize>,
    pub(super) attention_total: usize,
    running: HashMap<Uuid, usize>,
    attention_sessions: HashMap<Uuid, Vec<TaskAttention>>,
    progress: HashMap<Uuid, (u64, String)>,
}

struct TaskAttention {
    session_id: Uuid,
    title: String,
    reason: String,
}

fn board_preview(text: &str) -> String {
    let bounded = text.chars().take(160).collect::<String>();
    bounded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Use only the visible tail, never scan or parse a transcript in a frame.
/// An idle session is not evidence that its issue has succeeded.
fn session_progress_preview(session: &AgentSession) -> Option<String> {
    if !session.detail_loaded {
        return None;
    }
    if let Some(block) = session.transcript_blocks.last()
        && block.after_message >= session.messages.len()
        && let Some(activity) = block.activities.last()
        && activity.kind != ActivityKind::Reasoning
    {
        return Some(board_preview(
            activity
                .display_description
                .as_deref()
                .unwrap_or(&activity.title),
        ));
    }
    session
        .messages
        .last()
        .filter(|message| message.role == MessageRole::Assistant)
        .map(|message| {
            board_preview(
                message
                    .display_content
                    .as_deref()
                    .unwrap_or(&message.content),
            )
        })
        .filter(|preview| !preview.is_empty())
}

impl TaskStats {
    fn record_session(
        &mut self,
        session: &AgentSession,
        active: bool,
        background: bool,
        reason: Option<String>,
    ) {
        let Some(task_id) = session.task_id else {
            return;
        };
        *self.conversations.entry(task_id).or_default() += 1;
        // A parked foreground turn can still have a live background process.
        // Preserve both signals instead of letting attention hide running work.
        if session.status.is_busy() || background {
            self.busy.insert(task_id);
            *self.running.entry(task_id).or_default() += 1;
        }
        if let Some(reason) = reason {
            *self.attention.entry(task_id).or_default() += 1;
            self.attention_sessions
                .entry(task_id)
                .or_default()
                .push(TaskAttention {
                    session_id: session.id,
                    title: board_preview(session.display_title()),
                    reason,
                });
            if active {
                self.attention_total += 1;
            }
        }
        if let Some(preview) = session_progress_preview(session) {
            let at = session.last_reply_at.unwrap_or(session.created_at);
            if self
                .progress
                .get(&task_id)
                .is_none_or(|(previous, _)| at >= *previous)
            {
                self.progress.insert(task_id, (at, preview));
            }
        }
    }
}

fn ready_task_draft(mut task: Task) -> Option<Task> {
    let title = task.title.trim();
    if title.is_empty() {
        return None;
    }
    task.title = title.to_owned();
    task.touch();
    Some(task)
}

/// First-guess card height for a column's virtualized list; the list measures
/// the real heights as cards are rendered.
const BOARD_CARD_ESTIMATED_HEIGHT: f32 = 92.0;

/// One board column's virtualized card list. Held across frames so measured
/// heights and the scroll position survive; `ids` is what the list renders.
pub(super) struct BoardColumnList {
    pub(super) ids: Vec<Uuid>,
    pub(super) list: ListState,
}

impl BoardColumnList {
    pub(super) fn new() -> Self {
        Self {
            ids: Vec::new(),
            list: ListState::new(0, ListAlignment::Top, px(256.0)),
        }
    }
}

pub(super) fn task_state_label(state: TaskState) -> String {
    match state {
        TaskState::Backlog => tr!("board.backlog"),
        TaskState::Doing => tr!("board.doing"),
        TaskState::Done => tr!("board.done"),
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum BoardActivityFilter {
    #[default]
    All,
    Running,
    Attention,
}

fn board_activity_matches(task: &Task, filter: BoardActivityFilter, stats: &TaskStats) -> bool {
    match filter {
        BoardActivityFilter::All => true,
        BoardActivityFilter::Running => !task.state.is_done() && stats.busy.contains(&task.id),
        BoardActivityFilter::Attention => {
            !task.state.is_done()
                && stats
                    .attention
                    .get(&task.id)
                    .is_some_and(|count| *count > 0)
        }
    }
}

impl Waku {
    // ── Derived state ─────────────────────────────────────────────────────

    /// One pass over the filed conversations, memoized for the frame; every
    /// card, badge, and task header reads the same result.
    pub(super) fn task_stats(&self) -> Rc<TaskStats> {
        if let Some(stats) = self.task_stats_cache.borrow().as_ref() {
            return Rc::clone(stats);
        }
        let active = self
            .state
            .tasks
            .iter()
            .filter(|task| !task.state.is_done())
            .map(|task| task.id)
            .collect::<HashSet<_>>();
        let mut stats = TaskStats::default();
        for session in &self.state.sessions {
            let Some(task_id) = session.task_id else {
                continue;
            };
            let runtime = self.runtimes.get(&session.id);
            let reason = if runtime.is_some_and(|runtime| {
                runtime.pending_permission.is_some() || runtime.pending_computer_approval.is_some()
            }) {
                Some(tr!("board.awaiting_permission"))
            } else if runtime.is_some_and(|runtime| runtime.pending_user_input.is_some()) {
                Some(tr!("board.awaiting_input"))
            } else if session.status == SessionStatus::Failed {
                Some(
                    runtime
                        .and_then(|runtime| runtime.last_driver_error.as_deref())
                        .map(|error| {
                            format!("{}: {}", tr!("board.session_failed"), board_preview(error))
                        })
                        .unwrap_or_else(|| tr!("board.session_failed")),
                )
            } else if session.status == SessionStatus::Waiting {
                Some(tr!("board.awaiting_input"))
            } else {
                None
            };
            stats.record_session(
                session,
                active.contains(&task_id),
                self.session_has_live_background_work(session.id),
                reason,
            );
        }
        let stats = Rc::new(stats);
        *self.task_stats_cache.borrow_mut() = Some(Rc::clone(&stats));
        stats
    }

    /// Project and text scope stay intact while inspecting an issue/session.
    pub(super) fn task_matches_board_scope(&self, task: &Task) -> bool {
        let project_id = task_details::task_project_id(task);
        if self
            .board_project_filter
            .is_some_and(|id| Some(id) != project_id)
        {
            return false;
        }
        let query = self.board_search_query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        task.title.to_lowercase().contains(&query)
            || task.description.to_lowercase().contains(&query)
            || self
                .state
                .projects
                .iter()
                .find(|project| Some(project.id) == project_id)
                .is_some_and(|project| project.display_name().to_lowercase().contains(&query))
            || matches!(&task.workspace, Some(TaskWorkspace::Worktree { branch, .. }) if branch.to_lowercase().contains(&query))
    }

    /// Task ids in one column, in board order.
    fn tasks_in_state(&self, state: TaskState) -> Vec<Uuid> {
        let stats = self.task_stats();
        let mut tasks = self
            .state
            .tasks
            .iter()
            .filter(|task| {
                task.state == state
                    && self.task_matches_board_scope(task)
                    && board_activity_matches(task, self.board_activity_filter, &stats)
            })
            .collect::<Vec<_>>();
        tasks.sort_by(|a, b| {
            a.position
                .cmp(&b.position)
                .then_with(|| b.updated_at.cmp(&a.updated_at))
        });
        tasks.into_iter().map(|task| task.id).collect()
    }

    /// A position that sorts above everything else in the column.
    pub(super) fn next_task_position(&self, state: TaskState) -> i64 {
        self.state
            .tasks
            .iter()
            .filter(|task| task.state == state)
            .map(|task| task.position)
            .min()
            .unwrap_or(1)
            .saturating_sub(1)
    }

    // ── Board ─────────────────────────────────────────────────────────────

    pub(super) fn toggle_board(&mut self, cx: &mut Context<Self>) {
        self.board_open = !self.board_open || self.selected_task.is_some();
        self.selected_task = None;
        self.task_detail = None;
        cx.notify();
    }

    pub(super) fn render_board(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(task_id) = self.selected_task {
            return self.render_task_detail(task_id, window, cx);
        }
        let theme = Theme::current(cx);
        let stats = self.task_stats();
        div()
            .id("task-board")
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.surface)
            .child(
                div()
                    .id("board-columns")
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .overflow_x_scroll()
                    .flex()
                    .gap(px(14.0))
                    .p(px(14.0))
                    .children(
                        [TaskState::Backlog, TaskState::Doing, TaskState::Done]
                            .map(|state| self.render_board_column(state, Rc::clone(&stats), cx)),
                    ),
            )
            .into_any_element()
    }

    fn render_board_column(
        &mut self,
        state: TaskState,
        stats: Rc<TaskStats>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let task_ids = self.tasks_in_state(state);
        let count = task_ids.len();
        let column_id = SharedString::from(format!("board-column-{}", state.key()));
        // One list state per column preserves measured card heights and the
        // scroll position across frames; `ids` is what the list renders.
        let list_state = {
            let mut columns = self.board_column_lists.borrow_mut();
            let slot = &mut columns[(state.ordinal() - 1) as usize];
            sync_virtual_ids(
                &slot.list,
                &mut slot.ids,
                &task_ids,
                px(BOARD_CARD_ESTIMATED_HEIGHT),
            );
            slot.list.clone()
        };
        let ids = Rc::new(task_ids);
        let card_stats = Rc::clone(&stats);
        let entity = cx.entity().downgrade();
        div()
            .id(column_id)
            .flex_1()
            .min_w(px(240.0))
            .h_full()
            .flex()
            .flex_col()
            .rounded(px(10.0))
            .bg(theme.overlay)
            .border_1()
            .border_color(theme.border)
            .drag_over::<TaskCardDrag>(move |style, _, _, _| {
                style.border_color(theme.accent).bg(theme.overlay_strong)
            })
            .on_drop(cx.listener(move |this, drag: &TaskCardDrag, window, cx| {
                this.move_task(drag.task_id, state, window, cx);
            }))
            .child(
                div()
                    .h(px(36.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .px(px(12.0))
                    .child(
                        div()
                            .text_size(sp(12.5))
                            .text_color(theme.text_secondary)
                            .child(task_state_label(state)),
                    )
                    .child(
                        div()
                            .text_size(sp(12.5))
                            .text_color(theme.text_ghost)
                            .child(count.to_string()),
                    )
                    .child(div().flex_1())
                    .child(self.render_task_create_button(
                        match state {
                            TaskState::Backlog => "board-new-task-backlog",
                            TaskState::Doing => "board-new-task-doing",
                            TaskState::Done => "board-new-task-done",
                        },
                        state,
                        cx,
                    )),
            )
            .when(count == 0, |element| {
                element.child(
                    div()
                        .px(px(16.0))
                        .py(px(28.0))
                        .text_center()
                        .text_size(sp(12.0))
                        .text_color(theme.text_tertiary)
                        .child(
                            if self.board_project_filter.is_some()
                                || !self.board_search_query.trim().is_empty()
                                || self.board_activity_filter != BoardActivityFilter::All
                            {
                                tr!("board.no_matching_issues")
                            } else {
                                match state {
                                    TaskState::Backlog => tr!("board.empty_backlog"),
                                    TaskState::Doing => tr!("board.empty_doing"),
                                    TaskState::Done => tr!("board.empty_done"),
                                }
                            },
                        ),
                )
            })
            .child(
                div().flex_1().min_h_0().p(px(8.0)).child(
                    list(list_state, move |index, _window, cx| {
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
            )
            .into_any_element()
    }

    fn render_board_card(
        &mut self,
        task_id: Uuid,
        stats: &TaskStats,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let Some(task) = self
            .state
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .cloned()
        else {
            return div().into_any_element();
        };
        let attention = stats.attention.get(&task_id).copied().unwrap_or(0);
        let running = stats.running.get(&task_id).copied().unwrap_or(0);
        let conversations = stats.conversations.get(&task_id).copied().unwrap_or(0);
        // A bound workspace shows where the task works: the worktree branch,
        // or the project a local task works in.
        let workspace = match task.workspace.as_ref() {
            Some(TaskWorkspace::Worktree {
                project_id, branch, ..
            }) => {
                let project = self
                    .state
                    .projects
                    .iter()
                    .find(|project| project.id == *project_id)
                    .map(Project::display_name)
                    .unwrap_or_default();
                Some((
                    "icons/git-branch.svg",
                    if self.board_project_filter.is_none() {
                        format!("{project} · {branch}")
                    } else {
                        branch.clone()
                    },
                ))
            }
            Some(TaskWorkspace::NewWorktree { project_id }) => {
                let project = self
                    .state
                    .projects
                    .iter()
                    .find(|project| project.id == *project_id)
                    .map(Project::display_name)
                    .unwrap_or_default();
                Some((
                    "icons/git-branch.svg",
                    if self.board_project_filter.is_none() {
                        format!("{project} · {}", tr!("workspace.new_worktree"))
                    } else {
                        tr!("workspace.new_worktree")
                    },
                ))
            }
            Some(TaskWorkspace::Local { project_id }) => self
                .state
                .projects
                .iter()
                .find(|project| project.id == *project_id)
                .filter(|_| self.board_project_filter.is_none())
                .map(|project| ("icons/folder.svg", project.display_name())),
            None => None,
        };
        let menu = self.menu_handle(format!("task-{task_id}"), cx);
        let row_focus = menu.trigger_focus_handle().clone();
        let keyboard_menu = menu.clone();
        let dragging = TaskCardDrag {
            task_id,
            title: SharedString::from(task.title.clone()),
        };
        let open_weak = cx.entity().downgrade();
        let rename_weak = cx.entity().downgrade();
        let done_weak = cx.entity().downgrade();
        let backlog_weak = cx.entity().downgrade();
        let doing_weak = cx.entity().downgrade();

        let renaming = self.task_rename == Some(task_id);
        let title: AnyElement = if renaming {
            div()
                .id(SharedString::from(format!("task-rename-field-{task_id}")))
                .key_context(TASK_RENAME_PARENT_CONTEXT)
                .on_action(cx.listener(|this, _: &CancelTaskRename, window, cx| {
                    this.cancel_task_rename(window, cx);
                }))
                .h(px(20.0))
                .flex_1()
                .min_w_0()
                .px(px(4.0))
                .rounded(px(4.0))
                .border_1()
                .border_color(theme.accent)
                .bg(theme.inset)
                .flex()
                .items_center()
                .text_size(sp(13.0))
                .text_color(theme.text)
                .child(self.task_rename_input.clone())
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_w_0()
                .whitespace_normal()
                .line_clamp(2)
                .text_overflow(gpui::TextOverflow::Truncate("…".into()))
                .text_size(sp(13.0))
                .child(SharedString::from(task.title.clone()))
                .into_any_element()
        };

        let attention_sessions = stats
            .attention_sessions
            .get(&task_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let attention_rows = attention_sessions
            .iter()
            .take(2)
            .map(|session| {
                let session_id = session.session_id;
                self.task_action_button(
                    format!("board-attention-{session_id}"),
                    format!("{} · {}", session.reason, session.title),
                    false,
                    true,
                    move |this, _, cx| this.select_session(session_id, cx),
                    cx,
                )
                .w_full()
                .min_w_0()
                .h(px(28.0))
                .px(px(6.0))
                .justify_start()
                .text_size(sp(11.0))
                .text_color(theme.warning)
                .truncate()
                .into_any_element()
            })
            .collect::<Vec<_>>();
        let more_attention = if attention_sessions.len() > 2 {
            let handle = self.menu_handle(format!("board-attention-more-{task_id}"), cx);
            let more_count = attention_sessions.len() - 2;
            let weak = cx.entity().downgrade();
            Some(dropdown_menu_list(
                MenuChip::new(SharedString::from(format!(
                    "board-attention-more-{task_id}"
                )))
                .label(tr!("board.more_attention", count = more_count))
                .height(px(28.0))
                .selected(handle.is_open()),
                SharedString::from(format!("board-attention-more-menu-{task_id}")),
                &handle,
                MenuAlign::BelowLeft,
                move |cx| {
                    let Some(entity) = weak.upgrade() else {
                        return Vec::new();
                    };
                    let stats = entity.read(cx).task_stats();
                    stats
                        .attention_sessions
                        .get(&task_id)
                        .into_iter()
                        .flatten()
                        .skip(2)
                        .map(|session| {
                            let weak = weak.clone();
                            let id = session.session_id;
                            let label = format!("{} · {}", session.reason, session.title);
                            MenuItem::new(label, move |_, cx| {
                                let _ = weak.update(cx, |this, cx| this.select_session(id, cx));
                            })
                            .semantic_id(format!("board-attention-menu-session-{id}"))
                        })
                        .collect()
                },
            ))
        } else {
            None
        };

        let card = div()
            .id(SharedString::from(format!("task-card-{task_id}")))
            .semantic_control(gpui::Role::Button, renaming)
            .aria_label(task.title.clone())
            .w_full()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .px(px(10.0))
            .py(px(8.0))
            .rounded(px(9.0))
            .bg(theme.raised)
            .border_1()
            .border_color(if attention > 0 {
                theme.warning.opacity(0.45)
            } else {
                theme.border
            })
            .cursor_default()
            .hover(|element| element.border_color(theme.border_strong))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_color(if task.state.is_done() {
                        theme.text_secondary
                    } else {
                        theme.text
                    })
                    .child(title),
            )
            .when(!task.description.trim().is_empty(), |element| {
                element.child(
                    div()
                        .text_size(sp(11.5))
                        .text_color(theme.text_tertiary)
                        .whitespace_normal()
                        .line_clamp(2)
                        .text_overflow(gpui::TextOverflow::Truncate("…".into()))
                        .child(task.description.clone()),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(sp(11.5))
                    .text_color(theme.text_tertiary)
                    .when_some(workspace, |element, (icon_path, label)| {
                        element
                            .child(icon(icon_path, 11.0, theme.text_tertiary))
                            .child(div().min_w_0().truncate().child(label))
                            .child(div().text_color(theme.text_ghost).child("·"))
                    })
                    .child(conversations_label(conversations)),
            )
            .when(running > 0 || attention > 0, |element| {
                element.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(10.0))
                        .text_size(sp(11.0))
                        .when(running > 0, |row| {
                            row.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.0))
                                    .text_color(theme.accent)
                                    .child(motion::spin_slow(icon(
                                        "icons/loader-circle.svg",
                                        11.0,
                                        theme.accent,
                                    )))
                                    .child(tr!("board.running_sessions", count = running)),
                            )
                        })
                        .when(attention > 0, |row| {
                            row.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.0))
                                    .text_color(theme.warning)
                                    .child(icon("icons/alert.svg", 11.0, theme.warning))
                                    .child(tr!("board.attention_count", count = attention)),
                            )
                        }),
                )
            })
            .children(attention_rows)
            .when_some(more_attention, |element, menu| element.child(menu))
            .when_some(stats.progress.get(&task_id), |element, (_, preview)| {
                element.child(
                    div()
                        .text_size(sp(11.0))
                        .text_color(theme.text_secondary)
                        .whitespace_normal()
                        .line_clamp(2)
                        .child(tr!("board.latest_progress", progress = preview)),
                )
            })
            .when(!renaming, |element| {
                element
                    .track_focus(&row_focus)
                    .tab_index(0)
                    .tab_stop(true)
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        let key = event.keystroke.key.as_str();
                        if !event.keystroke.modifiers.modified() && matches!(key, "enter" | "space")
                        {
                            this.open_task_from_card(task_id, cx);
                            cx.stop_propagation();
                        } else if key == "f10" && event.keystroke.modifiers.shift {
                            keyboard_menu.open_context_menu(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_task_from_card(task_id, cx);
                    }))
                    .on_drag(dragging, move |drag, _, _, cx| {
                        cx.new(|_| TaskCardDragPreview {
                            title: drag.title.clone(),
                        })
                    })
            });

        let card: AnyElement = if renaming {
            div()
                .w_full()
                .child(card)
                .on_mouse_down_out(cx.listener(move |this, _, _, cx| {
                    if this.task_rename == Some(task_id) {
                        this.commit_task_rename(cx);
                    }
                }))
                .into_any_element()
        } else {
            context_menu(
                card,
                SharedString::from(format!("task-card-menu-{task_id}")),
                &menu,
                move |_| {
                    let open_weak = open_weak.clone();
                    let rename_weak = rename_weak.clone();
                    let done_weak = done_weak.clone();
                    let backlog_weak = backlog_weak.clone();
                    let doing_weak = doing_weak.clone();
                    let mut items = Vec::new();
                    {
                        items.push(MenuItem::new(tr!("board.open_task"), move |_, cx| {
                            let _ = open_weak
                                .update(cx, |this, cx| this.open_task_from_card(task_id, cx));
                        }));
                        items.push(MenuItem::Separator);
                    }
                    items.push(MenuItem::new(tr!("common.rename"), move |window, cx| {
                        let _ = rename_weak.update(cx, |this, cx| {
                            this.begin_task_rename(task_id, window, cx);
                        });
                    }));
                    items.push(MenuItem::Separator);
                    if task.state != TaskState::Backlog {
                        items.push(MenuItem::new(
                            tr!("board.move_backlog"),
                            move |window, cx| {
                                let _ = backlog_weak.update(cx, |this, cx| {
                                    this.move_task(task_id, TaskState::Backlog, window, cx);
                                });
                            },
                        ));
                    }
                    if task.state != TaskState::Doing && !task.state.is_done() {
                        let move_weak = doing_weak.clone();
                        items.push(MenuItem::new(tr!("board.move_doing"), move |window, cx| {
                            let _ = move_weak.update(cx, |this, cx| {
                                this.move_task(task_id, TaskState::Doing, window, cx);
                            });
                        }));
                    }
                    if !task.state.is_done() {
                        items.push(MenuItem::new(tr!("board.complete"), move |window, cx| {
                            let _ = done_weak.update(cx, |this, cx| {
                                this.move_task(task_id, TaskState::Done, window, cx);
                            });
                        }));
                    } else {
                        let reopen_weak = doing_weak.clone();
                        items.push(MenuItem::new(tr!("board.reopen"), move |window, cx| {
                            let _ = reopen_weak.update(cx, |this, cx| {
                                this.move_task(task_id, TaskState::Doing, window, cx);
                            });
                        }));
                    }
                    items
                },
            )
            .into_any_element()
        };
        card
    }

    /// Every card opens its issue, including cards with no conversations yet.
    pub(super) fn open_task_from_card(&mut self, task_id: Uuid, cx: &mut Context<Self>) {
        if !self.state.tasks.iter().any(|task| task.id == task_id) {
            return;
        }
        self.capture_and_save_current_composer_draft(cx);
        self.settings_page = None;
        self.board_open = true;
        if self.selected_task != Some(task_id) {
            self.task_detail = None;
        }
        self.selected_task = Some(task_id);
        let sessions = self
            .state
            .sessions
            .iter()
            .filter(|session| session.task_id == Some(task_id))
            .map(|session| (session.id, session.has_started()))
            .collect::<Vec<_>>();
        for (id, started) in sessions {
            self.ensure_session_loaded(id, cx);
            if started {
                self.start_runtime_attachment(id, cx);
            }
        }
        cx.notify();
    }

    // ── Task creation ─────────────────────────────────────────────────────

    /// New issues use the same editor as existing issues. Keep the draft out
    /// of the persisted catalog until the user explicitly creates it.
    pub(super) fn open_new_task(
        &mut self,
        state: TaskState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.capture_and_save_current_composer_draft(cx);
        if self.new_task_draft.is_none() {
            let mut task = Task::new("", state, 0);
            task.workspace = self
                .board_project_filter
                .or_else(|| {
                    self.selected_project()
                        .filter(|project| !project.is_projectless())
                        .map(|project| project.id)
                })
                .map(|project_id| TaskWorkspace::NewWorktree { project_id });
            self.new_task_draft = Some(task);
        }
        let task = self
            .new_task_draft
            .as_ref()
            .expect("draft initialized")
            .clone();
        self.settings_page = None;
        self.board_open = true;
        self.selected_task = Some(task.id);
        self.ensure_task_detail(&task, window, cx);
        cx.notify();
    }

    pub(super) fn task_for_editor(&self, task_id: Uuid) -> Option<&Task> {
        self.new_task_draft
            .as_ref()
            .filter(|task| task.id == task_id)
            .or_else(|| self.state.tasks.iter().find(|task| task.id == task_id))
    }

    pub(super) fn is_task_draft(&self, task_id: Uuid) -> bool {
        self.new_task_draft
            .as_ref()
            .is_some_and(|task| task.id == task_id)
    }

    pub(super) fn commit_new_task(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.new_task_draft.as_ref() else {
            return;
        };
        let mut task = draft.clone();
        if let Some((title, description)) = self.task_detail_values(task.id, cx) {
            task.title = title;
            task.description = description;
        }
        let Some(mut task) = ready_task_draft(task) else {
            return;
        };
        task.position = self.next_task_position(task.state);
        let task_id = task.id;
        self.state.tasks.push(task);
        self.new_task_draft = None;
        self.reveal_new_task();
        self.sidebar_rows_fingerprint.set(None);
        self.save_task_changes(cx);
        self.open_task_from_card(task_id, cx);
    }

    pub(super) fn cancel_new_task(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.new_task_draft = None;
        self.show_task_board(window, cx);
    }

    /// A declared task should be visible wherever the user is looking: the
    /// task view when it can be, by opening the board otherwise.
    fn reveal_new_task(&mut self) {
        if !self.state.sidebar_grouping_chosen {
            self.state.sidebar_grouping = SidebarGrouping::Task;
        } else if self.state.sidebar_grouping != SidebarGrouping::Task {
            self.board_open = true;
        }
    }

    /// Focus that exists while the board is open: the sidebar's board row, or
    /// the board header's New issue control when the sidebar is hidden. Focusing the row
    /// while it is off screen would leave the keyboard nowhere visible.
    pub(super) fn board_dialog_focus(&self, cx: &mut Context<Self>) -> FocusHandle {
        if self.sidebar_visible {
            return self.board_focus.clone();
        }
        let control = if self.selected_task.is_some() {
            "board-detail-back"
        } else {
            "board-new-issue"
        };
        self.transcript_control_focuses
            .borrow_mut()
            .entry(control.to_owned())
            .or_insert_with(|| cx.focus_handle().tab_stop(true))
            .clone()
    }

    /// The shared "+" control. Each mount site passes its own id, which keys a
    /// focus handle: several elements sharing one handle would all look
    /// focused while only the last-prepped one receives keys.
    pub(super) fn render_task_create_button(
        &self,
        id: &'static str,
        state: TaskState,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let focus = self
            .task_create_focuses
            .borrow_mut()
            .entry(id)
            .or_insert_with(|| cx.focus_handle().tab_stop(true))
            .clone();
        div()
            .id(id)
            .semantic_control(gpui::Role::Button, false)
            .aria_label(tr!("board.new_task"))
            .track_focus(&focus)
            .tab_index(0)
            .tab_stop(true)
            .w(px(20.0))
            .h(px(20.0))
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .hover(|element| element.bg(theme.overlay))
            .active(|element| element.bg(theme.overlay_strong))
            .tooltip(Tooltip::text(tr!("board.new_task")))
            .child(icon("icons/plus.svg", 13.0, theme.text_secondary))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.open_new_task(state, window, cx);
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.open_new_task(state, window, cx);
                    cx.stop_propagation();
                }
            }))
    }

    // ── Rename ────────────────────────────────────────────────────────────

    pub(super) fn begin_task_rename(
        &mut self,
        task_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(title) = self
            .state
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .map(|task| task.title.clone())
        else {
            return;
        };
        self.task_rename = Some(task_id);
        self.task_rename_input.update(cx, |input, cx| {
            input.set_content(title, cx);
            input.select_all_text(cx);
        });
        let focus = self.task_rename_input.read(cx).focus();
        window.on_next_frame(move |window, cx| window.focus(&focus, cx));
        cx.notify();
    }

    pub(super) fn commit_task_rename(&mut self, cx: &mut Context<Self>) {
        let Some(task_id) = self.task_rename.take() else {
            return;
        };
        let title = self.task_rename_input.read(cx).content().trim().to_owned();
        let changed = !title.is_empty()
            && self
                .state
                .tasks
                .iter()
                .find(|task| task.id == task_id)
                .is_some_and(|task| task.title != title);
        if changed && let Some(task) = self.state.tasks.iter_mut().find(|task| task.id == task_id) {
            task.title = title;
            task.touch();
            self.save_task_changes(cx);
        }
        cx.notify();
    }

    pub(super) fn cancel_task_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.task_rename.take().is_none() {
            return;
        }
        let focus = if self.board_open {
            self.board_dialog_focus(cx)
        } else {
            self.composer_focus(cx)
        };
        window.focus(&focus, cx);
        cx.notify();
    }

    // ── Filing conversations under tasks ──────────────────────────────────

    pub(super) fn bind_session_to_task(
        &mut self,
        session_id: Uuid,
        task_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        let Some(task) = self.state.tasks.iter().find(|task| task.id == task_id) else {
            return;
        };
        let Some(session) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        else {
            return;
        };
        if task_details::task_project_id(task)
            .is_some_and(|project_id| project_id != session.project_id)
        {
            self.show_toast(tr!("board.session_project_mismatch"));
            cx.notify();
            return;
        }
        let old_draft_key = crate::persistence::ComposerDraftKey::for_session(session);
        if self.state.selected_session == Some(session_id) {
            self.capture_and_save_current_composer_draft(cx);
        }
        let workspace = {
            let Some(session) = self.state.session_mut(session_id) else {
                return;
            };
            if session.task_id == Some(task_id) {
                return;
            }
            session.task_id = Some(task_id);
            session.updated_at = session
                .updated_at
                .max(crate::model::unix_time())
                .saturating_add(1);
            task_workspace_from_session(session)
        };
        if let Some(task) = self.state.tasks.iter_mut().find(|task| task.id == task_id)
            && task.workspace.is_none()
        {
            task.workspace = Some(workspace);
            // The inferred workspace is a mutation like any other; without the
            // stamp a reload's merge would keep the older workspace-less copy.
            task.touch();
        }
        if let Some(session) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        {
            let key = crate::persistence::ComposerDraftKey::for_session(session);
            if self.composer_drafts.move_to_empty(old_draft_key, key) {
                self.schedule_composer_draft_save(cx);
            }
        }
        self.ensure_session_loaded(session_id, cx);
        self.sidebar_rows_fingerprint.set(None);
        self.save_task_changes(cx);
        // Filing new work under a finished task is continuing it.
        if self.reopen_task_for_continued_work(task_id, cx) {
            self.show_success_toast(tr!("board.reopened_toast"));
        }
        cx.notify();
    }

    pub(super) fn unbind_session_from_task(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        if self
            .state
            .sessions
            .iter()
            .any(|session| session.id == session_id && !session.has_started())
        {
            self.show_toast(tr!("board.detach_draft_hint"));
            cx.notify();
            return;
        }
        let Some(session) = self.state.session_mut(session_id) else {
            return;
        };
        if session.task_id.take().is_none() {
            return;
        }
        session.updated_at = session
            .updated_at
            .max(crate::model::unix_time())
            .saturating_add(1);
        self.sidebar_rows_fingerprint.set(None);
        self.save_task_changes(cx);
        cx.notify();
    }

    /// One action turns a conversation into the task it belongs to.
    pub(super) fn create_task_from_session(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        let Some(session) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        else {
            return;
        };
        let title = session_task_title(session);
        let position = self.next_task_position(TaskState::Doing);
        let task = Task::new(title, TaskState::Doing, position);
        let task_id = task.id;
        self.state.tasks.push(task);
        self.reveal_new_task();
        self.bind_session_to_task(session_id, task_id, cx);
        self.open_task_from_card(task_id, cx);
    }

    /// Restoring from the archive puts the task back on the board; its
    /// conversations were only hidden behind the Done state, so they return
    /// to the sidebar with it.
    pub(super) fn restore_archived_task(
        &mut self,
        task_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_task(task_id, TaskState::Doing, window, cx);
        // The restored row leaves the list; keep focus on the archive surface
        // so the keyboard does not sit on a detached row.
        let focus = self.archive_focus.clone();
        window.focus(&focus, cx);
    }

    /// Completed issues remain inspectable even if they never had a session.
    pub(super) fn open_archived_task(&mut self, task_id: Uuid, cx: &mut Context<Self>) {
        self.settings_page = None;
        self.open_task_from_card(task_id, cx);
    }

    /// A conversation that keeps going reopens its task: work and task state
    /// stay in step in both directions. The task returns to the top of the
    /// Doing column. Returns whether the task was actually reopened.
    pub(super) fn reopen_task_for_continued_work(
        &mut self,
        task_id: Uuid,
        cx: &mut Context<Self>,
    ) -> bool {
        // An explicit continuation supersedes a pending archive request.
        self.task_completion_stops.remove(&task_id);
        self.task_completion_save_rollbacks.remove(&task_id);
        self.task_completion_preflights.remove(&task_id);
        if self
            .state
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .is_none_or(|task| !task.state.is_done())
        {
            return false;
        }
        let position = self.next_task_position(TaskState::Doing);
        let Some(task) = self.state.tasks.iter_mut().find(|task| task.id == task_id) else {
            return false;
        };
        task.set_state(TaskState::Doing);
        task.position = position;
        self.sidebar_rows_fingerprint.set(None);
        self.save_task_changes(cx);
        cx.notify();
        true
    }

    // ── Overlays ──────────────────────────────────────────────────────────

    fn scrim(theme: &Theme) -> Hsla {
        if theme.is_dark {
            gpui::hsla(0.0, 0.0, 0.0, 0.34)
        } else {
            gpui::hsla(0.0, 0.0, 0.0, 0.16)
        }
    }

    pub(super) fn render_task_completion_confirm(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let pending = self.pending_task_completion.as_ref()?;
        let theme = Theme::current(cx);
        let task_id = pending.task_id;
        let title = self
            .state
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .map(|task| task.title.clone())
            .unwrap_or_default();
        let affected = self.task_completion_stop_count(task_id);
        let confirm_focus = pending.confirm_focus.clone();
        let cancel_focus = pending.cancel_focus.clone();

        let cancel = div()
            .id("task-complete-cancel")
            .semantic_control(gpui::Role::Button, false)
            .aria_label(tr!("common.cancel"))
            .track_focus(&cancel_focus)
            .tab_index(0)
            .tab_stop(true)
            .h(px(30.0))
            .px(px(12.0))
            .rounded(px(7.0))
            .flex()
            .items_center()
            .cursor_default()
            .border_1()
            .border_color(theme.border_strong)
            .text_size(sp(13.0))
            .text_color(theme.text_secondary)
            .focus_visible(|style| style.border_color(theme.accent))
            .hover(|element| element.bg(theme.overlay))
            .child(tr!("common.cancel"))
            .on_click(cx.listener(|this, _, window, cx| {
                this.cancel_task_completion(window, cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if !event.keystroke.modifiers.modified()
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                {
                    this.cancel_task_completion(window, cx);
                    cx.stop_propagation();
                }
            }));

        let confirm = div()
            .id("task-complete-confirm")
            .semantic_control(gpui::Role::Button, false)
            .aria_label(tr!("board.complete_and_stop"))
            .track_focus(&confirm_focus)
            .tab_index(0)
            .tab_stop(true)
            .h(px(30.0))
            .px(px(12.0))
            .rounded(px(7.0))
            .flex()
            .items_center()
            .cursor_default()
            .bg(theme.inverse)
            .text_size(sp(13.0))
            .text_color(theme.on_inverse)
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .child(tr!("board.complete_and_stop"))
            .on_click(cx.listener(|this, _, window, cx| {
                this.confirm_task_completion(window, cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if !event.keystroke.modifiers.modified()
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                {
                    this.confirm_task_completion(window, cx);
                    cx.stop_propagation();
                }
            }));

        let card = div()
            .id("task-complete-card")
            .w_full()
            .max_w(px(440.0))
            .rounded(px(14.0))
            .bg(theme.composer)
            .shadow_xl()
            .overflow_hidden()
            .flex()
            .flex_col()
            .key_context(TASK_COMPLETION_DIALOG_CONTEXT)
            .on_action(cx.listener(|this, _: &DismissTaskCompletion, window, cx| {
                this.cancel_task_completion(window, cx);
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .h(px(48.0))
                    .px(px(16.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .text_size(sp(14.0))
                    .text_color(theme.text)
                    .child(icon("icons/alert.svg", 15.0, theme.warning))
                    .child(tr!("board.complete_confirm_title", title = title)),
            )
            .child(
                div()
                    .px(px(16.0))
                    .pb(px(10.0))
                    .text_size(sp(13.0))
                    .line_height(sp(20.0))
                    .text_color(theme.text_secondary)
                    .child(tr!("board.complete_confirm_body", count = affected)),
            )
            .child(
                div()
                    .px(px(16.0))
                    .pb(px(16.0))
                    .text_size(sp(12.0))
                    .text_color(theme.text_tertiary)
                    .child(tr!("board.archive_not_delete")),
            )
            .child(div().mx(px(8.0)).h(px(1.0)).bg(theme.border))
            .child(
                div()
                    .p(px(8.0))
                    .flex()
                    .justify_end()
                    .gap(px(2.0))
                    .child(cancel)
                    .child(confirm),
            )
            .with_animation(
                SharedString::from("task-complete-dialog-enter"),
                gpui::Animation::new(std::time::Duration::from_millis(150))
                    .with_easing(ease_out_quint()),
                |element, delta| element.opacity(delta),
            );
        let layer = div()
            .id("task-complete-layer")
            .absolute()
            .inset_0()
            .occlude()
            .bg(Self::scrim(&theme))
            .p(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.cancel_task_completion(window, cx);
                }),
            )
            .child(card);
        // Below the unsaved-edits confirmation (priority 7): ⌘W must still
        // reach that dialog while a completion confirmation is open.
        Some(gpui::deferred(layer).with_priority(6).into_any_element())
    }
}

/// Hand focus to a control that lives on GPUI's deferred overlay plane.
///
/// The overlay's subtree joins the dispatch tree a frame after it first
/// renders, so a handle focused in the opening frame receives nothing; wait
/// two frames, the same way the command palette hands focus to its search
/// field. `still_open` re-checks the dialog because the user can close it
/// inside that window.
pub(super) fn focus_overlay_control(
    window: &mut Window,
    cx: &mut Context<Waku>,
    focus: FocusHandle,
    still_open: impl Fn(&Waku) -> bool + 'static,
) {
    let weak = cx.entity().downgrade();
    window.on_next_frame(move |window, _| {
        window.on_next_frame(move |window, cx| {
            let open = weak.update(cx, |this, _| still_open(this)).unwrap_or(false);
            if open {
                window.focus(&focus, cx);
            }
        });
    });
}

/// Workspace metadata is carried by list projections as well as loaded history.
fn task_workspace_from_session(session: &AgentSession) -> TaskWorkspace {
    match &session.workspace {
        SessionWorkspace::Worktree { path, branch } => TaskWorkspace::Worktree {
            project_id: session.project_id,
            path: path.clone(),
            branch: branch.clone(),
        },
        SessionWorkspace::Local => TaskWorkspace::Local {
            project_id: session.project_id,
        },
        SessionWorkspace::NewWorktree { .. } => TaskWorkspace::NewWorktree {
            project_id: session.project_id,
        },
    }
}

/// Count of conversations, pluralised the way the rest of the app does it.
pub(super) fn conversations_label(count: usize) -> String {
    if count == 1 {
        tr!("board.conversations_one", count = count)
    } else {
        tr!("board.conversations_many", count = count)
    }
}

/// The title a task born from a session should carry.
fn session_task_title(session: &AgentSession) -> String {
    let title = session.title.trim();
    if !title.is_empty() && title != AgentSession::DEFAULT_TITLE {
        return title.to_owned();
    }
    session
        .auto_title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| tr!("board.untitled_task").to_string())
}

#[cfg(test)]
mod filter_tests {
    use super::*;
    use crate::ui::SemanticControlExt;

    #[test]
    fn creating_a_task_preserves_the_whole_brief_and_workspace() {
        let mut draft = Task::new("  Fix checkout  ", TaskState::Doing, 0);
        draft.description = "Keep existing carts intact\nVerify guest checkout".into();
        draft.workspace = Some(TaskWorkspace::NewWorktree {
            project_id: Uuid::new_v4(),
        });
        let expected = draft.clone();
        let created = ready_task_draft(draft).expect("valid title");
        assert_eq!(created.id, expected.id);
        assert_eq!(created.title, "Fix checkout");
        assert_eq!(created.description, expected.description);
        assert_eq!(created.workspace, expected.workspace);
        assert_eq!(created.state, TaskState::Doing);
    }

    #[test]
    fn blank_task_drafts_cannot_be_created() {
        for title in ["", "   ", "\n\t", "\u{3000}"] {
            let mut draft = Task::new(title, TaskState::Backlog, 0);
            draft.description = "A description is not a substitute for a title".into();
            assert!(ready_task_draft(draft).is_none());
        }
    }

    #[test]
    fn all_attention_sessions_remain_visible_alongside_running_work() {
        let task_id = Uuid::new_v4();
        let mut waiting = AgentSession::new(Uuid::new_v4(), ProviderKind::Pi);
        waiting.task_id = Some(task_id);
        waiting.status = SessionStatus::Waiting;
        let mut running = waiting.clone();
        running.id = Uuid::new_v4();
        running.status = SessionStatus::Working;
        let mut failed = waiting.clone();
        failed.id = Uuid::new_v4();
        failed.status = SessionStatus::Failed;
        let mut stats = TaskStats::default();
        stats.record_session(&waiting, true, true, Some("Awaiting permission".into()));
        stats.record_session(&running, true, false, None);
        stats.record_session(&failed, true, false, Some("Failed".into()));
        assert_eq!(stats.conversations[&task_id], 3);
        assert_eq!(stats.running[&task_id], 2);
        assert_eq!(stats.attention[&task_id], 2);
        assert_eq!(
            stats.attention_sessions[&task_id]
                .iter()
                .map(|item| item.session_id)
                .collect::<Vec<_>>(),
            vec![waiting.id, failed.id]
        );
        assert_eq!(waiting.status, SessionStatus::Waiting);
    }

    #[test]
    fn latest_preview_uses_visible_tail_and_never_claims_idle_is_done() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Pi);
        session
            .messages
            .push(Message::new(MessageRole::Assistant, "Tests are running."));
        assert_eq!(
            session_progress_preview(&session).as_deref(),
            Some("Tests are running.")
        );
        session.transcript_blocks.push(TranscriptBlock {
            after_message: 1,
            turn_id: None,
            activities: vec![ActivityItem::new(
                None,
                ActivityKind::Reasoning,
                "private reasoning",
                None,
                false,
            )],
        });
        assert_eq!(
            session_progress_preview(&session).as_deref(),
            Some("Tests are running.")
        );
        session.detail_loaded = false;
        assert_eq!(session_progress_preview(&session), None);
        assert_eq!(board_preview(&"字".repeat(400)).chars().count(), 160);
    }

    #[test]
    fn activity_filters_describe_live_work_without_moving_workflow_columns() {
        let task = Task::new("Follow through", TaskState::Doing, 0);
        let mut stats = TaskStats::default();
        assert!(board_activity_matches(
            &task,
            BoardActivityFilter::All,
            &stats
        ));
        assert!(!board_activity_matches(
            &task,
            BoardActivityFilter::Running,
            &stats
        ));
        stats.busy.insert(task.id);
        stats.attention.insert(task.id, 1);
        assert!(board_activity_matches(
            &task,
            BoardActivityFilter::Running,
            &stats
        ));
        assert!(board_activity_matches(
            &task,
            BoardActivityFilter::Attention,
            &stats
        ));
        assert_eq!(task.state, TaskState::Doing);
        let mut done = task;
        done.set_state(TaskState::Done);
        assert!(!board_activity_matches(
            &done,
            BoardActivityFilter::Running,
            &stats
        ));
        assert!(!board_activity_matches(
            &done,
            BoardActivityFilter::Attention,
            &stats
        ));
    }
}
