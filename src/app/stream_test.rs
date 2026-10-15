//! Streaming-render playground (debug tool).
//!
//! Modeled on streamdown's playground: pick a sample, dial in the provider
//! rate, chunk size and jitter, then watch the dissolve, the container height
//! and the windowed body against a live render — no provider, no tokens. The
//! header button opens it on debug builds only; release builds do not compile
//! the entry point (see `sidebar.rs`).
//!
//! The file is intentionally self-contained so it can move to `src/app/dev/`
//! once a second debug tool wants a home.

use std::time::Instant;

use super::*;
use crate::md::render::{clip_body, markdown, markdown_windowed};

/// The stress fixture committed with the streaming work. Read on open; if it
/// is missing, a generated long-prose sample stands in.
const STRESS_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/docs/fixtures/streaming-stress.md"
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sample {
    Stress,
    Prose,
}

#[derive(Clone, Copy, Debug)]
enum PlaygroundSet {
    Rate(usize),
    Chunk(usize),
    Jitter(u32),
    Sample(Sample),
    ToggleRunning,
    Restart,
    ToggleFollow,
}

pub(super) struct StreamPlayground {
    /// The whole sample the simulated provider draws from.
    source: String,
    /// Bytes of `source` handed to the stream so far.
    sent: usize,
    /// Bytes that have arrived.
    arrived: String,
    /// Fractional characters owed by the provider pacing.
    carried: f32,
    last_tick: Instant,
    running: bool,
    jitter_seed: u32,

    rate: usize,
    chunk: usize,
    jitter: u32,
    sample: Sample,
    follow: bool,

    scroll: ScrollHandle,
    view: MarkdownView,

    commits_window: u32,
    commits_per_second: f32,
    chars_window: usize,
    chars_per_second: f32,
    rate_window_start: Instant,
    last_frame: Instant,
    fps: f32,
    build_ms: f32,
}

impl StreamPlayground {
    pub(super) fn new() -> Self {
        let mut playground = Self {
            source: String::new(),
            sent: 0,
            arrived: String::new(),
            carried: 0.0,
            last_tick: Instant::now(),
            running: false,
            jitter_seed: 0x9e37_79b9,
            rate: 500,
            chunk: 6,
            jitter: 50,
            sample: Sample::Stress,
            follow: true,
            scroll: ScrollHandle::new(),
            view: MarkdownView::new(),
            commits_window: 0,
            commits_per_second: 0.0,
            chars_window: 0,
            chars_per_second: 0.0,
            rate_window_start: Instant::now(),
            last_frame: Instant::now(),
            fps: 0.0,
            build_ms: 0.0,
        };
        playground.load_sample();
        playground
    }

    fn load_sample(&mut self) {
        self.source = match self.sample {
            Sample::Stress => {
                std::fs::read_to_string(STRESS_FIXTURE).unwrap_or_else(|_| prose_sample())
            }
            Sample::Prose => prose_sample(),
        };
        self.restart();
    }

    fn restart(&mut self) {
        self.sent = 0;
        self.arrived.clear();
        self.carried = 0.0;
        self.last_tick = Instant::now();
        self.running = true;
        self.jitter_seed = 0x9e37_79b9;
        self.view = MarkdownView::new();
        self.scroll = ScrollHandle::new();
        self.commits_window = 0;
        self.chars_window = 0;
        self.rate_window_start = Instant::now();
    }

    fn next_chunk_size(&mut self) -> usize {
        // A small deterministic LCG stands in for provider jitter.
        self.jitter_seed = self
            .jitter_seed
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        let unit = ((self.jitter_seed >> 8) % 1_000) as f32 / 1_000.0;
        let spread = self.chunk as f32 * (self.jitter as f32 / 100.0);
        (self.chunk as f32 + (unit * 2.0 - 1.0) * spread).max(1.0) as usize
    }

    /// Advance the simulated provider by one frame. The reveal itself is the
    /// production path: `set_text` shows the whole arrived body and
    /// `advance_clip` paves the container height ahead of it.
    fn tick(&mut self, now: Instant) {
        if self.running {
            let dt = now.saturating_duration_since(self.last_tick).as_secs_f32();
            self.carried += self.rate as f32 * dt;
            let mut guard = 0;
            while self.carried >= 1.0 && self.sent < self.source.len() && guard < 64 {
                guard += 1;
                let want = self.next_chunk_size();
                let mut end = (self.sent + want).min(self.source.len());
                while end < self.source.len() && !self.source.is_char_boundary(end) {
                    end += 1;
                }
                self.arrived.push_str(&self.source[self.sent..end]);
                self.chars_window += end - self.sent;
                self.sent = end;
                self.carried -= want as f32;
                self.commits_window += 1;
            }
            if self.sent >= self.source.len() {
                self.running = false;
            }
        }
        self.last_tick = now;

        self.view.set_text(&self.arrived, true);
        self.view.advance_clip(true, now);

        let dt = now.saturating_duration_since(self.last_frame).as_secs_f32();
        if dt > 0.0 {
            self.fps = 0.9 * self.fps + 0.1 * (1.0 / dt);
        }
        self.last_frame = now;

        let elapsed = now
            .saturating_duration_since(self.rate_window_start)
            .as_secs_f32();
        if elapsed >= 0.5 {
            self.commits_per_second = self.commits_window as f32 / elapsed;
            self.chars_per_second = self.chars_window as f32 / elapsed;
            self.commits_window = 0;
            self.chars_window = 0;
            self.rate_window_start = now;
        }
    }

    fn is_active(&self) -> bool {
        self.running || self.view.is_fading() || self.view.clip_height().is_some()
    }
}

fn prose_sample() -> String {
    let mut source = String::new();
    for index in 0..120 {
        source.push_str(&format!(
            "Paragraph {index}: streaming prose should read as typing, not as a slideshow. \
The reveal follows the provider's pace, the body grows a few graphemes at a time, and \
the tail stays pinned without the rows above it jumping. Every now and then a **bold** \
or a `code span` keeps the inline styles in the mix.\n\n"
        ));
    }
    source
}

impl Waku {
    pub(super) fn render_stream_playground_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        div()
            .id("stream-playground")
            .h(px(22.0))
            .px(px(8.0))
            .flex_none()
            .rounded(px(6.0))
            .flex()
            .items_center()
            .cursor_default()
            .bg(theme.overlay)
            .text_size(sp(11.5))
            .text_color(theme.text_secondary)
            .hover(|element| element.bg(theme.overlay_strong))
            .child("Playground")
            .on_click(cx.listener(|this, _, _, cx| {
                this.stream_playground = Some(StreamPlayground::new());
                cx.notify();
            }))
            .into_any_element()
    }

    fn playground_set(&mut self, set: PlaygroundSet, cx: &mut Context<Self>) {
        let Some(playground) = self.stream_playground.as_mut() else {
            return;
        };
        match set {
            PlaygroundSet::Rate(rate) => playground.rate = rate,
            PlaygroundSet::Chunk(chunk) => playground.chunk = chunk,
            PlaygroundSet::Jitter(jitter) => playground.jitter = jitter,
            PlaygroundSet::Sample(sample) => {
                playground.sample = sample;
                playground.load_sample();
            }
            PlaygroundSet::ToggleRunning => {
                playground.running = !playground.running;
                playground.last_tick = Instant::now();
            }
            PlaygroundSet::Restart => playground.restart(),
            PlaygroundSet::ToggleFollow => playground.follow = !playground.follow,
        }
        cx.notify();
    }

    pub(super) fn render_stream_playground(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let now = Instant::now();
        let dissolve_fps = self.state.dissolve_fps;
        let animate = !cx.reduce_motion();
        let render_math = self.state.render_math;

        if let Some(playground) = self.stream_playground.as_mut() {
            playground.tick(now);
        } else {
            return div().into_any_element();
        }

        let palette = MarkdownPalette::from_theme(&theme);
        let metrics = self.scaled_markdown_metrics(MarkdownMetrics::BODY);
        let ctx = MarkdownCtx::new(
            "playground".to_owned(),
            &palette,
            metrics,
            TranscriptSelection::default(),
        )
        .with_math_enabled(render_math)
        .with_streaming_animation(animate);

        let Some(playground) = self.stream_playground.as_mut() else {
            return div().into_any_element();
        };

        let build_start = Instant::now();
        let width = f32::from(playground.scroll.bounds().size.width);
        let visible_top = f32::from(-playground.scroll.offset().y);
        let visible_height = f32::from(playground.scroll.bounds().size.height);
        let body = if width > 1.0 {
            markdown_windowed(
                &playground.view,
                &ctx,
                MessageBodyWindow {
                    visible_top,
                    visible_height,
                },
            )
        } else {
            markdown(&playground.view, &ctx)
        }
        .unwrap_or_else(|| div().into_any_element());
        let body = clip_body(&playground.view, body);
        playground.build_ms =
            0.8 * playground.build_ms + 0.2 * build_start.elapsed().as_secs_f32() * 1_000.0;

        if playground.follow {
            playground.scroll.scroll_to_bottom();
        }
        let active = playground.is_active();
        if active {
            crate::ui::motion::dissolve_lease(window.current_view(), dissolve_fps, cx);
        }

        let scroll = playground.scroll.clone();
        let follow = playground.follow;
        let running = playground.running;
        let clip = playground.view.clip_height().map(f32::from);
        let stats = PlaygroundStats {
            commits_per_second: playground.commits_per_second,
            chars_per_second: playground.chars_per_second,
            arrived: playground.arrived.len(),
            total: playground.source.len(),
            clip,
            offset: f32::from(playground.scroll.offset().y),
            max_offset: f32::from(playground.scroll.max_offset().y),
            build_ms: playground.build_ms,
            fps: playground.fps,
            phase: if playground.running {
                "streaming"
            } else if playground.is_active() {
                "draining"
            } else {
                "settled"
            },
        };

        let controls = div()
            .id("pg-controls")
            .w(px(252.0))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .px(px(14.0))
            .py(px(12.0))
            .overflow_y_scroll()
            .child(section_label("Sample", theme))
            .child(chip_row(
                "pg-sample",
                &[("stress", Sample::Stress), ("prose", Sample::Prose)],
                playground.sample,
                PlaygroundSet::Sample,
                theme,
                cx,
            ))
            .child(section_label("Rate (chars/s)", theme))
            .child(chip_row(
                "pg-rate",
                &[
                    ("120", 120usize),
                    ("250", 250),
                    ("500", 500),
                    ("1000", 1000),
                    ("2000", 2000),
                ],
                playground.rate,
                PlaygroundSet::Rate,
                theme,
                cx,
            ))
            .child(section_label("Chunk (chars)", theme))
            .child(chip_row(
                "pg-chunk",
                &[("1", 1usize), ("6", 6), ("24", 24), ("120", 120)],
                playground.chunk,
                PlaygroundSet::Chunk,
                theme,
                cx,
            ))
            .child(section_label("Jitter", theme))
            .child(chip_row(
                "pg-jitter",
                &[("0%", 0u32), ("25%", 25), ("50%", 50), ("90%", 90)],
                playground.jitter,
                PlaygroundSet::Jitter,
                theme,
                cx,
            ))
            .child(section_label("Surface", theme))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(6.0))
                    .child(action_button(
                        "pg-run",
                        if running { "Pause" } else { "Play" },
                        PlaygroundSet::ToggleRunning,
                        theme,
                        cx,
                    ))
                    .child(action_button(
                        "pg-restart",
                        "Restart",
                        PlaygroundSet::Restart,
                        theme,
                        cx,
                    ))
                    .child(action_button(
                        "pg-follow",
                        if follow { "Follow: on" } else { "Follow: off" },
                        PlaygroundSet::ToggleFollow,
                        theme,
                        cx,
                    )),
            )
            .child(
                div()
                    .mt_auto()
                    .text_size(sp(11.0))
                    .line_height(sp(16.0))
                    .text_color(theme.text_ghost)
                    .child(SharedString::from(format!(
                        "fixture: {}\npayload has no images or formulas, so the body stays windowed",
                        STRESS_FIXTURE
                    ))),
            );

        let surface = div()
            .flex_1()
            .h_full()
            .min_w_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("pg-scroll")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scroll()
                    .track_scroll(&scroll)
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .justify_center()
                            .px(px(20.0))
                            .py(px(16.0))
                            .child(
                                div()
                                    .w_full()
                                    .max_w(px(CONTENT_MAX_WIDTH))
                                    .min_w_0()
                                    .child(body),
                            ),
                    ),
            )
            .child(stats_strip(stats, theme));

        let header = div()
            .h(px(44.0))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(14.0))
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_size(sp(14.0))
                    .font_weight(FontWeight::MEDIUM)
                    .child("Streaming playground"),
            )
            .child(
                div()
                    .text_size(sp(11.5))
                    .text_color(theme.text_ghost)
                    .child("dissolve + leading container height + windowed body, no provider"),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("pg-close")
                    .h(px(24.0))
                    .px(px(10.0))
                    .rounded(px(6.0))
                    .flex()
                    .items_center()
                    .cursor_default()
                    .text_size(sp(12.0))
                    .text_color(theme.text_secondary)
                    .hover(|element| element.bg(theme.overlay))
                    .child("Close")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.stream_playground = None;
                        cx.notify();
                    })),
            );

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.surface)
            .text_color(theme.text)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.stream_playground = None;
                    cx.notify();
                    cx.stop_propagation();
                }
            }))
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(controls)
                    .child(div().w(px(1.0)).h_full().bg(theme.border))
                    .child(surface),
            )
            .into_any_element()
    }
}

fn section_label(label: &str, theme: Theme) -> AnyElement {
    div()
        .text_size(sp(11.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.text_tertiary)
        .child(SharedString::from(label.to_owned()))
        .into_any_element()
}

/// A row of mutually exclusive chips. `set` maps a choice to the state update.
fn chip_row<T: Copy + PartialEq + 'static>(
    id: &'static str,
    options: &[(&'static str, T)],
    selected: T,
    set: fn(T) -> PlaygroundSet,
    theme: Theme,
    cx: &mut Context<Waku>,
) -> AnyElement {
    let mut row = div().flex().flex_wrap().gap(px(5.0));
    for (label, value) in options {
        let value = *value;
        let active = value == selected;
        row = row.child(
            div()
                .id(SharedString::from(format!("{id}-{label}")))
                .h(px(22.0))
                .px(px(8.0))
                .rounded(px(6.0))
                .flex()
                .items_center()
                .cursor_default()
                .text_size(sp(11.5))
                .bg(if active { theme.inverse } else { theme.overlay })
                .text_color(if active {
                    theme.on_inverse
                } else {
                    theme.text_secondary
                })
                .hover(|element| element.opacity(0.9))
                .child(SharedString::from(*label))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.playground_set(set(value), cx);
                })),
        );
    }
    row.into_any_element()
}

fn action_button(
    id: &'static str,
    label: &'static str,
    set: PlaygroundSet,
    theme: Theme,
    cx: &mut Context<Waku>,
) -> AnyElement {
    div()
        .id(id)
        .h(px(24.0))
        .px(px(10.0))
        .rounded(px(6.0))
        .flex()
        .items_center()
        .cursor_default()
        .text_size(sp(11.5))
        .text_color(theme.text_secondary)
        .bg(theme.overlay)
        .hover(|element| element.bg(theme.overlay_strong))
        .child(SharedString::from(label))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.playground_set(set, cx);
        }))
        .into_any_element()
}

struct PlaygroundStats {
    commits_per_second: f32,
    chars_per_second: f32,
    arrived: usize,
    total: usize,
    clip: Option<f32>,
    offset: f32,
    max_offset: f32,
    build_ms: f32,
    fps: f32,
    phase: &'static str,
}

fn stats_strip(stats: PlaygroundStats, theme: Theme) -> AnyElement {
    let cell = |label: &str, value: String| {
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .text_size(sp(10.5))
                    .text_color(theme.text_ghost)
                    .child(SharedString::from(label.to_owned())),
            )
            .child(
                div()
                    .text_size(sp(12.0))
                    .font_weight(FontWeight::MEDIUM)
                    .child(SharedString::from(value)),
            )
    };
    div()
        .h(px(52.0))
        .flex_none()
        .flex()
        .items_center()
        .gap(px(22.0))
        .px(px(14.0))
        .border_t_1()
        .border_color(theme.border)
        .child(cell(
            "commits",
            format!("{:.1}/s", stats.commits_per_second),
        ))
        .child(cell("chars", format!("{:.0}/s", stats.chars_per_second)))
        .child(cell(
            "arrived",
            format!("{} / {}", stats.arrived, stats.total),
        ))
        .child(cell(
            "clip",
            stats
                .clip
                .map(|clip| format!("{clip:.1} px"))
                .unwrap_or_else(|| "—".to_owned()),
        ))
        .child(cell(
            "offset",
            format!("{:.0} / {:.0}", stats.offset, stats.max_offset),
        ))
        .child(cell("build", format!("{:.2} ms", stats.build_ms)))
        .child(cell("fps", format!("{:.0}", stats.fps)))
        .child(cell("phase", stats.phase.to_owned()))
        .into_any_element()
}
