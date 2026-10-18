//! A discrete, controlled slider for the composer's reasoning-effort options.

use std::{cell::Cell, rc::Rc};

use gpui::{
    App, BorderStyle, Bounds, Context, Div, ElementId, FocusHandle, FontWeight, InteractiveElement,
    Interactivity, KeyDownEvent, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, Render,
    RenderOnce, SharedString, StyleRefinement, Styled, Window, canvas, div, point, prelude::*, px,
    quad, size,
};

use crate::theme::{Theme, sp};

use super::SemanticControlExt;

const CONTROL_HEIGHT: f32 = 34.0;
const TRACK_HEIGHT: f32 = 24.0;
const THUMB_DIAMETER: f32 = 28.0;
const STOP_DIAMETER: f32 = 3.0;

type ChangeCallback = Rc<dyn Fn(&usize, &mut Window, &mut App)>;

#[derive(Default)]
struct SliderState {
    dragging: Cell<bool>,
    preview: Cell<usize>,
    bounds: Cell<Bounds<Pixels>>,
    position: Cell<f32>,
}

/// The owner supplies the committed index and updates it in `on_change`.
///
/// Pointer movement previews a supported option locally, then commits once on
/// release. Capture loss cancels the preview; keyboard and accessibility actions
/// commit immediately. Zero or one option renders inert.
#[derive(IntoElement)]
pub struct EffortSlider {
    id: ElementId,
    base: Div,
    option_count: usize,
    selected: usize,
    focus: FocusHandle,
    label: SharedString,
    value_label: Option<SharedString>,
    value_labels: Vec<SharedString>,
    context_key: SharedString,
    on_change: Option<ChangeCallback>,
}

impl EffortSlider {
    pub fn new(
        id: impl Into<ElementId>,
        option_count: usize,
        selected: usize,
        focus: &FocusHandle,
    ) -> Self {
        Self {
            id: id.into(),
            base: div().w_full(),
            option_count,
            selected,
            focus: focus.clone(),
            label: "Reasoning effort".into(),
            value_label: None,
            value_labels: Vec::new(),
            context_key: "".into(),
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

    /// Provider-native labels, in option order. Also shows a compact heading
    /// whose preview updates without notifying the owning application view.
    pub fn value_labels(mut self, labels: Vec<SharedString>) -> Self {
        self.value_labels = labels;
        self
    }

    /// Identity of the setting's session/model context, independent of the
    /// stable accessibility id. Changing it cancels any uncommitted drag.
    pub fn context_key(mut self, key: impl Into<SharedString>) -> Self {
        self.context_key = key.into();
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

fn pointer_fraction(position: Pixels, bounds: Bounds<Pixels>) -> f32 {
    let (start, width) = travel(bounds);
    if width <= Pixels::ZERO {
        return 0.0;
    }
    ((position - start) / width).clamp(0.0, 1.0)
}

fn index_at(position: Pixels, bounds: Bounds<Pixels>, count: usize) -> usize {
    if count < 2 {
        return 0;
    }
    clamped_index(
        (pointer_fraction(position, bounds) * (count - 1) as f32).round() as usize,
        count,
    )
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

/// Only this small view is notified during pointer preview. In particular, it
/// must not be the entity returned directly by `use_keyed_state`: GPUI observes
/// that entity and forwards its notifications to the owning view.
struct SliderView {
    id: ElementId,
    count: usize,
    selected: usize,
    focus: FocusHandle,
    label: SharedString,
    value_label: Option<SharedString>,
    value_labels: Vec<SharedString>,
    context_key: SharedString,
    on_change: Option<ChangeCallback>,
    state: Rc<SliderState>,
}

impl SliderView {
    fn disabled(&self) -> bool {
        self.count < 2 || self.on_change.is_none()
    }

    fn settle(&self) {
        self.state.preview.set(self.selected);
        self.state.position.set(fraction(self.selected, self.count));
    }

    fn move_thumb(&self, position: Pixels, cx: &mut Context<Self>) {
        let preview = index_at(position, self.state.bounds.get(), self.count);
        let position = pointer_fraction(position, self.state.bounds.get());
        let changed = self.state.position.replace(position) != position;
        self.state.preview.set(preview);
        if changed {
            cx.notify();
        }
    }

    fn cancel_drag(&self, cx: &mut Context<Self>) {
        if self.state.dragging.replace(false) {
            self.settle();
            cx.notify();
        }
    }

    fn commit(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let index = clamped_index(index, self.count);
        self.state.dragging.set(false);
        if !self.disabled() && self.selected != index {
            self.selected = index;
            if let Some(on_change) = self.on_change.clone() {
                on_change(&index, window, cx);
            }
        }
        self.settle();
        cx.notify();
    }

    fn finish_drag(&mut self, position: Pixels, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.dragging.get() {
            self.move_thumb(position, cx);
            self.commit(self.state.preview.get(), window, cx);
        }
    }
}

impl RenderOnce for EffortSlider {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let id = self.id.clone();
        let focus = self.focus.clone();
        // The keyed holder never emits a notification. The inner entity owns
        // pointer/header paint, so a drag cannot trigger Waku's notify observers
        // and rebuild its otherwise cached transcript/sidebar/right panes.
        let view = window
            .use_keyed_state(self.id, cx, |_, cx| {
                cx.new(|_| SliderView {
                    id,
                    count: 0,
                    selected: 0,
                    focus,
                    label: "Reasoning effort".into(),
                    value_label: None,
                    value_labels: Vec::new(),
                    context_key: "".into(),
                    on_change: None,
                    state: Rc::default(),
                })
            })
            .read(cx)
            .clone();
        view.update(cx, |view, _| {
            let selected = clamped_index(self.selected, self.option_count);
            if view.count != self.option_count
                || view.selected != selected
                || view.value_labels != self.value_labels
                || view.context_key != self.context_key
                || self.on_change.is_none()
            {
                view.state.dragging.set(false);
            }
            view.count = self.option_count;
            view.selected = selected;
            view.focus = self.focus;
            view.label = self.label;
            view.value_label = self.value_label;
            view.value_labels = self.value_labels;
            view.context_key = self.context_key;
            view.on_change = self.on_change;
            if !view.state.dragging.get() {
                view.settle();
            }
        });
        self.base.child(view)
    }
}

impl Render for SliderView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::current(cx);
        let disabled = self.disabled();
        let preview = self.state.preview.get();
        let value_label = self
            .value_labels
            .get(preview)
            .cloned()
            .or_else(|| self.value_label.clone());
        let paint = SliderPaint {
            count: self.count,
            selected: preview,
            progress: self.state.position.get(),
        };
        let mut control = div()
            .id(self.id.clone())
            .w_full()
            .h(px(CONTROL_HEIGHT))
            .semantic_control(gpui::Role::Slider, disabled)
            .aria_label(self.label.clone())
            .key_context("EffortSlider")
            .aria_numeric_value(preview as f64)
            .aria_min_numeric_value(0.0)
            .aria_max_numeric_value(self.count.saturating_sub(1) as f64)
            .aria_numeric_value_step(1.0)
            .aria_orientation(gpui::accesskit::Orientation::Horizontal)
            .when_some(value_label.clone(), |element, label| {
                element.aria_value(label)
            })
            .track_focus(&self.focus)
            .tab_index(0)
            .tab_stop(!disabled)
            .min_w_0()
            .flex_none()
            .rounded(px(CONTROL_HEIGHT / 2.0))
            .border_1()
            .border_color(gpui::transparent_black())
            .focus_visible(|style| style.border_color(theme.text_tertiary))
            .cursor_default()
            .when(disabled, |element| element.opacity(0.55));

        if !disabled {
            control = control
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                        window.focus(&this.focus, cx);
                        this.state.dragging.set(true);
                        this.move_thumb(event.position.x, cx);
                        cx.notify();
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_down_out(cx.listener(|this, _, _, cx| this.cancel_drag(cx)))
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    if event.keystroke.modifiers.modified() {
                        return;
                    }
                    if let Some(index) =
                        index_for_key(&event.keystroke.key, this.state.preview.get(), this.count)
                    {
                        this.commit(index, window, cx);
                        cx.stop_propagation();
                    }
                }));
            for (action, key) in [
                (gpui::AccessibleAction::Increment, "right"),
                (gpui::AccessibleAction::Decrement, "left"),
            ] {
                let weak = cx.entity().downgrade();
                control = control.on_a11y_action(action, move |_, window, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        if let Some(index) =
                            index_for_key(key, this.state.preview.get(), this.count)
                        {
                            this.commit(index, window, cx);
                        }
                    });
                });
            }
        }
        let state = self.state.clone();
        let weak = cx.entity().downgrade();
        control = control.child(
            canvas(
                move |bounds, _, _| state.bounds.set(bounds),
                move |bounds, _, window, _| {
                    paint_slider(bounds, paint, theme, window);
                    if disabled {
                        return;
                    }
                    // Capture keeps an active drag working outside the rail;
                    // the Div's hitbox remains the only way to start a drag.
                    window.on_mouse_event({
                        let weak = weak.clone();
                        move |event: &MouseMoveEvent, phase, _, cx| {
                            if phase != gpui::DispatchPhase::Capture {
                                return;
                            }
                            let _ = weak.update(cx, |this, cx| {
                                if !this.state.dragging.get() {
                                    return;
                                }
                                if event.pressed_button == Some(MouseButton::Left) {
                                    this.move_thumb(event.position.x, cx);
                                } else {
                                    this.cancel_drag(cx);
                                }
                                cx.stop_propagation();
                            });
                        }
                    });
                    window.on_mouse_event({
                        let weak = weak.clone();
                        move |event: &MouseUpEvent, phase, window, cx| {
                            if phase != gpui::DispatchPhase::Capture
                                || event.button != MouseButton::Left
                            {
                                return;
                            }
                            let _ = weak.update(cx, |this, cx| {
                                if this.state.dragging.get() {
                                    this.finish_drag(event.position.x, window, cx);
                                    cx.stop_propagation();
                                }
                            });
                        }
                    });
                },
            )
            .size_full(),
        );
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .when(!self.value_labels.is_empty(), |panel| {
                panel.child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(8.0))
                        .mb(px(3.0))
                        .text_size(sp(12.0))
                        .child(
                            div()
                                .text_color(theme.text_secondary)
                                .child(self.label.clone()),
                        )
                        .children(value_label.map(|label| {
                            div()
                                .min_w_0()
                                .truncate()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(label)
                        })),
                )
            })
            .child(control)
    }
}

#[derive(Clone, Copy)]
struct SliderPaint {
    count: usize,
    selected: usize,
    progress: f32,
}

fn paint_slider(bounds: Bounds<Pixels>, paint: SliderPaint, theme: Theme, window: &mut Window) {
    if bounds.size.width <= Pixels::ZERO || bounds.size.height <= Pixels::ZERO {
        return;
    }
    let (start, width) = travel(bounds);
    let center_y = bounds.top() + bounds.size.height / 2.0;
    let track_height = px(TRACK_HEIGHT)
        .min(bounds.size.height)
        .min(bounds.size.width);
    let inset = ((px(THUMB_DIAMETER) - track_height) / 2.0).min(bounds.size.width / 2.0);
    let track = Bounds::new(
        point(bounds.left() + inset, center_y - track_height / 2.0),
        size(
            (bounds.size.width - inset * 2.0).max(Pixels::ZERO),
            track_height,
        ),
    );
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
    if paint.count > 0 {
        let filled_width =
            (start + width * paint.progress - track.left()).clamp(Pixels::ZERO, track.size.width);
        if filled_width > Pixels::ZERO {
            paint_capsule(
                Bounds::new(track.origin, size(filled_width, track_height)),
                track_height / 2.0,
                theme.text.opacity(0.14),
                window,
            );
        }
    }
    for index in 0..paint.count {
        let center_x = start + width * fraction(index, paint.count);
        let diameter = px(STOP_DIAMETER)
            .min(bounds.size.width)
            .min(bounds.size.height);
        paint_capsule(
            Bounds::new(
                point(center_x - diameter / 2.0, center_y - diameter / 2.0),
                size(diameter, diameter),
            ),
            diameter / 2.0,
            if index <= paint.selected {
                theme.text_secondary
            } else {
                theme.text_tertiary
            },
            window,
        );
    }
    if paint.count > 0 {
        let diameter = px(THUMB_DIAMETER)
            .min(bounds.size.width)
            .min(bounds.size.height);
        let thumb_x = start + width * paint.progress;
        window.paint_quad(quad(
            Bounds::new(
                point(thumb_x - diameter / 2.0, center_y - diameter / 2.0),
                size(diameter, diameter),
            ),
            diameter / 2.0,
            theme.text,
            px(1.0),
            theme.border_strong,
            BorderStyle::default(),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext};

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
        let bounds = Bounds::new(
            point(px(10.0), px(0.0)),
            size(px(140.0), px(CONTROL_HEIGHT)),
        );
        for (x, expected) in [
            (0.0, 0),
            (24.0, 0),
            (37.0, 0),
            (39.0, 1),
            (52.0, 1),
            (80.0, 2),
            (108.0, 3),
            (136.0, 4),
            (200.0, 4),
        ] {
            assert_eq!(index_at(px(x), bounds, 5), expected);
        }
        assert_eq!(index_at(px(100.0), bounds, 0), 0);
        assert_eq!(index_at(px(100.0), bounds, 1), 0);
        assert_eq!(index_at(px(100.0), Bounds::default(), 5), 0);
        let narrow = Bounds::new(point(px(0.0), px(0.0)), size(px(4.0), px(CONTROL_HEIGHT)));
        assert_eq!(travel(narrow), (px(2.0), Pixels::ZERO));
        assert_eq!(index_at(px(100.0), narrow, 5), 0);
    }

    struct SliderFixture {
        focus: FocusHandle,
        count: usize,
        selected: usize,
        changes: Vec<usize>,
        notifications: Rc<Cell<usize>>,
        value_labels: Vec<SharedString>,
        context_key: SharedString,
        mounted: bool,
    }

    impl Render for SliderFixture {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .tab_group()
                .key_context("WakuMenu")
                .when(self.mounted, |parent| {
                    parent.child(
                        EffortSlider::new("effort-slider", self.count, self.selected, &self.focus)
                            .w(px(214.0))
                            .value_label(format!("Effort {}", self.selected))
                            .value_labels(self.value_labels.clone())
                            .context_key(self.context_key.clone())
                            .on_change(cx.listener(|this, index: &usize, _, cx| {
                                this.selected = *index;
                                this.changes.push(*index);
                                cx.notify();
                            })),
                    )
                })
        }
    }

    fn fixture(cx: &mut Context<SliderFixture>, count: usize, selected: usize) -> SliderFixture {
        let notifications = Rc::new(Cell::new(0));
        let observed = notifications.clone();
        cx.observe_self(move |_, _| observed.set(observed.get() + 1))
            .detach();
        SliderFixture {
            focus: cx.focus_handle(),
            count,
            selected,
            changes: vec![],
            notifications,
            value_labels: Vec::new(),
            context_key: "".into(),
            mounted: true,
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
            let view = fixture(cx, 5, 2);
            window.focus(&view.focus, cx);
            view
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
        for action in ["Increment", "Decrement"] {
            assert!(
                node["actions"]
                    .as_array()
                    .unwrap()
                    .contains(&serde_json::json!(action))
            );
        }
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
            window.dispatch_a11y_action(node_id, gpui::AccessibleAction::Increment, None, cx)
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.selected, 1));
        cx.update(|window, cx| {
            window.dispatch_a11y_action(node_id, gpui::AccessibleAction::Decrement, None, cx)
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.selected, 0));
    }

    #[gpui::test]
    fn slider_drag_previews_locally_and_commits_once_on_outside_release(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| fixture(cx, 5, 0));
        cx.run_until_parked();
        let before = view.read_with(cx, |view, _| view.notifications.get());
        cx.simulate_mouse_down(
            point(px(108.0), px(17.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.run_until_parked();
        // Cross stops and move within one stop: neither path writes settings or
        // notifies the parent (which would fan out to the expensive app panes).
        for x in [112.0, 118.0, 160.0, 500.0] {
            cx.simulate_mouse_move(
                point(px(x), px(50.0)),
                Some(MouseButton::Left),
                Modifiers::none(),
            );
            cx.run_until_parked();
        }
        view.read_with(cx, |view, _| {
            assert_eq!(view.selected, 0);
            assert!(view.changes.is_empty());
            assert_eq!(view.notifications.get(), before);
        });
        cx.simulate_mouse_up(
            point(px(500.0), px(50.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.selected, 4);
            assert_eq!(view.changes, vec![4]);
            assert_eq!(view.notifications.get(), before + 1);
        });
        cx.simulate_mouse_move(point(px(100.0), px(17.0)), None, Modifiers::none());
        cx.simulate_click(point(px(200.0), px(17.0)), Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.changes, vec![4]));
    }

    #[gpui::test]
    fn slider_capture_loss_cancels_without_committing(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|window, cx| {
            window.set_a11y_automation(true);
            fixture(cx, 5, 1)
        });
        cx.run_until_parked();
        cx.simulate_mouse_down(
            point(px(200.0), px(17.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.run_until_parked();
        cx.simulate_mouse_move(point(px(200.0), px(50.0)), None, Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.selected, 1);
            assert!(view.changes.is_empty());
        });
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
        assert_eq!(node["aria"]["numeric_value"], 1.0);
    }

    #[gpui::test]
    fn slider_header_previews_native_labels_without_committing(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|window, cx| {
            window.set_a11y_automation(true);
            let mut view = fixture(cx, 5, 0);
            view.value_labels = (0..view.count)
                .map(|index| format!("Effort {index}").into())
                .collect();
            view
        });
        cx.run_until_parked();
        cx.simulate_mouse_down(
            point(px(108.0), px(40.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.run_until_parked();
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
        assert_eq!(node["value"], "Effort 2");
        view.read_with(cx, |view, _| assert!(view.changes.is_empty()));
        cx.simulate_mouse_up(
            point(px(108.0), px(40.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.changes, vec![2]));
    }

    #[gpui::test]
    fn slider_external_option_changes_cancel_pending_preview(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| fixture(cx, 5, 0));
        cx.run_until_parked();
        for change in 0..3 {
            cx.simulate_mouse_down(
                point(px(200.0), px(17.0)),
                MouseButton::Left,
                Modifiers::none(),
            );
            cx.run_until_parked();
            view.update(cx, |view, cx| {
                match change {
                    0 => view.selected = 1,
                    1 => view.count = 3,
                    _ => view.value_labels = vec!["Low".into(), "Medium".into(), "High".into()],
                }
                cx.notify();
            });
            cx.run_until_parked();
            cx.simulate_mouse_up(
                point(px(200.0), px(50.0)),
                MouseButton::Left,
                Modifiers::none(),
            );
            cx.run_until_parked();
            view.read_with(cx, |view, _| {
                assert_eq!(view.selected, 1);
                assert!(view.changes.is_empty());
            });
        }
    }

    #[gpui::test]
    fn slider_new_context_cancels_preview_with_identical_options(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|window, cx| {
            window.set_a11y_automation(true);
            fixture(cx, 5, 0)
        });
        cx.run_until_parked();
        cx.simulate_mouse_down(
            point(px(200.0), px(17.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.run_until_parked();
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
        assert_eq!(node["aria"]["numeric_value"], 4.0);
        // Only domain identity changes. Equal option count, labels and selected
        // value must not allow the previous session/model's drag to commit.
        view.update(cx, |view, cx| {
            view.context_key = "different-session-or-model".into();
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_mouse_up(
            point(px(200.0), px(17.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.selected, 0);
            assert!(view.changes.is_empty());
        });
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
    }

    #[gpui::test]
    fn slider_outside_press_and_remount_discard_uncommitted_preview(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| fixture(cx, 5, 0));
        cx.run_until_parked();
        cx.simulate_mouse_down(
            point(px(200.0), px(17.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.run_until_parked();
        cx.simulate_mouse_down(
            point(px(400.0), px(80.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_up(
            point(px(400.0), px(80.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert!(view.changes.is_empty()));
        cx.simulate_mouse_down(
            point(px(200.0), px(17.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            view.mounted = false;
            cx.notify();
        });
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            view.mounted = true;
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_mouse_up(
            point(px(200.0), px(17.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.selected, 0);
            assert!(view.changes.is_empty());
        });
    }

    #[gpui::test]
    fn empty_and_single_option_sliders_do_not_emit_changes(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|window, cx| {
            window.set_a11y_automation(true);
            let view = fixture(cx, 0, 0);
            window.focus(&view.focus, cx);
            view
        });
        cx.run_until_parked();
        for count in [0, 1] {
            view.update(cx, |view, cx| {
                view.count = count;
                cx.notify();
            });
            cx.run_until_parked();
            cx.simulate_keystrokes("right end");
            cx.simulate_click(point(px(180.0), px(17.0)), Modifiers::none());
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
