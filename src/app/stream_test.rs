//! Streaming-render playground (debug tool).
//!
//! Modeled on streamdown's playground: pick a sample, dial in the provider
//! rate, chunk size and jitter, then watch the dissolve, the container height
//! and PR100's transcript reserve against a live render — no provider, no tokens. The
//! header button opens it on debug builds only; release builds do not compile
//! the entry point (see `sidebar.rs`).
//!
//! The file is intentionally self-contained so it can move to `src/app/dev/`
//! once a second debug tool wants a home.

use std::time::Instant;

use super::transcript_view::{
    BufferedTranscriptList, BufferedTranscriptScroll, transcript_scroll_observer,
};
use super::*;
use crate::md::render::{markdown, markdown_windowed};
use crate::ui::ActivationExt;
use crate::ui::scrollbar::Scrollable;

/// Embedded, tracked fixture: launching from a different directory cannot lose it.
const STRESS_FIXTURE: &str = include_str!("../../docs/fixtures/streaming-stress.md");

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
    Finish,
}

pub(super) struct StreamPlayground {
    /// The whole sample the simulated provider draws from.
    source: String,
    source_chars: usize,
    arrived_chars: usize,
    layout_reconcile: bool,
    /// Bytes of `source` handed to the stream so far.
    sent: usize,
    /// Bytes that have arrived.
    arrived: String,
    /// Fractional characters owed by the provider pacing.
    carried: f32,
    last_tick: Instant,
    last_commit: Instant,
    focuses: Rc<RefCell<HashMap<String, FocusHandle>>>,
    running: bool,
    jitter_seed: u32,

    rate: usize,
    chunk: usize,
    jitter: u32,
    sample: Sample,
    scroll: BufferedTranscriptScroll,
    scrollbar: Rc<ScrollbarState>,
    width: f32,
    delivery_wake: Option<gpui::Task<()>>,
    selection: TranscriptSelection,
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
            source_chars: 0,
            arrived_chars: 0,
            layout_reconcile: false,
            sent: 0,
            arrived: String::new(),
            carried: 0.0,
            last_tick: Instant::now(),
            last_commit: Instant::now(),
            focuses: Rc::new(RefCell::new(HashMap::new())),
            running: false,
            jitter_seed: 0x9e37_79b9,
            rate: 500,
            chunk: 6,
            jitter: 50,
            sample: Sample::Stress,
            scroll: playground_scroll(0.0),
            scrollbar: ScrollbarState::new(),
            width: 0.0,
            delivery_wake: None,
            selection: TranscriptSelection::default(),
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
            Sample::Stress => STRESS_FIXTURE.to_owned(),
            Sample::Prose => prose_sample(),
        };
        self.source_chars = self.source.chars().count();
        self.restart();
    }

    fn restart(&mut self) {
        self.sent = 0;
        self.arrived_chars = 0;
        self.delivery_wake = None;
        self.layout_reconcile = true;
        self.arrived.clear();
        self.carried = 0.0;
        self.last_tick = Instant::now();
        self.last_commit = self.last_tick;
        self.running = true;
        self.jitter_seed = 0x9e37_79b9;
        self.view = MarkdownView::new();
        self.width = 0.0;
        let height = f32::from(self.scroll.rows.viewport_bounds().size.height);
        self.scroll = playground_scroll(height);
        self.selection = TranscriptSelection::default();
        self.commits_window = 0;
        self.chars_window = 0;
        self.rate_window_start = Instant::now();
    }

    fn finish(&mut self) {
        self.arrived.clone_from(&self.source);
        self.sent = self.source.len();
        self.arrived_chars = self.source_chars;
        self.running = false;
        self.delivery_wake = None;
        self.layout_reconcile = true;
        self.scroll.rows.remeasure_items(1..2);
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
    /// production incremental Markdown parser. Delivery commits are coalesced
    /// at 120 ms; the shared motion clock paints reveal/following separately.
    fn tick(&mut self, now: Instant) {
        if self.running {
            let dt = now.saturating_duration_since(self.last_tick).as_secs_f32();
            self.carried += self.rate as f32 * dt;
            // Count Unicode scalar values, not bytes; a delayed frame commits
            // one accumulated prefix, never 64 redundant parser updates.
            self.carried = self
                .carried
                .min((self.rate as f32 * 0.5).max(self.chunk as f32 * 4.0));
            let want = self.next_chunk_size();
            if self.carried >= want as f32
                && self.sent < self.source.len()
                && now.saturating_duration_since(self.last_commit) >= Duration::from_millis(120)
            {
                self.last_commit = now;
                let count = (self.carried as usize / want) * want;
                let tail = &self.source[self.sent..];
                let end = tail
                    .char_indices()
                    .nth(count)
                    .map_or(self.source.len(), |(offset, _)| self.sent + offset);
                let chars = self.source[self.sent..end].chars().count();
                self.arrived.push_str(&self.source[self.sent..end]);
                self.chars_window += chars;
                self.arrived_chars += chars;
                self.sent = end;
                self.carried -= chars as f32;
                self.commits_window += 1;
                self.layout_reconcile = true;
                self.scroll.rows.remeasure_items(1..2);
            }
            if self.sent >= self.source.len() {
                self.running = false;
            }
        }
        self.last_tick = now;

        self.view
            .set_text(&self.arrived, self.sent < self.source.len());
        // PR100's anchored transcript owns height/following. Do not run the
        // legacy per-body height spring underneath it.
        self.view.advance_clip(false, now);

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
        self.running || self.view.is_fading()
    }
}

fn playground_scroll(height: f32) -> BufferedTranscriptScroll {
    let rows = ListState::new(2, ListAlignment::Top, px(2048.0));
    let following = Rc::new(Cell::new(true));
    let is_scrolled = Rc::new(Cell::new(false));
    let recheck = Rc::new(Cell::new(false));
    rows.set_scroll_handler({
        let following = following.clone();
        let is_scrolled = is_scrolled.clone();
        let recheck = recheck.clone();
        move |event, window, _| {
            following.set(false);
            is_scrolled.set(event.is_scrolled);
            recheck.set(true);
            window.refresh();
        }
    });
    let mut buffer = transcript_buffer::TranscriptBuffer::default();
    buffer.reset(height.max(0.0));
    BufferedTranscriptScroll {
        rows,
        buffer: Rc::new(RefCell::new(buffer)),
        padding: Rc::new(Cell::new(px(height.max(0.0)))),
        following,
        is_scrolled,
        recheck,
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
        let focus = self.transcript_control_focus("stream-playground", cx);
        div()
            .id("stream-playground")
            .track_focus(&focus)
            .tab_index(0)
            .focus_visible(|style| style.border_1().border_color(theme.accent))
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
            .on_activation(cx, |this, window, cx| {
                let focus = this.transcript_control_focus("pg-root", cx);
                window.focus(&focus, cx);
                this.stream_playground = Some(StreamPlayground::new());
                cx.notify();
            })
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
            PlaygroundSet::ToggleFollow => {
                let following = !playground.scroll.following.get();
                playground.scroll.following.set(following);
                if following {
                    playground.scroll.buffer.borrow_mut().resume_following();
                    playground.scroll.rows.scroll_to_end();
                    playground.scroll.buffer.borrow_mut().adopt_position();
                }
            }
            PlaygroundSet::Finish => playground.finish(),
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

        let playground = self.stream_playground.as_mut().unwrap();
        if std::mem::take(&mut playground.layout_reconcile) {
            // Final delivery can retire both transport and dissolve clocks.
            // Reconcile its measured row on the next frame even at rest.
            let view = window.current_view();
            window.on_next_frame(move |_, cx| cx.notify(view));
        }
        let viewport = playground.scroll.rows.viewport_bounds();
        let width = (f32::from(viewport.size.width) - 40.0)
            .min(CONTENT_MAX_WIDTH)
            .max(0.0);
        let changed_width = (width - playground.width).abs() > 0.5;
        if changed_width {
            playground.width = width;
            playground.view.set_render_width(width);
            playground.scroll.rows.remeasure_items(0..2);
            let view = window.current_view();
            window.on_next_frame(move |_, cx| cx.notify(view));
        }
        let resolving_end = playground.scroll.rows.logical_scroll_top().item_ix >= 2;
        if resolving_end {
            let view = window.current_view();
            window.on_next_frame(move |_, cx| cx.notify(view));
        }
        let tail = playground.scroll.rows.bounds_for_item(1);
        let bottom = (!changed_width && !resolving_end)
            .then(|| tail.map(|bounds| f32::from(bounds.bottom() - viewport.top())))
            .flatten();
        if playground.scroll.recheck.get()
            && !playground.scrollbar.is_grabbed()
            && let Some(rests) = transcript_rests_at_tail(
                viewport.bottom(),
                tail.map(|b| b.bottom()),
                playground.scroll.padding.get(),
            )
        {
            playground.scroll.recheck.set(false);
            if rests && playground.scroll.buffer.borrow().may_resume_following() {
                playground.scroll.following.set(true);
            }
        }
        let layout = playground.scroll.buffer.borrow_mut().update(
            transcript_buffer::BufferFrame {
                content_bottom: bottom,
                anchor_top: playground
                    .scroll
                    .rows
                    .bounds_for_item(0)
                    .map(|bounds| f32::from(bounds.top() - viewport.top())),
                viewport_height: f32::from(viewport.size.height),
                width,
                running: playground.sent < playground.source.len(),
                following: playground.scroll.following.get() && !playground.scrollbar.is_grabbed(),
                reduce_motion: !animate,
            },
            now,
        );
        playground.scroll.padding.set(px(layout.padding));
        if let Some(delta) = layout.scroll_by {
            playground.scroll.rows.scroll_by(px(delta));
        } else if !playground.scroll.buffer.borrow().initialized() {
            maintain_transcript_anchor(
                &playground.scroll.rows,
                0,
                playground.scroll.following.get(),
                px(layout.padding),
            );
        }
        if animate && (playground.is_active() || layout.animating) {
            crate::ui::motion::dissolve_lease(window.current_view(), dissolve_fps, cx);
        } else if !animate && playground.running && playground.delivery_wake.is_none() {
            // Reduced motion still delivers content, but has no decorative
            // display-rate wake. Dropping the playground cancels this task.
            playground.delivery_wake = Some(cx.spawn(async move |entity, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(120))
                    .await;
                let _ = entity.update(cx, |this, cx| {
                    if let Some(playground) = &mut this.stream_playground {
                        playground.delivery_wake = None;
                    }
                    cx.notify();
                });
            }));
        }
        let scroll = playground.scroll.clone();
        let scrollbar = playground.scrollbar.clone();
        let selection = playground.selection.clone();
        let body_window = tail
            .filter(|_| !changed_width && playground.sent < playground.source.len())
            .map(|bounds| MessageBodyWindow {
                visible_top: f32::from(viewport.top() - bounds.top() - px(16.0)).max(0.0),
                visible_height: f32::from(viewport.size.height),
            });
        let focuses = playground.focuses.clone();
        let follow = scroll.following.get();
        let running = playground.running;
        let stats = PlaygroundStats {
            commits_per_second: playground.commits_per_second,
            chars_per_second: playground.chars_per_second,
            arrived: playground.arrived_chars,
            total: playground.source_chars,
            clip: Some(layout.padding),
            offset: scroll.scrolled(),
            max_offset: f32::from(scroll.max_offset()),
            build_ms: playground.build_ms,
            fps: playground.fps,
            phase: if playground.running {
                "streaming"
            } else if playground.sent < playground.source.len() {
                "paused"
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
                &focuses,
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
                &focuses,
                cx,
            ))
            .child(section_label("Chunk (chars)", theme))
            .child(chip_row(
                "pg-chunk",
                &[("1", 1usize), ("6", 6), ("24", 24), ("120", 120)],
                playground.chunk,
                PlaygroundSet::Chunk,
                theme,
                &focuses,
                cx,
            ))
            .child(section_label("Jitter", theme))
            .child(chip_row(
                "pg-jitter",
                &[("0%", 0u32), ("25%", 25), ("50%", 50), ("90%", 90)],
                playground.jitter,
                PlaygroundSet::Jitter,
                theme,
                &focuses,
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
                        &focuses,
                        cx,
                    ))
                    .child(action_button(
                        "pg-finish",
                        "Finish",
                        PlaygroundSet::Finish,
                        theme,
                        &focuses,
                        cx,
                    ))
                    .child(action_button(
                        "pg-restart",
                        "Restart",
                        PlaygroundSet::Restart,
                        theme,
                        &focuses,
                        cx,
                    ))
                    .child(action_button(
                        "pg-follow",
                        if follow { "Follow: on" } else { "Follow: off" },
                        PlaygroundSet::ToggleFollow,
                        theme,
                        &focuses,
                        cx,
                    )),
            )
            .child(
                div()
                    .mt_auto()
                    .text_size(sp(11.0))
                    .line_height(sp(16.0))
                    .text_color(theme.text_ghost)
                    .child("Embedded stress fixture · PR100 transcript reserve · no provider"),
            );

        let entity = cx.entity().downgrade();
        let surface = div().flex_1().h_full().min_w_0().flex().flex_col()
            .child(div().id("pg-scroll").flex_1().min_h_0().w_full().relative()
                .child(md::render::frame_reset(selection.clone()))
                .child(transcript_scroll_observer(scroll.clone()))
                .child(BufferedTranscriptList {
                    scroll: scroll.clone(),
                    inner: list(scroll.rows.clone(), move |index, _, cx| {
                        if index == 0 {
                            return div().px(px(20.0)).py(px(16.0)).text_size(sp(12.0)).child("Replay the selected sample through Doki's streaming renderer").into_any_element();
                        }
                        entity.upgrade().map(|entity| entity.update(cx, |this, cx| {
                            let palette = MarkdownPalette::from_theme(&Theme::current(cx));
                            let metrics = this.scaled_markdown_metrics(MarkdownMetrics::BODY);
                            let playground = this.stream_playground.as_mut().unwrap();
                            let start = Instant::now();
                            let ctx = MarkdownCtx::new("playground".to_owned(), &palette, metrics, playground.selection.clone())
                                .with_math_enabled(render_math).with_streaming_animation(animate);
                            let body = if let Some(body_window) = body_window {
                                markdown_windowed(&playground.view, &ctx, body_window)
                            } else { markdown(&playground.view, &ctx) }.unwrap_or_else(|| div().into_any_element());
                            playground.build_ms = start.elapsed().as_secs_f32() * 1000.0;
                            div().w_full().flex().justify_center().px(px(20.0)).py(px(16.0))
                                .child(div().w_full().max_w(px(CONTENT_MAX_WIDTH)).min_w_0().child(body)).into_any_element()
                        })).unwrap_or_else(|| div().into_any_element())
                    }).size_full(),
                })
                .child(scrollbar::vertical(&scroll, &scrollbar))
                .child(canvas(|_, _, _| (), move |_, _, window, _| md::render::install_selection_input(window, &selection)).absolute().w(px(0.0)).h(px(0.0))))
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
                    .child("PR100 · adaptive reserve + native Markdown · no provider"),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("pg-close")
                    .track_focus(&playground_focus(&focuses, "pg-close".to_owned(), cx))
                    .tab_index(0)
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
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
                    .on_activation(cx, |this, window, cx| {
                        let focus = this.transcript_control_focus("stream-playground", cx);
                        window.focus(&focus, cx);
                        this.stream_playground = None;
                        cx.notify();
                    }),
            );

        let root_focus = self.transcript_control_focus("pg-root", cx);
        div()
            .id("pg-root")
            .key_context("Waku")
            .track_focus(&root_focus)
            .tab_group()
            .on_action(cx.listener(|_, _: &FocusNext, window, cx| window.focus_next(cx)))
            .on_action(cx.listener(|_, _: &FocusPrev, window, cx| window.focus_prev(cx)))
            .on_action(cx.listener(|this, _: &CancelTurn, window, cx| {
                let focus = this.transcript_control_focus("stream-playground", cx);
                window.focus(&focus, cx);
                this.stream_playground = None;
                cx.notify();
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.surface)
            .text_color(theme.text)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    let focus = this.transcript_control_focus("stream-playground", cx);
                    window.focus(&focus, cx);
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

fn playground_focus(
    focuses: &Rc<RefCell<HashMap<String, FocusHandle>>>,
    id: String,
    cx: &mut App,
) -> FocusHandle {
    focuses
        .borrow_mut()
        .entry(id)
        .or_insert_with(|| cx.focus_handle())
        .clone()
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
    focuses: &Rc<RefCell<HashMap<String, FocusHandle>>>,
    cx: &mut Context<Waku>,
) -> AnyElement {
    let mut row = div().flex().flex_wrap().gap(px(5.0));
    for (label, value) in options {
        let value = *value;
        let active = value == selected;
        let focus = playground_focus(focuses, format!("{id}-{label}"), cx);
        row = row.child(
            div()
                .id(SharedString::from(format!("{id}-{label}")))
                .track_focus(&focus)
                .tab_index(0)
                .focus_visible(|style| style.border_1().border_color(theme.accent))
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
                .child(SharedString::from(if active {
                    format!("✓ {label}")
                } else {
                    (*label).to_owned()
                }))
                .on_activation(cx, move |this, _, cx| this.playground_set(set(value), cx)),
        );
    }
    row.into_any_element()
}

fn action_button(
    id: &'static str,
    label: &'static str,
    set: PlaygroundSet,
    theme: Theme,
    focuses: &Rc<RefCell<HashMap<String, FocusHandle>>>,
    cx: &mut Context<Waku>,
) -> AnyElement {
    let focus = playground_focus(focuses, id.to_owned(), cx);
    div()
        .id(id)
        .track_focus(&focus)
        .tab_index(0)
        .focus_visible(|style| style.border_1().border_color(theme.accent))
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
        .on_activation(cx, move |this, _, cx| this.playground_set(set, cx))
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
            "reserve",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playground_slow_large_chunks_eventually_progress() {
        let mut pg = StreamPlayground::new();
        pg.rate = 120;
        pg.chunk = 120;
        pg.jitter = 0;
        let start = pg.last_tick;
        for tick in 1..=20 {
            pg.tick(start + Duration::from_millis(tick * 120));
        }
        assert!(pg.sent >= 240);
        assert_eq!(pg.arrived, pg.source[..pg.sent]);
    }

    #[test]
    fn playground_unicode_delivery_is_valid_and_settles() {
        let mut pg = StreamPlayground::new();
        pg.source = "中文🙂e\u{301} **bold**\n".repeat(8);
        pg.source_chars = pg.source.chars().count();
        pg.rate = 250;
        pg.chunk = 6;
        pg.restart();
        let start = pg.last_tick;
        let mut final_reconcile = false;
        for tick in 1..=60 {
            pg.layout_reconcile = false;
            let was_running = pg.running;
            pg.tick(start + Duration::from_millis(tick * 120));
            assert_eq!(pg.arrived, pg.source[..pg.sent]);
            assert_eq!(pg.arrived_chars, pg.arrived.chars().count());
            if was_running && !pg.running {
                final_reconcile = pg.layout_reconcile;
            }
        }
        assert_eq!(pg.arrived, pg.source);
        assert!(!pg.running);
        assert_eq!(pg.view.clip_height(), None);
        assert!(
            final_reconcile,
            "final delivery requests a measured follow-up frame"
        );
    }

    #[test]
    fn playground_pause_and_restart_do_not_leak_previous_delivery() {
        let mut pg = StreamPlayground::new();
        let start = pg.last_tick;
        pg.tick(start + Duration::from_secs(1));
        let sent = pg.sent;
        pg.running = false;
        pg.tick(start + Duration::from_secs(60));
        assert_eq!(pg.sent, sent);
        pg.restart();
        assert_eq!(pg.sent, 0);
        assert_eq!(pg.arrived_chars, 0);
        assert!(pg.arrived.is_empty());
        assert_eq!(pg.carried, 0.0);
        assert!(!pg.scroll.buffer.borrow().initialized());
        assert!(pg.scroll.following.get());
    }
    #[test]
    fn playground_finish_after_pause_requests_a_settled_layout() {
        let mut pg = StreamPlayground::new();
        let start = pg.last_tick;
        pg.tick(start + Duration::from_millis(120));
        assert!(pg.sent < pg.source.len());
        pg.running = false;
        pg.layout_reconcile = false;
        pg.finish();
        assert!(pg.layout_reconcile);
        assert!(!pg.running);
        assert_eq!(pg.arrived, pg.source);
        assert_eq!(pg.arrived_chars, pg.source_chars);
        pg.tick(start + Duration::from_secs(60));
        assert!(!pg.running);
        assert_eq!(pg.arrived, pg.source);
        assert_eq!(pg.view.clip_height(), None);
    }
}
