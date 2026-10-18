//! Settings owned by one project, without changing the active task or draft.

use gpui::{KeyBinding, actions};

use super::*;
use crate::ui::SemanticControlExt;

actions!(waku_project_settings, [DismissProjectSettings]);

const PROJECT_SETTINGS_CONTEXT: &str = "ProjectSettings";
const PROJECT_SETTINGS_LAYER: usize = 4;

pub fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new(
        "escape",
        DismissProjectSettings,
        Some(PROJECT_SETTINGS_CONTEXT),
    )]);
}

pub(super) struct ProjectSettingsState {
    id: Uuid,
    project_id: Uuid,
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
}

/// An exact owner is required. A removed or projectless target must never
/// silently become the active project (or the first project in the catalog).
fn settings_project(projects: &[Project], id: Uuid) -> Option<&Project> {
    projects
        .iter()
        .find(|project| project.id == id && !project.is_projectless())
}

fn base_menu_id(project_id: Uuid) -> SharedString {
    format!("project-worktree-base-{project_id}").into()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsControl {
    Close,
    Base,
}

fn next_settings_control(
    current: Option<SettingsControl>,
    base_enabled: bool,
    reverse: bool,
) -> SettingsControl {
    if !base_enabled {
        return SettingsControl::Close;
    }
    match current {
        Some(SettingsControl::Close) => SettingsControl::Base,
        Some(SettingsControl::Base) => SettingsControl::Close,
        None if reverse => SettingsControl::Base,
        None => SettingsControl::Close,
    }
}

fn cycle_settings_focus(
    close: &FocusHandle,
    base: &FocusHandle,
    base_enabled: bool,
    reverse: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let current = if close.is_focused(window) {
        Some(SettingsControl::Close)
    } else if base.is_focused(window) {
        Some(SettingsControl::Base)
    } else {
        None
    };
    let next = match next_settings_control(current, base_enabled, reverse) {
        SettingsControl::Close => close,
        SettingsControl::Base => base,
    };
    window.focus(next, cx);
}

impl Waku {
    pub(super) fn open_project_settings(
        &mut self,
        project_id: Uuid,
        return_focus: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.project_settings.is_some() {
            return;
        }
        let Some(project) = settings_project(&self.state.projects, project_id) else {
            return;
        };
        self.branch_snapshots.invalidate(&project.path);
        let id = Uuid::new_v4();
        let focus = cx.focus_handle();
        self.project_settings = Some(ProjectSettingsState {
            id,
            project_id,
            focus: focus.clone(),
            previous_focus: Some(return_focus),
        });
        let weak = cx.entity().downgrade();
        // The modal must enter the deferred dispatch tree before it owns focus.
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| {
                let current = weak
                    .update(cx, |this, _| {
                        this.project_settings
                            .as_ref()
                            .is_some_and(|state| state.id == id)
                    })
                    .unwrap_or(false);
                if current {
                    window.focus(&focus, cx);
                }
            });
        });
        cx.notify();
    }

    fn close_project_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(settings) = self.project_settings.take() else {
            return;
        };
        let menu = self
            .menus
            .borrow()
            .get(&base_menu_id(settings.project_id))
            .cloned();
        if let Some(menu) = menu {
            menu.close(window, cx);
        }
        if let Some(focus) = settings.previous_focus {
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    fn set_project_worktree_base(
        &mut self,
        settings_id: Uuid,
        project_id: Uuid,
        reference: Option<String>,
        cx: &mut Context<Self>,
    ) {
        // An old menu callback must not write after the dialog was dismissed
        // or retargeted. No session selection or draft workspace is changed.
        if self
            .project_settings
            .as_ref()
            .is_none_or(|settings| settings.id != settings_id || settings.project_id != project_id)
        {
            return;
        }
        let Some(project) = self
            .state
            .projects
            .iter_mut()
            .find(|project| project.id == project_id && !project.is_projectless())
        else {
            return;
        };
        if project.worktree_base_branch == reference {
            return;
        }
        project.worktree_base_branch = reference;
        self.save(cx);
        cx.notify();
    }

    pub(super) fn render_project_settings(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        use super::branches::{planned_worktree_base_label, worktree_base_label};

        let settings = self.project_settings.as_ref()?;
        let settings_id = settings.id;
        let project_id = settings.project_id;
        let focus = settings.focus.clone();
        let Some(project) = settings_project(&self.state.projects, project_id).cloned() else {
            self.close_project_settings(window, cx);
            return None;
        };
        let theme = Theme::current(cx);
        let snapshot = self.branch_snapshot_for_workspace(&project.path, cx);
        let configured = project.worktree_base_branch.clone();
        let label = snapshot
            .as_ref()
            .map(|snapshot| {
                let resolved = planned_worktree_base_label(&project, None, snapshot);
                if configured.is_none() {
                    tr!("worktree_base.automatic_resolved", branch = resolved)
                } else {
                    resolved
                }
            })
            .unwrap_or_else(|| {
                if matches!(
                    self.branch_snapshots.observe(&project.path),
                    crate::query::QueryState::Ready(_)
                ) {
                    tr!("worktree_base.unavailable")
                } else {
                    tr!("worktree_base.loading")
                }
            });
        let refs = snapshot
            .map(|snapshot| snapshot.worktree_base_refs)
            .unwrap_or_default();
        let id = base_menu_id(project_id);
        let handle = self.menu_handle(id.clone(), cx);
        let loading = matches!(
            self.branch_snapshots.observe(&project.path),
            crate::query::QueryState::Missing | crate::query::QueryState::Pending
        );
        let trigger = MenuChip::new(id.clone())
            .label(label)
            .outlined()
            .selected(handle.is_open())
            .max_w(px(300.0))
            .tooltip(Tooltip::text(tr!("worktree_base.settings_hint")))
            .disabled(loading);
        let selector = if loading {
            trigger.into_any_element()
        } else {
            let weak = cx.entity().downgrade();
            crate::ui::menu::dropdown_menu_list_with_priority(
                trigger,
                SharedString::from(format!("{id}-menu")),
                &handle,
                MenuAlign::BelowRight,
                PROJECT_SETTINGS_LAYER + 1,
                move |_| {
                    let reset_weak = weak.clone();
                    let mut items = vec![
                        MenuItem::new(tr!("worktree_base.automatic"), move |_, cx| {
                            let _ = reset_weak.update(cx, |this, cx| {
                                this.set_project_worktree_base(settings_id, project_id, None, cx);
                            });
                        })
                        .semantic_id("worktree-base-automatic")
                        .selected(configured.is_none()),
                    ];
                    items.extend(refs.iter().map(|reference| {
                        let reference = reference.clone();
                        let selected = configured.as_deref() == Some(reference.as_str());
                        let semantic_id = format!("worktree-base-ref-{reference}");
                        let weak = weak.clone();
                        MenuItem::new(worktree_base_label(&reference), move |_, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.set_project_worktree_base(
                                    settings_id,
                                    project_id,
                                    Some(reference.clone()),
                                    cx,
                                );
                            });
                        })
                        .semantic_id(semantic_id)
                        .selected(selected)
                    }));
                    items
                },
            )
        };
        let close = self
            .task_action_button(
                "project-settings-close",
                tr!("common.close"),
                false,
                true,
                |this, window, cx| this.close_project_settings(window, cx),
                cx,
            )
            .tooltip(Tooltip::text(tr!("common.close")));
        let close_focus =
            self.transcript_control_focuses.borrow()["project-settings-close"].clone();
        let next_close = close_focus.clone();
        let next_base = handle.trigger_focus_handle().clone();
        let previous_base = next_base.clone();
        let next_menu = handle.clone();
        let previous_menu = handle.clone();
        let card = div()
            .id("project-settings-card")
            .semantic_control(gpui::Role::Group, false)
            .aria_label(tr!("project.settings"))
            .track_focus(&focus)
            .key_context(PROJECT_SETTINGS_CONTEXT)
            .on_action(cx.listener(|this, _: &DismissProjectSettings, window, cx| {
                this.close_project_settings(window, cx);
            }))
            .on_action(move |_: &FocusNext, window, cx| {
                if next_menu.is_open() {
                    next_menu.close(window, cx);
                }
                cycle_settings_focus(&next_close, &next_base, !loading, false, window, cx);
            })
            .on_action(move |_: &FocusPrev, window, cx| {
                if previous_menu.is_open() {
                    previous_menu.close(window, cx);
                }
                cycle_settings_focus(&close_focus, &previous_base, !loading, true, window, cx);
            })
            .tab_group()
            .tab_stop(false)
            .w_full()
            .max_w(px(520.0))
            .rounded(px(18.0))
            .bg(theme.composer)
            .shadow_xl()
            .p(px(20.0))
            .flex()
            .flex_col()
            .gap(px(20.0))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(icon("icons/folder.svg", 18.0, theme.text_secondary))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .child(
                                div()
                                    .id("project-settings-name")
                                    .truncate()
                                    .text_size(sp(15.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .tooltip(Tooltip::text(
                                        project.path.to_string_lossy().into_owned(),
                                    ))
                                    .child(project.display_name()),
                            )
                            .child(
                                div()
                                    .mt(px(3.0))
                                    .text_size(sp(12.0))
                                    .text_color(theme.text_tertiary)
                                    .child(tr!("project.settings")),
                            ),
                    )
                    .child(close),
            )
            .child(
                div()
                    .p(px(14.0))
                    .rounded(px(11.0))
                    .bg(theme.raised)
                    .child(
                        div()
                            .text_size(sp(13.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(tr!("worktree_base.section")),
                    )
                    .child(
                        div()
                            .mt(px(12.0))
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .justify_between()
                            .gap(px(10.0))
                            .child(
                                div()
                                    .text_size(sp(12.5))
                                    .text_color(theme.text_secondary)
                                    .child(tr!("worktree_base.settings_title")),
                            )
                            .child(selector),
                    ),
            );
        let scrim = if theme.is_dark {
            gpui::hsla(0.0, 0.0, 0.0, 0.34)
        } else {
            gpui::hsla(0.0, 0.0, 0.0, 0.16)
        };
        let layer = div()
            .id("project-settings-layer")
            .absolute()
            .inset_0()
            .occlude()
            .bg(scrim)
            .p(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_project_settings(window, cx)),
            )
            .child(card);
        Some(
            gpui::deferred(layer)
                .with_priority(PROJECT_SETTINGS_LAYER)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_traversal_stays_within_the_dialog_and_skips_a_loading_selector() {
        use SettingsControl::{Base, Close};
        assert_eq!(next_settings_control(None, true, false), Close);
        assert_eq!(next_settings_control(None, true, true), Base);
        for reverse in [false, true] {
            assert_eq!(next_settings_control(Some(Close), true, reverse), Base);
            assert_eq!(next_settings_control(Some(Base), true, reverse), Close);
            for current in [None, Some(Close), Some(Base)] {
                assert_eq!(next_settings_control(current, false, reverse), Close);
            }
        }
    }

    #[test]
    fn settings_are_scoped_to_the_exact_requested_project_without_fallback() {
        let mut alpha = Project::from_path(PathBuf::from("/projects/alpha"));
        alpha.worktree_base_branch = Some("refs/heads/develop".into());
        let mut beta = Project::from_path(PathBuf::from("/projects/beta"));
        beta.worktree_base_branch = Some("refs/remotes/origin/dev".into());
        let projects = vec![alpha.clone(), beta.clone()];
        assert_eq!(
            settings_project(&projects, alpha.id).map(|project| project.id),
            Some(alpha.id)
        );
        assert_eq!(
            settings_project(&projects, beta.id).map(|project| project.id),
            Some(beta.id)
        );
        assert!(settings_project(&projects, Uuid::new_v4()).is_none());
        assert!(settings_project(&[beta], alpha.id).is_none());

        let alias = Project::from_path(alpha.path.clone());
        assert!(settings_project(&[alias], alpha.id).is_none());
        let projectless =
            Project::from_path(dirs::home_dir().expect("test user home").join(".waku"));
        assert!(projectless.is_projectless());
        assert!(settings_project(&[projectless.clone()], projectless.id).is_none());
    }
}
