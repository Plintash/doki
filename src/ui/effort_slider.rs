//! A discrete, controlled slider for the composer's reasoning-effort options.

use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    App, BorderStyle, Bounds, Div, ElementId, EntityId, FocusHandle, InteractiveElement,
    Interactivity, KeyDownEvent, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, RenderOnce,
    SharedString, Stateful, StyleRefinement, Styled, Window, canvas, div, point, prelude::*, px,
    quad, size,
};

use crate::theme::Theme;

use super::SemanticControlExt;

const CONTROL_HEIGHT: f32 = 28.0;
const TRACK_HEIGHT: f32 = 3.0;
const THUMB_DIAMETER: f32 = 12.0;
const STOP_DIAMETER: f32 = 4.0;
const SNAP_DURATION: Duration = Duration::from_millis(120);
const GALAXY_ARRIVAL: Duration = Duration::from_millis(420);

/// A visual treatment of the provider's real final option, never a new setting.
fn highest_effort(index: usize, count: usize, highest: Option<usize>) -> bool {
    count > 1 && highest.is_some_and(|highest| highest < count && index == highest)
}

fn arrival_progress(elapsed: Duration, reduced_motion: bool) -> Option<f32> {
    (!reduced_motion && elapsed < GALAXY_ARRIVAL)
        .then(|| elapsed.as_secs_f32() / GALAXY_ARRIVAL.as_secs_f32())
}

#[derive(Clone, Copy)]
struct SnapTransition {
    from: f32,
    started: Instant,
}

fn snap_value(from: f32, to: f32, elapsed: Duration) -> Option<f32> {
    let progress = elapsed.as_secs_f32() / SNAP_DURATION.as_secs_f32();
    (progress < 1.0).then(|| from + (to - from) * (1.0 - (1.0 - progress).powi(3)))
}

type ChangeCallback = Rc<dyn Fn(&usize, &mut Window, &mut App)>;

#[derive(Default)]
struct SliderState {
    dragging: Cell<bool>,
    selected: Cell<usize>,
    bounds: Cell<Bounds<Pixels>>,
    position: Cell<f32>,
    initialized: Cell<bool>,
    snap: Cell<Option<SnapTransition>>,
    highest: Cell<bool>,
    arrival: Cell<Option<Instant>>,
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
    highest_index: Option<usize>,
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
            highest_index: None,
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

    /// The caller identifies the real highest effort. A catalog may append
    /// orchestration modes after it, so list position alone is not sufficient.
    pub fn highest_index(mut self, index: Option<usize>) -> Self {
        self.highest_index = index;
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

fn finish_drag(state: &SliderState, owner: EntityId, cx: &mut App) {
    if state.dragging.replace(false) {
        state.snap.set(Some(SnapTransition {
            from: state.position.get(),
            started: Instant::now(),
        }));
        cx.notify(owner);
    }
}

fn move_thumb(state: &SliderState, position: Pixels, owner: EntityId, cx: &mut App) {
    let position = pointer_fraction(position, state.bounds.get());
    if state.position.replace(position) != position {
        cx.notify(owner);
    }
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
        if !state.dragging.get() {
            state.snap.set(Some(SnapTransition {
                from: state.position.get(),
                started: Instant::now(),
            }));
        }
        on_change(&index, window, cx);
    }
}

struct SliderMotion {
    position: f32,
    active: bool,
    arrival: Option<f32>,
    needs_frame: bool,
}

fn advance_motion(
    state: &SliderState,
    target: f32,
    now: Instant,
    reduced_motion: bool,
    disabled: bool,
) -> SliderMotion {
    if disabled {
        state.dragging.set(false);
    }
    if disabled || reduced_motion {
        state.snap.set(None);
        state.arrival.set(None);
    }
    let arrival = state.arrival.get().and_then(|started| {
        arrival_progress(now.saturating_duration_since(started), reduced_motion)
    });
    if arrival.is_none() {
        state.arrival.set(None);
    }
    if !state.dragging.get() {
        if let Some(value) = state.snap.get().and_then(|snap| {
            snap_value(
                snap.from,
                target,
                now.saturating_duration_since(snap.started),
            )
        }) {
            state.position.set(value);
        } else {
            state.snap.set(None);
            state.position.set(target);
        }
    }
    SliderMotion {
        position: state.position.get(),
        active: state.dragging.get() || state.snap.get().is_some(),
        arrival,
        needs_frame: state.snap.get().is_some() || arrival.is_some(),
    }
}

impl RenderOnce for EffortSlider {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = Theme::current(cx);
        let owner = window.current_view();
        let count = self.option_count;
        let selected = clamped_index(self.selected, count);
        let disabled = count < 2 || self.on_change.is_none();
        // Keyed GPUI state survives the owner rerender caused by each selection,
        // but disappears when this slider is unmounted or its identity changes.
        let state = window
            .use_keyed_state(self.id, cx, |_, _| Rc::<SliderState>::default())
            .read(cx)
            .clone();
        let target = fraction(selected, count);
        let initialized = state.initialized.replace(true);
        if !initialized {
            state.position.set(target);
        } else if state.selected.get() != selected && !state.dragging.get() {
            state.snap.set(Some(SnapTransition {
                from: state.position.get(),
                started: Instant::now(),
            }));
        }
        state.selected.set(selected);
        let highest = highest_effort(selected, count, self.highest_index);
        if initialized && !state.highest.get() && highest {
            state.arrival.set(Some(Instant::now()));
        }
        state.highest.set(highest);
        if !highest {
            state.arrival.set(None);
        }
        let motion = advance_motion(&state, target, Instant::now(), cx.reduce_motion(), disabled);
        if motion.needs_frame {
            // Transient feedback uses the existing self-parking 30 Hz clock,
            // never a repeating or display-rate window animation.
            super::motion::pulse_lease(owner, cx);
        }
        let paint = SliderPaint {
            count,
            selected,
            progress: motion.position,
            active: motion.active,
            highest,
            arrival: motion.arrival,
        };

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
            .focus_visible(|style| style.border_color(theme.text_tertiary))
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
                        state.snap.set(None);
                        move_thumb(&state, event.position.x, owner, cx);
                        // Press feedback is visible even when the selected detent
                        // and pointer position did not change.
                        cx.notify(owner);
                        let index = index_at(event.position.x, state.bounds.get(), count);
                        select(&state, index, &on_change, window, cx);
                        cx.stop_propagation();
                    }
                })
                .on_mouse_down_out({
                    let state = state.clone();
                    move |_, _, cx| finish_drag(&state, owner, cx)
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
                            state.dragging.set(false);
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
                        paint_slider(bounds, paint, theme, window);
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
                                    finish_drag(&state, owner, cx);
                                    return;
                                }
                                move_thumb(&state, event.position.x, owner, cx);
                                let index = index_at(event.position.x, state.bounds.get(), count);
                                select(&state, index, &on_change, window, cx);
                                cx.stop_propagation();
                            }
                        });
                        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                            if phase == gpui::DispatchPhase::Capture
                                && event.button == MouseButton::Left
                                && state.dragging.get()
                            {
                                finish_drag(&state, owner, cx);
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
                        paint_slider(bounds, paint, theme, window);
                    },
                )
                .size_full(),
            );
        }
        base
    }
}

#[derive(Clone, Copy)]
struct SliderPaint {
    count: usize,
    selected: usize,
    progress: f32,
    active: bool,
    highest: bool,
    arrival: Option<f32>,
}

fn paint_slider(bounds: Bounds<Pixels>, paint: SliderPaint, theme: Theme, window: &mut Window) {
    let SliderPaint {
        count,
        selected,
        progress,
        active,
        highest,
        arrival,
    } = paint;
    if bounds.size.width <= Pixels::ZERO || bounds.size.height <= Pixels::ZERO {
        return;
    }
    let (start, width) = travel(bounds);
    let center_y = bounds.top() + bounds.size.height / 2.0;
    let track_height = px(TRACK_HEIGHT).min(bounds.size.height);
    let track = Bounds::new(
        point(start, center_y - track_height / 2.0),
        size(width, track_height),
    );
    let thumb_color = theme.text;
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
    if count > 0 {
        paint_capsule(
            Bounds::new(track.origin, size(width * progress, track.size.height)),
            track_height / 2.0,
            theme.text_tertiary,
            window,
        );
    }
    if highest {
        for index in 1..24 {
            let x = start + width * progress * (index as f32 / 24.0);
            let y = center_y + px(if index % 3 == 0 { -2.0 } else { 2.0 });
            paint_capsule(
                Bounds::new(point(x, y), size(px(1.0), px(1.0))),
                px(0.5),
                theme.text.opacity(if index % 3 == 0 { 0.28 } else { 0.16 }),
                window,
            );
        }
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
                theme.text_secondary
            } else {
                theme.text_tertiary
            },
            window,
        );
    }
    if count > 0 {
        let diameter = px(THUMB_DIAMETER)
            .min(bounds.size.width)
            .min(bounds.size.height);
        let flourish = arrival
            .map(|progress| (std::f32::consts::PI * progress).sin())
            .unwrap_or(0.0);
        if active || flourish > 0.0 {
            let halo = (diameter + px(8.0))
                .min(bounds.size.height)
                .min(bounds.size.width);
            let center_x = (start + width * progress)
                .clamp(bounds.left() + halo / 2.0, bounds.right() - halo / 2.0);
            paint_capsule(
                Bounds::new(
                    point(center_x - halo / 2.0, center_y - halo / 2.0),
                    size(halo, halo),
                ),
                halo / 2.0,
                theme.text.opacity(0.09 + flourish * 0.08),
                window,
            );
        }
        if let Some(phase) = arrival.filter(|_| bounds.size.width >= px(4.0)) {
            let center_x = start + width * progress;
            for (dx, dy) in [
                (-20.0, -8.0),
                (-14.0, 9.0),
                (-7.0, -11.0),
                (5.0, 10.0),
                (12.0, -7.0),
                (19.0, 5.0),
            ] {
                let x = (center_x + px(dx * (1.0 - phase)))
                    .clamp(bounds.left() + px(2.0), bounds.right() - px(2.0));
                let y = center_y + px(dy * (1.0 - phase));
                let color = theme.text.opacity(flourish * 0.45);
                for (w, h) in [(3.0, 1.0), (1.0, 3.0)] {
                    paint_capsule(
                        Bounds::new(point(x - px(w / 2.0), y - px(h / 2.0)), size(px(w), px(h))),
                        px(0.5),
                        color,
                        window,
                    );
                }
            }
        }
        let thumb_width = (diameter + px(flourish * 2.0)).min(bounds.size.width);
        let thumb_height = (diameter - px(flourish)).max(px(0.0));
        let thumb_x = (start + width * progress).clamp(
            bounds.left() + thumb_width / 2.0,
            bounds.right() - thumb_width / 2.0,
        );
        window.paint_quad(quad(
            Bounds::new(
                point(thumb_x - thumb_width / 2.0, center_y - thumb_height / 2.0),
                size(thumb_width, thumb_height),
            ),
            thumb_height / 2.0,
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
            (16.0, 0),
            (29.0, 0),
            (31.0, 1),
            (44.0, 1),
            (72.0, 2),
            (100.0, 3),
            (128.0, 4),
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

    #[test]
    fn drag_position_is_continuous_while_the_value_stays_discrete() {
        let bounds = Bounds::new(point(px(10.0), px(0.0)), size(px(124.0), px(28.0)));
        let first = pointer_fraction(px(55.0), bounds);
        let second = pointer_fraction(px(60.0), bounds);
        assert!(second > first);
        assert_eq!(index_at(px(55.0), bounds, 5), 1);
        assert_eq!(index_at(px(60.0), bounds, 5), 2);
        assert_eq!(pointer_fraction(px(-100.0), bounds), 0.0);
        assert_eq!(pointer_fraction(px(500.0), bounds), 1.0);
    }

    #[test]
    fn detent_transition_is_bounded_and_retargets_from_the_visible_position() {
        assert_eq!(snap_value(0.4, 0.5, Duration::ZERO), Some(0.4));
        let middle = snap_value(0.4, 0.5, SNAP_DURATION / 2).unwrap();
        assert!(middle > 0.45 && middle < 0.5);
        assert_eq!(snap_value(middle, 0.25, Duration::ZERO), Some(middle));
        assert_eq!(snap_value(0.4, 0.5, SNAP_DURATION), None);
        assert_eq!(snap_value(0.4, 0.5, SNAP_DURATION * 2), None);
    }

    #[test]
    fn galaxy_is_only_the_real_highest_choice_and_arrival_always_retires() {
        assert!(!highest_effort(0, 0, Some(3)));
        assert!(!highest_effort(0, 1, Some(3)));
        assert!(!highest_effort(2, 4, Some(3)));
        assert!(highest_effort(3, 4, Some(3)));
        assert!(!highest_effort(4, 4, Some(3)));
        assert!(!highest_effort(3, 4, None));
        assert!(highest_effort(4, 6, Some(4)));
        assert!(!highest_effort(5, 6, Some(4)));
        assert_eq!(arrival_progress(Duration::ZERO, false), Some(0.0));
        assert_eq!(arrival_progress(GALAXY_ARRIVAL / 2, false), Some(0.5));
        assert_eq!(arrival_progress(GALAXY_ARRIVAL, false), None);
        assert_eq!(arrival_progress(Duration::ZERO, true), None);
    }

    #[test]
    fn settled_and_reduced_motion_states_do_not_request_idle_frames() {
        let now = Instant::now();
        let state = SliderState::default();
        state.position.set(0.7);
        state.snap.set(Some(SnapTransition {
            from: 0.7,
            started: now,
        }));
        state.arrival.set(Some(now));
        assert!(advance_motion(&state, 1.0, now, false, false).needs_frame);
        let settled = advance_motion(&state, 1.0, now + GALAXY_ARRIVAL, false, false);
        assert_eq!(settled.position, 1.0);
        assert!(!settled.needs_frame);
        assert!(!settled.active);
        assert!(settled.arrival.is_none());
        assert!(!advance_motion(&state, 1.0, now + GALAXY_ARRIVAL * 10, false, false).needs_frame);
        for (reduced, disabled) in [(true, false), (false, true)] {
            state.snap.set(Some(SnapTransition {
                from: 0.7,
                started: now,
            }));
            state.arrival.set(Some(now));
            let motion = advance_motion(&state, 1.0, now, reduced, disabled);
            assert_eq!(motion.position, 1.0);
            assert!(!motion.needs_frame);
            assert!(motion.arrival.is_none());
        }
    }

    #[test]
    fn pointer_following_needs_no_timer_and_cancel_settles_at_the_selected_value() {
        let now = Instant::now();
        let state = SliderState::default();
        state.position.set(0.43);
        state.dragging.set(true);
        let dragging = advance_motion(&state, 0.5, now, false, false);
        assert_eq!(dragging.position, 0.43);
        assert!(!dragging.needs_frame);
        // The capture-cancel path keeps the last discrete value and settles
        // from the visible pointer position, never reverting session settings.
        state.dragging.set(false);
        state.snap.set(Some(SnapTransition {
            from: state.position.get(),
            started: now,
        }));
        let canceled = advance_motion(&state, 0.5, now + SNAP_DURATION, false, false);
        assert_eq!(canceled.position, 0.5);
        assert!(!canceled.needs_frame);
        assert!(!canceled.active);
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
                    .highest_index(self.count.checked_sub(1))
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
