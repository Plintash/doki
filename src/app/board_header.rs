//! One compact toolbar for the board and its issue inspector.
//!
//! Filters live on Waku so opening an issue and going back keeps the same
//! project, query, and activity scope. Window chrome stays outside the wrapping
//! controls, preserving native titlebar placement in narrow windows.

use super::*;
use crate::ui::menu::dropdown_menu_list;

impl Waku {
    fn ensure_board_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.board_search.is_some() {
            return;
        }
        let input = cx.new(|cx| {
            TextInput::new(window, cx)
                .semantic_id("board-search-input")
                .tab_stop(true)
                .clear_on_escape()
                .placeholder(tr!("board.search_issues"))
        });
        if !self.board_search_query.is_empty() {
            input.update(cx, |input, cx| {
                input.set_content(self.board_search_query.clone(), cx)
            });
        }
        cx.subscribe(&input, |this, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Edited) {
                this.board_search_query = input.read(cx).content().to_owned();
                cx.notify();
            }
        })
        .detach();
        self.board_search = Some(input);
    }

    fn render_board_project_filter(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let selected = self.board_project_filter;
        let label = selected
            .and_then(|id| self.state.projects.iter().find(|project| project.id == id))
            .map(Project::display_name)
            .unwrap_or_else(|| tr!("board.all_projects"));
        let handle = self.menu_handle("board-project-filter", cx);
        let weak = cx.entity().downgrade();
        dropdown_menu_list(
            MenuChip::new("board-project-filter")
                .icon("icons/folder.svg", theme.text_tertiary)
                .label(label)
                .height(px(30.0))
                .max_w(px(180.0))
                .flex_none()
                .selected(handle.is_open()),
            "board-project-filter-menu",
            &handle,
            MenuAlign::BelowLeft,
            move |cx| {
                let Some(entity) = weak.upgrade() else {
                    return Vec::new();
                };
                let projects = entity
                    .read(cx)
                    .state
                    .projects
                    .iter()
                    .filter(|project| !project.is_projectless())
                    .map(|project| (project.id, project.display_name()))
                    .collect::<Vec<_>>();
                let all = weak.clone();
                let mut items = vec![
                    MenuItem::new(tr!("board.all_projects"), move |_, cx| {
                        let _ = all.update(cx, |this, cx| {
                            this.board_project_filter = None;
                            cx.notify();
                        });
                    })
                    .semantic_id("board-project-all")
                    .selected(selected.is_none()),
                ];
                if !projects.is_empty() {
                    items.push(MenuItem::Separator);
                }
                items.extend(projects.iter().map(|(id, name)| {
                    let weak = weak.clone();
                    let id = *id;
                    MenuItem::new(name.clone(), move |_, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.board_project_filter = Some(id);
                            cx.notify();
                        });
                    })
                    .semantic_id(format!("board-project-{id}"))
                    .selected(selected == Some(id))
                }));
                items
            },
        )
    }

    fn render_board_activity_filter(
        &self,
        id: &'static str,
        label: String,
        filter: board::BoardActivityFilter,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let selected = self.board_activity_filter == filter;
        self.task_action_button(
            id,
            label,
            false,
            true,
            move |this, _, cx| {
                this.board_activity_filter = if this.board_activity_filter == filter {
                    board::BoardActivityFilter::All
                } else {
                    filter
                };
                cx.notify();
            },
            cx,
        )
        .gap(px(5.0))
        .bg(if selected {
            theme.overlay_strong
        } else {
            gpui::transparent_black()
        })
        .border_color(if selected {
            theme.border_strong
        } else {
            gpui::transparent_black()
        })
        .when(selected, |element| {
            element.text_color(theme.text).child(icon(
                "icons/check.svg",
                12.0,
                theme.text_secondary,
            ))
        })
    }

    fn render_board_detail_controls(&self, task: &Task, cx: &mut Context<Self>) -> AnyElement {
        let task_id = task.id;
        let task_state = task.state;
        let completing = self.task_completion_stops.contains_key(&task_id);
        let handle = self.menu_handle("board-detail-state", cx);
        let weak = cx.entity().downgrade();
        let state_picker = dropdown_menu(
            MenuChip::new("board-detail-state")
                .label(board::task_state_label(task_state))
                .height(px(30.0))
                .disabled(completing)
                .selected(handle.is_open()),
            "board-detail-state-menu",
            &handle,
            MenuAlign::BelowRight,
            move |_| {
                [TaskState::Backlog, TaskState::Doing, TaskState::Done]
                    .into_iter()
                    .map(|state| {
                        let weak = weak.clone();
                        MenuItem::new(board::task_state_label(state), move |window, cx| {
                            let _ = weak
                                .update(cx, |this, cx| this.move_task(task_id, state, window, cx));
                        })
                        .semantic_id(format!("board-detail-state-{}", state.key()))
                        .selected(task_state == state)
                        .disabled(completing)
                    })
                    .collect()
            },
        );
        let complete = self.task_action_button(
            "board-detail-complete",
            if completing {
                tr!("board.cancel_completion")
            } else if task_state.is_done() {
                tr!("board.reopen")
            } else {
                tr!("board.complete")
            },
            false,
            true,
            move |this, window, cx| {
                if completing {
                    this.cancel_task_completion_stop(task_id, cx);
                } else {
                    this.move_task(
                        task_id,
                        if task_state.is_done() {
                            TaskState::Doing
                        } else {
                            TaskState::Done
                        },
                        window,
                        cx,
                    );
                }
            },
            cx,
        );
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_end()
            .gap(px(8.0))
            .child(state_picker)
            .child(complete)
            .into_any_element()
    }

    pub(super) fn render_board_header(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let selected_task = self
            .selected_task
            .and_then(|id| self.state.tasks.iter().find(|task| task.id == id))
            .cloned();
        let left_controls = (!self.sidebar_visible)
            .then(|| {
                self.render_client_window_controls(
                    super::window_chrome::WindowControlSide::Left,
                    window,
                    cx,
                )
            })
            .flatten();
        let right_controls = self.render_client_window_controls(
            super::window_chrome::WindowControlSide::Right,
            window,
            cx,
        );
        let show_save_status =
            self.task_save_in_flight || self.task_save_pending || self.task_save_error.is_some();
        let mut controls = div()
            .min_w_0()
            .flex_1()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.0))
            .py(px(9.0));

        if let Some(task) = selected_task {
            controls = controls
                .child(self.task_action_button(
                    "board-detail-back",
                    tr!("board.back_to_board"),
                    false,
                    true,
                    |this, window, cx| this.show_task_board(window, cx),
                    cx,
                ))
                .child(
                    self.window_drag_region(
                        div()
                            .id("board-detail-header-drag-region")
                            .h(px(30.0))
                            .min_w(px(8.0))
                            .flex_1(),
                        cx,
                    ),
                )
                .when(show_save_status, |element| {
                    element.child(self.render_task_save_status(cx))
                })
                .child(self.render_board_detail_controls(&task, cx));
        } else {
            self.ensure_board_search(window, cx);
            let search = self
                .board_search
                .as_ref()
                .expect("board search initialized")
                .clone();
            let stats = self.task_stats();
            let (running, attention) = self
                .state
                .tasks
                .iter()
                .filter(|task| !task.state.is_done() && self.task_matches_board_scope(task))
                .fold((0, 0), |(running, attention), task| {
                    (
                        running + usize::from(stats.busy.contains(&task.id)),
                        attention + usize::from(stats.attention.contains_key(&task.id)),
                    )
                });
            let running_filter = self.render_board_activity_filter(
                "board-running-filter",
                tr!("board.running_count", count = running),
                board::BoardActivityFilter::Running,
                cx,
            );
            let attention_filter = self.render_board_activity_filter(
                "board-attention-filter",
                tr!("board.attention_count", count = attention),
                board::BoardActivityFilter::Attention,
                cx,
            );
            let new_issue = self.task_action_button(
                "board-new-issue",
                tr!("board.new_issue"),
                true,
                true,
                |this, window, cx| this.open_new_task_dialog(TaskState::Backlog, window, cx),
                cx,
            );
            controls = controls
                .child(
                    self.window_drag_region(
                        div()
                            .id("board-title-drag-region")
                            .h(px(30.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .pr(px(4.0))
                            .text_size(sp(13.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(tr!("board.title")),
                        cx,
                    ),
                )
                .child(self.render_board_project_filter(cx))
                .child(
                    TextField::new("board-search", search)
                        .icon("icons/search.svg", 13.0)
                        .w(px(190.0))
                        .min_w(px(140.0))
                        .max_w(px(240.0))
                        .flex_1(),
                )
                .child(
                    self.window_drag_region(
                        div()
                            .id("board-header-drag-region")
                            .h(px(30.0))
                            .min_w(px(8.0))
                            .flex_1(),
                        cx,
                    ),
                )
                .child(
                    div()
                        .min_w_0()
                        .ml_auto()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .justify_end()
                        .gap(px(8.0))
                        .child(running_filter)
                        .child(attention_filter)
                        .when(show_save_status, |element| {
                            element.child(self.render_task_save_status(cx))
                        })
                        .child(new_issue),
                );
        }

        div()
            .id("board-window-header")
            .min_h(px(48.0))
            .min_w_0()
            .flex_none()
            .flex()
            .items_start()
            .gap(px(8.0))
            .pl(if self.sidebar_visible {
                px(14.0 + (TRAFFIC_LIGHT_CLEARANCE - self.sidebar_rendered_width).max(0.0))
            } else {
                px(0.0)
            })
            .pr(px(14.0))
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .when(!self.sidebar_visible, |element| {
                element.child(
                    div()
                        .h(px(48.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .children(left_controls)
                        .child(
                            self.window_drag_region(
                                div()
                                    .id("board-traffic-light-drag-region")
                                    .w(px(TRAFFIC_LIGHT_CLEARANCE - 8.0))
                                    .h_full()
                                    .flex_none(),
                                cx,
                            ),
                        )
                        .child(
                            self.task_action_button(
                                "board-show-sidebar",
                                "",
                                false,
                                true,
                                |this, _, cx| this.set_sidebar_visible(true, cx),
                                cx,
                            )
                            .w(px(26.0))
                            .h(px(26.0))
                            .px_0()
                            .rounded(px(6.0))
                            .bg(gpui::transparent_black())
                            .border_color(gpui::transparent_black())
                            .tooltip(Tooltip::text(tr!("menu.toggle_sidebar")))
                            .child(icon(
                                "icons/panel-left.svg",
                                14.0,
                                theme.text_tertiary,
                            )),
                        ),
                )
            })
            .child(controls)
            .when_some(right_controls, |element, controls| {
                element.child(div().h(px(48.0)).flex_none().child(controls))
            })
            .into_any_element()
    }
}
