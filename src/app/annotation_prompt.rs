//! The selection's comment composer. Its input entity stays mounted while the
//! pill grows into a text area; animation never swaps fields or edits the text.

use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, Context, Entity, EventEmitter, FocusHandle, MouseButton, Pixels, Render,
    Subscription, Window, div, point, prelude::*, px,
};

use crate::input::{InputEvent, InputLayoutEvent, TextInput};
use crate::theme::{Theme, sp};
use crate::ui::{ActivationExt, SemanticControlExt, icon, tooltip::Tooltip};
use crate::{DismissAnnotationEditor, FocusNext, FocusPrev};

const WIDTH: f32 = 360.0;
const MIN_INPUT: f32 = 24.0;
const MAX_INPUT: f32 = 240.0;
const PADDING: f32 = 12.0;
const FOOTER: f32 = 32.0;
const MARGIN: f32 = 8.0;
const DURATION: Duration = Duration::from_millis(180);

/// A retargetable one-shot transition. A new target starts at the current
/// position, including a reversal; unrelated edits never restart the clock.
#[derive(Clone, Copy)]
struct Transition {
    from: f32,
    target: f32,
    started: Option<Instant>,
}

impl Transition {
    fn settled(value: f32) -> Self {
        Self {
            from: value,
            target: value,
            started: None,
        }
    }

    fn sample(&self, now: Instant) -> f32 {
        let Some(started) = self.started else {
            return self.target;
        };
        let t = (now.saturating_duration_since(started).as_secs_f32() / DURATION.as_secs_f32())
            .clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - t).powi(3);
        self.from + (self.target - self.from) * eased
    }

    fn retarget(&mut self, target: f32, now: Instant, reduced: bool) {
        if reduced {
            *self = Self::settled(target);
        } else if (target - self.target).abs() > 0.01 {
            self.from = self.sample(now);
            self.target = target;
            self.started = Some(now);
        }
    }

    fn moving(&self, now: Instant) -> bool {
        self.started
            .is_some_and(|start| now.saturating_duration_since(start) < DURATION)
    }
}

pub(super) enum PromptEvent {
    Submit(String),
    Dismiss,
}

pub(super) struct AnnotationPrompt {
    input: Entity<TextInput>,
    save_focus: FocusHandle,
    selection: Bounds<Pixels>,
    safe: Option<Bounds<Pixels>>,
    measured: InputLayoutEvent,
    height: Transition,
    expansion: Transition,
    feedback: Transition,
    hovered: bool,
    pressed: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PromptEvent> for AnnotationPrompt {}

impl AnnotationPrompt {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            TextInput::new(window, cx)
                .semantic_id("annotation-comment-input")
                .tab_stop(true)
                .multi_line()
                .submit_on_enter()
                .auto_height()
                .placeholder(tr!("annotation.comment_placeholder"))
        });
        let subscriptions = vec![
            cx.subscribe(&input, |_, _, event: &InputEvent, cx| {
                if let InputEvent::Submit(text) = event {
                    cx.emit(PromptEvent::Submit(text.clone()));
                }
            }),
            cx.subscribe(&input, |this, _, event: &InputLayoutEvent, cx| {
                this.measured = *event;
                cx.notify();
            }),
        ];
        Self {
            input,
            save_focus: cx.focus_handle().tab_stop(true),
            selection: Bounds::default(),
            safe: None,
            measured: InputLayoutEvent {
                content_height: px(0.0),
                line_height: px(22.0),
            },
            height: Transition::settled(MIN_INPUT),
            expansion: Transition::settled(0.0),
            feedback: Transition::settled(0.0),
            hovered: false,
            pressed: false,
            _subscriptions: subscriptions,
        }
    }

    pub(super) fn content<'a>(&'a self, cx: &'a App) -> &'a str {
        self.input.read(cx).content()
    }

    pub(super) fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let single_line = f32::from(self.measured.line_height).max(MIN_INPUT);
        self.input.update(cx, |input, cx| {
            input.clear(cx);
            input.set_auto_height_viewport(px(single_line), cx);
        });
        // Layout events are deduplicated by natural metrics, so reopening an
        // already single-line field may not emit another one. Keep its font's
        // known line height instead of waiting for an identical measurement.
        self.measured.content_height = self.measured.line_height;
        self.height = Transition::settled(single_line);
        self.expansion = Transition::settled(0.0);
        self.feedback = Transition::settled(0.0);
        self.hovered = false;
        self.pressed = false;
        window.focus(&self.input.read(cx).focus(), cx);
        cx.notify();
    }

    pub(super) fn set_bounds(
        &mut self,
        selection: Bounds<Pixels>,
        safe: Option<Bounds<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if self.selection != selection || self.safe != safe {
            self.selection = selection;
            self.safe = safe;
            cx.notify();
        }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.submit(cx));
    }
}

/// Use the same, roomier side throughout growth. This both avoids covering the
/// selected passage and avoids flipping from above to below midway through it.
fn free_sides(selection: Bounds<Pixels>, safe: Bounds<Pixels>) -> (f32, f32) {
    (
        f32::from(selection.top() - safe.top()) - 2.0 * MARGIN,
        f32::from(safe.bottom() - selection.bottom()) - 2.0 * MARGIN,
    )
}

fn input_limit(selection: Bounds<Pixels>, safe: Bounds<Pixels>, footer: f32) -> f32 {
    let (above, below) = free_sides(selection, safe);
    let chrome = PADDING * 2.0 + footer;
    let available = above.max(below);
    // Only a selection leaving less than one usable row on both sides needs
    // the viewport fallback. Long notes otherwise scroll on the free side.
    let available = if available < chrome + MIN_INPUT {
        f32::from(safe.size.height) - MARGIN * 2.0
    } else {
        available
    };
    (available - chrome).clamp(MIN_INPUT, MAX_INPUT)
}

fn placement(
    selection: Bounds<Pixels>,
    safe: Bounds<Pixels>,
    height: f32,
    width: f32,
) -> gpui::Point<Pixels> {
    let (above, below) = free_sides(selection, safe);
    let y = if above >= below && above >= height {
        selection.top() - px(MARGIN + height)
    } else if below >= height {
        selection.bottom() + px(MARGIN)
    } else {
        (selection.top() - px(MARGIN + height))
            .max(safe.top() + px(MARGIN))
            .min((safe.bottom() - px(height + MARGIN)).max(safe.top() + px(MARGIN)))
    };
    let x = selection
        .left()
        .min(safe.right() - px(width + MARGIN))
        .max(safe.left() + px(MARGIN));
    point(x, y)
}

impl Render for AnnotationPrompt {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::current(cx);
        let now = Instant::now();
        let reduced = cx.reduce_motion();
        let safe = self
            .safe
            .unwrap_or_else(|| Bounds::new(point(px(0.0), px(0.0)), window.viewport_size()));
        let width = WIDTH
            .min(f32::from(safe.size.width) - MARGIN * 2.0)
            .max(120.0);
        let expanded = self.measured.content_height > self.measured.line_height + px(1.0);
        let footer = if expanded { FOOTER } else { 0.0 };
        let max_input = input_limit(self.selection, safe, footer);
        let target_height = f32::from(self.measured.content_height).clamp(MIN_INPUT, max_input);
        self.height.retarget(target_height, now, reduced);
        self.expansion
            .retarget(if expanded { 1.0 } else { 0.0 }, now, reduced);
        self.feedback.retarget(
            if self.pressed {
                -1.0
            } else if self.hovered {
                1.0
            } else {
                0.0
            },
            now,
            reduced,
        );
        let input_height = self.height.sample(now).min(max_input);
        let expansion = self.expansion.sample(now);
        let feedback = self.feedback.sample(now);
        let height = PADDING * 2.0 + input_height + FOOTER * expansion;
        self.input.update(cx, |input, cx| {
            input.set_auto_height_max(px(MAX_INPUT), cx);
            input.set_auto_height_viewport(px(input_height), cx);
        });
        if !reduced
            && (self.height.moving(now) || self.expansion.moving(now) || self.feedback.moving(now))
        {
            // Bounded to this small child view, only while a 180 ms edge moves.
            // No repeated animation, root notify, or work on each keystroke.
            window.request_animation_frame();
        }
        // Anchor the moving edge directly to the displayed height. Independent
        // position easing can drift across the selection when a height tween
        // is interrupted while the footer is still expanding.
        let position = placement(self.selection, safe, height, width);
        let circle = 28.0 + feedback * 2.0;
        let body = div()
            .id("annotation-comment-field")
            .semantic_control(gpui::Role::Group, false)
            .aria_label(tr!("annotation.comment_placeholder"))
            .key_context("AnnotationEditor")
            .tab_group()
            .on_action(cx.listener(|_, _: &FocusNext, window, cx| window.focus_next(cx)))
            .on_action(cx.listener(|_, _: &FocusPrev, window, cx| window.focus_prev(cx)))
            .relative()
            .w(px(width))
            .h(px(height))
            .rounded(px(24.0 - 6.0 * expansion))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.raised)
            .shadow_md()
            .occlude()
            .on_action(
                cx.listener(|_, _: &DismissAnnotationEditor, _, cx| cx.emit(PromptEvent::Dismiss)),
            )
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(PromptEvent::Dismiss)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
            .child(
                div()
                    .absolute()
                    .top(px(PADDING))
                    .left(px(14.0))
                    .right(px(50.0))
                    // Keep the same wrap width through the transition. Moving
                    // the button cannot rewrap text and oscillate the target.
                    .child(self.input.clone()),
            )
            .child(
                div()
                    .absolute()
                    .left(px(14.0))
                    .bottom(px(14.0))
                    .max_w(px((width - 70.0).max(0.0)))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(sp(10.5))
                    .text_color(theme.text_secondary)
                    .opacity(((expansion - 0.5) * 2.0).clamp(0.0, 1.0))
                    .child(if expanded || expansion > 0.0 {
                        tr!("annotation.newline_hint")
                    } else {
                        String::new()
                    }),
            )
            .child(
                div()
                    .id("annotation-comment-save")
                    .semantic_control(gpui::Role::Button, false)
                    .aria_label(tr!("annotation.add_to_chat"))
                    .absolute()
                    .right(px(8.0))
                    .bottom(px(8.0))
                    .size(px(32.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .track_focus(&self.save_focus)
                    .tab_index(0)
                    .focus_visible(|style| style.border_2().border_color(theme.accent))
                    .cursor_default()
                    .tooltip(Tooltip::text(format!(
                        "{} · {}",
                        tr!("annotation.add_to_chat"),
                        tr!("annotation.newline_hint")
                    )))
                    .on_hover(cx.listener(|this, hovered, _, cx| {
                        this.hovered = *hovered;
                        cx.notify();
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.pressed = true;
                            cx.notify();
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.pressed = false;
                            cx.notify();
                        }),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.pressed = false;
                            cx.notify();
                        }),
                    )
                    .on_activation(cx, |this, _, cx| this.submit(cx))
                    .child(
                        div()
                            .size(px(circle))
                            .rounded_full()
                            .bg(theme.inverse)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon("icons/arrow-up.svg", 16.0, theme.on_inverse)),
                    ),
            );
        gpui::deferred(
            gpui::anchored()
                .position(position)
                .snap_to_window_with_margin(px(MARGIN))
                .child(body),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{EntityInputHandler, Modifiers, TestAppContext, size};
    use std::{cell::RefCell, rc::Rc};

    #[test]
    fn growth_retargets_and_reverses_from_the_visible_height() {
        let now = Instant::now();
        let mut motion = Transition::settled(24.0);
        motion.retarget(100.0, now, false);
        let halfway = now + DURATION / 2;
        let visible = motion.sample(halfway);
        assert!(visible > 24.0 && visible < 100.0);
        motion.retarget(150.0, halfway, false);
        assert_eq!(motion.sample(halfway), visible);
        let reversal = halfway + DURATION / 4;
        let visible = motion.sample(reversal);
        motion.retarget(24.0, reversal, false);
        assert_eq!(motion.sample(reversal), visible);
        assert!(motion.sample(reversal + DURATION / 2) < visible);
        assert_eq!(motion.sample(reversal + DURATION), 24.0);
        assert!(!motion.moving(reversal + DURATION));
    }

    #[test]
    fn same_target_edits_do_not_restart_and_reduce_motion_snaps() {
        let now = Instant::now();
        let mut motion = Transition::settled(24.0);
        motion.retarget(100.0, now, false);
        motion.retarget(100.0, now + DURATION / 2, false);
        assert_eq!(motion.started, Some(now));
        assert_eq!(motion.sample(now + DURATION), 100.0);
        motion.retarget(44.0, now + DURATION, true);
        assert_eq!(motion.sample(now + DURATION), 44.0);
        assert!(!motion.moving(now + DURATION));
    }

    #[test]
    fn popup_prefers_space_clear_of_selection_and_stays_in_viewport() {
        let safe = Bounds::new(point(px(0.0), px(0.0)), size(px(800.0), px(600.0)));
        let selection = Bounds::new(point(px(100.0), px(300.0)), size(px(200.0), px(22.0)));
        assert_eq!(
            placement(selection, safe, 100.0, WIDTH),
            point(px(100.0), px(192.0))
        );
        let high = Bounds::new(point(px(700.0), px(30.0)), size(px(50.0), px(22.0)));
        assert_eq!(
            placement(high, safe, 100.0, WIDTH),
            point(px(432.0), px(60.0))
        );
    }

    fn setup(cx: &mut TestAppContext) -> (Entity<AnnotationPrompt>, &mut gpui::VisualTestContext) {
        cx.update(|cx| {
            crate::input::init(cx);
            cx.set_reduce_motion(true);
            cx.bind_keys([
                gpui::KeyBinding::new("tab", FocusNext, Some("AnnotationEditor")),
                gpui::KeyBinding::new("shift-tab", FocusPrev, Some("AnnotationEditor")),
                gpui::KeyBinding::new("escape", DismissAnnotationEditor, Some("AnnotationEditor")),
            ]);
        });
        let (prompt, cx) = cx.add_window_view(|window, cx| {
            let mut prompt = AnnotationPrompt::new(window, cx);
            prompt.set_bounds(
                Bounds::new(point(px(100.0), px(300.0)), size(px(200.0), px(22.0))),
                Some(Bounds::new(
                    point(px(0.0), px(0.0)),
                    size(px(800.0), px(600.0)),
                )),
                cx,
            );
            prompt.reset(window, cx);
            prompt
        });
        cx.run_until_parked();
        (prompt, cx)
    }

    fn events(
        prompt: &Entity<AnnotationPrompt>,
        cx: &mut gpui::VisualTestContext,
    ) -> Rc<RefCell<Vec<String>>> {
        let events = Rc::new(RefCell::new(Vec::new()));
        let sink = events.clone();
        cx.update(|_, cx| {
            cx.subscribe(prompt, move |_, event: &PromptEvent, _| {
                sink.borrow_mut().push(match event {
                    PromptEvent::Submit(text) => format!("submit:{text}"),
                    PromptEvent::Dismiss => "dismiss".to_owned(),
                });
            })
            .detach();
        });
        events
    }

    #[gpui::test]
    fn creation_prompt_wraps_and_preserves_shift_enter_and_exact_submit(cx: &mut TestAppContext) {
        let (prompt, cx) = setup(cx);
        let events = events(&prompt, cx);
        cx.simulate_input("  第一段");
        cx.simulate_keystrokes("shift-enter");
        cx.simulate_keystrokes("shift-enter");
        cx.simulate_input("Second paragraph  ");
        assert!(events.borrow().is_empty());
        cx.read_entity(&prompt, |prompt, cx| {
            assert_eq!(prompt.content(cx), "  第一段\n\nSecond paragraph  ");
            assert!(prompt.measured.content_height > prompt.measured.line_height);
            assert_eq!(prompt.expansion.target, 1.0);
        });
        cx.simulate_keystrokes("enter");
        assert_eq!(
            &*events.borrow(),
            &["submit:  第一段\n\nSecond paragraph  "]
        );
    }

    #[gpui::test]
    fn creation_prompt_soft_wrap_grows_and_deleting_contracts_without_replacing_input(
        cx: &mut TestAppContext,
    ) {
        let (prompt, cx) = setup(cx);
        let input = cx.read_entity(&prompt, |prompt, _| prompt.input.clone());
        cx.simulate_input(&"wrapping words ".repeat(20));
        cx.read_entity(&prompt, |prompt, cx| {
            assert_eq!(prompt.input.entity_id(), input.entity_id());
            assert!(!prompt.content(cx).contains('\n'));
            assert!(prompt.measured.content_height > prompt.measured.line_height);
            assert!(prompt.height.target <= MAX_INPUT);
            assert_eq!(prompt.expansion.target, 1.0);
        });
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("short");
        cx.read_entity(&prompt, |prompt, cx| {
            assert_eq!(prompt.input.entity_id(), input.entity_id());
            assert_eq!(prompt.content(cx), "short");
            assert_eq!(
                prompt.height.target,
                f32::from(prompt.measured.content_height).max(MIN_INPUT)
            );
            assert_eq!(prompt.expansion.target, 0.0);
        });
    }

    #[gpui::test]
    fn creation_prompt_arrow_click_uses_same_submit_path_and_escape_dismisses(
        cx: &mut TestAppContext,
    ) {
        let (prompt, cx) = setup(cx);
        let events = events(&prompt, cx);
        cx.simulate_input("saved by click");
        cx.simulate_click(point(px(436.0), px(268.0)), Modifiers::none());
        assert_eq!(&*events.borrow(), &["submit:saved by click"]);
        prompt.update(cx, |prompt, cx| {
            prompt.input.update(cx, |input, cx| input.clear(cx))
        });
        cx.simulate_keystrokes("escape");
        assert_eq!(events.borrow().last().unwrap(), "dismiss");
    }

    #[gpui::test]
    fn creation_prompt_composition_never_confirms_via_enter_or_arrow(cx: &mut TestAppContext) {
        let (prompt, cx) = setup(cx);
        let events = events(&prompt, cx);
        cx.update(|window, cx| {
            prompt.update(cx, |prompt, cx| {
                prompt.input.update(cx, |input, cx| {
                    input.replace_and_mark_text_in_range(None, "ni", Some(2..2), window, cx)
                });
            });
        });
        cx.simulate_keystrokes("enter");
        prompt.update(cx, |prompt, cx| prompt.submit(cx));
        assert!(events.borrow().is_empty());
        cx.update(|window, cx| {
            prompt.update(cx, |prompt, cx| {
                prompt.input.update(cx, |input, cx| {
                    input.replace_text_in_range(None, "你", window, cx)
                });
            });
        });
        prompt.update(cx, |prompt, cx| prompt.submit(cx));
        assert_eq!(&*events.borrow(), &["submit:你"]);
    }

    #[test]
    fn a_long_note_scrolls_on_the_free_side_of_a_centered_selection() {
        let safe = Bounds::new(point(px(0.0), px(0.0)), size(px(800.0), px(600.0)));
        let selection = Bounds::new(point(px(100.0), px(280.0)), size(px(200.0), px(22.0)));
        let limit = input_limit(selection, safe, FOOTER);
        assert_eq!(limit, 226.0);
        let height = limit + PADDING * 2.0 + FOOTER;
        let position = placement(selection, safe, height, WIDTH);
        assert!(position.y >= selection.bottom() + px(MARGIN));
        assert!(position.y + px(height) <= safe.bottom() - px(MARGIN));
    }

    #[gpui::test]
    fn tab_and_shift_tab_reach_input_and_arrow_and_keyboard_activation_submits(
        cx: &mut TestAppContext,
    ) {
        let (prompt, cx) = setup(cx);
        let events = events(&prompt, cx);
        cx.simulate_input("keyboard save");
        cx.simulate_keystrokes("tab");
        assert!(cx.update(|window, cx| prompt.read(cx).save_focus.is_focused(window)));
        cx.simulate_keystrokes("shift-tab");
        assert!(cx.update(|window, cx| prompt.read(cx).input.read(cx).focus().is_focused(window)));
        cx.simulate_keystrokes("tab enter");
        assert_eq!(&*events.borrow(), &["submit:keyboard save"]);
        cx.simulate_keystrokes("space");
        assert_eq!(events.borrow().len(), 2);
    }

    #[test]
    fn every_growth_frame_stays_on_one_side_of_the_selection() {
        let safe = Bounds::new(point(px(0.0), px(0.0)), size(px(800.0), px(600.0)));
        let now = Instant::now();
        for top in [90.0, 280.0, 450.0] {
            let selection = Bounds::new(point(px(100.0), px(top)), size(px(200.0), px(22.0)));
            let mut height = Transition::settled(MIN_INPUT);
            let mut expansion = Transition::settled(0.0);
            height.retarget(input_limit(selection, safe, FOOTER), now, false);
            expansion.retarget(1.0, now, false);
            for elapsed in [0, 15, 45, 90, 135, 180] {
                let at = now + Duration::from_millis(elapsed);
                let h = PADDING * 2.0 + height.sample(at) + FOOTER * expansion.sample(at);
                let pos = placement(selection, safe, h, WIDTH);
                assert!(
                    pos.y + px(h) <= selection.top() - px(MARGIN)
                        || pos.y >= selection.bottom() + px(MARGIN),
                    "frame {elapsed} at selection {top}"
                );
            }
        }
    }

    #[gpui::test]
    fn reopening_an_empty_prompt_keeps_cached_font_metrics(cx: &mut TestAppContext) {
        let (prompt, cx) = setup(cx);
        cx.simulate_input("single line");
        cx.update(|window, cx| prompt.update(cx, |prompt, cx| prompt.reset(window, cx)));
        cx.run_until_parked();
        cx.read_entity(&prompt, |prompt, cx| {
            assert_eq!(prompt.content(cx), "");
            assert_eq!(
                prompt.height.target,
                f32::from(prompt.measured.line_height).max(MIN_INPUT)
            );
            assert_eq!(prompt.expansion.target, 0.0);
        });
    }
}
