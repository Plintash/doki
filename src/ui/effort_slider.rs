//! A discrete, controlled slider for the composer's reasoning-effort options.

use std::{cell::Cell, rc::Rc};

use gpui::{
    App, BorderStyle, Bounds, Div, ElementId, FocusHandle, InteractiveElement, Interactivity,
    KeyDownEvent, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, RenderOnce, SharedString,
    Stateful, StyleRefinement, Styled, Window, canvas, div, point, prelude::*, px, quad, size,
};

use crate::theme::Theme;

use super::SemanticControlExt;

const CONTROL_HEIGHT: f32 = 28.0;
const TRACK_HEIGHT: f32 = 20.0;
const THUMB_DIAMETER: f32 = 24.0;
const STOP_DIAMETER: f32 = 3.0;

type ChangeCallback = Rc<dyn Fn(&usize, &mut Window, &mut App)>;

#[derive(Default)]
struct SliderState {
    dragging: Cell<bool>,
    selected: Cell<usize>,
    bounds: Cell<Bounds<Pixels>>,
}

/// The owner supplies the current index and updates it in `on_change`.
///
/// Option order is authoritative: moving right/up increases the index. The
/// visible value label belongs above the slider; `value_label` exposes that
/// same label to assistive technology. Zero or one option renders inert.
#[derive(IntoElement)]
pub struct EffortSlider {
    id: ElementId,
    base: Stateful<Div>,
    option_count: usize,
    selected: usize,
    focus: FocusHandle,
    label: SharedString,
    value_label: Option<SharedString>,
    on_change: Option<ChangeCallback>,
}

impl EffortSlider {
    pub fn new(
        id: impl Into<ElementId>,
        option_count: usize,
        selected: usize,
        focus: &FocusHandle,
    ) -> Self {
        let id = id.into();
        Self {
            base: div().id(id.clone()).w_full().h(px(CONTROL_HEIGHT)),
            id,
            option_count,
            selected,
            focus: focus.clone(),
            label: "Reasoning effort".into(),
            value_label: None,
            on_change: None,
        }
    }

    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = label.into();
        self
    }

    pub fn value_label(mut self, label: impl Into<SharedString>) -> Self {
        self.value_label = Some(label.into());
        self
    }

    pub fn on_change(mut self, callback: impl Fn(&usize, &mut Window, &mut App) + 'static) -> Self {
        self.on_change = Some(Rc::new(callback));
        self
    }
}

impl Styled for EffortSlider {
    fn style(&mut self) -> &mut StyleRefinement {
        self.base.style()
    }
}

impl InteractiveElement for EffortSlider {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.base.interactivity()
    }
}

fn clamped_index(index: usize, count: usize) -> usize {
    index.min(count.saturating_sub(1))
}

fn fraction(index: usize, count: usize) -> f32 {
    if count < 2 {
        0.0
    } else {
        clamped_index(index, count) as f32 / (count - 1) as f32
    }
}

/// Thumb centers stay inside the canvas, including when the parent gets narrow.
fn travel(bounds: Bounds<Pixels>) -> (Pixels, Pixels) {
    let inset = px(THUMB_DIAMETER / 2.0).min(bounds.size.width / 2.0);
    (
        bounds.left() + inset,
        (bounds.size.width - inset * 2.0).max(Pixels::ZERO),
    )
}

fn index_at(position: Pixels, bounds: Bounds<Pixels>, count: usize) -> usize {
    let (start, width) = travel(bounds);
    if count < 2 || width <= Pixels::ZERO {
        return 0;
    }
    let progress = ((position - start) / width).clamp(0.0, 1.0);
    clamped_index((progress * (count - 1) as f32).round() as usize, count)
}

fn index_for_key(key: &str, selected: usize, count: usize) -> Option<usize> {
    if count < 2 {
        return None;
    }
    let selected = clamped_index(selected, count);
    match key {
        "left" | "down" => Some(selected.saturating_sub(1)),
        "right" | "up" => Some(selected.saturating_add(1).min(count - 1)),
        "home" => Some(0),
        "end" => Some(count - 1),
        _ => None,
    }
}

fn select(
    state: &SliderState,
    index: usize,
    on_change: &ChangeCallback,
    window: &mut Window,
    cx: &mut App,
) {
    // Pointer motion within a stop must not keep writing the owner's settings.
    if state.selected.replace(index) != index {
        on_change(&index, window, cx);
    }
}

impl RenderOnce for EffortSlider {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = Theme::current(cx);
        let count = self.option_count;
        let selected = clamped_index(self.selected, count);
        let disabled = count < 2 || self.on_change.is_none();
        // Keyed GPUI state survives the owner rerender caused by each selection,
        // but disappears when this slider is unmounted or its identity changes.
        let state = window
            .use_keyed_state(self.id, cx, |_, _| Rc::<SliderState>::default())
            .read(cx)
            .clone();
        state.selected.set(selected);
        if disabled {
            state.dragging.set(false);
        }

        let mut base = self
            .base
            .semantic_control(gpui::Role::Slider, disabled)
            .aria_label(self.label)
            .key_context("EffortSlider")
            .aria_numeric_value(selected as f64)
            .aria_min_numeric_value(0.0)
            .aria_max_numeric_value(count.saturating_sub(1) as f64)
            .aria_numeric_value_step(1.0)
            .aria_orientation(gpui::accesskit::Orientation::Horizontal)
            .when_some(self.value_label, |element, label| element.aria_value(label))
            .track_focus(&self.focus)
            .tab_index(0)
            .tab_stop(!disabled)
            .min_w_0()
            .flex_none()
            .rounded(px(6.0))
            .border_1()
            .border_color(gpui::transparent_black())
            .focus_visible(|style| style.border_color(theme.gauge))
            .cursor_default()
            .when(disabled, |element| element.opacity(0.55));

        if let Some(on_change) = self.on_change.filter(|_| !disabled) {
            base = base
                .on_mouse_down(MouseButton::Left, {
                    let state = state.clone();
                    let focus = self.focus;
                    let on_change = on_change.clone();
                    move |event, window, cx| {
                        window.focus(&focus, cx);
                        state.dragging.set(true);
                        let index = index_at(event.position.x, state.bounds.get(), count);
                        select(&state, index, &on_change, window, cx);
                        cx.stop_propagation();
                    }
                })
                .on_mouse_down_out({
                    let state = state.clone();
                    move |_, _, _| state.dragging.set(false)
                })
                .on_key_down({
                    let state = state.clone();
                    let on_change = on_change.clone();
                    move |event: &KeyDownEvent, window, cx| {
                        if event.keystroke.modifiers.modified() {
                            return;
                        }
                        if let Some(index) =
                            index_for_key(&event.keystroke.key, state.selected.get(), count)
                        {
                            select(&state, index, &on_change, window, cx);
                            cx.stop_propagation();
                        }
                    }
                });
            for (action, key) in [
                (gpui::AccessibleAction::Increment, "right"),
                (gpui::AccessibleAction::Decrement, "left"),
            ] {
                let state = state.clone();
                let on_change = on_change.clone();
                base = base.on_a11y_action(action, move |_, window, cx| {
                    if let Some(index) = index_for_key(key, state.selected.get(), count) {
                        select(&state, index, &on_change, window, cx);
                    }
                });
            }
            base = base.child(
                canvas(
                    {
                        let state = state.clone();
                        move |bounds, _, _| state.bounds.set(bounds)
                    },
                    move |bounds, _, window, _| {
                        paint_slider(bounds, count, selected, theme, window);
                        // Window-level listeners keep a captured drag working beyond
                        // either end of the track. Mouse-down still uses the Div's
                        // native hitbox, so an occluding popover cannot start one.
                        window.on_mouse_event({
                            let state = state.clone();
                            let on_change = on_change.clone();
                            move |event: &MouseMoveEvent, phase, window, cx| {
                                if phase != gpui::DispatchPhase::Capture || !state.dragging.get() {
                                    return;
                                }
                                if event.pressed_button != Some(MouseButton::Left) {
                                    state.dragging.set(false);
                                    return;
                                }
                                let index = index_at(event.position.x, state.bounds.get(), count);
                                select(&state, index, &on_change, window, cx);
                                cx.stop_propagation();
                            }
                        });
                        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                            if phase == gpui::DispatchPhase::Capture
                                && event.button == MouseButton::Left
                                && state.dragging.replace(false)
                            {
                                cx.stop_propagation();
                            }
                        });
                    },
                )
                .size_full(),
            );
        } else {
            base = base.child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        paint_slider(bounds, count, selected, theme, window);
                    },
                )
                .size_full(),
            );
        }
        base
    }
}

fn paint_slider(
    bounds: Bounds<Pixels>,
    count: usize,
    selected: usize,
    theme: Theme,
    window: &mut Window,
) {
    if bounds.size.width <= Pixels::ZERO || bounds.size.height <= Pixels::ZERO {
        return;
    }
    let (start, width) = travel(bounds);
    let center_y = bounds.top() + bounds.size.height / 2.0;
    let track_height = px(TRACK_HEIGHT).min(bounds.size.height);
    let track = Bounds::new(
        point(bounds.left(), center_y - track_height / 2.0),
        size(bounds.size.width, track_height),
    );
    let thumb_color = if theme.is_dark {
        theme.inverse
    } else {
        theme.composer
    };
    let paint_capsule = |bounds: Bounds<Pixels>, radius, color, window: &mut Window| {
        window.paint_quad(quad(
            bounds,
            radius,
            color,
            px(0.0),
            gpui::transparent_black(),
            BorderStyle::default(),
        ));
    };
    paint_capsule(track, track_height / 2.0, theme.overlay_strong, window);
    let progress = fraction(selected, count);
    if count > 0 {
        paint_capsule(
            Bounds::new(
                track.origin,
                size(start - bounds.left() + width * progress, track.size.height),
            ),
            track_height / 2.0,
            theme.gauge,
            window,
        );
    }
    for index in 0..count {
        let center_x = start + width * fraction(index, count);
        paint_capsule(
            Bounds::new(
                point(
                    center_x - px(STOP_DIAMETER / 2.0),
                    center_y - px(STOP_DIAMETER / 2.0),
                ),
                size(px(STOP_DIAMETER), px(STOP_DIAMETER)),
            ),
            px(STOP_DIAMETER / 2.0),
            if index <= selected {
                thumb_color.opacity(0.45)
            } else {
                theme.text_tertiary.opacity(0.65)
            },
            window,
        );
    }
    if count > 0 {
        let diameter = px(THUMB_DIAMETER)
            .min(bounds.size.width)
            .min(bounds.size.height);
        window.paint_quad(quad(
            Bounds::new(
                point(
                    start + width * progress - diameter / 2.0,
                    center_y - diameter / 2.0,
                ),
                size(diameter, diameter),
            ),
            diameter / 2.0,
            thumb_color,
            px(1.0),
            theme.border_strong,
            BorderStyle::default(),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, Modifiers, Render, TestAppContext};

    #[test]
    fn endpoints_and_degenerate_option_sets_are_clamped() {
        assert_eq!(clamped_index(usize::MAX, 0), 0);
        assert_eq!(clamped_index(usize::MAX, 1), 0);
        assert_eq!(clamped_index(usize::MAX, 5), 4);
        assert_eq!(fraction(usize::MAX, 0), 0.0);
        assert_eq!(fraction(usize::MAX, 1), 0.0);
        assert_eq!(fraction(usize::MAX, 5), 1.0);
        assert_eq!(index_for_key("right", 0, 0), None);
        assert_eq!(index_for_key("right", 0, 1), None);
        assert_eq!(index_for_key("left", 0, 5), Some(0));
        assert_eq!(index_for_key("right", usize::MAX, 5), Some(4));
        assert_eq!(index_for_key("home", 4, 5), Some(0));
        assert_eq!(index_for_key("end", 0, 5), Some(4));
        assert_eq!(index_for_key("enter", 2, 5), None);
    }

    #[test]
    fn pointer_mapping_snaps_to_nearest_stop_and_clamps_outside() {
        let bounds = Bounds::new(point(px(10.0), px(0.0)), size(px(124.0), px(28.0)));
        for (x, expected) in [
            (0.0, 0),
            (22.0, 0),
            (34.0, 0),
            (35.0, 1),
            (47.0, 1),
            (72.0, 2),
            (97.0, 3),
            (122.0, 4),
            (200.0, 4),
        ] {
            assert_eq!(index_at(px(x), bounds, 5), expected);
        }
        assert_eq!(index_at(px(100.0), bounds, 0), 0);
        assert_eq!(index_at(px(100.0), bounds, 1), 0);
        assert_eq!(index_at(px(100.0), Bounds::default(), 5), 0);
        let narrow = Bounds::new(point(px(0.0), px(0.0)), size(px(4.0), px(28.0)));
        assert_eq!(travel(narrow), (px(2.0), Pixels::ZERO));
        assert_eq!(index_at(px(100.0), narrow, 5), 0);
    }

    struct SliderFixture {
        focus: FocusHandle,
        count: usize,
        selected: usize,
        changes: Vec<usize>,
    }

    impl Render for SliderFixture {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().tab_group().key_context("WakuMenu").child(
                EffortSlider::new("effort-slider", self.count, self.selected, &self.focus)
                    .w(px(214.0))
                    .value_label(format!("Effort {}", self.selected))
                    .on_change(cx.listener(|this, index: &usize, _, cx| {
                        this.selected = *index;
                        this.changes.push(*index);
                        cx.notify();
                    })),
            )
        }
    }

    #[gpui::test]
    fn slider_keyboard_and_semantics_follow_the_controlled_value(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::input::init(cx);
            crate::ui::menu::init(cx);
        });
        let (view, cx) = cx.add_window_view(|window, cx| {
            window.set_a11y_automation(true);
            let focus = cx.focus_handle();
            window.focus(&focus, cx);
            SliderFixture {
                focus,
                count: 5,
                selected: 2,
                changes: vec![],
            }
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("right up end right left down home left");
        view.read_with(cx, |view, _| {
            assert_eq!(view.selected, 0);
            assert_eq!(view.changes, vec![3, 4, 3, 2, 0]);
        });
        cx.simulate_keystrokes("ctrl-right");
        view.read_with(cx, |view, _| assert_eq!(view.selected, 0));
        let raw = cx
            .update(|window, _| window.a11y_automation_snapshot())
            .unwrap();
        let tree: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let node = tree["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["element_id"] == "effort-slider")
            .unwrap();
        assert_eq!(node["role"], "Slider");
        assert_eq!(node["label"], "Reasoning effort");
        assert_eq!(node["value"], "Effort 0");
        assert!(
            node["actions"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("Increment"))
        );
        assert!(
            node["actions"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("Decrement"))
        );
        let node_id = gpui::accesskit::NodeId(node["node"].as_str().unwrap().parse().unwrap());
        let raw = cx
            .update(|window, _| window.debug_a11y_tree_json())
            .unwrap();
        let tree: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let node = tree["nodes"]
            .as_object()
            .unwrap()
            .values()
            .find(|node| node["element_id"] == "effort-slider")
            .unwrap();
        assert_eq!(node["aria"]["numeric_value"], 0.0);
        assert_eq!(node["aria"]["min_numeric_value"], 0.0);
        assert_eq!(node["aria"]["max_numeric_value"], 4.0);
        assert_eq!(node["aria"]["numeric_value_step"], 1.0);
        assert_eq!(node["aria"]["orientation"], "Horizontal");
        cx.update(|window, cx| {
            window.dispatch_a11y_action(node_id, gpui::AccessibleAction::Increment, None, cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.selected, 1));
        cx.update(|window, cx| {
            window.dispatch_a11y_action(node_id, gpui::AccessibleAction::Decrement, None, cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.selected, 0));
    }

    #[gpui::test]
    fn slider_drag_survives_rerenders_and_releases_outside(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| SliderFixture {
            focus: cx.focus_handle(),
            count: 5,
            selected: 0,
            changes: vec![],
        });
        cx.run_until_parked();
        cx.simulate_mouse_down(
            point(px(108.0), px(14.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.selected, 2));
        cx.simulate_mouse_move(
            point(px(500.0), px(50.0)),
            Some(MouseButton::Left),
            Modifiers::none(),
        );
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.selected, 4));
        cx.simulate_mouse_move(
            point(px(500.0), px(50.0)),
            Some(MouseButton::Left),
            Modifiers::none(),
        );
        cx.simulate_mouse_move(
            point(px(-20.0), px(50.0)),
            Some(MouseButton::Left),
            Modifiers::none(),
        );
        cx.run_until_parked();
        cx.simulate_mouse_up(
            point(px(-20.0), px(50.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_move(point(px(200.0), px(14.0)), None, Modifiers::none());
        view.read_with(cx, |view, _| {
            assert_eq!(view.selected, 0);
            assert_eq!(view.changes, vec![2, 4, 0]);
        });
    }

    #[gpui::test]
    fn empty_and_single_option_sliders_do_not_emit_changes(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|window, cx| {
            window.set_a11y_automation(true);
            let focus = cx.focus_handle();
            window.focus(&focus, cx);
            SliderFixture {
                focus,
                count: 0,
                selected: 0,
                changes: vec![],
            }
        });
        cx.run_until_parked();
        for count in [0, 1] {
            view.update(cx, |view, cx| {
                view.count = count;
                cx.notify();
            });
            cx.run_until_parked();
            cx.simulate_keystrokes("right end");
            cx.simulate_click(point(px(180.0), px(14.0)), Modifiers::none());
            view.read_with(cx, |view, _| assert!(view.changes.is_empty()));
            let raw = cx
                .update(|window, _| window.a11y_automation_snapshot())
                .unwrap();
            let tree: serde_json::Value = serde_json::from_str(&raw).unwrap();
            let node = tree["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|node| node["element_id"] == "effort-slider")
                .unwrap();
            assert_eq!(node["disabled"], true);
        }
    }
}
