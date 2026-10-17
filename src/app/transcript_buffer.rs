//! Transcript-owned runway. The extent moves continuously; actual text never
//! clips to it. A burst may briefly outrun following, but cannot move an
//! explicit reader anchor or turn a layout-height step into a scroll step.
use std::time::Instant;

const RATE_DECAY_SECONDS: f32 = 0.6;
const RUNWAY_SECONDS: f32 = 0.35;
const MIN_RUNWAY: f32 = 32.0;
const MAX_RUNWAY: f32 = 180.0;
const FOLLOW_RESPONSE: f32 = 12.0;
const MAX_VIEWPORTS_PER_SECOND: f32 = 2.0;

#[derive(Clone, Copy, Debug)]
pub(super) struct BufferFrame {
    /// Viewport-relative coordinates. Offscreen folds can change document
    /// offsets without moving the reader; those changes are not generation.
    pub content_bottom: Option<f32>,
    pub anchor_top: Option<f32>,
    pub viewport_height: f32,
    pub width: f32,
    pub running: bool,
    pub following: bool,
    pub reduce_motion: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct BufferLayout {
    pub padding: f32,
    pub scroll_by: Option<f32>,
    pub animating: bool,
}

#[derive(Debug, Default)]
pub(super) struct TranscriptBuffer {
    content: Option<f32>,
    extent: f32,
    target: f32,
    floor: f32,
    padding: f32,
    rate: f32,
    at: Option<Instant>,
    geometry: Option<(f32, f32)>,
    reflow_pending: bool,
    settled: bool,
    replenish_armed: bool,
    upward_scroll: bool,
    provisional_consumed: f32,
}

impl TranscriptBuffer {
    pub fn reset(&mut self, provisional_padding: f32) {
        *self = Self {
            padding: provisional_padding.max(0.0),
            ..Self::default()
        };
    }

    pub fn padding(&self) -> f32 {
        self.padding
    }
    pub fn initialized(&self) -> bool {
        self.content.is_some()
    }
    pub fn max_scroll(&self, scrolled: f32, viewport_height: f32) -> Option<f32> {
        (!self.reflow_pending)
            .then_some(self.content)
            .flatten()
            .map(|content| (scrolled + content + self.padding - viewport_height).max(0.0))
    }
    pub fn may_resume_following(&self) -> bool {
        !self.upward_scroll
    }
    pub fn resume_following(&mut self) {
        self.upward_scroll = false;
    }
    pub fn adopt_position(&mut self) {
        self.reflow_pending = true;
        self.rate = 0.0;
        self.replenish_armed = false;
    }

    fn translate(&mut self, delta: f32) {
        self.content = self.content.map(|content| content + delta);
        self.extent += delta;
        self.target += delta;
        self.floor += delta;
    }

    pub fn cap_padding(&mut self, maximum: f32) {
        self.shrink((self.padding - maximum.max(0.0)).max(0.0));
    }

    fn shrink(&mut self, consumed: f32) {
        self.padding -= consumed;
        self.extent -= consumed;
        self.target -= consumed;
        self.floor -= consumed;
    }

    /// The list already applied this movement. Only shorten its trailing
    /// padding; never scroll a second time. Reverse motion cannot add it back.
    pub fn consume_scroll(&mut self, before: f32, after: f32) {
        let movement = before - after;
        self.translate(movement);
        if movement.abs() > 0.01 {
            self.upward_scroll = movement > 0.0;
        }
        if movement > 0.0 {
            if self.content.is_none() {
                self.provisional_consumed += movement;
            }
            let consumed = movement.min(self.padding);
            self.shrink(consumed);
            self.replenish_armed = false;
        }
    }

    pub fn update(&mut self, frame: BufferFrame, now: Instant) -> BufferLayout {
        let mut layout = self.advance(frame, now);
        // GPUI omits all row prepaint if list padding fills the viewport.
        // A shorter resized viewport must remain paintable, even when the
        // previous prompt reservation was larger than its new height.
        let cap = (frame.viewport_height - 1.0).max(0.0);
        if self.padding > cap {
            self.shrink(self.padding - cap);
            layout.padding = self.padding;
            layout.scroll_by = None;
        }
        if let Some(delta) = layout.scroll_by {
            // The caller applies exactly this incremental movement. Retain
            // geometry in the resulting viewport for the next measurement.
            self.translate(-delta);
        }
        layout
    }

    fn advance(&mut self, frame: BufferFrame, now: Instant) -> BufferLayout {
        let elapsed = self
            .at
            .replace(now)
            .map_or(0.0, |at| now.saturating_duration_since(at).as_secs_f32());
        // An app resumed after a stall should catch up over normal frames,
        // rather than pricing the whole elapsed interval into one movement.
        let dt = elapsed.min(0.05);
        let geometry = (frame.width, frame.viewport_height);
        if self
            .geometry
            .replace(geometry)
            .is_some_and(|old| old != geometry)
        {
            self.adopt_position();
        }
        let Some(content) = frame.content_bottom.or(self.content) else {
            return BufferLayout {
                padding: self.padding,
                ..BufferLayout::default()
            };
        };
        if self.content.is_none() {
            let Some(anchor) = frame.anchor_top else {
                return BufferLayout {
                    padding: self.padding,
                    ..BufferLayout::default()
                };
            };
            self.floor = anchor + frame.viewport_height;
            self.padding = (self.floor - content - self.provisional_consumed).max(0.0);
            self.extent = content + self.padding;
            self.floor = self.extent;
            self.target = self.extent;
            self.content = Some(content);
            self.settled = !frame.running;
            self.replenish_armed = self.provisional_consumed <= 0.01;
            return BufferLayout {
                padding: self.padding,
                scroll_by: frame.following.then_some(anchor),
                animating: false,
            };
        }

        // Reflow is not generation. Keep the remaining reserve (including its
        // consumed amount), adopt fresh geometry, and leave this frame's
        // logical reader anchor alone. Wait through unmeasured reflow frames.
        if self.reflow_pending {
            if frame.content_bottom.is_some() {
                self.content = Some(content);
                self.extent = if frame.following {
                    frame.viewport_height.min(content + self.padding)
                } else {
                    content + self.padding
                };
                self.target = content + self.padding;
                self.floor = self.floor.min(self.extent);
                self.reflow_pending = false;
            }
            return BufferLayout {
                padding: self.padding,
                ..BufferLayout::default()
            };
        }

        let previous_content = self.content.replace(content).unwrap_or(content);
        let growth = (content - previous_content).max(0.0);
        self.rate = (self.rate * (-elapsed / RATE_DECAY_SECONDS).exp()
            + growth / RATE_DECAY_SECONDS)
            .min(2400.0);

        if !frame.following {
            // New text spends the existing runway while the reader is away.
            // A folded row or resize must not replenish a consumed reserve.
            self.padding = self.padding.min((self.extent - content).max(0.0));
            self.extent = content + self.padding;
            self.target = self.extent;
            self.floor = self.floor.min(self.extent);
            self.rate = 0.0;
            self.settled |= !frame.running;
            return BufferLayout {
                padding: self.padding,
                ..BufferLayout::default()
            };
        }

        if !frame.running {
            if !self.settled {
                // Keep the visible extent when working rows fold away. Do
                // not retract the runway just because the provider finished.
                self.target = self.extent.max(content);
                self.settled = true;
            } else {
                // The final footer may be measured one frame after finish.
                self.target = self.target.max(content);
            }
        } else if !self.settled {
            self.replenish_armed |= growth > 0.0;
            let runway = (MIN_RUNWAY + self.rate * RUNWAY_SECONDS)
                .min(MAX_RUNWAY.min(frame.viewport_height * 0.3));
            self.target = if self.replenish_armed {
                self.floor.max(content + runway)
            } else {
                self.extent.max(content)
            };
        }
        // Never pull the viewport backward when generation slows or pauses.
        let target = self.target.max(self.extent);
        if frame.reduce_motion {
            self.extent = target;
        } else {
            let advance = (target - self.extent) * (1.0 - (-FOLLOW_RESPONSE * dt).exp());
            // An entire response can arrive in one provider batch. Bound
            // motion by viewport distance as well as response time, so that
            // a very large backlog cannot become a multi-screen frame jump.
            let maximum = frame.viewport_height.max(1.0) * MAX_VIEWPORTS_PER_SECOND * dt;
            self.extent += advance.min(maximum);
            if target - self.extent < 0.1 {
                self.extent = target;
            }
        }
        self.padding = (self.extent - content).max(0.0);
        BufferLayout {
            padding: self.padding,
            scroll_by: Some((self.extent - frame.viewport_height).max(0.0)),
            animating: !frame.reduce_motion && target - self.extent > 0.1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct Rig {
        buffer: TranscriptBuffer,
        start: Instant,
        scroll: f32,
    }
    impl Rig {
        fn new() -> Self {
            Self {
                buffer: TranscriptBuffer::default(),
                start: Instant::now(),
                scroll: 0.0,
            }
        }
        fn tick(&mut self, mut frame: BufferFrame, ms: u64) -> BufferLayout {
            frame.content_bottom = frame.content_bottom.map(|height| height - self.scroll);
            frame.anchor_top = frame.anchor_top.map(|height| height - self.scroll);
            let layout = self
                .buffer
                .update(frame, self.start + Duration::from_millis(ms));
            self.scroll += layout.scroll_by.unwrap_or(0.0);
            layout
        }
        fn scroll_to(&mut self, after: f32) {
            self.buffer.consume_scroll(self.scroll, after);
            self.scroll = after;
        }
    }
    fn frame(content: f32) -> BufferFrame {
        BufferFrame {
            content_bottom: Some(content),
            anchor_top: Some(100.0),
            viewport_height: 600.0,
            width: 720.0,
            running: true,
            following: true,
            reduce_motion: false,
        }
    }
    fn simulate(rate: f32, fps: u64) -> (Rig, f32) {
        let mut rig = Rig::new();
        rig.tick(frame(650.0), 0);
        let mut largest_step: f32 = 0.0;
        for n in 1..=fps * 4 {
            let content = 650.0 + (n as f32 / fps as f32 / 0.12).floor() * 0.12 * rate;
            let layout = rig.tick(frame(content), n * 1000 / fps);
            let step = layout.scroll_by.unwrap();
            largest_step = largest_step.max(step);
            assert!(step >= 0.0, "slower generation must not reverse following");
            assert!(layout.padding >= 0.0 && layout.padding <= 180.1);
        }
        (rig, largest_step)
    }

    #[test]
    fn slow_and_fast_streams_have_bounded_rate_adaptive_runway() {
        let (slow, slow_step) = simulate(24.0, 60);
        let (fast, fast_step) = simulate(400.0, 60);
        assert!(fast.buffer.padding > slow.buffer.padding + 20.0);
        assert!(slow_step < 4.0);
        assert!(
            fast_step < 20.0,
            "48px commits should not become 48px jumps: {fast_step}"
        );
    }

    #[test]
    fn rate_estimate_is_independent_of_animation_cadence() {
        let (thirty, _) = simulate(160.0, 30);
        let (sixty, _) = simulate(160.0, 60);
        let (one_twenty, _) = simulate(160.0, 120);
        assert!((thirty.buffer.rate - one_twenty.buffer.rate).abs() < 8.0);
        assert!((sixty.buffer.padding - one_twenty.buffer.padding).abs() < 5.0);
    }

    #[test]
    fn burst_catches_up_without_clipping_or_a_layout_sized_scroll_step() {
        let mut rig = Rig::new();
        rig.tick(frame(650.0), 0);
        let burst = rig.tick(frame(1100.0), 16);
        assert_eq!(burst.padding, 0.0);
        assert!(rig.scroll < 210.0);
        assert!(burst.animating);
        for n in 2..180 {
            rig.tick(frame(1100.0), n * 16);
        }
        assert!(rig.scroll + 600.0 >= 1100.0);
    }

    #[test]
    fn an_entire_response_burst_is_slew_limited_through_completion() {
        let mut rig = Rig::new();
        rig.tick(frame(650.0), 0);
        let finished = BufferFrame {
            running: false,
            ..frame(10_000.0)
        };
        for n in 1..800 {
            let layout = rig.tick(finished, n * 16);
            assert!(
                layout.scroll_by.unwrap() <= 19.21,
                "one frame cannot jump multiple screens"
            );
        }
        assert!(rig.scroll + 600.0 >= 9999.9);
        assert!(!rig.tick(finished, 13_000).animating);
    }

    #[test]
    fn completion_and_interruption_retain_blank_without_a_snap() {
        for finished_height in [650.0, 610.0] {
            let mut rig = Rig::new();
            rig.tick(frame(650.0), 0);
            let before = rig.scroll;
            let finished = BufferFrame {
                running: false,
                ..frame(finished_height)
            };
            let after = rig.tick(finished, 16);
            assert_eq!(before, rig.scroll);
            assert_eq!(finished_height + after.padding, 700.0);
            assert!(!after.animating);
            let later = rig.tick(finished, 10_000);
            assert_eq!(after.padding, later.padding);
        }
    }

    #[test]
    fn manual_scroll_consumes_only_actual_upward_movement_and_never_restores_it() {
        let mut rig = Rig::new();
        rig.tick(frame(300.0), 0);
        rig.scroll_to(60.0);
        assert_eq!(rig.buffer.padding(), 360.0);
        assert!(!rig.buffer.may_resume_following());
        rig.scroll_to(90.0);
        assert_eq!(rig.buffer.padding(), 360.0);
        assert!(rig.buffer.may_resume_following());
        rig.scroll_to(0.0);
        assert_eq!(rig.buffer.padding(), 270.0);
        rig.scroll_to(0.0);
        assert_eq!(rig.buffer.padding(), 270.0);
        assert!(!rig.buffer.may_resume_following());
        let detached = BufferFrame {
            following: false,
            running: false,
            ..frame(300.0)
        };
        let layout = rig.tick(detached, 30);
        assert_eq!(layout.padding, 270.0);
        assert_eq!(layout.scroll_by, None);
        assert!(!layout.animating);
    }

    #[test]
    fn reverse_scroll_cannot_replenish_without_new_generation() {
        let (mut rig, _) = simulate(160.0, 60);
        let content = 650.0 + (4.0_f32 / 0.12).floor() * 0.12 * 160.0;
        let consumed = rig.buffer.padding();
        rig.scroll_to(rig.scroll - consumed);
        rig.tick(
            BufferFrame {
                following: false,
                ..frame(content)
            },
            4016,
        );
        assert!(rig.buffer.padding() < 0.01);
        for n in 1..30 {
            rig.tick(frame(content), 4016 + n * 16);
            assert!(rig.buffer.padding() < 0.01);
        }
        rig.tick(frame(content + 24.0), 4512);
        assert!(rig.buffer.replenish_armed);
    }

    #[test]
    fn detached_growth_spends_reserve_without_reclaiming_viewport() {
        let mut rig = Rig::new();
        rig.tick(frame(650.0), 0);
        let away = BufferFrame {
            following: false,
            ..frame(680.0)
        };
        let layout = rig.tick(away, 16);
        assert_eq!(layout.padding, 20.0);
        assert_eq!(layout.scroll_by, None);
        let layout = rig.tick(
            BufferFrame {
                content_bottom: Some(900.0),
                ..away
            },
            32,
        );
        assert_eq!(layout.padding, 0.0);
        assert_eq!(layout.scroll_by, None);
    }

    #[test]
    fn unknown_bounds_hold_reserve_and_reflow_is_not_generation() {
        let mut rig = Rig::new();
        rig.buffer.reset(600.0);
        let unknown = BufferFrame {
            content_bottom: None,
            ..frame(0.0)
        };
        assert_eq!(rig.tick(unknown, 0).padding, 599.0);
        rig.tick(frame(650.0), 16);
        let resize = BufferFrame {
            width: 400.0,
            content_bottom: None,
            ..frame(0.0)
        };
        let unknown = rig.tick(resize, 32);
        assert_eq!(unknown.padding, 50.0);
        assert_eq!(unknown.scroll_by, None);
        let reflowed = rig.tick(
            BufferFrame {
                content_bottom: Some(1200.0),
                ..resize
            },
            48,
        );
        assert_eq!(reflowed.padding, 50.0);
        assert_eq!(reflowed.scroll_by, None);
        assert_eq!(rig.buffer.rate, 0.0);
    }

    #[test]
    fn reflow_and_programmatic_adoption_do_not_restore_consumed_blank() {
        let mut rig = Rig::new();
        rig.tick(frame(650.0), 0);
        rig.scroll_to(50.0);
        assert_eq!(rig.buffer.padding(), 0.0);
        let narrower_body = BufferFrame {
            width: 900.0,
            ..frame(350.0)
        };
        rig.tick(narrower_body, 16);
        for n in 2..20 {
            assert_eq!(rig.tick(narrower_body, n * 16).padding, 0.0);
        }
        rig.buffer.adopt_position();
        rig.tick(narrower_body, 400);
        assert_eq!(rig.tick(narrower_body, 416).padding, 0.0);
    }

    #[test]
    fn provisional_user_consumption_survives_first_measurement() {
        let mut rig = Rig::new();
        rig.buffer.reset(600.0);
        rig.buffer.consume_scroll(100.0, 60.0);
        let layout = rig.tick(
            BufferFrame {
                following: false,
                ..frame(300.0)
            },
            16,
        );
        assert_eq!(layout.padding, 360.0);
        assert_eq!(layout.scroll_by, None);
    }

    #[test]
    fn offscreen_fold_changes_document_coordinates_without_moving_the_view() {
        let (mut rig, _) = simulate(160.0, 60);
        let content = 650.0 + (4.0_f32 / 0.12).floor() * 0.12 * 160.0;
        let reserve = rig.buffer.padding();
        // GPUI preserves the visible logical row while preceding working
        // rows disappear: both document offset and document bottom lose 400.
        rig.scroll -= 400.0;
        let settled = rig.tick(
            BufferFrame {
                running: false,
                ..frame(content - 400.0)
            },
            4016,
        );
        assert!(settled.scroll_by.unwrap().abs() < 0.01);
        assert!((settled.padding - reserve).abs() < 0.01);
    }

    #[test]
    fn resize_shorter_cannot_blank_the_gpui_list() {
        let mut rig = Rig::new();
        rig.tick(frame(300.0), 0);
        let short = rig.tick(
            BufferFrame {
                viewport_height: 200.0,
                running: false,
                following: false,
                ..frame(300.0)
            },
            16,
        );
        assert!(short.padding < 200.0);
        assert_eq!(short.scroll_by, None);
    }

    #[test]
    fn reduced_motion_and_repeat_turns_do_not_keep_an_animation_lease() {
        let mut rig = Rig::new();
        rig.tick(frame(650.0), 0);
        let layout = rig.tick(
            BufferFrame {
                reduce_motion: true,
                ..frame(900.0)
            },
            16,
        );
        assert!(!layout.animating);
        rig.scroll_to(0.0);
        rig.buffer.reset(600.0);
        let next = rig.tick(frame(300.0), 32);
        assert_eq!(next.padding, 400.0);
        assert_eq!(rig.buffer.rate, 0.0);
        assert!(!rig.buffer.settled);
    }
}
