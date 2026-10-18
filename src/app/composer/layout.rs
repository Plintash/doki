//! The composer's shared toolbar geometry. Keep the row used by the app and
//! the headless GPUI regressions identical: the model yields available space
//! while the secondary controls and submission targets retain their hit areas.

use gpui::{
    AnyElement, Div, ElementId, FocusHandle, MouseButton, ParentElement, Stateful, Styled, div,
    prelude::*, px,
};

use crate::theme::{Theme, sp};
use crate::ui::MenuChip;

/// A model name is a compact menu trigger, not a fill-width selected bar.
pub(super) const MODEL_CONTROL_MAX_WIDTH: f32 = 280.0;

pub(super) fn model_control(chip: MenuChip) -> MenuChip {
    chip.min_w_0()
        .flex_shrink_1()
        .max_w(px(MODEL_CONTROL_MAX_WIDTH))
}

pub(super) fn model_options_control(chip: MenuChip) -> MenuChip {
    chip.flex_none().max_w(px(88.0))
}

/// Related settings stay adjacent, but each target opens only its own surface.
/// The whole group yields space before fixed submission targets do.
pub(super) fn model_controls_group(
    model: AnyElement,
    options: Option<AnyElement>,
) -> Stateful<Div> {
    div()
        .id("composer-model-controls")
        .role(gpui::Role::Group)
        .min_w_0()
        .flex_shrink_1()
        .max_w(px(MODEL_CONTROL_MAX_WIDTH))
        .flex()
        .items_center()
        .child(model)
        .children(options)
}

pub(super) fn focus_model_options_after_open(
    handle: crate::ui::menu::ContextMenuHandle,
    focus: FocusHandle,
    window: &mut gpui::Window,
) {
    window.on_next_frame(move |window, _| {
        window.on_next_frame(move |window, cx| {
            if handle.is_open() {
                window.focus(&focus, cx);
            }
        });
    });
}

pub(super) fn model_options_panel(
    viewport_width: gpui::Pixels,
    focus: &FocusHandle,
    theme: Theme,
) -> Stateful<Div> {
    div()
        .id("model-options-panel")
        .track_focus(focus)
        .role(gpui::Role::Group)
        .w(px(280.0).min((viewport_width - px(16.0)).max(px(0.0))))
        .p(px(14.0))
        .tab_group()
        .rounded(px(8.0))
        .border_1()
        .border_color(theme.border_strong)
        .bg(theme.raised)
        .shadow_lg()
}

/// Clicking the catalog rail returns arrows/typing to model search,
/// including an already-selected provider tab.
pub(super) fn model_picker_rail(search_focus: &FocusHandle, theme: Theme) -> Div {
    let focus = search_focus.clone();
    div()
        .w(px(50.0))
        .h_full()
        .flex_none()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(4.0))
        .p(px(5.0))
        .rounded_tl(px(7.0))
        .rounded_bl(px(7.0))
        .bg(theme.canvas)
        .border_r_1()
        .border_color(theme.border)
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            window.focus(&focus, cx)
        })
}

/// The model catalog keeps its full height; settings have a separate popover.
pub(super) fn model_picker_content(
    search: AnyElement,
    list: AnyElement,
    theme: Theme,
) -> Stateful<Div> {
    div()
        .id("model-picker-content")
        .role(gpui::Role::Group)
        .min_w_0()
        .flex_1()
        .flex()
        .flex_col()
        .rounded_tr(px(7.0))
        .rounded_br(px(7.0))
        .bg(theme.surface)
        .child(search)
        .child(
            div()
                .id("model-picker-scroll-area")
                .role(gpui::Role::Group)
                .flex_1()
                .min_h_0()
                .relative()
                .child(list),
        )
}

/// Width of the toolbar's content box, after the card border and row padding.
/// Below this point secondary labels give their space back to the model.
pub(super) fn compact_controls(toolbar_width: f32) -> bool {
    toolbar_width < 640.0
}

/// A submission target keeps its full hit area when the toolbar gets narrow.
/// The caller supplies its role, label, focus handling, and activation behavior.
pub(super) fn action_target(id: impl Into<ElementId>) -> Stateful<Div> {
    div().id(id).flex_none().size(px(26.0))
}

pub(super) fn working_actions() -> Stateful<Div> {
    div()
        .id("working-actions")
        .flex_none()
        .flex()
        .items_center()
        .gap(px(6.0))
}

pub(super) fn secondary_control(chip: MenuChip, compact: bool, max_width: f32) -> MenuChip {
    chip.icon_only(compact).flex_none().max_w(px(max_width))
}

pub(super) fn goal_control() -> Stateful<Div> {
    div()
        .id("composer-goal")
        .flex_none()
        .max_w(px(130.0))
        .h(px(24.0))
        .px(px(7.0))
        .rounded(px(6.0))
        .flex()
        .items_center()
        .gap(px(6.0))
}

pub(super) fn controls_row(
    model: AnyElement,
    preset: Option<AnyElement>,
    access: AnyElement,
    goal: Option<AnyElement>,
    actions: AnyElement,
) -> Div {
    div()
        .mt(px(8.0))
        .px(px(10.0))
        .min_w_0()
        .flex()
        .items_center()
        .gap(px(4.0))
        .text_size(sp(12.5))
        .line_height(sp(14.0))
        .child(
            div()
                .id("composer-settings")
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(4.0))
                .child(
                    div()
                        .id("composer-model-space")
                        .role(gpui::Role::Group)
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .child(model),
                )
                .children(preset)
                .child(access)
                .children(goal),
        )
        .child(actions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;
    use crate::ui::{SemanticControlExt, icon};
    use gpui::{
        Bounds, Context, FocusHandle, Focusable, IntoElement, Modifiers, Pixels, Render,
        TestAppContext, VisualTestContext, Window, point,
    };
    use serde_json::Value;

    const MODEL_LABEL: &str = "An unusually long provider model name with reasoning and service-tier details that must truncate rather than push Send outside the composer";
    const PRESET_LABEL: &str = "A deliberately long custom agent preset name";
    const ACCESS_LABEL: &str = "A deliberately long access permission description";
    const GOAL_LABEL: &str = "Pursuing a deliberately long goal label";
    const TOOLBAR_WIDTHS: [f32; 11] = [
        278.0, 318.0, 418.0, 558.0, 559.0, 560.0, 561.0, 639.0, 640.0, 641.0, 698.0,
    ];

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Actions {
        Send,
        Preparing,
        StopAndFollowUp,
    }

    impl Actions {
        fn width(self) -> f32 {
            if self == Self::StopAndFollowUp {
                58.0
            } else {
                26.0
            }
        }

        fn label(self) -> &'static str {
            match self {
                Self::Send => "Send",
                Self::Preparing => "Preparing task",
                Self::StopAndFollowUp => "Stop",
            }
        }
    }

    struct ToolbarFixture {
        width: f32,
        model_label: &'static str,
        preset: bool,
        goal: bool,
        actions: Actions,
        focus: [FocusHandle; 7],
        activated: Vec<&'static str>,
    }

    impl ToolbarFixture {
        fn new(cx: &mut Context<Self>) -> Self {
            Self {
                width: TOOLBAR_WIDTHS[0],
                model_label: MODEL_LABEL,
                preset: true,
                goal: true,
                actions: Actions::Send,
                focus: std::array::from_fn(|_| cx.focus_handle()),
                activated: Vec::new(),
            }
        }
    }

    impl Render for ToolbarFixture {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let theme = Theme::current(cx);
            let compact = compact_controls(self.width);
            let model = model_control(
                MenuChip::new("composer-provider-model")
                    .icon("icons/bot.svg", theme.text_secondary)
                    .label(self.model_label)
                    .caret(false),
            )
            .track_focus(&self.focus[0])
            .tab_index(0)
            .on_click(cx.listener(|this, _, _, _| this.activated.push("model")))
            .into_any_element();
            let options = model_options_control(
                MenuChip::new("composer-model-options")
                    .label("Extra high · Fast")
                    .caret(false),
            )
            .track_focus(&self.focus[6])
            .tab_index(0)
            .on_click(cx.listener(|this, _, _, _| this.activated.push("options")))
            .into_any_element();
            let model = model_controls_group(model, Some(options)).into_any_element();
            let preset = self.preset.then(|| {
                secondary_control(
                    MenuChip::new("agent-preset")
                        .icon("icons/bot.svg", theme.text_secondary)
                        .label(PRESET_LABEL)
                        .caret(false),
                    compact,
                    120.0,
                )
                .track_focus(&self.focus[1])
                .tab_index(0)
                .on_click(cx.listener(|this, _, _, _| this.activated.push("preset")))
                .into_any_element()
            });
            let access = secondary_control(
                MenuChip::new("runtime-mode")
                    .icon("icons/lock.svg", theme.text_secondary)
                    .label(ACCESS_LABEL)
                    .caret(false),
                compact,
                148.0,
            )
            .track_focus(&self.focus[2])
            .tab_index(0)
            .on_click(cx.listener(|this, _, _, _| this.activated.push("access")))
            .into_any_element();
            let goal = self.goal.then(|| {
                goal_control()
                    .role(gpui::Role::Button)
                    .aria_label(GOAL_LABEL)
                    .track_focus(&self.focus[3])
                    .tab_index(0)
                    .text_size(sp(12.5))
                    .line_height(sp(14.0))
                    .child(icon("icons/target.svg", 10.5, theme.text_secondary))
                    .when(!compact, |goal| {
                        goal.child(div().min_w_0().truncate().child(GOAL_LABEL))
                    })
                    .on_click(cx.listener(|this, _, _, _| this.activated.push("goal")))
                    .into_any_element()
            });
            let preparing = self.actions == Actions::Preparing;
            let primary = action_target("send-or-stop")
                .semantic_control(gpui::Role::Button, preparing)
                .aria_label(self.actions.label())
                .track_focus(&self.focus[4])
                .tab_index(0)
                .when(!preparing, |button| {
                    button.on_click(cx.listener(|this, _, _, _| this.activated.push("primary")))
                });
            let actions = if self.actions == Actions::StopAndFollowUp {
                working_actions()
                    .child(primary)
                    .child(
                        action_target("follow-up-action")
                            .role(gpui::Role::Button)
                            .aria_label("Queue follow-up")
                            .track_focus(&self.focus[5])
                            .tab_index(0)
                            .on_click(
                                cx.listener(|this, _, _, _| this.activated.push("follow-up")),
                            ),
                    )
                    .into_any_element()
            } else {
                primary.into_any_element()
            };

            // This is the production card's border plus the real row's 10px
            // horizontal padding. `width` is its usable toolbar content width.
            div()
                .id("composer-card-fixture")
                .role(gpui::Role::Group)
                .w(px(self.width + 22.0))
                .border_1()
                .child(controls_row(model, preset, access, goal, actions))
        }
    }

    fn snapshot(cx: &mut VisualTestContext) -> Value {
        let raw = cx
            .update(|window, _| window.a11y_automation_snapshot())
            .expect("automation should retain the drawn GPUI accessibility tree");
        serde_json::from_str(&raw).expect("valid GPUI accessibility JSON")
    }

    fn node<'a>(tree: &'a Value, id: &str) -> &'a Value {
        tree["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["element_id"] == id)
            .unwrap_or_else(|| panic!("missing {id}: {tree}"))
    }

    fn bounds(tree: &Value, id: &str) -> Bounds<Pixels> {
        let bounds = &node(tree, id)["bounds"];
        Bounds::new(
            point(
                px(bounds["x"].as_f64().unwrap() as f32),
                px(bounds["y"].as_f64().unwrap() as f32),
            ),
            gpui::size(
                px(bounds["width"].as_f64().unwrap() as f32),
                px(bounds["height"].as_f64().unwrap() as f32),
            ),
        )
    }

    fn approx(actual: Pixels, expected: f32, message: &str) {
        assert!(
            (f32::from(actual) - expected).abs() < 0.01,
            "{message}: got {actual:?}, expected {expected}"
        );
    }

    fn assert_geometry(tree: &Value, width: f32, actions: Actions, preset: bool, goal: bool) {
        let card = bounds(tree, "composer-card-fixture");
        let model = bounds(tree, "composer-provider-model");
        let primary = bounds(tree, "send-or-stop");
        let content_left = card.left() + px(11.0);
        let content_right = card.right() - px(11.0);
        approx(card.size.width, width + 22.0, "card width");
        assert_eq!(model.left(), content_left);
        assert!(
            model.size.width > px(32.0),
            "model hit target collapsed: {tree}"
        );
        assert_eq!(node(tree, "composer-provider-model")["role"], "Button");
        approx(primary.size.width, 26.0, "primary target width");
        approx(primary.size.height, 26.0, "primary target height");
        assert_eq!(node(tree, "send-or-stop")["label"], actions.label());
        if actions == Actions::Preparing {
            assert_eq!(node(tree, "send-or-stop")["disabled"], true);
        }
        let action_right = if actions == Actions::StopAndFollowUp {
            let follow_up = bounds(tree, "follow-up-action");
            approx(follow_up.size.width, 26.0, "follow-up target width");
            approx(follow_up.size.height, 26.0, "follow-up target height");
            approx(follow_up.left() - primary.right(), 6.0, "action gap");
            assert_eq!(follow_up.top(), primary.top());
            follow_up.right()
        } else {
            primary.right()
        };
        approx(
            action_right - primary.left(),
            actions.width(),
            "fixed action group width",
        );
        assert_eq!(
            action_right, content_right,
            "actions must stay within row padding: {tree}"
        );

        let group = bounds(tree, "composer-model-controls");
        let options = bounds(tree, "composer-model-options");
        assert!(group.size.width <= px(MODEL_CONTROL_MAX_WIDTH));
        assert_eq!(
            model.right(),
            options.left(),
            "related triggers stay adjacent"
        );
        assert!(options.size.width > px(32.0));
        assert_eq!(
            node(tree, "composer-model-options")["label"],
            "Extra high · Fast"
        );
        let mut previous = bounds(tree, "composer-model-space");
        assert!(options.right() <= previous.right());
        for (id, label, present, compact_width, full_width) in [
            ("agent-preset", PRESET_LABEL, preset, 26.0, 120.0),
            ("runtime-mode", ACCESS_LABEL, true, 26.0, 148.0),
            ("composer-goal", GOAL_LABEL, goal, 24.5, 130.0),
        ] {
            if !present {
                assert!(
                    tree["nodes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|node| node["element_id"] != id)
                );
                continue;
            }
            let control = bounds(tree, id);
            approx(control.left() - previous.right(), 4.0, "settings gap");
            assert_eq!(
                node(tree, id)["label"],
                label,
                "compaction must retain the full semantic label"
            );
            assert_eq!(node(tree, id)["role"], "Button");
            // At 640px labels return. Exact icon-only widths also catch a
            // hidden label/caret accidentally continuing to consume space.
            if compact_controls(width) {
                approx(control.size.width, compact_width, "compact icon-only width");
            } else {
                approx(
                    control.size.width,
                    full_width,
                    "long visible label width cap",
                );
            }
            assert!(control.left() >= content_left && control.right() <= content_right);
            assert_eq!(control.center().y, primary.center().y);
            previous = control;
        }
        approx(
            primary.left() - previous.right(),
            4.0,
            "settings-to-actions gap",
        );
    }

    #[test]
    fn compact_controls_switch_at_the_toolbar_content_boundary() {
        assert!(compact_controls(278.0));
        assert!(compact_controls(559.0));
        assert!(compact_controls(560.0));
        assert!(compact_controls(639.0));
        assert!(!compact_controls(640.0));
        assert!(!compact_controls(641.0));
        assert!(!compact_controls(698.0));
    }

    #[gpui::test]
    fn toolbar_keeps_actions_and_all_controls_inside_at_every_width(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|window, cx| {
            window.set_a11y_automation(true);
            window.set_rem_size(px(waku_client::persistence::DEFAULT_UI_FONT_SIZE));
            ToolbarFixture::new(cx)
        });
        for width in TOOLBAR_WIDTHS {
            for actions in [Actions::Send, Actions::Preparing, Actions::StopAndFollowUp] {
                view.update(cx, |view, cx| {
                    view.width = width;
                    view.actions = actions;
                    cx.notify();
                });
                cx.run_until_parked();
                let tree = snapshot(cx);
                assert_geometry(&tree, width, actions, true, true);
                assert_eq!(node(&tree, "composer-provider-model")["label"], MODEL_LABEL);
            }
        }
    }

    #[gpui::test]
    fn toolbar_resize_and_optional_controls_leave_the_model_flexible(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|window, cx| {
            window.set_a11y_automation(true);
            window.set_rem_size(px(waku_client::persistence::DEFAULT_UI_FONT_SIZE));
            ToolbarFixture::new(cx)
        });
        // Revisit narrow and expanded widths after crossing the threshold in
        // both directions; no stale minimum-content width should survive.
        for width in [698.0, 278.0, 560.0, 559.0, 561.0, 318.0] {
            for (preset, goal) in [(true, true), (false, true), (true, false), (false, false)] {
                let mut action_left = None;
                for label in [
                    "Short",
                    MODEL_LABEL,
                    "非常に長いモデル名と推論設定・サービス層・コンテキストの詳細を含む表示名",
                ] {
                    view.update(cx, |view, cx| {
                        view.width = width;
                        view.actions = Actions::StopAndFollowUp;
                        view.preset = preset;
                        view.goal = goal;
                        view.model_label = label;
                        cx.notify();
                    });
                    cx.run_until_parked();
                    let tree = snapshot(cx);
                    assert_geometry(&tree, width, Actions::StopAndFollowUp, preset, goal);
                    let measured = bounds(&tree, "send-or-stop").left();
                    if let Some(previous) = action_left {
                        assert_eq!(
                            measured, previous,
                            "model content must not change reserved action geometry"
                        );
                    }
                    action_left = Some(measured);
                    assert_eq!(node(&tree, "composer-provider-model")["label"], label);
                }
            }
        }
    }

    #[gpui::test]
    fn toolbar_compacted_controls_and_action_edges_keep_real_hit_targets(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|window, cx| {
            window.set_a11y_automation(true);
            window.set_rem_size(px(waku_client::persistence::DEFAULT_UI_FONT_SIZE));
            ToolbarFixture::new(cx)
        });
        for actions in [Actions::Send, Actions::Preparing, Actions::StopAndFollowUp] {
            view.update(cx, |view, cx| {
                view.width = 278.0;
                view.actions = actions;
                view.activated.clear();
                cx.notify();
            });
            cx.run_until_parked();
            let tree = snapshot(cx);
            for id in [
                "composer-provider-model",
                "composer-model-options",
                "agent-preset",
                "runtime-mode",
                "composer-goal",
            ] {
                cx.simulate_click(bounds(&tree, id).center(), Modifiers::none());
            }
            // The trailing edge is the portion that used to be pushed beyond
            // the card, so check it rather than only the icon's center.
            let primary = bounds(&tree, "send-or-stop");
            cx.simulate_click(
                point(primary.right() - px(1.0), primary.center().y),
                Modifiers::none(),
            );
            if actions == Actions::StopAndFollowUp {
                let follow_up = bounds(&tree, "follow-up-action");
                cx.simulate_click(
                    point(follow_up.right() - px(1.0), follow_up.center().y),
                    Modifiers::none(),
                );
            }
            cx.run_until_parked();
            view.read_with(cx, |view, _| {
                let mut expected = vec!["model", "options", "preset", "access", "goal"];
                if actions != Actions::Preparing {
                    expected.push("primary");
                }
                if actions == Actions::StopAndFollowUp {
                    expected.push("follow-up");
                }
                assert_eq!(
                    view.activated, expected,
                    "real pointer targets for {actions:?}"
                );
            });
        }
    }

    #[gpui::test]
    fn model_trigger_fits_short_names_and_caps_long_names_without_moving_actions(
        cx: &mut TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|window, cx| {
            window.set_a11y_automation(true);
            let mut fixture = ToolbarFixture::new(cx);
            fixture.width = 698.0;
            fixture.preset = false;
            fixture.goal = false;
            fixture.model_label = "Short";
            fixture
        });
        cx.run_until_parked();
        let short = snapshot(cx);
        let short_model = bounds(&short, "composer-provider-model");
        let short_action = bounds(&short, "send-or-stop");
        assert!(short_model.size.width < px(150.0));
        assert!(short_model.right() < bounds(&short, "composer-model-space").right());
        view.update(cx, |view, cx| {
            view.model_label = MODEL_LABEL;
            cx.notify();
        });
        cx.run_until_parked();
        let long = snapshot(cx);
        approx(
            bounds(&long, "composer-model-controls").size.width,
            MODEL_CONTROL_MAX_WIDTH,
            "long model cap",
        );
        assert_eq!(bounds(&long, "send-or-stop"), short_action);
        assert!(bounds(&long, "composer-provider-model").size.width > short_model.size.width);
    }

    struct PickerFixture {
        width: f32,
        height: f32,
    }

    impl Render for PickerFixture {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(self.width))
                .h(px(self.height))
                .flex()
                .child(model_picker_content(
                    div()
                        .id("picker-search-fixture")
                        .role(gpui::Role::Group)
                        .h(px(52.0))
                        .flex_none()
                        .into_any_element(),
                    div()
                        .id("picker-list-fixture")
                        .role(gpui::Role::List)
                        .size_full()
                        .into_any_element(),
                    Theme::current(cx),
                ))
        }
    }

    #[gpui::test]
    fn catalog_uses_all_space_below_search_without_a_settings_footer(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|window, _| {
            window.set_a11y_automation(true);
            PickerFixture {
                width: 408.0,
                height: 388.0,
            }
        });
        for (width, height) in [(408.0, 388.0), (260.0, 300.0)] {
            view.update(cx, |view, cx| {
                view.width = width;
                view.height = height;
                cx.notify();
            });
            cx.run_until_parked();
            let tree = snapshot(cx);
            let search = bounds(&tree, "picker-search-fixture");
            let list = bounds(&tree, "picker-list-fixture");
            approx(search.size.height, 52.0, "search height");
            assert_eq!(search.bottom(), list.top());
            approx(list.size.width, width, "catalog width");
            approx(list.size.height, height - 52.0, "full catalog height");
            approx(list.bottom(), height, "catalog bottom");
        }
    }

    struct OptionsPanelFixture {
        viewport_width: f32,
        focus: FocusHandle,
    }

    impl Render for OptionsPanelFixture {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().flex().items_start().child(
                model_options_panel(px(self.viewport_width), &self.focus, Theme::current(cx))
                    .child(
                        div()
                            .id("options-content-fixture")
                            .role(gpui::Role::Group)
                            .w_full()
                            .h(px(96.0)),
                    ),
            )
        }
    }

    #[gpui::test]
    fn settings_popover_is_compact_and_clamped_to_narrow_windows(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|window, cx| {
            window.set_a11y_automation(true);
            OptionsPanelFixture {
                viewport_width: 1000.0,
                focus: cx.focus_handle(),
            }
        });
        for (width, expected) in [(1000.0, 280.0), (240.0, 224.0)] {
            view.update(cx, |view, cx| {
                view.viewport_width = width;
                cx.notify();
            });
            cx.run_until_parked();
            let tree = snapshot(cx);
            let panel = bounds(&tree, "model-options-panel");
            let content = bounds(&tree, "options-content-fixture");
            approx(panel.size.width, expected, "compact options width");
            approx(
                content.left() - panel.left(),
                15.0,
                "comfortable content inset",
            );
            approx(content.size.width, expected - 30.0, "options content width");
            approx(panel.size.height, 126.0, "content-sized options height");
        }
    }

    struct FixedOptionsFixture {
        handle: crate::ui::menu::ContextMenuHandle,
        panel_focus: FocusHandle,
        slider_focus: FocusHandle,
    }

    impl Render for FixedOptionsFixture {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let panel_focus = self.panel_focus.clone();
            let slider_focus = self.slider_focus.clone();
            crate::ui::menu::popover(
                MenuChip::new("fixed-options-trigger").label("Fixed"),
                &self.handle,
                crate::ui::menu::MenuAlign::BelowLeft,
                move |_, window, cx| {
                    model_options_panel(
                        window.viewport_size().width,
                        &panel_focus,
                        Theme::current(cx),
                    )
                    .child(
                        crate::ui::effort_slider::EffortSlider::new(
                            "fixed-effort",
                            1,
                            0,
                            &slider_focus,
                        )
                        .value_labels(vec!["Fixed".into()]),
                    )
                    .into_any_element()
                },
            )
        }
    }

    #[gpui::test]
    fn fixed_only_options_receive_focus_and_escape_without_stealing_focus_after_dismissal(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::ui::menu::init);
        let (view, cx) = cx.add_window_view(|_, cx| {
            let panel_focus = cx.focus_handle();
            let focus = panel_focus.clone();
            let handle = crate::ui::menu::ContextMenuHandle::new(cx);
            let focus_handle = handle.clone();
            let handle = handle.on_toggle(move |open, window, _| {
                if open {
                    focus_model_options_after_open(focus_handle.clone(), focus.clone(), window);
                }
            });
            FixedOptionsFixture {
                handle,
                panel_focus,
                slider_focus: cx.focus_handle(),
            }
        });
        cx.run_until_parked();
        let handle = view.read_with(cx, |view, _| view.handle.clone());
        cx.update(|window, cx| window.focus(handle.trigger_focus_handle(), cx));
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(handle.is_open());
        // The headless platform has no display frame loop.
        for _ in 0..2 {
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
            });
            cx.run_until_parked();
        }
        let panel_focus = view.read_with(cx, |view, _| view.panel_focus.clone());
        cx.update(|window, _| assert!(panel_focus.is_focused(window)));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(
            !handle.is_open(),
            "the inert fixed slider must not trap the popover"
        );
        // An open immediately followed by dismissal must not queue stale focus.
        cx.update(|window, cx| {
            crate::ui::menu::toggle_popover(
                &handle,
                crate::ui::menu::MenuAlign::BelowLeft,
                window,
                cx,
            );
            handle.close(window, cx);
            window.focus(handle.trigger_focus_handle(), cx);
        });
        for _ in 0..2 {
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
            });
            cx.run_until_parked();
        }
        cx.update(|window, _| assert!(handle.trigger_focus_handle().is_focused(window)));
    }

    struct PickerFocusFixture {
        search: gpui::Entity<crate::input::TextInput>,
        selected: usize,
        catalog_moves: usize,
    }

    impl Render for PickerFocusFixture {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let theme = Theme::current(cx);
            let search_focus = self.search.read(cx).focus_handle(cx);
            div()
                .w(px(460.0))
                .h(px(390.0))
                .key_context("WakuMenu")
                .flex()
                .on_action(
                    cx.listener(|this, _: &crate::ui::menu::SelectNextEntry, _, _| {
                        this.catalog_moves += 1
                    }),
                )
                .child(
                    model_picker_rail(&search_focus, theme).child(
                        div()
                            .id("rail-tab-fixture")
                            .role(gpui::Role::Button)
                            .size(px(38.0)),
                    ),
                )
                .child(model_picker_content(
                    div()
                        .h(px(52.0))
                        .flex_none()
                        .child(self.search.clone())
                        .into_any_element(),
                    div().size_full().into_any_element(),
                    theme,
                ))
        }
    }

    #[gpui::test]
    fn rail_click_returns_external_keyboard_focus_to_catalog_search(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::input::init(cx);
            crate::ui::menu::init(cx);
        });
        let (view, cx) = cx.add_window_view(|window, cx| {
            window.set_a11y_automation(true);
            let slider_focus = cx.focus_handle();
            window.focus(&slider_focus, cx);
            PickerFocusFixture {
                search: cx.new(|cx| crate::input::TextInput::new(window, cx).tab_stop(true)),
                selected: 2,
                catalog_moves: 0,
            }
        });
        cx.run_until_parked();

        let tree = snapshot(cx);
        cx.simulate_click(
            bounds(&tree, "rail-tab-fixture").center(),
            Modifiers::none(),
        );
        cx.run_until_parked();
        cx.simulate_keystrokes("down");
        view.read_with(cx, |view, _| {
            assert_eq!(view.catalog_moves, 1);
            assert_eq!(
                view.selected, 2,
                "catalog navigation must not edit the effort value"
            );
        });
    }
}
