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

actions!(waku_board, [DismissNewTaskDialog, DismissTaskCompletion]);

const NEW_TASK_DIALOG_CONTEXT: &str = "NewTaskDialog";
const TASK_COMPLETION_DIALOG_CONTEXT: &str = "TaskCompletionDialog";

/// Escape closes each board dialog through a key binding scoped to it — the
/// mechanism the commit and goal dialogs use. (A raw key listener on the
/// deferred overlay would depend on the binding path not consuming the key.)
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(
            "escape",
            DismissNewTaskDialog,
            Some(NEW_TASK_DIALOG_CONTEXT),
        ),
        KeyBinding::new(
            "escape",
            DismissTaskCompletion,
            Some(TASK_COMPLETION_DIALOG_CONTEXT),
        ),
    ]);
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

/// The task-creation dialog. It exists between the trigger and Enter; the
/// task itself is created by [`Waku::commit_new_task`].
pub(super) struct NewTaskDialog {
    pub input: Entity<TextInput>,
    pub state: TaskState,
}

/// A move to Done that must be confirmed because conversations are running.
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

fn task_state_label(state: TaskState) -> String {
    match state {
        TaskState::Backlog => tr!("board.backlog"),
        TaskState::Doing => tr!("board.doing"),
        TaskState::Done => tr!("board.done"),
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
            *stats.conversations.entry(task_id).or_default() += 1;
            match session.status {
                // A failed turn needs the user as much as a question does.
                SessionStatus::Waiting | SessionStatus::Failed => {
                    *stats.attention.entry(task_id).or_default() += 1;
                    if active.contains(&task_id) {
                        stats.attention_total += 1;
                    }
                }
                status if status.is_busy() => {
                    stats.busy.insert(task_id);
                }
                _ => {}
            }
        }
        let stats = Rc::new(stats);
        *self.task_stats_cache.borrow_mut() = Some(Rc::clone(&stats));
        stats
    }

    /// One-shot action path: a click may arrive between frames, so it reads
    /// the live sessions rather than the frame's memoized stats.
    fn task_is_busy(&self, task_id: Uuid) -> bool {
        self.state
            .sessions
            .iter()
            .any(|session| session.task_id == Some(task_id) && session.status.is_busy())
    }

    /// Task ids in one column, in board order.
    fn tasks_in_state(&self, state: TaskState) -> Vec<Uuid> {
        let mut tasks = self
            .state
            .tasks
            .iter()
            .filter(|task| task.state == state)
            .collect::<Vec<_>>();
        tasks.sort_by(|a, b| {
            a.position
                .cmp(&b.position)
                .then_with(|| b.updated_at.cmp(&a.updated_at))
        });
        tasks.into_iter().map(|task| task.id).collect()
    }

    /// A position that sorts above everything else in the column.
    fn next_task_position(&self, state: TaskState) -> i64 {
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
        self.board_open = !self.board_open;
        cx.notify();
    }

    pub(super) fn render_board(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let stats = self.task_stats();
        let attention = stats.attention_total;
        div()
            .id("task-board")
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.surface)
            .child(
                div()
                    .h(px(48.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(16.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_size(sp(15.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(tr!("board.title")),
                    )
                    .when(attention > 0, |element| {
                        element.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(4.0))
                                .px(px(7.0))
                                .h(px(18.0))
                                .rounded(px(9.0))
                                .bg(theme.warning.opacity(0.14))
                                .text_size(sp(11.0))
                                .text_color(theme.warning)
                                .child(icon("icons/alert.svg", 11.0, theme.warning))
                                .child(tr!("board.needs_you", count = attention)),
                        )
                    })
                    .child(div().flex_1())
                    .child(self.render_task_create_button(
                        "board-new-task",
                        TaskState::Backlog,
                        cx,
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
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
            .min_w_0()
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
        let busy = stats.busy.contains(&task_id);
        let conversations = stats.conversations.get(&task_id).copied().unwrap_or(0);
        // A bound workspace shows where the task works: the worktree branch,
        // or the project a local task works in.
        let workspace = match task.workspace.as_ref() {
            Some(TaskWorkspace::Worktree { branch, .. }) => {
                Some(("icons/git-branch.svg", branch.clone()))
            }
            Some(TaskWorkspace::Local { project_id }) => self
                .state
                .projects
                .iter()
                .find(|project| project.id == *project_id)
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

        let status = if attention > 0 {
            Some((
                "icons/alert.svg",
                tr!("board.needs_you_short"),
                theme.warning,
                false,
            ))
        } else if busy {
            Some((
                "icons/loader-circle.svg",
                tr!("board.running"),
                theme.accent,
                true,
            ))
        } else {
            None
        };

        let card = div()
            .id(SharedString::from(format!("task-card-{task_id}")))
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
            .when_some(status, |element, (icon_path, label, tint, spin)| {
                let glyph = icon(icon_path, 11.0, tint);
                element.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .text_size(sp(11.0))
                        .text_color(tint)
                        .child(if spin {
                            motion::spin_slow(glyph).into_any_element()
                        } else {
                            glyph.into_any_element()
                        })
                        .child(label),
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
                    if conversations > 0 {
                        items.push(MenuItem::new(tr!("board.open_latest"), move |_, cx| {
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

    /// Clicking a card resumes the task's most recent conversation; a task
    /// with no conversations only takes focus.
    pub(super) fn open_task_from_card(&mut self, task_id: Uuid, cx: &mut Context<Self>) {
        let newest = self
            .state
            .sessions
            .iter()
            .filter(|session| session.task_id == Some(task_id) && session.has_started())
            .max_by_key(|session| session.last_reply_at.unwrap_or(session.updated_at))
            .map(|session| session.id);
        if let Some(session_id) = newest {
            self.select_session(session_id, cx);
        }
    }

    // ── Moves and completion ──────────────────────────────────────────────

    /// The single entry point for column changes. Done routes through the
    /// running-session confirmation.
    pub(super) fn move_task(
        &mut self,
        task_id: Uuid,
        state: TaskState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self
            .state
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .map(|task| task.state)
        else {
            return;
        };
        if current == state {
            return;
        }
        if state.is_done() {
            self.request_task_completion(task_id, window, cx);
            return;
        }
        let position = self.next_task_position(state);
        let Some(task) = self.state.tasks.iter_mut().find(|task| task.id == task_id) else {
            return;
        };
        task.set_state(state);
        task.position = position;
        // The row list filters by task state (archived conversations leave
        // every view), so a move — including a restore out of Done — has to
        // rebuild it.
        self.sidebar_rows_fingerprint.set(None);
        self.save(cx);
        cx.notify();
    }

    /// A running conversation that is still preparing its worktree or
    /// checkpoint cannot be interrupted, so completion has to wait instead of
    /// archiving work that is about to start.
    fn task_has_unstoppable_work(&self, task_id: Uuid) -> bool {
        self.state.sessions.iter().any(|session| {
            session.task_id == Some(task_id)
                && session.status.is_busy()
                && self.submission_preparations.contains(&session.id)
        })
    }

    fn request_task_completion(
        &mut self,
        task_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.task_has_unstoppable_work(task_id) {
            self.show_toast(tr!("board.completion_waiting_for_setup"));
            cx.notify();
            return;
        }
        if self.task_is_busy(task_id) {
            let confirm_focus = cx.focus_handle();
            let cancel_focus = cx.focus_handle();
            self.pending_task_completion = Some(TaskCompletionConfirm {
                task_id,
                confirm_focus,
                cancel_focus: cancel_focus.clone(),
            });
            // A destructive confirmation opens on Cancel: one stray Return
            // must not stop running turns and archive them.
            focus_overlay_control(window, cx, cancel_focus, move |this| {
                this.pending_task_completion
                    .as_ref()
                    .is_some_and(|pending| pending.task_id == task_id)
            });
            cx.notify();
            return;
        }
        self.finish_task_completion(task_id, cx);
    }

    fn finish_task_completion(&mut self, task_id: Uuid, cx: &mut Context<Self>) {
        // Re-checked here because the confirmation can sit open while a
        // conversation starts preparing.
        if self.task_has_unstoppable_work(task_id) {
            self.show_toast(tr!("board.completion_waiting_for_setup"));
            cx.notify();
            return;
        }
        // Archive stops work: anything still running goes through the app's
        // full stop sequence before its conversation leaves the sidebar.
        // Archiving never deletes.
        let running = self
            .state
            .sessions
            .iter()
            .filter(|session| session.task_id == Some(task_id) && session.status.is_busy())
            .map(|session| session.id)
            .collect::<Vec<_>>();
        for session_id in running {
            self.stop_session_turn(session_id, cx);
        }
        let Some(task) = self.state.tasks.iter_mut().find(|task| task.id == task_id) else {
            return;
        };
        task.set_state(TaskState::Done);
        self.pending_task_completion = None;
        self.sidebar_rows_fingerprint.set(None);
        self.save(cx);
        self.show_success_toast(tr!("board.completed_toast"));
        cx.notify();
    }

    pub(super) fn confirm_task_completion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_task_completion.take() else {
            return;
        };
        let focus = self.board_dialog_focus(cx);
        window.focus(&focus, cx);
        self.finish_task_completion(pending.task_id, cx);
    }

    pub(super) fn cancel_task_completion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_task_completion.take().is_none() {
            return;
        }
        let focus = self.board_dialog_focus(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    // ── Task creation ─────────────────────────────────────────────────────

    pub(super) fn open_new_task_dialog(
        &mut self,
        state: TaskState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input =
            cx.new(|cx| TextInput::new(window, cx).placeholder(tr!("board.new_task_placeholder")));
        let focus = input.read(cx).focus();
        // Enter arrives through the field's own submit event, the same way the
        // sidebar's inline rename commits.
        cx.subscribe(&input, |this: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Submit(_)) {
                this.commit_new_task(cx);
            }
        })
        .detach();
        self.new_task_dialog = Some(NewTaskDialog { input, state });
        focus_overlay_control(window, cx, focus, |this| this.new_task_dialog.is_some());
        cx.notify();
    }

    pub(super) fn commit_new_task(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.new_task_dialog.take() else {
            return;
        };
        let title = dialog.input.read(cx).content().trim().to_owned();
        if !title.is_empty() {
            let position = self.next_task_position(dialog.state);
            self.state
                .tasks
                .push(Task::new(title, dialog.state, position));
            self.reveal_new_task();
            self.sidebar_rows_fingerprint.set(None);
            self.save(cx);
        }
        cx.notify();
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

    pub(super) fn close_new_task_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.new_task_dialog.take().is_none() {
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

    /// Focus that exists while the board is open: the sidebar's board row, or
    /// the board header's own "+" when the sidebar is hidden. Focusing the row
    /// while it is off screen would leave the keyboard nowhere visible.
    fn board_dialog_focus(&self, cx: &mut Context<Self>) -> FocusHandle {
        if self.sidebar_visible {
            return self.board_focus.clone();
        }
        self.task_create_focuses
            .borrow_mut()
            .entry("board-new-task")
            .or_insert_with(|| cx.focus_handle())
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
            .or_insert_with(|| cx.focus_handle())
            .clone();
        div()
            .id(id)
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
                this.open_new_task_dialog(state, window, cx);
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.open_new_task_dialog(state, window, cx);
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
            self.save(cx);
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
        let workspace = {
            let Some(session) = self.state.session_mut(session_id) else {
                return;
            };
            if session.task_id == Some(task_id) {
                return;
            }
            session.task_id = Some(task_id);
            session.updated_at = crate::model::unix_time();
            session
                .detail_loaded
                .then(|| task_workspace_from_session(session))
                .flatten()
        };
        if let Some(workspace) = workspace
            && let Some(task) = self.state.tasks.iter_mut().find(|task| task.id == task_id)
            && task.workspace.is_none()
        {
            task.workspace = Some(workspace);
            // The inferred workspace is a mutation like any other; without the
            // stamp a reload's merge would keep the older workspace-less copy.
            task.touch();
        }
        self.sidebar_rows_fingerprint.set(None);
        self.save(cx);
        // Filing new work under a finished task is continuing it.
        if self.reopen_task_for_continued_work(task_id, cx) {
            self.show_success_toast(tr!("board.reopened_toast"));
        }
        cx.notify();
    }

    pub(super) fn unbind_session_from_task(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        let Some(session) = self.state.session_mut(session_id) else {
            return;
        };
        if session.task_id.take().is_none() {
            return;
        }
        session.updated_at = crate::model::unix_time();
        self.sidebar_rows_fingerprint.set(None);
        self.save(cx);
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

    /// Opening a task from the archive closes settings and resumes its newest
    /// conversation. A completed task may have none, and there is nowhere to
    /// land then, so Settings stays put.
    pub(super) fn open_archived_task(&mut self, task_id: Uuid, cx: &mut Context<Self>) {
        let has_conversations = self
            .state
            .sessions
            .iter()
            .any(|session| session.task_id == Some(task_id) && session.has_started());
        if !has_conversations {
            return;
        }
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
        self.save(cx);
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

    pub(super) fn render_new_task_dialog(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.new_task_dialog.as_ref()?;
        let theme = Theme::current(cx);
        let state_label = task_state_label(dialog.state);
        let input = dialog.input.clone();
        let card = div()
            .id("new-task-card")
            .w_full()
            .max_w(px(430.0))
            .rounded(px(14.0))
            .bg(theme.composer)
            .shadow_xl()
            .overflow_hidden()
            .flex()
            .flex_col()
            .key_context(NEW_TASK_DIALOG_CONTEXT)
            .on_action(cx.listener(|this, _: &DismissNewTaskDialog, window, cx| {
                this.close_new_task_dialog(window, cx);
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .h(px(44.0))
                    .px(px(14.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(icon("icons/columns.svg", 15.0, theme.text_secondary))
                    .child(
                        div()
                            .text_size(sp(14.0))
                            .text_color(theme.text)
                            .child(tr!("board.new_task")),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .px(px(7.0))
                            .h(px(18.0))
                            .flex()
                            .items_center()
                            .rounded(px(9.0))
                            .bg(theme.overlay_strong)
                            .text_size(sp(11.0))
                            .text_color(theme.text_secondary)
                            .child(state_label),
                    ),
            )
            .child(
                div()
                    .m(px(12.0))
                    .h(px(34.0))
                    .px(px(10.0))
                    .rounded(px(8.0))
                    .bg(theme.inset)
                    .border_1()
                    .border_color(theme.border)
                    .flex()
                    .items_center()
                    .text_size(sp(13.5))
                    .text_color(theme.text)
                    .child(input),
            )
            .child(
                div()
                    .mx(px(12.0))
                    .mb(px(12.0))
                    .text_size(sp(12.0))
                    .text_color(theme.text_tertiary)
                    .child(tr!("board.new_task_hint")),
            )
            .with_animation(
                SharedString::from("new-task-dialog-enter"),
                gpui::Animation::new(std::time::Duration::from_millis(150))
                    .with_easing(ease_out_quint()),
                |element, delta| element.opacity(delta),
            );
        let layer = div()
            .id("new-task-layer")
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
                cx.listener(|this, _, window, cx| this.close_new_task_dialog(window, cx)),
            )
            .child(card);
        Some(gpui::deferred(layer).with_priority(6).into_any_element())
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
        let running = self
            .state
            .sessions
            .iter()
            .filter(|session| session.task_id == Some(task_id) && session.status.is_busy())
            .count();
        let confirm_focus = pending.confirm_focus.clone();
        let cancel_focus = pending.cancel_focus.clone();

        let cancel = div()
            .id("task-complete-cancel")
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
                    .child(tr!("board.complete_confirm_body", count = running)),
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
fn focus_overlay_control(
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

/// The workspace a session runs in, when the session knows it. Skeletons
/// normalize their workspace to `Local`, so they are not consulted.
fn task_workspace_from_session(session: &AgentSession) -> Option<TaskWorkspace> {
    match &session.workspace {
        SessionWorkspace::Worktree { path, branch } => Some(TaskWorkspace::Worktree {
            project_id: session.project_id,
            path: path.clone(),
            branch: branch.clone(),
        }),
        SessionWorkspace::Local => Some(TaskWorkspace::Local {
            project_id: session.project_id,
        }),
        SessionWorkspace::NewWorktree { .. } => None,
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
