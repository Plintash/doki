//! One compact toolbar for the board and its issue inspector.
//!
//! Filters live on Waku so opening an issue and going back keeps the same
//! project, query, and activity scope. Window chrome stays outside the wrapping
//! controls, preserving native titlebar placement in narrow windows.

use super::*;
use crate::model::BoardStage;
use crate::ui::menu::dropdown_menu_list;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DetailStageTarget {
    Stage {
        project_id: Uuid,
        stage_id: Uuid,
    },
    Lifecycle {
        project_id: Option<Uuid>,
        state: TaskState,
    },
}

struct DetailStageChoice {
    target: DetailStageTarget,
    label: String,
    selected: bool,
}

fn detail_stage_owner<'a>(task: &Task, projects: &'a [Project]) -> Option<&'a Project> {
    projects
        .iter()
        .find(|project| Some(project.id) == task.project_id() && !project.board.stages.is_empty())
}

fn detail_current_stage<'a>(task: &Task, owner: &'a Project) -> Option<&'a BoardStage> {
    owner.board.stage_for_task(task).or_else(|| {
        owner.board.stages.iter().find(|stage| {
            stage.id == crate::model::canonical_stage_id(owner.id, task.state)
                && stage.state == task.state
        })
    })
}

fn detail_stage_label(task: &Task, projects: &[Project]) -> String {
    detail_stage_owner(task, projects)
        .and_then(|owner| detail_current_stage(task, owner))
        .map(|stage| stage.name.clone())
        .unwrap_or_else(|| board::task_state_label(task.state))
}

/// Resolve the issue's exact owner, independently of the Board's current filter.
/// Same-path project aliases must never lend one another their stage defaults.
fn detail_stage_choices(task: &Task, projects: &[Project]) -> Vec<DetailStageChoice> {
    if let Some(owner) = detail_stage_owner(task, projects) {
        let selected = detail_current_stage(task, owner).map(|stage| stage.id);
        return owner
            .board
            .stages
            .iter()
            .map(|stage| DetailStageChoice {
                target: DetailStageTarget::Stage {
                    project_id: owner.id,
                    stage_id: stage.id,
                },
                label: stage.name.clone(),
                selected: selected == Some(stage.id),
            })
            .collect();
    }
    [TaskState::Backlog, TaskState::Doing, TaskState::Done]
        .into_iter()
        .map(|state| DetailStageChoice {
            target: DetailStageTarget::Lifecycle {
                project_id: task.project_id(),
                state,
            },
            label: board::task_state_label(state),
            selected: task.state == state,
        })
        .collect()
}

/// Menu rows are snapshots. Revalidate ownership and resolve the latest saved
/// stage at activation, including after a rename, deletion or project change.
fn resolve_detail_stage(
    task: &Task,
    projects: &[Project],
    target: DetailStageTarget,
) -> Result<(TaskState, Option<BoardStage>), ()> {
    match target {
        DetailStageTarget::Stage {
            project_id,
            stage_id,
        } => {
            let owner = detail_stage_owner(task, projects)
                .filter(|owner| owner.id == project_id)
                .ok_or(())?;
            let stage = owner
                .board
                .stages
                .iter()
                .find(|stage| stage.id == stage_id)
                .ok_or(())?;
            Ok((stage.state, Some(stage.clone())))
        }
        DetailStageTarget::Lifecycle { project_id, state }
            if task.project_id() == project_id && detail_stage_owner(task, projects).is_none() =>
        {
            Ok((state, None))
        }
        _ => Err(()),
    }
}

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
                let this = entity.read(cx);
                let projects = projects::picker_projects(&this.state.projects, selected)
                    .into_iter()
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

    fn render_board_detail_controls(
        &mut self,
        task: &Task,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.ensure_task_detail(task, window, cx);
        let task_id = task.id;
        let task_state = task.state;
        let completing = self.task_completion_stops.contains_key(&task_id);
        // The cached rows belong to this inspector visit, not a global menu
        // site. Navigating to another issue (or Back/reopen) drops the snapshot.
        let handle = self
            .task_detail
            .as_ref()
            .expect("detail initialized")
            .stage_menu
            .clone();
        let weak = cx.entity().downgrade();
        let state_picker = dropdown_menu_list(
            MenuChip::new("board-detail-state")
                .label(detail_stage_label(task, &self.state.projects))
                .height(px(30.0))
                .max_w(px(240.0))
                .min_w_0()
                .disabled(completing)
                .selected(handle.is_open()),
            "board-detail-state-menu",
            &handle,
            MenuAlign::BelowRight,
            move |cx| {
                let Some(entity) = weak.upgrade() else {
                    return Vec::new();
                };
                let this = entity.read(cx);
                let Some(task) = this.state.tasks.iter().find(|task| task.id == task_id) else {
                    return Vec::new();
                };
                let completing = this.task_completion_stops.contains_key(&task_id);
                detail_stage_choices(task, &this.state.projects)
                    .into_iter()
                    .map(|choice| {
                        let weak = weak.clone();
                        let target = choice.target;
                        let semantic_id = match target {
                            DetailStageTarget::Stage { stage_id, .. } => {
                                format!("board-detail-stage-{stage_id}")
                            }
                            DetailStageTarget::Lifecycle { state, .. } => {
                                format!("board-detail-state-{}", state.key())
                            }
                        };
                        MenuItem::new(choice.label, move |window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.move_task_from_detail(task_id, target, window, cx);
                            });
                        })
                        .semantic_id(semantic_id)
                        .selected(choice.selected)
                        .disabled(completing)
                    })
                    .collect()
            },
        );
        let complete = self.task_icon_action(
            "board-detail-complete",
            if completing {
                tr!("board.cancel_completion")
            } else if task_state.is_done() {
                tr!("board.reopen")
            } else {
                tr!("board.complete")
            },
            if completing {
                "icons/x.svg"
            } else if task_state.is_done() {
                "icons/rewind.svg"
            } else {
                "icons/check.svg"
            },
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

    fn move_task_from_detail(
        &mut self,
        task_id: Uuid,
        target: DetailStageTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_task != Some(task_id) || self.task_completion_stops.contains_key(&task_id)
        {
            return;
        }
        let Some(task) = self.state.tasks.iter().find(|task| task.id == task_id) else {
            return;
        };
        let Ok((state, stage)) = resolve_detail_stage(task, &self.state.projects, target) else {
            self.show_toast(tr!("board.stages_changed"));
            cx.notify();
            return;
        };
        // Terminal stages retain the shared stop/confirmation/save barrier.
        // Moving an issue never prepares a draft or invokes a provider.
        self.move_task_to_stage(task_id, state, stage, window, cx);
    }

    pub(super) fn render_board_header(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let selected_task = self
            .selected_task
            .and_then(|id| self.task_for_editor(id))
            .cloned();
        let right_controls = (!self.assistant.open)
            .then(|| {
                self.render_client_window_controls(
                    super::window_chrome::WindowControlSide::Right,
                    window,
                    cx,
                )
            })
            .flatten();
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
                .child(self.task_icon_action(
                    "board-detail-back",
                    tr!("board.back_to_board"),
                    "icons/arrow-left.svg",
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
                .child(self.render_task_save_status(cx))
                .child(if self.is_task_draft(task.id) {
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(self.task_action_button(
                            "new-task-cancel",
                            tr!("common.cancel"),
                            false,
                            true,
                            |this, window, cx| this.cancel_new_task(window, cx),
                            cx,
                        ))
                        .child(
                            self.task_action_button(
                                "new-task-create",
                                tr!("board.create_task"),
                                true,
                                self.task_detail_values(task.id, cx)
                                    .is_some_and(|(title, _)| !title.trim().is_empty()),
                                |this, _, cx| this.commit_new_task(cx),
                                cx,
                            ),
                        )
                        .into_any_element()
                } else {
                    self.render_board_detail_controls(&task, window, cx)
                });
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
                |this, window, cx| this.open_new_task(TaskState::Backlog, window, cx),
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
                        .child(self.render_task_save_status(cx))
                        .child(running_filter)
                        .child(attention_filter)
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
            .pl(px(self.titlebar_content_inset(window, cx)))
            .pr(px(14.0))
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .child(controls)
            .child(
                div()
                    .h(px(48.0))
                    .flex()
                    .items_center()
                    .child(self.render_assistant_toggle(cx)),
            )
            .when_some(right_controls, |element, controls| {
                element.child(div().h(px(48.0)).flex_none().child(controls))
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Task, Project) {
        let mut project = Project::from_path("/projects/doki".into());
        project.board.stages[0].name = "Plan".into();
        project.board.stages[1].name = "Implement".into();
        project.board.stages[2].name = "Archive".into();
        let mut review = project.board.stages[1].clone();
        review.id = Uuid::new_v4();
        review.name = "Review".into();
        review.provider = Some(ProviderKind::Codex);
        project.board.stages.insert(2, review.clone());
        let mut task = Task::new("Ship checkout", TaskState::Doing, 7);
        task.description = "Preserve existing carts".into();
        task.workspace = Some(TaskWorkspace::Worktree {
            project_id: project.id,
            path: "/projects/worktrees/checkout".into(),
            branch: "checkout".into(),
        });
        task.stage_id = Some(review.id);
        (task, project)
    }

    #[test]
    fn inspector_names_the_actual_stage_and_distinguishes_same_lifecycle_choices() {
        let (task, project) = fixture();
        let projects = [project];
        assert_eq!(detail_stage_label(&task, &projects), "Review");
        let choices = detail_stage_choices(&task, &projects);
        assert_eq!(
            choices
                .iter()
                .map(|choice| choice.label.as_str())
                .collect::<Vec<_>>(),
            ["Plan", "Implement", "Review", "Archive"]
        );
        assert_eq!(
            choices
                .iter()
                .map(|choice| choice.selected)
                .collect::<Vec<_>>(),
            [false, false, true, false]
        );
        let (state, stage) = resolve_detail_stage(&task, &projects, choices[1].target).unwrap();
        assert_eq!(state, TaskState::Doing);
        assert_eq!(stage.unwrap().id, projects[0].board.stages[1].id);
    }

    #[test]
    fn inspector_refreshes_saved_names_order_and_selection_without_changing_provenance() {
        let (task, mut project) = fixture();
        let original_task = task.clone();
        project.board.stages[2].name = "Security review".into();
        project.board.stages.swap(1, 2);
        let choices = detail_stage_choices(&task, std::slice::from_ref(&project));
        assert_eq!(detail_stage_label(&task, &[project]), "Security review");
        assert_eq!(choices[1].label, "Security review");
        assert!(choices[1].selected);
        assert_eq!(choices[2].label, "Implement");
        assert_eq!(task, original_task);
    }

    #[test]
    fn inspector_uses_exact_project_owner_even_when_a_same_path_alias_comes_first() {
        let (task, project) = fixture();
        let alias = Project::from_path(project.path.clone());
        let projects = [alias, project];
        let choices = detail_stage_choices(&task, &projects);
        assert_eq!(detail_stage_label(&task, &projects), "Review");
        assert!(choices.iter().all(|choice| matches!(choice.target,
            DetailStageTarget::Stage { project_id, .. } if project_id == projects[1].id)));
        let foreign = DetailStageTarget::Stage {
            project_id: projects[0].id,
            stage_id: projects[0].board.stages[1].id,
        };
        assert!(resolve_detail_stage(&task, &projects, foreign).is_err());
    }

    #[test]
    fn legacy_stage_identity_falls_back_to_its_owners_canonical_lifecycle() {
        let (mut task, project) = fixture();
        for stage_id in [None, Some(Uuid::new_v4())] {
            task.stage_id = stage_id;
            let choices = detail_stage_choices(&task, std::slice::from_ref(&project));
            assert_eq!(
                detail_stage_label(&task, std::slice::from_ref(&project)),
                "Implement"
            );
            assert!(choices[1].selected);
            assert_eq!(choices.iter().filter(|choice| choice.selected).count(), 1);
        }
    }

    #[test]
    fn unbound_or_unavailable_project_retains_legacy_lifecycle_controls() {
        let (task, mut project) = fixture();
        project.board.stages.clear();
        for projects in [Vec::new(), vec![project]] {
            for bound in [true, false] {
                let mut task = task.clone();
                if !bound {
                    task.workspace = None;
                }
                let choices = detail_stage_choices(&task, &projects);
                assert_eq!(choices.len(), 3);
                assert_eq!(
                    detail_stage_label(&task, &projects),
                    board::task_state_label(task.state)
                );
                assert!(choices[1].selected);
                for (choice, state) in
                    choices
                        .iter()
                        .zip([TaskState::Backlog, TaskState::Doing, TaskState::Done])
                {
                    assert_eq!(
                        resolve_detail_stage(&task, &projects, choice.target).unwrap(),
                        (state, None)
                    );
                }
            }
        }
    }

    #[test]
    fn stale_menu_targets_cannot_revive_deleted_stages_or_cross_project_changes() {
        let (mut task, mut project) = fixture();
        let choice = detail_stage_choices(&task, std::slice::from_ref(&project))[2].target;
        let legacy = detail_stage_choices(&task, &[])[1].target;
        assert!(resolve_detail_stage(&task, std::slice::from_ref(&project), legacy).is_err());
        project.board.stages[2].name = "Renamed review".into();
        let (_, resolved) =
            resolve_detail_stage(&task, std::slice::from_ref(&project), choice).unwrap();
        assert_eq!(resolved.unwrap().name, "Renamed review");
        project.board.stages.remove(2);
        assert!(resolve_detail_stage(&task, std::slice::from_ref(&project), choice).is_err());
        task.workspace = Some(TaskWorkspace::Local {
            project_id: Uuid::new_v4(),
        });
        assert!(resolve_detail_stage(&task, std::slice::from_ref(&project), choice).is_err());
        assert!(resolve_detail_stage(&task, &[], legacy).is_err());
    }

    #[test]
    fn active_moves_preserve_issue_workspace_and_archive_is_a_terminal_destination() {
        let (mut task, project) = fixture();
        let original = task.clone();
        let choices = detail_stage_choices(&task, std::slice::from_ref(&project));
        let (state, stage) =
            resolve_detail_stage(&task, std::slice::from_ref(&project), choices[0].target).unwrap();
        super::super::task_completion::TaskCompletionSaveRollback::move_to(
            &mut task,
            state,
            stage.as_ref(),
            3,
        );
        assert_eq!(task.stage_id, Some(project.board.stages[0].id));
        assert_eq!(task.state, TaskState::Backlog);
        assert_eq!(task.id, original.id);
        assert_eq!(task.title, original.title);
        assert_eq!(task.description, original.description);
        assert_eq!(task.workspace, original.workspace);
        assert!(task.completed_at.is_none());
        let (state, stage) = resolve_detail_stage(&task, &[project], choices[3].target).unwrap();
        assert!(state.is_done());
        assert_eq!(stage.unwrap().name, "Archive");
        assert!(
            !task.state.is_done(),
            "resolving Archive cannot bypass completion acknowledgment"
        );
    }
}
