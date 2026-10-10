//! [`BlockTree`] → GPUI elements.
//!
//! Two properties drive every decision here.
//!
//! **One shaped element per block.** A paragraph becomes a single
//! [`StyledText`] over one flat string with `TextRun`s for its inline styles —
//! not one element per line, and not an element per styled span. Layout cost
//! per block is therefore one measured-layout node and one `shape_text` call,
//! which GPUI's line-layout cache reuses verbatim across frames when the text
//! and wrap width are unchanged.
//! Math paragraphs use one measured element with retained native glyph layouts
//! and cached formula images (see [`math_text`]); ordinary prose stays on the
//! StyledText path.
//!
//! **Color is paint, geometry is layout.** Syntax highlighting, inline-code
//! washes and the selection wash are all painted from geometry read back out of
//! the text's own [`TextLayout`], so none of them can change a row's measured
//! height. That is what lets a streaming code block colorize progressively
//! without ever reflowing, and what keeps the transcript's row measurements
//! stable while a selection is dragged across it.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, BorderStyle, Bounds, ClipboardItem, CursorStyle, DispatchPhase, Element, Font,
    FontStyle, FontWeight, Hsla, InteractiveText, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, SharedString,
    StrikethroughStyle, StyledText, TextLayout, TextRun, UnderlineStyle, Window, canvas, div, font,
    img, point, prelude::*, px, quad, relative, size,
};
use regex::Regex;

use super::highlight::{self, Lang, TokenClass};
use super::mend::PENDING_LINK_URL;
use super::parser::{Block, IncrementalParser, InlineRun, ListItem, TableAlign, TopBlock};
use super::selection::{
    RegisteredText, Selection, SelectionRegistry, SelectionState, TextKey, line_range, word_range,
};
use super::veil::{RowVeil, apply_veil};
use crate::theme::Theme;
use crate::ui::menu::{ContextMenuHandle, context_menu};
use crate::ui::tooltip::Tooltip;

mod math_text;

/// Selection geometry: the laid-out text handle for one painted element.
#[derive(Clone)]
pub enum TextGeometry {
    Text(TextLayout),
    Math(math_text::Geometry),
}

impl TextGeometry {
    /// The element's painted bounds, used to place annotation badges against
    /// the text column's own right edge.
    pub fn bounds(&self) -> Bounds<Pixels> {
        match self {
            Self::Text(layout) => layout.bounds(),
            Self::Math(layout) => layout.bounds(),
        }
    }

    fn index_for_position(&self, position: Point<Pixels>) -> Result<usize, usize> {
        match self {
            Self::Text(layout) => layout.index_for_position(position),
            Self::Math(layout) => layout.index_for_position(position),
        }
    }

    fn is_missing(&self) -> bool {
        match self {
            Self::Text(layout) => layout_missing(layout),
            Self::Math(layout) => layout.is_missing(),
        }
    }
}

/// The transcript's shared selection handles, specialised to real geometry.
pub type TranscriptSelection = SelectionState<TextGeometry>;

/// An optional app-owned override for clicked markdown links.
///
/// The markdown renderer stays unaware of projects and workspace surfaces;
/// callers that do have that context can intercept a link, while every other
/// markdown view continues to use GPUI's ordinary URL opener.
pub type LinkHandler = Rc<dyn Fn(&str, &mut Window, &mut gpui::App)>;

// ── Layout metrics ─────────────────────────────────────────────────────────
//
// Everything in this block participates in measurement, so these are the only
// numbers that can change a transcript row's height.

/// Paragraph and inline metrics for one text scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    pub text_size: f32,
    pub line_height: f32,
    pub code_text_size: f32,
    pub code_line_height: f32,
    /// Vertical gap between sibling blocks.
    pub block_gap: f32,
}

impl Metrics {
    /// Assistant response scale, matching the transcript's body text.
    pub const BODY: Self = Self {
        text_size: 14.0,
        line_height: 21.0,
        code_text_size: 13.0,
        code_line_height: 19.5,
        block_gap: 10.0,
    };

    /// User-message scale. Markdown blocks keep the bubble's established body
    /// geometry instead of making every existing plain prompt subtly reflow.
    pub const USER_MESSAGE: Self = Self {
        text_size: 14.5,
        line_height: 21.0,
        code_text_size: 13.0,
        code_line_height: 19.5,
        block_gap: 10.0,
    };

    /// Compact scale for reasoning, tool detail, and other secondary content.
    /// One notch under [`Metrics::BODY`], not a miniature: secondary reading
    /// text stays close to prose size and leans on color for its hierarchy.
    pub const COMPACT: Self = Self {
        text_size: 13.5,
        line_height: 19.5,
        code_text_size: 13.0,
        code_line_height: 19.5,
        block_gap: 7.0,
    };

    /// The UI and code font sizes the constants above were authored against.
    /// [`Metrics::scaled`] is the identity at these values, so the settings'
    /// defaults reproduce the authored transcript exactly.
    const AUTHORED_UI_FONT_SIZE: f32 = 14.0;
    const AUTHORED_CODE_FONT_SIZE: f32 = 13.0;

    /// These metrics rescaled to the user's font settings: prose follows the
    /// UI font size, code spans and blocks follow the code font size, and each
    /// surface keeps its authored proportions. Values land on half pixels so
    /// scaled text stays as crisp as the authored sizes.
    pub fn scaled(self, ui_font_size: f32, code_font_size: f32) -> Self {
        let ui = ui_font_size / Self::AUTHORED_UI_FONT_SIZE;
        let code = code_font_size / Self::AUTHORED_CODE_FONT_SIZE;
        let half = |value: f32| (value * 2.0).round() / 2.0;
        Self {
            text_size: half(self.text_size * ui),
            line_height: half(self.line_height * ui),
            code_text_size: half(self.code_text_size * code),
            code_line_height: half(self.code_line_height * code),
            block_gap: half(self.block_gap * ui),
        }
    }

    /// Document scale for a full-page reading surface: prose at the user's UI
    /// font size and code at the code font size, keeping [`Metrics::BODY`]'s
    /// proportions.
    pub fn document(text_size: f32, code_text_size: f32) -> Self {
        Self {
            text_size,
            line_height: (text_size * 1.55).round(),
            code_text_size,
            code_line_height: (code_text_size * 1.5).round(),
            block_gap: (text_size * 0.72).round(),
        }
    }
}

pub const SANS_FAMILY: &str = ".SystemUIFont";
/// The bundled mono face. "SF Mono" only exists on machines that installed it
/// with Xcode or Terminal, and silently falls back to the sans face when it
/// does not — which reads as proportional code.
pub const MONO_FAMILY: &str = "JetBrains Mono";

/// Inline-code wash geometry. Paint-only: the box overhangs the glyphs
/// horizontally and insets vertically inside the line box.
const CODE_WASH_RADIUS: f32 = 4.0;
const CODE_WASH_PAD_X: f32 = 2.5;
const CODE_WASH_INSET_Y: f32 = 1.5;

/// Heading scale relative to body text, by level.
fn heading_metrics(level: u8, metrics: &Metrics) -> (f32, f32, FontWeight) {
    let (scale, weight) = match level {
        1 => (1.45, FontWeight::BOLD),
        2 => (1.28, FontWeight::BOLD),
        3 => (1.14, FontWeight::SEMIBOLD),
        4 => (1.05, FontWeight::SEMIBOLD),
        _ => (1.0, FontWeight::SEMIBOLD),
    };
    let size = (metrics.text_size * scale).round();
    (size, (size * 1.42).round(), weight)
}

// ── Palette ────────────────────────────────────────────────────────────────

/// Colors for markdown paint, resolved once per render from the theme.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub text: Hsla,
    pub secondary: Hsla,
    pub tertiary: Hsla,
    pub ghost: Hsla,
    pub border: Hsla,
    pub inset: Hsla,
    pub overlay: Hsla,
    pub code_text: Hsla,
    pub code_wash: Hsla,
    pub selection: Hsla,
    pub search_match: Hsla,
    pub active_search_match: Hsla,
    pub accent: Hsla,
    pub added: Hsla,
    pub removed: Hsla,
    is_dark: bool,
}

impl Palette {
    pub fn from_theme(theme: &Theme) -> Self {
        let search_yellow = gpui::hsla(
            48.0 / 360.0,
            0.95,
            if theme.is_dark { 0.48 } else { 0.55 },
            1.0,
        );
        let active_search_orange = gpui::hsla(
            30.0 / 360.0,
            1.0,
            if theme.is_dark { 0.50 } else { 0.54 },
            1.0,
        );
        Self {
            text: theme.text,
            secondary: theme.text_secondary,
            tertiary: theme.text_tertiary,
            ghost: theme.text_ghost,
            border: theme.border,
            inset: theme.inset,
            overlay: theme.overlay,
            code_text: theme.code_text,
            code_wash: theme.code_wash,
            selection: theme.selection,
            search_match: search_yellow.opacity(if theme.is_dark { 0.18 } else { 0.20 }),
            active_search_match: active_search_orange.opacity(if theme.is_dark {
                0.78
            } else {
                0.70
            }),
            accent: theme.accent,
            added: theme.success,
            removed: theme.danger,
            is_dark: theme.is_dark,
        }
    }

    /// Token colors. Deliberately restrained: three hues plus muted comments,
    /// so a code block still reads as part of a graphite transcript. Shared with
    /// the code editor, so both surfaces colour code identically.
    pub fn token(&self, class: TokenClass) -> Hsla {
        let dark = self.is_dark;
        match class {
            TokenClass::Keyword => hue(dark, 0xC98BC0, 0x9A4B92),
            TokenClass::Literal => hue(dark, 0xD9A05B, 0x9A6019),
            TokenClass::String => hue(dark, 0x94C08A, 0x3F7A36),
            TokenClass::Comment => self.ghost,
            TokenClass::Number => hue(dark, 0xD9A05B, 0x9A6019),
            TokenClass::Type => hue(dark, 0x8FB8D9, 0x2F6690),
            TokenClass::Function => hue(dark, 0x8FB8D9, 0x2F6690),
            TokenClass::Meta => self.tertiary,
            TokenClass::Added => self.added,
            TokenClass::Removed => self.removed,
        }
    }
}

fn hue(is_dark: bool, dark: u32, light: u32) -> Hsla {
    gpui::rgb(if is_dark { dark } else { light }).into()
}

// ── Flattened inline text ──────────────────────────────────────────────────

/// One block's inline content, ready to shape: a flat string, the `TextRun`s
/// that tile it exactly, plus the byte ranges that need paint-only decoration.
#[derive(Debug)]
pub struct FlatText {
    pub text: SharedString,
    pub runs: Vec<TextRun>,
    pub links: Vec<(Range<usize>, String)>,
    pub code_ranges: Vec<Range<usize>>,
    pub math: Option<Rc<math_text::MathData>>,
}

/// One literal find-in-page hit inside a shaped markdown text element.
///
/// `ordinal` is the same stable per-row element ordinal used by [`TextKey`],
/// so a search performed before an off-screen row is mounted can still point
/// at the exact range the renderer will paint after navigation reveals it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextSearchMatch {
    pub ordinal: usize,
    pub range: Range<usize>,
}

/// Paint-only search state for one markdown row. The match list is prepared
/// when the query changes; rendering only indexes the ranges for each visible
/// text element and never scans the message again on a frame.
#[derive(Clone)]
pub struct SearchHighlights {
    pub matches: Rc<Vec<TextSearchMatch>>,
    pub active: Option<TextSearchMatch>,
}

/// One annotation's mark inside a message.
///
/// `range` addresses the same flattened rendered text a search hit does, and
/// `number` is the annotation's position in the composer list — the number the
/// user reads on the card, so a mark and a card can be matched by eye. A mark
/// for a *sent* annotation has no draft number: the badge is dropped and only
/// the wash remains, because the number was draft vocabulary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnnotationMark {
    /// The annotation the mark belongs to, so a badge can jump back to it.
    pub id: uuid::Uuid,
    pub ordinal: usize,
    pub range: Range<usize>,
    pub number: Option<usize>,
}

/// This message's marks and the treatment they paint with.
#[derive(Clone)]
pub struct AnnotationMarks {
    pub marks: Rc<Vec<AnnotationMark>>,
    pub style: AnnotationStyle,
    /// A span a card activation is briefly flashing. Painted over its mark, on
    /// the same underlay, so the flash needs no element of its own.
    pub flash: Option<AnnotationFlash>,
}

/// The transient highlight a card activation leaves on its span. The wash is
/// resolved by the caller from the shared pulse clock, so it fades without the
/// paint path knowing anything about motion.
#[derive(Clone)]
pub struct AnnotationFlash {
    pub ordinal: usize,
    pub range: Range<usize>,
    pub wash: Hsla,
}

/// Colors for annotation marks. The numbered badge is an element in the
/// transcript's right gutter now, so only the wash is painted here.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnnotationStyle {
    pub wash: Hsla,
}

impl AnnotationStyle {
    pub fn from_palette(palette: &Palette) -> Self {
        Self {
            wash: palette.accent.opacity(0.20),
        }
    }
}

/// Flatten inline runs for shaping. Pure given the palette and base weight.
pub fn flatten(
    runs: &[InlineRun],
    palette: &Palette,
    base_weight: FontWeight,
    base_color: Hsla,
) -> FlatText {
    let mut text = String::new();
    let mut out: Vec<TextRun> = Vec::with_capacity(runs.len());
    let mut links: Vec<(Range<usize>, String)> = Vec::new();
    let mut code_ranges: Vec<Range<usize>> = Vec::new();
    let mut math = Vec::new();

    for run in runs {
        if run.text.is_empty() {
            continue;
        }
        let start = text.len();
        text.push_str(&run.text);
        let end = text.len();
        if run.style.math {
            math.push(math_text::MathSpan {
                range: start..end,
                latex: std::sync::Arc::from(run.text.as_str()),
                display: false,
            });
        }

        let mut run_font = font(if run.style.code {
            MONO_FAMILY
        } else {
            SANS_FAMILY
        });
        run_font.weight = if run.style.bold && base_weight < FontWeight::SEMIBOLD {
            FontWeight::SEMIBOLD
        } else {
            base_weight
        };
        run_font.style = if run.style.italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        };

        if run.style.code {
            // Merge neighbouring code runs so their washes form one box.
            match code_ranges.last_mut() {
                Some(range) if range.end == start => range.end = end,
                _ => code_ranges.push(start..end),
            }
        }
        if let Some(url) = &run.style.link {
            // A still-streaming link keeps link styling — so the URL settling
            // changes nothing visually — but must not become clickable.
            if url != PENDING_LINK_URL {
                match links.last_mut() {
                    Some((range, last)) if range.end == start && last == url => range.end = end,
                    _ => links.push((start..end, url.clone())),
                }
            }
        }

        out.push(TextRun {
            len: run.text.len(),
            font: run_font,
            color: if run.style.code {
                palette.code_text
            } else {
                base_color
            },
            // Inline code's wash is painted as *rounded* quads by the canvas
            // underlay; a run background could only ever be a square box.
            background_color: None,
            underline: run.style.link.is_some().then_some(UnderlineStyle {
                color: Some(palette.tertiary),
                thickness: px(1.0),
                wavy: false,
            }),
            strikethrough: run.style.strikethrough.then_some(StrikethroughStyle {
                thickness: px(1.0),
                color: Some(palette.tertiary),
            }),
        });
    }

    FlatText {
        text: text.into(),
        runs: out,
        links,
        code_ranges,
        math: (!math.is_empty()).then(|| Rc::new(math_text::MathData::new(math))),
    }
}

/// A flat string with uniform styling, for non-markdown transcript text.
pub fn flatten_plain(
    text: impl Into<SharedString>,
    family: &'static str,
    weight: FontWeight,
    color: Hsla,
) -> FlatText {
    let text: SharedString = text.into();
    let mut run_font = font(family);
    run_font.weight = weight;
    let runs = if text.is_empty() {
        Vec::new()
    } else {
        vec![TextRun {
            len: text.len(),
            font: run_font,
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        }]
    };
    FlatText {
        text,
        runs,
        links: Vec::new(),
        code_ranges: Vec::new(),
        math: None,
    }
}

// ── Per-message state ──────────────────────────────────────────────────────

/// Ceiling, in pixels, on how far the leading height may run ahead of the
/// measured body; also caps the feed-forward lookahead on a fast stream.
const CLIP_RUNWAY_MAX: f32 = 44.0;

/// Everything the renderer keeps between frames for one markdown body.
///
/// The flatten cache is keyed by element ordinal and pruned only back to the
/// parser's stable prefix, so a streamed delta rebuilds the final block's
/// elements and reuses every settled one.
pub struct MarkdownView {
    parser: IncrementalParser,
    /// Mended replacement for the final block while streaming.
    tail: Vec<TopBlock>,
    flats: RefCell<HashMap<usize, Rc<FlatText>>>,
    /// First element ordinal belonging to the final block — the only block an
    /// append can change. Recorded during render, because only the renderer
    /// knows how many text elements each block expands into.
    volatile_from: Cell<usize>,
    /// Style the cached flats were built for. Colors live inside `TextRun`s, so
    /// a theme switch has to drop them or the transcript keeps painting the old
    /// palette.
    style: Cell<Option<(Palette, Metrics)>>,
    /// Per-element opacity spans for the live response. Text is committed to
    /// layout immediately; only these paint colors animate.
    veil: RefCell<RowVeil>,
    /// Measured height of every top-level block, recorded as the renderer
    /// lays each one out. A long streaming body is rendered as the window of
    /// blocks near the viewport plus the volatile tail; the unrendered blocks
    /// stand in as spacers sized from these heights. `None` means the block
    /// has never been laid out at the current width, which forces one full
    /// pass so the spacers cannot guess.
    /// Each entry is keyed by the block's *source range*: mending re-partitions
    /// the tail as it settles, so a block's index is not a stable identity and
    /// an index-keyed ledger hands one block's height to another.
    heights: Rc<RefCell<Vec<(Range<usize>, Option<Pixels>)>>>,
    /// Wrap width the recorded heights were measured at. A reflow invalidates
    /// every height, because a block's height is a function of its width.
    heights_width: Cell<Option<f32>>,
    /// Code-block ordinals currently showing successful copy feedback. Kept
    /// outside the parsed/flattened caches so a three-second icon change never
    /// invalidates text shaping.
    copied_code_blocks: Rc<RefCell<HashMap<usize, u64>>>,
    streaming: Cell<bool>,
    /// Whether the parsed body contains an image or a formula. Both size
    /// themselves asynchronously, so a block holding one can change height
    /// without being rebuilt; the window planner refuses to stand in for
    /// blocks it cannot re-measure.
    async_blocks: Cell<bool>,
    /// Height the streaming body is clipped to while it grows. `None` shows
    /// the whole body; the controller keeps it continuous so the row never
    /// jumps by a line, and never lets it fall behind the measured body.
    clip: Cell<Option<Pixels>>,
    /// Natural height of the full arrived body, measured at paint.
    body_height: Rc<Cell<Option<Pixels>>>,
    /// When the clip height last moved.
    clip_at: Cell<Instant>,
    /// Last measured body height and smoothed growth rate, for the leading
    /// container height.
    clip_last_height: Cell<Option<Pixels>>,
    clip_rate: Cell<f32>,
    /// Spring velocity of the leading container height.
    clip_velocity: Cell<f32>,
}

impl Default for MarkdownView {
    fn default() -> Self {
        Self::new()
    }
}

impl MarkdownView {
    pub fn new() -> Self {
        Self {
            parser: IncrementalParser::new(),
            tail: Vec::new(),
            flats: RefCell::new(HashMap::new()),
            volatile_from: Cell::new(0),
            style: Cell::new(None),
            veil: RefCell::new(RowVeil::default()),
            heights: Rc::new(RefCell::new(Vec::new())),
            heights_width: Cell::new(None),
            copied_code_blocks: Rc::new(RefCell::new(HashMap::new())),
            streaming: Cell::new(false),
            async_blocks: Cell::new(false),
            clip: Cell::new(None),
            body_height: Rc::new(Cell::new(None)),
            clip_at: Cell::new(Instant::now()),
            clip_last_height: Cell::new(None),
            clip_rate: Cell::new(0.0),
            clip_velocity: Cell::new(0.0),
        }
    }

    /// A view attached to an already-streaming body. Its first rendered text
    /// becomes the full-opacity baseline; later appends fade normally.
    pub fn seeded() -> Self {
        let view = Self::new();
        *view.veil.borrow_mut() = RowVeil::seeded();
        view
    }

    /// Reattach an existing parsed view without animating text that arrived
    /// while its session was off screen.
    pub fn seed_streaming_baseline(&self) {
        *self.veil.borrow_mut() = RowVeil::seeded();
    }

    /// Bytes of source this view retains. Parsed structures run to roughly
    /// seventeen times this, so it is the honest unit for bounding a cache.
    pub fn source_len(&self) -> usize {
        self.parser.text().len()
    }

    /// Parse and point the caches at `text`, the body in full. `mend` closes
    /// hanging inline markers, which is wanted while a response streams and not
    /// once it has settled.
    pub fn set_text(&mut self, text: &str, mend: bool) {
        let was_streaming = self.streaming.replace(mend);
        if !mend && was_streaming {
            // Settling drops the veil, so there is no invisible text left for
            // a cut to land on; the row reports the body's real height again.
            *self.veil.borrow_mut() = RowVeil::default();
            self.release_clip();
        } else if mend && !was_streaming && !self.parser.text().is_empty() {
            // A completed body that starts streaming again already has a
            // rendered baseline. Do not make that history dissolve again.
            *self.veil.borrow_mut() = RowVeil::seeded();
        }
        let changed = self.parser.text() != text;
        let append = !changed || text.starts_with(self.parser.text());
        if changed {
            self.parser.set_text(text);
            if !append {
                // A rewrite (edit, rewind, replacement) can change every
                // block, so the measured heights no longer describe the body.
                self.heights.borrow_mut().clear();
                self.release_clip();
            }
        }
        // The mended display tail depends only on the source and the
        // streaming flag. Deriving it re-mends — and, with a hanging marker,
        // re-parses — the final block, and `set_text` runs for every visible
        // row on every frame, so a frame that changed neither input must not
        // pay for it.
        if changed || mend != was_streaming {
            let tail = if mend {
                self.parser.display_tail().unwrap_or_default()
            } else {
                Vec::new()
            };
            if changed {
                // A height measured while a block sat in the mended tail can
                // differ from the same range's settled render (mending closes
                // markers without changing the source range), so those entries
                // go stale the moment a block settles. Drop everything from
                // the previous volatile start: those blocks are rendered every
                // frame anyway, and one that just left the region forces a
                // single measuring pass instead of sizing a spacer with a
                // mended height.
                let volatile = self.volatile_from.get() >> BLOCK_ORDINAL_STRIDE_BITS;
                self.heights
                    .borrow_mut()
                    .iter_mut()
                    .skip(volatile)
                    .for_each(|entry| entry.1 = None);
            }
            if changed || tail != self.tail {
                self.tail = tail;
                // Markdown block structure only ever extends the final block,
                // so every element before it is still valid. A streamed delta
                // thus re-flattens one block instead of the whole response.
                let boundary = if append { self.volatile_from.get() } else { 0 };
                self.flats
                    .borrow_mut()
                    .retain(|ordinal, _| *ordinal < boundary);
            }
            if changed {
                // Inline math or an image can arrive inside an existing block
                // without moving a block boundary, so the block count is no
                // test for whether the async content changed: scan whenever
                // the content did.
                self.async_blocks
                    .set(self.blocks().any(block_has_async_content));
            }
        }
    }

    /// Advance the clipped height toward the body's real height.
    ///
    /// The row's height is what the transcript pins to, and text layout grows
    /// it in whole-line steps; pinning that step is the vertical jolt no
    /// amount of grapheme fading hides. This keeps the row's *reported* height
    /// continuous instead: it leads the height measured last frame, so a line
    /// laid out this frame lands in space that already exists, and the row
    /// never reports a step. It never falls below that measured height either,
    /// so only text appended since the measurement can lie under the clip —
    /// and the veil has not painted that yet. That justification depends on the
    /// veil: without animation there is nothing holding appended text
    /// invisible, so the clip is released and the row reports the body's real
    /// height.
    pub fn advance_clip(&self, animate: bool, now: Instant) {
        let dt = now
            .saturating_duration_since(self.clip_at.get())
            .as_secs_f32();
        self.clip_at.set(now);
        let Some(height) = self.body_height.get() else {
            self.clip.set(None);
            return;
        };
        if !animate {
            // A pinned clip is the height measured last frame, which every
            // appended line has since grown past; with the veil off, those
            // bytes are opaque, so pinning would only cut the text the reader
            // is waiting for.
            self.release_clip();
            return;
        }
        // The container paves the road *ahead* of the body: it grows at the
        // body's smoothed rate so the next line lands in space that already
        // exists, and the row's reported height never steps. It stays at or
        // above the last measured body height and the veil holds newly
        // appended graphemes invisible, so the gap under the text is the only
        // artifact, bounded by the rate lead.
        let growth = match self.clip_last_height.get() {
            Some(previous) if height > previous => f32::from(height - previous),
            _ => 0.0,
        };
        let observed = if dt > 0.0 { growth / dt } else { 0.0 };
        let rate = if growth > 0.0 {
            self.clip_rate.get() * 0.75 + observed * 0.25
        } else {
            self.clip_rate.get() * 0.9
        };
        self.clip_rate.set(rate);
        self.clip_last_height.set(Some(height));

        let current = self.clip.get().unwrap_or(height);
        // Feed-forward: chase a target a short slice of growth ahead, so the
        // spring absorbs steps instead of lagging the arrival.
        let target = (height + px(rate * 0.10)).min(height + px(CLIP_RUNWAY_MAX));
        // Critically damped spring on (position, velocity). A body step
        // changes only the acceleration, so the row's motion stays smooth
        // through it.
        let stiffness = 220.0_f32;
        let damping = 2.0 * (stiffness).sqrt();
        let mut velocity = self.clip_velocity.get();
        let accel = stiffness * f32::from(target - current) - damping * velocity;
        velocity = (velocity + accel * dt).max(0.0);
        self.clip_velocity.set(velocity);
        let next = (current + px(velocity * dt)).max(height);
        self.clip.set(Some(next.min(height + px(CLIP_RUNWAY_MAX))));
    }

    /// Drop the leading container height. `None` reports the whole body, which
    /// is what anything that invalidates the measured height needs: a retained
    /// clip is a height the body has since grown past, and the row would cut
    /// off the text below it.
    fn release_clip(&self) {
        self.clip.set(None);
        self.clip_last_height.set(None);
        self.clip_rate.set(0.0);
        self.clip_velocity.set(0.0);
    }

    /// The container height the row should report, when the controller holds
    /// one. It is never below the height measured last frame, so it hides no
    /// text the veil has already painted.
    pub fn clip_height(&self) -> Option<Pixels> {
        self.clip.get()
    }

    pub fn is_fading(&self) -> bool {
        self.streaming.get() && self.veil.borrow().is_fading()
    }

    /// Whether an image or formula anywhere in the body can resize itself on
    /// a later frame, which makes it unsafe to window this body.
    pub fn has_async_blocks(&self) -> bool {
        self.async_blocks.get()
    }

    /// Point the height ledger at the width the next pass lays out at. A
    /// changed width drops every recorded height: a block re-wraps, so the
    /// spacer arithmetic would otherwise carry the old reflow into the row.
    pub fn set_render_width(&self, width: f32) {
        if self.heights_width.replace(Some(width)) != Some(width) {
            self.heights.borrow_mut().clear();
            // The clip is a height at the old wrapping, so it is no more valid
            // than the heights are: keeping it would cut the re-wrapped body.
            self.release_clip();
        }
    }

    /// Drop cached flats if the style they were built for no longer applies.
    fn sync_style(&self, palette: &Palette, metrics: &Metrics) {
        let current = (*palette, *metrics);
        if self.style.get() != Some(current) {
            self.style.set(Some(current));
            self.flats.borrow_mut().clear();
            // A metrics change moves every block's height, so the spacer
            // ledger has to be rebuilt at the new scale too.
            self.heights.borrow_mut().clear();
            self.release_clip();
        }
    }

    /// Flattened inline content for the element at `ordinal`, built on miss.
    fn flat(&self, ordinal: usize, build: impl FnOnce() -> FlatText) -> Rc<FlatText> {
        self.flats
            .borrow_mut()
            .entry(ordinal)
            .or_insert_with(|| Rc::new(build()))
            .clone()
    }

    /// Display blocks in document order: the settled prefix, then the mended
    /// tail when one is active.
    fn top_blocks(&self) -> impl Iterator<Item = &TopBlock> + '_ {
        let all = &self.parser.tree().blocks;
        let settled = if self.tail.is_empty() {
            all.len()
        } else {
            self.parser.display_tail_start()
        };
        all[..settled].iter().chain(self.tail.iter())
    }

    fn blocks(&self) -> impl Iterator<Item = &Block> + '_ {
        self.top_blocks().map(|top| &top.block)
    }
}

// ── Render context ─────────────────────────────────────────────────────────

/// Everything a render pass needs, plus the element counter that assigns
/// document-ordered keys. Keys stay stable frame to frame as long as the block
/// structure does, which is what lets a selection survive scrolling.
pub struct Ctx<'a> {
    row: Rc<str>,
    palette: &'a Palette,
    metrics: Metrics,
    selection: TranscriptSelection,
    search: Option<SearchHighlights>,
    annotations: Option<AnnotationMarks>,
    link_handler: Option<LinkHandler>,
    /// Cross-frame flatten cache, when this render has one to consult.
    cache: Option<&'a MarkdownView>,
    next_ordinal: Cell<usize>,
    /// Set while rendering the first element of a block, for copy spacing.
    starts_block: Cell<bool>,
    animate_streaming: bool,
    math_enabled: bool,
    math_menu: Option<ContextMenuHandle>,
    wrap_math_menu: bool,
    now: Instant,
}

impl<'a> Ctx<'a> {
    pub fn new(
        row: impl Into<Rc<str>>,
        palette: &'a Palette,
        metrics: Metrics,
        selection: TranscriptSelection,
    ) -> Self {
        Self {
            row: row.into(),
            palette,
            metrics,
            selection,
            search: None,
            annotations: None,
            link_handler: None,
            cache: None,
            next_ordinal: Cell::new(0),
            starts_block: Cell::new(true),
            animate_streaming: true,
            math_enabled: true,
            math_menu: None,
            wrap_math_menu: false,
            now: Instant::now(),
        }
    }

    pub fn selection(&self) -> &TranscriptSelection {
        &self.selection
    }

    /// Whether this render carries search marks whose geometry a reveal pass
    /// reads back from the frame's registry. A windowed body must not hide a
    /// block the reveal is looking for.
    pub fn has_search(&self) -> bool {
        self.search.is_some()
    }

    /// Annotation marks need the same registry geometry as search marks.
    pub fn has_annotations(&self) -> bool {
        self.annotations.is_some()
    }

    /// Whether a selection is live. Its spans and its drag anchor name
    /// registry entries — a shift-click resolves against them — so a windowed
    /// body must not omit the blocks they sit in.
    pub fn has_selection(&self) -> bool {
        let selection = self.selection.selection.borrow();
        selection.anchor().is_some() || !selection.spans().is_empty()
    }

    pub fn with_link_handler(mut self, handler: LinkHandler) -> Self {
        self.link_handler = Some(handler);
        self
    }

    pub fn with_search_highlights(mut self, highlights: SearchHighlights) -> Self {
        self.search = Some(highlights);
        self
    }

    /// Mark this row's staged annotations. Painted like a search hit, so the
    /// two treatments share one geometry pass and one paint order.
    pub fn with_annotations(mut self, marks: AnnotationMarks) -> Self {
        self.annotations = Some(marks);
        self
    }

    pub fn with_streaming_animation(mut self, animate: bool) -> Self {
        self.animate_streaming = animate;
        self
    }

    pub fn with_math_enabled(mut self, enabled: bool) -> Self {
        self.math_enabled = enabled;
        self
    }

    /// Contribute formula actions to an existing message menu.
    pub fn with_context_menu(mut self, menu: ContextMenuHandle) -> Self {
        self.math_menu = Some(menu);
        self.wrap_math_menu = false;
        self
    }

    /// Give a standalone Markdown surface its own formula context menu.
    pub fn with_math_context_menu(mut self, menu: ContextMenuHandle) -> Self {
        self.math_menu = Some(menu);
        self.wrap_math_menu = true;
        self
    }

    fn with_cache(&self, view: &'a MarkdownView) -> Self {
        Self {
            row: self.row.clone(),
            palette: self.palette,
            metrics: self.metrics,
            selection: self.selection.clone(),
            search: self.search.clone(),
            annotations: self.annotations.clone(),
            link_handler: self.link_handler.clone(),
            cache: Some(view),
            next_ordinal: Cell::new(self.next_ordinal.get()),
            starts_block: Cell::new(self.starts_block.get()),
            animate_streaming: self.animate_streaming,
            math_enabled: self.math_enabled,
            math_menu: self.math_menu.clone(),
            wrap_math_menu: self.wrap_math_menu,
            now: Instant::now(),
        }
    }

    fn next_key(&self) -> TextKey {
        let ordinal = self.next_ordinal.get();
        self.next_ordinal.set(ordinal + 1);
        TextKey::new(self.row.clone(), ordinal)
    }

    fn take_block_break(&self) -> bool {
        self.starts_block.replace(false)
    }

    /// Flatten through the cache when one is wired: a settled block reuses its
    /// string and `TextRun`s untouched, so an unchanged paragraph costs one
    /// `Rc` clone per frame instead of a fresh allocation.
    fn flat(&self, ordinal: usize, build: impl FnOnce() -> FlatText) -> Rc<FlatText> {
        match self.cache {
            Some(view) => view.flat(ordinal, build),
            None => Rc::new(build()),
        }
    }
}

// ── The shared text primitive ──────────────────────────────────────────────

/// One selectable, decorated text element.
///
/// The `canvas` underlay is an *earlier sibling* than the text, so GPUI paints
/// it first — underneath the glyphs — while the text's prepaint has already
/// filled in the shared [`TextLayout`]. That ordering is what lets a pure-paint
/// pass read real glyph geometry without a second layout pass.
fn text_element_with_selection(
    flat: &FlatText,
    runs: Vec<TextRun>,
    key: TextKey,
    selection: TranscriptSelection,
    search: Option<SearchHighlights>,
    annotations: Option<AnnotationMarks>,
    link_handler: Option<LinkHandler>,
    code_wash: Hsla,
    selection_wash: Hsla,
    search_match_wash: Hsla,
    active_search_match_wash: Hsla,
    block_break: bool,
) -> AnyElement {
    let styled = StyledText::new(flat.text.clone()).with_runs(runs);
    let layout = styled.layout().clone();

    let body: AnyElement = if flat.links.is_empty() {
        styled.into_any_element()
    } else {
        let (ranges, urls): (Vec<_>, Vec<_>) = flat.links.iter().cloned().unzip();
        let id = SharedString::from(format!("{}-t{}", key.row, key.index));
        InteractiveText::new(id, styled)
            .on_click(ranges, move |clicked, window, cx| {
                if let Some(url) = urls.get(clicked) {
                    if let Some(handler) = &link_handler {
                        handler(url, window, cx);
                    } else {
                        cx.open_url(url);
                    }
                }
            })
            .into_any_element()
    };

    let underlay = canvas(|_, _, _| (), {
        let text = flat.text.clone();
        let code_ranges = flat.code_ranges.clone();
        let layout = layout.clone();
        let key = key.clone();
        move |_, _, window, _| {
            for range in &code_ranges {
                for rect in range_rects(&layout, range, CODE_WASH_PAD_X, CODE_WASH_INSET_Y) {
                    window.paint_quad(quad(
                        rect,
                        px(CODE_WASH_RADIUS),
                        code_wash,
                        px(0.0),
                        gpui::transparent_black(),
                        BorderStyle::default(),
                    ));
                }
            }
            if let Some(search) = &search {
                let first = search
                    .matches
                    .partition_point(|found| found.ordinal < key.index);
                for found in search.matches[first..]
                    .iter()
                    .take_while(|found| found.ordinal == key.index)
                {
                    let color = if search.active.as_ref() == Some(found) {
                        active_search_match_wash
                    } else {
                        search_match_wash
                    };
                    for rect in range_rects(&layout, &found.range, 1.0, 1.0) {
                        window.paint_quad(quad(
                            rect,
                            px(2.0),
                            color,
                            px(0.0),
                            gpui::transparent_black(),
                            BorderStyle::default(),
                        ));
                    }
                }
            }
            if let Some(annotations) = &annotations {
                let first = annotations
                    .marks
                    .partition_point(|mark| mark.ordinal < key.index);
                for mark in annotations.marks[first..]
                    .iter()
                    .take_while(|mark| mark.ordinal == key.index)
                {
                    let rects = range_rects(
                        &layout,
                        &mark.range,
                        ANNOTATION_WASH_PAD_X,
                        ANNOTATION_WASH_INSET_Y,
                    );
                    for rect in &rects {
                        window.paint_quad(quad(
                            rect.clone(),
                            px(ANNOTATION_WASH_RADIUS),
                            annotations.style.wash,
                            px(0.0),
                            gpui::transparent_black(),
                            BorderStyle::default(),
                        ));
                    }
                }
            }
            if let Some(marks) = &annotations
                && let Some(flash) = &marks.flash
                && flash.ordinal == key.index
            {
                for rect in range_rects(
                    &layout,
                    &flash.range,
                    ANNOTATION_WASH_PAD_X,
                    ANNOTATION_WASH_INSET_Y,
                ) {
                    window.paint_quad(quad(
                        rect,
                        px(ANNOTATION_WASH_RADIUS),
                        flash.wash,
                        px(0.0),
                        gpui::transparent_black(),
                        BorderStyle::default(),
                    ));
                }
            }
            if let Some(range) = selection.selection.borrow().wash_range(&key) {
                for rect in range_rects(&layout, &range, 0.0, 0.0) {
                    window.paint_quad(quad(
                        rect,
                        px(0.0),
                        selection_wash,
                        px(0.0),
                        gpui::transparent_black(),
                        BorderStyle::default(),
                    ));
                }
            }
            // Paint order is document order, so simply appending here
            // rebuilds the frame's selection continuity.
            selection.registry.borrow_mut().push(RegisteredText {
                key: key.clone(),
                text: Rc::from(text.as_ref()),
                block_break,
                geometry: TextGeometry::Text(layout.clone()),
            });
        }
    })
    .absolute()
    .size_full();

    div()
        .relative()
        .w_full()
        .min_w_0()
        .cursor(CursorStyle::IBeam)
        .child(underlay)
        .child(body)
        .into_any_element()
}

/// How far a mark's wash extends around the glyphs it covers.
const ANNOTATION_WASH_PAD_X: f32 = 1.0;
const ANNOTATION_WASH_INSET_Y: f32 = 1.0;
const ANNOTATION_WASH_RADIUS: f32 = 2.0;

fn text_element(flat: &Rc<FlatText>, key: TextKey, ctx: &Ctx) -> AnyElement {
    if ctx.math_enabled && flat.math.is_some() {
        return math_text::element(flat.clone(), key, ctx);
    }
    let runs = match ctx
        .cache
        .filter(|view| ctx.animate_streaming && view.streaming.get())
    {
        Some(view) => {
            let spans = view
                .veil
                .borrow_mut()
                .advance(key.index, flat.text.as_ref(), ctx.now);
            apply_veil(flat.runs.clone(), &spans)
        }
        None => flat.runs.clone(),
    };
    text_element_with_selection(
        flat,
        runs,
        key,
        ctx.selection.clone(),
        ctx.search.clone(),
        ctx.annotations.clone(),
        ctx.link_handler.clone(),
        ctx.palette.code_wash,
        ctx.palette.selection,
        ctx.palette.search_match,
        ctx.palette.active_search_match,
        ctx.take_block_break(),
    )
}

/// A selectable styled line outside the markdown block renderer.
///
/// Diff viewers and other virtualized code surfaces can share the transcript's
/// cross-element selection behavior without manufacturing a markdown tree.
/// The caller supplies a stable key in paint order and decides whether copying
/// across this element should insert a paragraph break or a single newline.
pub fn selectable_flat_text(
    flat: &FlatText,
    key: TextKey,
    selection: TranscriptSelection,
    code_wash: Hsla,
    selection_wash: Hsla,
    block_break: bool,
) -> AnyElement {
    text_element_with_selection(
        flat,
        flat.runs.clone(),
        key,
        selection,
        None,
        None,
        None,
        code_wash,
        selection_wash,
        gpui::transparent_black(),
        gpui::transparent_black(),
        block_break,
    )
}

/// A selectable plain-text element: user messages, tool output, anything that
/// is not markdown but still takes part in transcript-wide selection.
pub fn plain_text(
    text: impl Into<SharedString>,
    family: &'static str,
    weight: FontWeight,
    color: Hsla,
    ctx: &Ctx,
) -> AnyElement {
    let key = ctx.next_key();
    let flat = ctx.flat(key.index, || flatten_plain(text, family, weight, color));
    text_element(&flat, key, ctx)
}

/// A zero-size canvas that clears the frame's registry. Paint it *before* any
/// transcript text so the registry holds exactly this frame's visible elements.
pub fn frame_reset(selection: TranscriptSelection) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |_, _, _, _| selection.registry.borrow_mut().clear(),
    )
    .absolute()
    .w(px(0.0))
    .h(px(0.0))
}

// ── Selection geometry and input ───────────────────────────────────────────

/// Wash boxes for one byte range: one box per visual row the range covers, in
/// window coordinates, from the laid-out text's own geometry. `pad_x` overhangs
/// horizontally (inline code) and `inset_y` shrinks vertically; a selection
/// wash passes zero for both so its boxes tile seamlessly across wrapped rows.
fn range_rects(
    layout: &TextLayout,
    range: &Range<usize>,
    pad_x: f32,
    inset_y: f32,
) -> Vec<Bounds<Pixels>> {
    let mut rects = Vec::new();
    if range.is_empty() || layout_missing(layout) {
        return rects;
    }

    let bounds = layout.bounds();
    let line_height = layout.line_height();
    let mut row_top = bounds.top();
    let mut line_start = 0;

    // A soft-wrap boundary belongs to both adjacent rows, but GPUI's generic
    // `position_for_index` gives it the preceding row's caret affinity. Walking
    // with that API therefore has to jump beyond the boundary to make progress,
    // dropping the first glyph of every continuation row. Use the shaped wrap
    // boundaries directly, as Zed's Markdown renderer does, so adjacent visual
    // rows share the exact same byte boundary without a gap.
    for line in layout.line_layouts() {
        let line_end = line_start + line.len();
        let unwrapped = &line.unwrapped_layout;
        let row_ends = line
            .wrap_boundaries()
            .iter()
            .map(|boundary| {
                let glyph = &unwrapped.runs[boundary.run_ix].glyphs[boundary.glyph_ix];
                (line_start + glyph.index, glyph.position.x)
            })
            .chain([(line_end, unwrapped.width)]);
        let mut row_start = line_start;
        let mut row_start_x = Pixels::ZERO;

        for (row_end, row_end_x) in row_ends {
            let selected_start = range.start.max(row_start);
            let selected_end = range.end.min(row_end);
            if selected_start < selected_end {
                let x_for_index =
                    |index| bounds.left() + unwrapped.x_for_index(index - line_start) - row_start_x;
                let start_x = x_for_index(selected_start);
                let end_x = x_for_index(selected_end);
                if end_x > start_x {
                    rects.push(Bounds::new(
                        point(start_x - px(pad_x), row_top + px(inset_y)),
                        size(
                            end_x - start_x + px(2.0 * pad_x),
                            line_height - px(2.0 * inset_y),
                        ),
                    ));
                }
            }

            row_start = row_end;
            row_start_x = row_end_x;
            row_top += line_height;
        }

        // `TextLayout` separates hard lines with one newline byte, which has
        // no glyph box of its own.
        line_start = line_end + 1;
        if line_start > range.end {
            break;
        }
    }
    rects
}

/// Painted glyph boxes for a byte range in a registered text element.
/// Find-in-page uses this after a virtualized row mounts to reveal the exact
/// wrapped line rather than stopping at the top of a long message.
pub fn text_range_bounds(layout: &TextGeometry, range: &Range<usize>) -> Vec<Bounds<Pixels>> {
    match layout {
        TextGeometry::Text(layout) => range_rects(layout, range, 0.0, 0.0),
        TextGeometry::Math(layout) => layout.range_rects(range),
    }
}

/// `TextLayout::bounds` panics before prepaint has run. A row that was spliced
/// this frame can reach paint with a fresh layout, so probe first.
fn layout_missing(layout: &TextLayout) -> bool {
    layout.line_layouts().is_empty()
}

/// The registry entry containing `position`, else the nearest by vertical
/// distance so a drag through a gutter or between blocks clamps sensibly.
fn registry_point(
    registry: &SelectionRegistry<TextGeometry>,
    position: Point<Pixels>,
) -> Option<(usize, usize)> {
    let mut best: Option<(usize, f32)> = None;
    for (index, entry) in registry.entries().iter().enumerate() {
        if entry.geometry.is_missing() {
            continue;
        }
        let bounds = entry.geometry.bounds();
        let distance = if position.y < bounds.top() {
            f32::from(bounds.top() - position.y)
        } else if position.y > bounds.bottom() {
            f32::from(position.y - bounds.bottom())
        } else {
            0.0
        };
        if best.is_none_or(|(_, best)| distance < best) {
            best = Some((index, distance));
        }
        if distance == 0.0 {
            break;
        }
    }
    let (index, _) = best?;
    let offset = match registry.entries()[index]
        .geometry
        .index_for_position(position)
    {
        Ok(offset) | Err(offset) => offset,
    };
    Some((index, offset))
}

/// One end of the current selection: where it sits in document order (registry
/// index, byte offset), and the key/offset needed to re-anchor a drag there.
struct SelectionEnd {
    position: (usize, usize),
    key: TextKey,
    offset: usize,
}

/// The document extent of the current selection, from its first span to its
/// last, used to decide which end a shift-click grows.
fn selection_extent(
    selection: &Selection,
    registry: &SelectionRegistry<TextGeometry>,
) -> Option<(SelectionEnd, SelectionEnd)> {
    let spans = selection.spans();
    let first = spans.first()?;
    let last = spans.last()?;
    Some((
        SelectionEnd {
            position: (registry.position(&first.key)?, first.range.start),
            key: first.key.clone(),
            offset: first.range.start,
        },
        SelectionEnd {
            position: (registry.position(&last.key)?, last.range.end),
            key: last.key.clone(),
            offset: last.range.end,
        },
    ))
}

/// Install the frame's selection mouse listeners.
///
/// These live once per frame at the transcript root rather than once per
/// painted text element: the registry already holds every element's geometry,
/// so three closures replace three-per-element and a mouse move costs one
/// registry scan instead of one dispatch per visible paragraph.
pub fn install_selection_input(window: &mut Window, state: &TranscriptSelection) {
    window.on_mouse_event({
        let state = state.clone();
        move |event: &MouseDownEvent, phase, window, _| {
            if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                return;
            }
            let registry = state.registry.borrow();
            let hit = registry.entries().iter().enumerate().find(|(_, entry)| {
                !entry.geometry.is_missing() && entry.geometry.bounds().contains(&event.position)
            });
            let mut selection = state.selection.borrow_mut();
            // Shift-click grows the selection from the end the click is beyond:
            // a click past the current end keeps the start and moves the end, a
            // click before the start keeps the end and moves the start. The
            // click never shrinks it, the way Codex behaves.
            if event.modifiers.shift
                && let Some(head) = registry_point(&registry, event.position)
                && let Some((start, end)) = selection_extent(&selection, &registry)
            {
                let spans = if head >= end.position {
                    registry.resolve(start.position, head)
                } else if head <= start.position {
                    registry.resolve(end.position, head)
                } else {
                    // Inside the selection: leave it alone.
                    drop(selection);
                    drop(registry);
                    return;
                };
                let pivot = if head >= end.position { start } else { end };
                selection.extend_from(pivot.key, pivot.offset);
                let changed = selection.set_spans(spans);
                drop(selection);
                drop(registry);
                if changed {
                    window.refresh();
                }
                return;
            }
            match hit {
                Some((_, entry)) => {
                    let offset = match entry.geometry.index_for_position(event.position) {
                        Ok(offset) | Err(offset) => offset,
                    };
                    match event.click_count {
                        2 => selection.begin_with_span(
                            entry.key.clone(),
                            entry.text.clone(),
                            word_range(&entry.text, offset),
                        ),
                        count if count >= 3 => selection.begin_with_span(
                            entry.key.clone(),
                            entry.text.clone(),
                            line_range(&entry.text, offset),
                        ),
                        _ => selection.begin(entry.key.clone(), offset),
                    }
                    drop(selection);
                    drop(registry);
                    window.refresh();
                }
                None => {
                    let had_selection = !selection.is_empty();
                    selection.clear();
                    drop(selection);
                    drop(registry);
                    if had_selection {
                        window.refresh();
                    }
                }
            }
        }
    });

    window.on_mouse_event({
        let state = state.clone();
        move |event: &MouseMoveEvent, phase, window, _| {
            if phase != DispatchPhase::Bubble || !event.dragging() {
                return;
            }
            let registry = state.registry.borrow();
            let anchor = {
                let selection = state.selection.borrow();
                selection
                    .anchor()
                    .cloned()
                    .and_then(|key| selection.drag_anchor(&key).map(|offset| (key, offset)))
                    .and_then(|(key, offset)| registry.position(&key).map(|index| (index, offset)))
            };
            // The anchor scrolling out of the frame keeps the existing spans
            // rather than collapsing the selection.
            let Some((anchor_index, anchor_offset)) = anchor else {
                return;
            };
            let Some(head) = registry_point(&registry, event.position) else {
                return;
            };
            let spans = registry.resolve((anchor_index, anchor_offset), head);
            drop(registry);
            if state.selection.borrow_mut().set_spans(spans) {
                window.refresh();
            }
        }
    });

    window.on_mouse_event({
        let state = state.clone();
        move |_: &MouseUpEvent, phase, window, _| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let was_dragging = state.selection.borrow().is_dragging();
            let key = state.selection.borrow().anchor().cloned();
            if let Some(key) = key {
                state.selection.borrow_mut().end_drag(&key);
            }
            // The selection toolbar appears on release, so this frame has to
            // happen even though the pointer stopped moving.
            if was_dragging {
                window.refresh();
            }
        }
    });
}

// ── Blocks ─────────────────────────────────────────────────────────────────

/// Per-top-level-block ordinal stride: an element's ordinal is
/// `block_index << 16 | position_within_block`. Deriving keys from the
/// block's document index rather than a running document counter means a
/// walk that skips leading blocks ([`markdown_tail`]) hands every rendered
/// block exactly the flatten-cache and veil keys a full walk would, so the
/// two can alternate without thrashing either.
const BLOCK_ORDINAL_STRIDE_BITS: u32 = 16;

fn block_ordinal_base(block_ix: usize) -> usize {
    block_ix << BLOCK_ORDINAL_STRIDE_BITS
}

/// Find every non-empty regex match in the text elements produced by the
/// markdown renderer, in paint order. This deliberately walks the same block
/// shapes and advances the same element ordinals as [`render_block`], keeping
/// off-screen search results aligned with the exact glyph ranges that appear
/// once their virtualized transcript row mounts.
pub fn markdown_search_matches(
    source: &str,
    regex: &Regex,
    cap: usize,
) -> (Vec<TextSearchMatch>, bool) {
    let tree = super::parser::parse(source);
    let mut matches = Vec::new();
    for (block_ix, top) in tree.blocks.iter().enumerate() {
        let mut ordinal = block_ordinal_base(block_ix);
        if search_block(&top.block, &mut ordinal, regex, cap, &mut matches) {
            return (matches, true);
        }
    }
    (matches, false)
}

/// Find matches in one non-markdown text element.
pub fn plain_search_matches(
    text: &str,
    ordinal: usize,
    regex: &Regex,
    cap: usize,
) -> (Vec<TextSearchMatch>, bool) {
    let mut matches = Vec::new();
    let limited = search_text(text, ordinal, regex, cap, &mut matches);
    (matches, limited)
}

/// Flatten every shaped text element of a markdown source into the ordinal the
/// renderer assigns it, in paint order.
///
/// Deliberately the same block shapes and ordinal advance as
/// [`markdown_search_matches`] — the find bar's canonical walk — so an anchor
/// re-resolved here points at the exact glyph range the renderer will paint.
/// Pure and gpui-free, so the annotation re-anchor pass can run it on the
/// background executor instead of against a live [`MarkdownView`].
pub fn markdown_element_texts(source: &str) -> Vec<(usize, String)> {
    let tree = super::parser::parse(source);
    let mut texts = Vec::new();
    for (block_ix, top) in tree.blocks.iter().enumerate() {
        let mut ordinal = block_ordinal_base(block_ix);
        collect_block_texts(&top.block, &mut ordinal, &mut texts);
    }
    texts
}

fn collect_block_texts(block: &Block, ordinal: &mut usize, texts: &mut Vec<(usize, String)>) {
    match block {
        Block::Paragraph { runs } | Block::Heading { runs, .. } => {
            let text = runs.iter().map(|run| run.text.as_str()).collect::<String>();
            let current = *ordinal;
            *ordinal += 1;
            texts.push((current, text));
        }
        Block::CodeBlock { code, .. } | Block::DisplayMath { latex: code } => {
            let current = *ordinal;
            *ordinal += 1;
            texts.push((current, code.clone()));
        }
        Block::Image { .. } => {
            // The renderer consumes an ordinal for the image id, but the alt
            // caption is not a shaped text element.
            *ordinal += 1;
        }
        Block::BlockQuote { children } => {
            for child in children {
                collect_block_texts(child, ordinal, texts);
            }
        }
        Block::List { items, .. } => {
            for item in items {
                for child in &item.blocks {
                    collect_block_texts(child, ordinal, texts);
                }
            }
        }
        Block::Table { header, rows, .. } => {
            for cell in header.iter().chain(rows.iter().flat_map(|row| row.iter())) {
                let text = cell.iter().map(|run| run.text.as_str()).collect::<String>();
                let current = *ordinal;
                *ordinal += 1;
                texts.push((current, text));
            }
        }
        Block::Rule => {}
    }
}

fn search_block(
    block: &Block,
    ordinal: &mut usize,
    regex: &Regex,
    cap: usize,
    matches: &mut Vec<TextSearchMatch>,
) -> bool {
    match block {
        Block::Paragraph { runs } | Block::Heading { runs, .. } => {
            let text = runs.iter().map(|run| run.text.as_str()).collect::<String>();
            let current = *ordinal;
            *ordinal += 1;
            search_text(&text, current, regex, cap, matches)
        }
        Block::CodeBlock { code, .. } | Block::DisplayMath { latex: code } => {
            let current = *ordinal;
            *ordinal += 1;
            search_text(code, current, regex, cap, matches)
        }
        Block::Image { .. } => {
            // The renderer consumes an ordinal for the image id, but its alt
            // caption is not a selectable/shaped text element and therefore
            // has no glyph geometry for a find highlight.
            *ordinal += 1;
            false
        }
        Block::BlockQuote { children } => children
            .iter()
            .any(|child| search_block(child, ordinal, regex, cap, matches)),
        Block::List { items, .. } => items.iter().any(|item| {
            item.blocks
                .iter()
                .any(|child| search_block(child, ordinal, regex, cap, matches))
        }),
        Block::Table { header, rows, .. } => header
            .iter()
            .chain(rows.iter().flat_map(|row| row.iter()))
            .any(|cell| {
                let text = cell.iter().map(|run| run.text.as_str()).collect::<String>();
                let current = *ordinal;
                *ordinal += 1;
                search_text(&text, current, regex, cap, matches)
            }),
        Block::Rule => false,
    }
}

fn search_text(
    text: &str,
    ordinal: usize,
    regex: &Regex,
    cap: usize,
    matches: &mut Vec<TextSearchMatch>,
) -> bool {
    for found in regex.find_iter(text).filter(|found| !found.is_empty()) {
        if matches.len() >= cap {
            return true;
        }
        matches.push(TextSearchMatch {
            ordinal,
            range: found.range(),
        });
    }
    false
}

/// Render a markdown body. Returns `None` when it has no content.
pub fn markdown<'a>(view: &'a MarkdownView, ctx: &Ctx<'a>) -> Option<AnyElement> {
    markdown_capped(view, ctx, usize::MAX)
}

/// Like [`markdown`], but builds only the trailing `max_blocks` top-level
/// blocks. The live reasoning peek shows a tail-pinned viewport while a
/// thought streams, and building the whole growing document every pulse tick
/// made a long think O(document) per frame; the cap makes it O(window).
pub fn markdown_tail<'a>(
    view: &'a MarkdownView,
    ctx: &Ctx<'a>,
    max_blocks: usize,
) -> Option<AnyElement> {
    markdown_capped(view, ctx, max_blocks)
}

/// Extra pixels above and below the viewport that a windowed body renders.
/// It covers a scroll between frames: the plan is built from the row bounds of
/// the previous frame, so a jump larger than this leaves the dropped blocks
/// unbuilt until the bounds catch up on the next frame.
pub const MARKDOWN_WINDOW_MARGIN: f32 = 600.0;

/// The prologue every body pass shares: the document's top-level blocks, the
/// flatten cache scoped to this view, the height ledger, and the veil frame a
/// streaming body opens.
struct BodyPass<'a> {
    blocks: Vec<&'a TopBlock>,
    ctx: Ctx<'a>,
    heights: Rc<RefCell<Vec<(Range<usize>, Option<Pixels>)>>>,
    /// Whether this body is streaming with the dissolve animating.
    animate: bool,
}

/// Collect what a body pass needs before it builds a block. `None` when the
/// body has no content — the veil frame is closed first, so a caller returns
/// straight through.
fn begin_body<'a>(view: &'a MarkdownView, ctx: &Ctx<'a>) -> Option<BodyPass<'a>> {
    let blocks = view.top_blocks().collect::<Vec<_>>();
    let animate = ctx.animate_streaming && view.streaming.get();
    if blocks.is_empty() {
        if animate {
            let mut veil = view.veil.borrow_mut();
            veil.begin_frame();
            veil.finish_frame(ctx.now);
        }
        return None;
    }

    view.sync_style(ctx.palette, &ctx.metrics);
    if animate {
        view.veil.borrow_mut().begin_frame();
    }
    // Everything before the final block is settled, so its flattened elements
    // stay cacheable across appends; the volatile region is the mended display
    // tail, whose elements must be built even when the viewport is elsewhere.
    view.volatile_from
        .set(block_ordinal_base(view.parser.display_tail_start()));
    // Sized before any block is built, so every `MeasuredBlock` knows its own
    // slot; `resize` truncates too, which is what a shrunk body needs.
    let heights = view.heights.clone();
    heights.borrow_mut().resize(blocks.len(), (0..0, None));
    Some(BodyPass {
        blocks,
        ctx: ctx.with_cache(view),
        heights,
        animate,
    })
}

/// The part of a long streaming body the transcript viewport can see, in the
/// row's own pixel coordinates. The renderer builds only the blocks that
/// intersect this range (plus a margin and the volatile tail), so a dissolve
/// tick costs the visible body rather than the whole response.
#[derive(Clone, Copy)]
pub struct MessageBodyWindow {
    pub visible_top: f32,
    pub visible_height: f32,
    pub width: f32,
}

/// Render only the part of a long streaming body that a frame can show.
///
/// A streaming response row is one virtualized list item: the moment any part
/// of it is visible, `list()` rebuilds and lays out the *whole* body, and the
/// dissolve lease repeats that at up to 120 fps. This walks the block offsets
/// recorded from earlier layouts, builds the blocks intersecting the viewport
/// (plus a margin and the volatile tail), and stands in for the rest with
/// spacers sized from the same ledger. A body whose heights are not all known
/// yet falls back to one full pass, which measures it.
pub fn markdown_windowed<'a>(
    view: &'a MarkdownView,
    ctx: &Ctx<'a>,
    window: MessageBodyWindow,
) -> Option<AnyElement> {
    view.set_render_width(window.width);
    if view.has_async_blocks() {
        return markdown_capped(view, ctx, usize::MAX);
    }
    let BodyPass {
        blocks,
        ctx,
        heights,
        animate,
    } = begin_body(view, ctx)?;

    let gap = px(ctx.metrics.block_gap);
    let plan = window_plan(
        &heights,
        &blocks,
        gap,
        window.visible_top,
        window.visible_height,
        view.parser.display_tail_start(),
    );

    let mut children = Vec::with_capacity(plan.child_count());
    for (group_ix, group) in plan.groups.iter().enumerate() {
        if let Some(spacer) = plan.spacers[group_ix] {
            children.push(spacer_element(spacer));
        }
        children.extend(render_block_range(&blocks, &ctx, group.clone(), &heights));
    }

    if animate {
        view.veil.borrow_mut().finish_frame(ctx.now);
    }

    Some(block_column(children, &ctx).into_any_element())
}

/// Which blocks one frame of a windowed body builds, and the spacers that
/// stand in for the rest. `spacers` runs one entry per group: the spacer that
/// enters *before* that group. `None` marks a boundary at the body's own edge,
/// where no block was dropped and so no gap is owed; `Some` is a dropped
/// span's exact height, which is zero for a span of exactly one block gap and
/// must still be built for that gap. No spacer is owed after the last group:
/// the volatile tail always closes the window at the body's last block.
struct WindowPlan {
    groups: Vec<Range<usize>>,
    spacers: Vec<Option<Pixels>>,
}

impl WindowPlan {
    /// Elements one frame of this plan builds: every block of every group,
    /// plus the spacers standing in for the blocks it dropped.
    fn child_count(&self) -> usize {
        self.spacers
            .iter()
            .filter(|spacer| spacer.is_some())
            .count()
            + self.groups.iter().map(Range::len).sum::<usize>()
    }
}

/// Choose the block window for a viewport at `visible_top` in the body's own
/// coordinates. Blocks whose height is not yet known make the spacer
/// arithmetic a guess, so the plan falls back to the whole body and lets that
/// pass measure it.
fn window_plan(
    heights: &Rc<RefCell<Vec<(Range<usize>, Option<Pixels>)>>>,
    blocks: &[&TopBlock],
    gap: Pixels,
    visible_top: f32,
    visible_height: f32,
    volatile_start: usize,
) -> WindowPlan {
    let heights = heights.borrow();
    let count = blocks.len();
    // A height counts only when it was measured for the block that currently
    // owns this index; a re-partition makes the stored range disagree, and the
    // plan then measures afresh instead of using another block's height.
    let measured: Option<Vec<Pixels>> = blocks
        .iter()
        .enumerate()
        .map(|(index, top)| match heights.get(index) {
            Some((stored, height)) if stored == &top.range => *height,
            // The volatile region is always built, so its heights never size
            // a spacer — and mending rewrites its range every frame. Only a
            // mismatch before it means the ledger itself is misaligned.
            _ if index >= volatile_start => Some(Pixels::ZERO),
            _ => None,
        })
        .collect();
    let Some(measured) = measured else {
        return WindowPlan {
            groups: vec![0..count],
            spacers: vec![None],
        };
    };

    // `starts[i]` is block `i`'s top within the body; the trailing `gap` is
    // folded into each step so the flex column's own gap matches it exactly.
    let mut starts = Vec::with_capacity(count);
    let mut cursor = Pixels::ZERO;
    for height in &measured {
        starts.push(cursor);
        cursor += *height + gap;
    }

    let from = visible_top - MARKDOWN_WINDOW_MARGIN;
    let to = visible_top + visible_height + MARKDOWN_WINDOW_MARGIN;
    let mut groups: Vec<Range<usize>> = Vec::with_capacity(2);
    let start = starts
        .iter()
        .enumerate()
        .position(|(index, top)| f32::from(*top) + f32::from(measured[index]) > from)
        .unwrap_or(count);
    let end = starts
        .iter()
        .position(|top| f32::from(*top) >= to)
        .unwrap_or(count);
    if start < end {
        groups.push(start..end);
    }
    // The volatile tail is built every frame and always reaches the body's
    // last block, so it closes the window: no block is ever dropped below the
    // last group, and nothing is owed after it.
    let volatile = volatile_start.min(count)..count;
    if volatile.start < volatile.end {
        match groups.last_mut() {
            Some(last) if volatile.start <= last.end => last.end = last.end.max(volatile.end),
            _ => groups.push(volatile),
        }
    }

    // One spacer per group, standing in for the blocks dropped above it: the
    // hidden span minus one `gap`, because the flex column adds a gap on each
    // side of it, which is exactly the boundary the hidden block would have
    // had. The first span has only one neighbour, and a boundary at the body's
    // own edge drops no block and so owes no gap.
    let mut spacers = Vec::with_capacity(groups.len());
    for (index, group) in groups.iter().enumerate() {
        let before = if index == 0 {
            (group.start > 0).then(|| starts[group.start] - gap)
        } else {
            Some(starts[group.start] - starts[groups[index - 1].end] - gap)
        };
        spacers.push(before);
    }

    WindowPlan { groups, spacers }
}

/// A spacer standing in for the dropped blocks between two groups. It is built
/// even at zero height: the flex column's gaps on either side are the whole
/// point of it, and a span of exactly one gap leaves no height to give it.
fn spacer_element(height: Pixels) -> AnyElement {
    div().flex_none().w_full().h(height).into_any_element()
}

fn markdown_capped<'a>(
    view: &'a MarkdownView,
    ctx: &Ctx<'a>,
    max_blocks: usize,
) -> Option<AnyElement> {
    let BodyPass {
        blocks,
        ctx,
        heights,
        animate,
    } = begin_body(view, ctx)?;

    let first = blocks.len().saturating_sub(max_blocks);
    let children = render_block_range(&blocks, &ctx, first..blocks.len(), &heights);
    if animate {
        // Every element visible on the attach pass has synchronously adopted
        // its baseline. Elements introduced by later appends should now fade.
        view.veil.borrow_mut().finish_frame(ctx.now);
    }

    let element = block_column(children, &ctx);
    Some(
        if ctx.math_enabled
            && ctx.wrap_math_menu
            && let Some(menu) = &ctx.math_menu
        {
            context_menu(
                element,
                SharedString::from(format!("math-menu-{}", ctx.row)),
                menu,
                |_| Vec::new(),
            )
        } else {
            element.into_any_element()
        },
    )
}

fn block_column(children: Vec<AnyElement>, ctx: &Ctx) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(ctx.metrics.block_gap))
        .children(children)
}

/// Whether `block` contains content whose layout arrives after its first
/// frame: an image loading, or a formula rasterizing from selectable source.
fn block_has_async_content(block: &Block) -> bool {
    match block {
        Block::Image { .. } | Block::DisplayMath { .. } => true,
        Block::Paragraph { runs } | Block::Heading { runs, .. } => {
            runs.iter().any(|run| run.style.math)
        }
        Block::CodeBlock { .. } | Block::Rule => false,
        Block::BlockQuote { children } => children.iter().any(block_has_async_content),
        Block::List { items, .. } => items
            .iter()
            .any(|item| item.blocks.iter().any(block_has_async_content)),
        Block::Table { header, rows, .. } => header
            .iter()
            .flatten()
            .chain(rows.iter().flatten().flatten())
            .any(|run| run.style.math),
    }
}

/// Build one contiguous run of top-level blocks. Every rendered block records
/// its laid-out height, which is what lets a later frame window the body.
fn render_block_range(
    blocks: &[&TopBlock],
    ctx: &Ctx,
    range: Range<usize>,
    heights: &Rc<RefCell<Vec<(Range<usize>, Option<Pixels>)>>>,
) -> Vec<AnyElement> {
    let mut children = Vec::with_capacity(range.len());
    for block_ix in range {
        ctx.next_ordinal.set(block_ordinal_base(block_ix));
        let rendered = render_block(&blocks[block_ix].block, ctx);
        debug_assert!(
            ctx.next_ordinal.get() - block_ordinal_base(block_ix) < 1 << BLOCK_ORDINAL_STRIDE_BITS,
            "a single block overflowed its ordinal stride"
        );
        children.push(
            MeasuredBlock {
                inner: rendered,
                heights: heights.clone(),
                index: block_ix,
                range: blocks[block_ix].range.clone(),
            }
            .into_any_element(),
        );
    }
    children
}

/// Wrap a streaming body in the clip layer: measure its real height, and
/// while a gap is open report the continuous clip height instead.
pub fn clip_body(view: &MarkdownView, body: AnyElement) -> AnyElement {
    let body = measure_body(view, body);
    match view.clip_height() {
        // `items_start` matters: a default flex row would stretch the body to
        // the clip height, and the controller would never see the real one.
        Some(clip) => div()
            .w_full()
            .min_w_0()
            .h(clip)
            .flex()
            .items_start()
            .overflow_hidden()
            .child(body)
            .into_any_element(),
        None => body,
    }
}

/// Wrap a streaming body so the clip controller knows its real height.
fn measure_body(view: &MarkdownView, inner: AnyElement) -> AnyElement {
    MeasuredBody {
        inner,
        height: view.body_height.clone(),
    }
    .into_any_element()
}

/// The body wrapper behind [`measure_body`].
struct MeasuredBody {
    inner: AnyElement,
    height: Rc<Cell<Option<Pixels>>>,
}

impl IntoElement for MeasuredBody {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for MeasuredBody {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut gpui::App,
    ) -> (gpui::LayoutId, ()) {
        (self.inner.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        self.height.set(Some(bounds.size.height));
        self.inner.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        self.inner.paint(window, cx);
    }
}

/// Records the height a block was laid out at, for the window planner.
struct MeasuredBlock {
    inner: AnyElement,
    heights: Rc<RefCell<Vec<(Range<usize>, Option<Pixels>)>>>,
    index: usize,
    range: Range<usize>,
}

impl IntoElement for MeasuredBlock {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for MeasuredBlock {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut gpui::App,
    ) -> (gpui::LayoutId, ()) {
        (self.inner.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        self.heights.borrow_mut()[self.index] = (self.range.clone(), Some(bounds.size.height));
        self.inner.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        self.inner.paint(window, cx);
    }
}

fn render_block(block: &Block, ctx: &Ctx) -> AnyElement {
    ctx.starts_block.set(true);
    match block {
        Block::Paragraph { runs } => {
            let key = ctx.next_key();
            let flat = ctx.flat(key.index, || {
                flatten(runs, ctx.palette, FontWeight::NORMAL, ctx.palette.text)
            });
            div()
                .w_full()
                .min_w_0()
                .text_size(px(ctx.metrics.text_size))
                .line_height(px(ctx.metrics.line_height))
                .child(text_element(&flat, key, ctx))
                .into_any_element()
        }
        Block::Heading { level, runs } => {
            let (size, line_height, weight) = heading_metrics(*level, &ctx.metrics);
            let key = ctx.next_key();
            let flat = ctx.flat(key.index, || {
                flatten(runs, ctx.palette, weight, ctx.palette.text)
            });
            div()
                .w_full()
                .min_w_0()
                .when(*level <= 2, |element| element.pt(px(4.0)))
                .text_size(px(size))
                .line_height(px(line_height))
                .child(text_element(&flat, key, ctx))
                .into_any_element()
        }
        Block::Image { url, alt } => render_image(url, alt, ctx),
        Block::DisplayMath { latex } => {
            let key = ctx.next_key();
            let flat = ctx.flat(key.index, || {
                let mut flat = flatten_plain(
                    latex.clone(),
                    MONO_FAMILY,
                    FontWeight::NORMAL,
                    ctx.palette.text,
                );
                flat.math = Some(Rc::new(math_text::MathData::new(vec![
                    math_text::MathSpan {
                        range: 0..latex.len(),
                        latex: std::sync::Arc::from(latex.as_str()),
                        display: true,
                    },
                ])));
                flat
            });
            div()
                .w_full()
                .min_w_0()
                .text_size(px(ctx.metrics.text_size))
                .line_height(px(ctx.metrics.line_height))
                .child(text_element(&flat, key, ctx))
                .into_any_element()
        }
        Block::CodeBlock { language, code } => render_code_block(language.as_deref(), code, ctx),
        Block::BlockQuote { children } => {
            let rendered = children
                .iter()
                .map(|child| render_block(child, ctx))
                .collect::<Vec<_>>();
            div()
                .w_full()
                .min_w_0()
                .flex()
                .gap(px(10.0))
                .child(
                    div()
                        .w(px(2.0))
                        .flex_none()
                        .rounded_full()
                        .bg(ctx.palette.border),
                )
                .child(
                    div()
                        // flex_auto, not flex_1: a zero flex-basis erases the
                        // content's intrinsic width, collapsing shrink-wrapped
                        // user bubbles to the quote bar.
                        .flex_auto()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(ctx.metrics.block_gap))
                        .children(rendered),
                )
                .into_any_element()
        }
        Block::List {
            ordered_start,
            items,
        } => render_list(*ordered_start, items, ctx),
        Block::Table {
            header,
            rows,
            align,
        } => render_table(header, rows, align, ctx),
        Block::Rule => div()
            .w_full()
            .h(px(1.0))
            .my(px(4.0))
            .bg(ctx.palette.border)
            .into_any_element(),
    }
}

fn render_list(ordered_start: Option<u64>, items: &[ListItem], ctx: &Ctx) -> AnyElement {
    let marker_width = if ordered_start.is_some() { 22.0 } else { 14.0 };
    let rendered = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let marker = match (ordered_start, item.task) {
                (_, Some(checked)) => div()
                    .w(px(marker_width))
                    .flex_none()
                    .flex()
                    .justify_start()
                    .child(checkbox(checked, ctx))
                    .into_any_element(),
                (Some(start), None) => {
                    marker_text(format!("{}.", start + index as u64), marker_width, ctx)
                }
                (None, None) => marker_text("•".to_owned(), marker_width, ctx),
            };
            let blocks = item
                .blocks
                .iter()
                .map(|block| render_block(block, ctx))
                .collect::<Vec<_>>();
            div()
                .w_full()
                .min_w_0()
                .flex()
                .items_start()
                .child(marker)
                .child(
                    div()
                        // flex_auto, not flex_1: a zero flex-basis erases the
                        // content's intrinsic width, collapsing shrink-wrapped
                        // user bubbles to the marker column.
                        .flex_auto()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(ctx.metrics.block_gap * 0.6))
                        .children(blocks),
                )
                .into_any_element()
        })
        .collect::<Vec<_>>();

    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(ctx.metrics.block_gap * 0.5))
        .children(rendered)
        .into_any_element()
}

/// A list marker. Markers are not selectable: they are generated ornament, not
/// content the user typed, so they stay out of the selection registry.
fn marker_text(label: String, width: f32, ctx: &Ctx) -> AnyElement {
    div()
        .w(px(width))
        .flex_none()
        .text_size(px(ctx.metrics.text_size))
        .line_height(px(ctx.metrics.line_height))
        .text_color(ctx.palette.tertiary)
        .child(SharedString::from(label))
        .into_any_element()
}

fn checkbox(checked: bool, ctx: &Ctx) -> AnyElement {
    let box_size = (ctx.metrics.text_size * 0.92).round();
    div()
        .size(px(box_size))
        .my(px(((ctx.metrics.line_height - box_size) / 2.0).max(0.0)))
        .flex_none()
        .rounded(px(3.0))
        .border_1()
        .border_color(if checked {
            ctx.palette.accent
        } else {
            ctx.palette.border
        })
        .when(checked, |element| element.bg(ctx.palette.accent))
        .flex()
        .items_center()
        .justify_center()
        .when(checked, |element| {
            element.child(crate::ui::icon(
                "icons/check.svg",
                box_size - 4.0,
                ctx.palette.inset,
            ))
        })
        .into_any_element()
}

/// An inline image. Data URLs decode in place; anything else is handed to GPUI
/// to load. The alt text renders beneath as a caption when there is one, so a
/// failed or slow load still says what it was.
fn render_image(url: &str, alt: &str, ctx: &Ctx) -> AnyElement {
    const MAX_HEIGHT: f32 = 320.0;

    let key = ctx.next_key();
    let id = SharedString::from(format!("image-{}-{}", key.row, key.index));
    let image = match decode_data_url(url) {
        Some(decoded) => img(decoded).id(id),
        None => img(url.to_owned()).id(id),
    };
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(
            image
                .max_w(relative(1.0))
                .max_h(px(MAX_HEIGHT))
                .rounded(px(6.0))
                .object_fit(gpui::ObjectFit::ScaleDown),
        )
        .when(!alt.trim().is_empty(), |element| {
            element.child(
                div()
                    .text_size(px((ctx.metrics.text_size - 2.0).max(12.5)))
                    .line_height(px(ctx.metrics.line_height - 4.0))
                    .text_color(ctx.palette.ghost)
                    .child(SharedString::from(alt.to_owned())),
            )
        })
        .into_any_element()
}

const CODE_COPY_FEEDBACK_DURATION: Duration = Duration::from_secs(3);
type CodeCopyFeedback = Rc<RefCell<HashMap<usize, u64>>>;

fn begin_code_copy_feedback(feedback: &CodeCopyFeedback, ordinal: usize) -> u64 {
    let mut feedback = feedback.borrow_mut();
    let generation = feedback
        .get(&ordinal)
        .copied()
        .unwrap_or_default()
        .wrapping_add(1);
    feedback.insert(ordinal, generation);
    generation
}

fn clear_code_copy_feedback(feedback: &CodeCopyFeedback, ordinal: usize, generation: u64) -> bool {
    let mut feedback = feedback.borrow_mut();
    if feedback.get(&ordinal) != Some(&generation) {
        return false;
    }
    feedback.remove(&ordinal);
    true
}

fn show_code_copied(feedback: CodeCopyFeedback, ordinal: usize, cx: &mut gpui::App) {
    let generation = begin_code_copy_feedback(&feedback, ordinal);
    cx.refresh_windows();
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(CODE_COPY_FEEDBACK_DURATION)
            .await;
        if clear_code_copy_feedback(&feedback, ordinal, generation) {
            cx.refresh();
        }
    })
    .detach();
}

/// Decode a `data:` image URL. Shared with the transcript's tool-output images.
pub fn decode_data_url(url: &str) -> Option<std::sync::Arc<gpui::Image>> {
    use base64::Engine as _;

    let (header, encoded) = url.split_once(',')?;
    let mime_type = header.strip_prefix("data:")?.split(';').next()?;
    let format = gpui::ImageFormat::from_mime_type(mime_type)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    (!bytes.is_empty()).then(|| std::sync::Arc::new(gpui::Image::from_bytes(format, bytes)))
}

fn render_code_block(language: Option<&str>, code: &str, ctx: &Ctx) -> AnyElement {
    let key = ctx.next_key();
    // Tokenizing is the most expensive flatten in the document, so a settled
    // code block is exactly the case the cache exists for.
    let flat = ctx.flat(key.index, || {
        let lang = language.and_then(highlight::lang_for_tag);
        let mut code_font = font(MONO_FAMILY);
        code_font.weight = FontWeight::NORMAL;
        FlatText {
            text: SharedString::from(code.to_owned()),
            runs: code_runs(code, lang, &code_font, ctx.palette),
            links: Vec::new(),
            code_ranges: Vec::new(),
            math: None,
        }
    });
    let label = language
        .filter(|language| !language.is_empty())
        .map(|language| language.to_ascii_lowercase());
    // Reuse the cached shaped string. Settled code blocks render every frame,
    // so cloning the whole source here would turn the copy affordance into a
    // permanent O(code length) render cost; allocate only when it is invoked.
    let copy_content = flat.text.clone();
    let keyboard_copy_content = copy_content.clone();
    let copy_feedback = ctx.cache.map(|view| view.copied_code_blocks.clone());
    let copied = copy_feedback
        .as_ref()
        .is_some_and(|feedback| feedback.borrow().contains_key(&key.index));
    let keyboard_copy_feedback = copy_feedback.clone();
    let ordinal = key.index;
    let copy_button = div()
        .id(SharedString::from(format!(
            "copy-code-{}-{}",
            key.row, key.index
        )))
        .tab_index(0)
        .size(px(24.0))
        .flex_none()
        .rounded(px(5.0))
        .flex()
        .items_center()
        .justify_center()
        .cursor_default()
        .focus_visible(|style| style.border_1().border_color(ctx.palette.accent))
        .hover(|style| style.bg(ctx.palette.overlay))
        .child(crate::ui::icon(
            if copied {
                "icons/check.svg"
            } else {
                "icons/copy.svg"
            },
            11.0,
            ctx.palette.ghost,
        ))
        .tooltip(Tooltip::text(if copied {
            tr!("common.copied")
        } else {
            tr!("common.copy_code")
        }))
        .on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(copy_content.to_string()));
            if let Some(feedback) = copy_feedback.clone() {
                show_code_copied(feedback, ordinal, cx);
            }
        })
        .on_key_down(move |event: &KeyDownEvent, _, cx| {
            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                cx.write_to_clipboard(ClipboardItem::new_string(keyboard_copy_content.to_string()));
                if let Some(feedback) = keyboard_copy_feedback.clone() {
                    show_code_copied(feedback, ordinal, cx);
                }
                cx.stop_propagation();
            }
        });

    div()
        .id(SharedString::from(format!(
            "code-block-{}-{}",
            key.row, key.index
        )))
        .tab_group()
        .tab_stop(false)
        .w_full()
        .min_w_0()
        .rounded(px(8.0))
        // The faint wash the tool activity detail blocks use, so a fenced
        // block reads as part of the prose instead of a dark panel.
        .bg(ctx.palette.overlay.opacity(0.7))
        .overflow_hidden()
        .child(
            div()
                .w_full()
                .h(px(28.0))
                .pl(px(10.0))
                .pr(px(2.0))
                .flex()
                .items_center()
                .border_b_1()
                .border_color(ctx.palette.border)
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_size(px(12.5))
                        .line_height(px(14.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(ctx.palette.ghost)
                        .when_some(label, |element, label| {
                            element.child(SharedString::from(label))
                        }),
                )
                .child(copy_button),
        )
        .child(
            div()
                .id(SharedString::from(format!(
                    "code-{}-{}",
                    key.row, key.index
                )))
                .w_full()
                .min_w_0()
                .px(px(10.0))
                .py(px(8.0))
                .child(
                    div()
                        .w_full()
                        .min_w_0()
                        .whitespace_normal()
                        .text_size(px(ctx.metrics.code_text_size))
                        .line_height(px(ctx.metrics.code_line_height))
                        .text_color(ctx.palette.secondary)
                        .child(text_element(&flat, key, ctx)),
                ),
        )
        .into_any_element()
}

/// `TextRun`s that tile `code` exactly, colored by the lexer. Every run shares
/// one font, so the shaped width of a line is identical with or without
/// highlighting — the property that makes coloring safe to defer.
fn code_runs(code: &str, lang: Option<Lang>, code_font: &Font, palette: &Palette) -> Vec<TextRun> {
    let plain = palette.secondary;
    let mut runs: Vec<TextRun> = Vec::new();
    let push = |runs: &mut Vec<TextRun>, len: usize, color: Hsla| {
        if len == 0 {
            return;
        }
        match runs.last_mut() {
            Some(last) if last.color == color => last.len += len,
            _ => runs.push(TextRun {
                len,
                font: code_font.clone(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }),
        }
    };

    let tokenized = lang.map(|lang| highlight::tokenize(lang, code));
    let lines = code.split('\n').collect::<Vec<_>>();
    for (index, line) in lines.iter().enumerate() {
        let tokens = tokenized
            .as_ref()
            .and_then(|lines| lines.get(index))
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut cursor = 0;
        for token in tokens {
            push(&mut runs, token.range.start.saturating_sub(cursor), plain);
            push(&mut runs, token.range.len(), palette.token(token.class));
            cursor = token.range.end;
        }
        push(&mut runs, line.len().saturating_sub(cursor), plain);
        if index + 1 < lines.len() {
            // The '\n' separator must belong to a run or shaping rejects them.
            push(&mut runs, 1, plain);
        }
    }
    runs
}

fn render_table(
    header: &[Vec<InlineRun>],
    rows: &[Vec<Vec<InlineRun>>],
    align: &[TableAlign],
    ctx: &Ctx,
) -> AnyElement {
    let columns = header
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if columns == 0 {
        return div().into_any_element();
    }
    let widths = column_widths(header, rows, columns);

    let mut table = div()
        .w_full()
        .min_w_0()
        .rounded(px(8.0))
        .border_1()
        .border_color(ctx.palette.border)
        .overflow_hidden()
        .flex()
        .flex_col();

    if !header.is_empty() {
        table = table.child(
            table_row(header, &widths, align, ctx, FontWeight::SEMIBOLD, true)
                .bg(ctx.palette.overlay),
        );
    }
    for (index, row) in rows.iter().enumerate() {
        table = table.child(table_row(
            row,
            &widths,
            align,
            ctx,
            FontWeight::NORMAL,
            index + 1 < rows.len(),
        ));
    }
    table.into_any_element()
}

fn table_row(
    cells: &[Vec<InlineRun>],
    widths: &[f32],
    align: &[TableAlign],
    ctx: &Ctx,
    weight: FontWeight,
    divider: bool,
) -> gpui::Div {
    let mut row = div()
        .w_full()
        .min_w_0()
        .flex()
        .items_start()
        .when(divider, |element| {
            element.border_b_1().border_color(ctx.palette.border)
        });
    for (index, cell) in cells.iter().enumerate() {
        let key = ctx.next_key();
        let flat = ctx.flat(key.index, || {
            flatten(cell, ctx.palette, weight, ctx.palette.text)
        });
        let alignment = align.get(index).copied().unwrap_or_default();
        row = row.child(
            div()
                .w(relative(widths.get(index).copied().unwrap_or(0.0)))
                .min_w_0()
                .px(px(9.0))
                .py(px(6.0))
                .text_size(px((ctx.metrics.text_size - 0.5).max(12.5)))
                .line_height(px(ctx.metrics.line_height - 2.0))
                .map(|element| match alignment {
                    TableAlign::Left => element,
                    TableAlign::Center => element.items_center().text_center(),
                    TableAlign::Right => element.items_end().text_right(),
                })
                .child(text_element(&flat, key, ctx)),
        );
    }
    row
}

/// Content-proportional column widths as fractions of the table, floored so a
/// narrow column stays readable.
fn column_widths(
    header: &[Vec<InlineRun>],
    rows: &[Vec<Vec<InlineRun>>],
    columns: usize,
) -> Vec<f32> {
    const MIN_FRACTION_SCALE: f32 = 0.55;

    let mut content = vec![0.0f32; columns];
    let mut note = |index: usize, cell: &Vec<InlineRun>| {
        if let Some(slot) = content.get_mut(index) {
            let length = cell
                .iter()
                .map(|run| run.text.chars().count())
                .sum::<usize>();
            *slot = slot.max(length as f32);
        }
    };
    for (index, cell) in header.iter().enumerate() {
        note(index, cell);
    }
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            note(index, cell);
        }
    }

    let even = 1.0 / columns as f32;
    let floor = even * MIN_FRACTION_SCALE;
    if content.iter().sum::<f32>() <= 0.0 {
        return vec![even; columns];
    }

    // Water-fill rather than clamp-then-renormalise: renormalising after a
    // clamp erodes the very floor it just applied. Each pass pins whatever fell
    // under the floor at exactly the floor and shares the remaining budget
    // among the rest, so the fractions still sum to one and every column clears
    // the floor. Terminates in at most `columns` passes.
    let mut widths = vec![even; columns];
    let mut pinned = vec![false; columns];
    loop {
        let free = (0..columns).filter(|index| !pinned[*index]).count();
        if free == 0 {
            break;
        }
        let budget = 1.0 - floor * (columns - free) as f32;
        let free_content = (0..columns)
            .filter(|index| !pinned[*index])
            .map(|index| content[index])
            .sum::<f32>();
        let mut pinned_any = false;
        for index in 0..columns {
            if pinned[index] {
                continue;
            }
            widths[index] = if free_content > 0.0 {
                budget * content[index] / free_content
            } else {
                budget / free as f32
            };
            if widths[index] < floor {
                widths[index] = floor;
                pinned[index] = true;
                pinned_any = true;
            }
        }
        if !pinned_any {
            break;
        }
    }
    widths
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::md::parser;
    use gpui::TestAppContext;

    fn palette() -> Palette {
        Palette::from_theme(&Theme::dark())
    }

    #[test]
    fn math_search_uses_the_same_source_ranges_and_ordinals_as_selection() {
        let source = "before $x^2$ after\n\n$$x^2$$\n\n| Value |\n| --- |\n| $x^2$ |";
        let (matches, limited) = markdown_search_matches(source, &Regex::new(r"x\^2").unwrap(), 20);
        assert!(!limited);
        assert_eq!(
            matches
                .iter()
                .map(|found| (found.ordinal, found.range.clone()))
                .collect::<Vec<_>>(),
            vec![(0, 7..10), (1 << 16, 0..3), ((2 << 16) + 1, 0..3)]
        );
    }

    /// The re-anchor pass flattens a reply's text elements itself, on the
    /// background executor, so it must land on exactly the ordinals the
    /// renderer and the find bar use. Checked against the search walk's hits,
    /// which are the same address space a mark paints in.
    #[test]
    fn markdown_element_texts_agrees_with_the_search_walk() {
        let source = "first\n\n> quoted *needle*\n\n```txt\nneedle\n```";
        let (matches, limited) =
            markdown_search_matches(source, &Regex::new("needle").unwrap(), 20);
        assert!(!limited);
        assert_eq!(matches.len(), 2);
        let texts = markdown_element_texts(source);
        for found in &matches {
            let (_, text) = texts
                .iter()
                .find(|(ordinal, _)| *ordinal == found.ordinal)
                .expect("every search ordinal has an element");
            assert_eq!(&text[found.range.clone()], "needle");
        }
        // Card markers, images and rules consume ordinals without a shaped
        // element; the walk still has to leave the gap so later blocks agree.
        assert!(texts.iter().any(|(ordinal, _)| *ordinal == 2 << 16));
    }

    fn runs_of(source: &str) -> Vec<InlineRun> {
        match &parser::parse(source).blocks[0].block {
            Block::Paragraph { runs } => runs.clone(),
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    /// `StyledText::with_runs` panics unless the runs tile the text exactly.
    fn assert_runs_tile(flat: &FlatText) {
        let total = flat.runs.iter().map(|run| run.len).sum::<usize>();
        assert_eq!(
            total,
            flat.text.len(),
            "runs must tile the text exactly: {:?}",
            flat.text
        );
    }

    #[test]
    fn flattened_runs_tile_the_text_and_carry_styles() {
        let flat = flatten(
            &runs_of("plain **bold** `code` [link](https://example.com) ~~gone~~"),
            &palette(),
            FontWeight::NORMAL,
            palette().text,
        );
        assert_runs_tile(&flat);
        assert_eq!(flat.text.as_ref(), "plain bold code link gone");
        assert_eq!(flat.links.len(), 1);
        assert_eq!(&flat.text[flat.links[0].0.clone()], "link");
        assert_eq!(flat.links[0].1, "https://example.com");
        assert_eq!(flat.code_ranges.len(), 1);
        assert_eq!(&flat.text[flat.code_ranges[0].clone()], "code");
        assert!(
            flat.runs
                .iter()
                .any(|run| run.strikethrough.is_some() && run.len == 4)
        );
        assert!(flat.runs.iter().any(|run| run.underline.is_some()));
    }

    #[test]
    fn a_streaming_link_is_styled_but_not_clickable() {
        let flat = flatten(
            &runs_of(&format!("see [docs]({PENDING_LINK_URL})")),
            &palette(),
            FontWeight::NORMAL,
            palette().text,
        );
        assert_runs_tile(&flat);
        assert!(
            flat.links.is_empty(),
            "the pending sentinel must not register a clickable range"
        );
        assert!(
            flat.runs.iter().any(|run| run.underline.is_some()),
            "but it should still look like a link"
        );
    }

    #[test]
    fn adjacent_code_and_link_runs_merge_into_one_range() {
        let flat = flatten(
            &runs_of("[**a** `b`](https://x) tail"),
            &palette(),
            FontWeight::NORMAL,
            palette().text,
        );
        assert_runs_tile(&flat);
        assert_eq!(flat.links.len(), 1, "one link, not one per styled run");
        assert_eq!(&flat.text[flat.links[0].0.clone()], "a b");
    }

    #[test]
    fn plain_flatten_tiles_and_handles_empty_text() {
        let flat = flatten_plain("hello", MONO_FAMILY, FontWeight::NORMAL, palette().text);
        assert_runs_tile(&flat);
        assert_eq!(flat.runs.len(), 1);

        let empty = flatten_plain("", SANS_FAMILY, FontWeight::NORMAL, palette().text);
        assert_runs_tile(&empty);
        assert!(empty.runs.is_empty());
    }

    #[test]
    fn code_runs_tile_the_block_including_newlines() {
        let code = "fn main() {\n    let x = 1; // c\n}";
        let mut code_font = font(MONO_FAMILY);
        code_font.weight = FontWeight::NORMAL;
        let runs = code_runs(code, Some(Lang::Rust), &code_font, &palette());
        assert_eq!(
            runs.iter().map(|run| run.len).sum::<usize>(),
            code.len(),
            "code runs must tile the whole block, newlines included"
        );
        assert!(runs.len() > 1, "highlighting should produce several runs");

        // Without a language the block is one plain run of the same length.
        let plain = code_runs(code, None, &code_font, &palette());
        assert_eq!(plain.iter().map(|run| run.len).sum::<usize>(), code.len());
        assert_eq!(plain.len(), 1);
    }

    #[test]
    fn code_block_rendering_wraps_and_exposes_a_keyboard_copy_control() {
        let source = include_str!("render.rs");
        let start = source
            .find("\nfn render_code_block(")
            .expect("code block renderer");
        let body = &source[start + 1..];
        let end = body
            .find("\nfn code_runs(")
            .expect("code block renderer end");
        let body = &body[..end];

        assert!(body.contains(".whitespace_normal()"));
        assert!(!body.contains(".overflow_x_scroll()"));
        assert!(!body.contains(".whitespace_nowrap()"));
        assert!(body.contains("\"icons/copy.svg\""));
        assert!(body.contains("\"icons/check.svg\""));
        assert!(body.contains("ClipboardItem::new_string"));
        assert!(body.contains("show_code_copied"));
        assert!(body.contains(".tab_index(0)"));
        assert!(body.contains(".on_key_down"));
    }

    #[test]
    fn copied_code_feedback_resets_after_three_seconds_and_ignores_stale_timers() {
        assert_eq!(CODE_COPY_FEEDBACK_DURATION, Duration::from_secs(3));

        let feedback = Rc::new(RefCell::new(HashMap::new()));
        let first = begin_code_copy_feedback(&feedback, 4);
        let second = begin_code_copy_feedback(&feedback, 4);

        assert!(!clear_code_copy_feedback(&feedback, 4, first));
        assert!(feedback.borrow().contains_key(&4));
        assert!(clear_code_copy_feedback(&feedback, 4, second));
        assert!(!feedback.borrow().contains_key(&4));
    }

    /// A soft-wrap boundary has two caret affinities. GPUI's generic
    /// `position_for_index` resolves it to the preceding row, so selection
    /// geometry must use the wrapped rows themselves or it can skip the first
    /// glyph on every continuation row.
    #[gpui::test]
    fn wrapped_selection_starts_at_each_continuation_row_origin(cx: &mut TestAppContext) {
        struct TestWindow;

        impl gpui::Render for TestWindow {
            fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
                div()
            }
        }

        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        let text: SharedString =
            "one two three four five six seven eight nine ten eleven twelve".into();
        let styled = StyledText::new(text.clone());
        let layout = styled.layout().clone();

        cx.draw(Point::default(), size(px(96.0), px(400.0)), move |_, _| {
            div()
                .w(px(96.0))
                .text_size(px(14.0))
                .line_height(px(20.0))
                .child(styled)
        });

        let rects = range_rects(&layout, &(0..text.len()), 0.0, 0.0);
        assert!(rects.len() >= 3, "fixture must wrap across several rows");
        let left = layout.bounds().left();
        assert!(
            rects.iter().all(|rect| rect.left() == left),
            "a full selection must include each wrapped row's first glyph: {rects:?}"
        );
    }

    /// Highlighting must never change the shaped length of a code block, or a
    /// deferred colorize would reflow the row.
    #[test]
    fn highlighting_never_changes_run_lengths() {
        let code = "const a = `t ${b}`;\n// note\nlet n = 0x1F;";
        let mut code_font = font(MONO_FAMILY);
        code_font.weight = FontWeight::NORMAL;
        let highlighted = code_runs(code, Some(Lang::Script), &code_font, &palette());
        let plain = code_runs(code, None, &code_font, &palette());
        assert_eq!(
            highlighted.iter().map(|run| run.len).sum::<usize>(),
            plain.iter().map(|run| run.len).sum::<usize>()
        );
        assert!(highlighted.iter().all(|run| &run.font == &code_font));
    }

    #[test]
    fn markdown_view_reuses_settled_elements_across_appends() {
        fn stub(label: &str) -> FlatText {
            flatten_plain(
                label.to_owned(),
                SANS_FAMILY,
                FontWeight::NORMAL,
                palette().text,
            )
        }

        let mut view = MarkdownView::new();
        view.set_text("First block.\n\nSecond bl", true);
        // Stand in for a render pass: two blocks, the second still streaming.
        let settled = view.flat(0, || stub("a"));
        let streaming = view.flat(1, || stub("b"));
        view.volatile_from.set(1);

        view.set_text("First block.\n\nSecond block.", true);

        // The settled block is reused by identity — no re-flatten, no alloc.
        let after = view.flat(0, || panic!("a settled block must not be rebuilt"));
        assert!(Rc::ptr_eq(&settled, &after));

        // The block that changed is rebuilt.
        let rebuilt = view.flat(1, || stub("c"));
        assert!(!Rc::ptr_eq(&streaming, &rebuilt));
        assert_eq!(rebuilt.text.as_ref(), "c");
    }

    /// Colors live inside `TextRun`s, so a theme switch has to drop the cache
    /// or the transcript keeps painting the previous palette.
    #[test]
    fn a_style_change_drops_cached_flats() {
        let view = MarkdownView::new();
        let dark = Palette::from_theme(&Theme::dark());
        let light = Palette::from_theme(&Theme::light());

        view.sync_style(&dark, &Metrics::BODY);
        let cached = view.flat(0, || {
            flatten_plain("a", SANS_FAMILY, FontWeight::NORMAL, dark.text)
        });

        view.sync_style(&dark, &Metrics::BODY);
        assert!(
            Rc::ptr_eq(
                &cached,
                &view.flat(0, || panic!("an unchanged style must reuse the cache"))
            ),
            "re-syncing the same style must not invalidate"
        );

        view.sync_style(&light, &Metrics::BODY);
        let relit = view.flat(0, || {
            flatten_plain("a", SANS_FAMILY, FontWeight::NORMAL, light.text)
        });
        assert!(!Rc::ptr_eq(&cached, &relit));
        assert_eq!(relit.runs[0].color, light.text);
    }

    /// Reasoning text goes through the same view as a response, so a plain
    /// prose block must actually produce renderable blocks.
    #[test]
    fn a_view_over_plain_prose_yields_blocks() {
        let mut view = MarkdownView::new();
        view.set_text("Let me check the parser first.", false);
        assert_eq!(view.blocks().count(), 1);

        // Streaming (mended) content too.
        let mut streaming = MarkdownView::new();
        streaming.set_text("Let me check the **parser", true);
        assert_eq!(streaming.blocks().count(), 1);

        // Empty content has nothing to render, which is the only case where
        // the renderer legitimately produces no element.
        let mut empty = MarkdownView::new();
        empty.set_text("", false);
        assert_eq!(empty.blocks().count(), 0);
    }

    #[test]
    fn markdown_view_blocks_swap_in_the_mended_tail() {
        let mut view = MarkdownView::new();
        view.set_text("Settled.\n\nNow **bold", true);
        let bold = view.blocks().any(|block| match block {
            Block::Paragraph { runs } => runs.iter().any(|run| run.style.bold),
            _ => false,
        });
        assert!(bold, "streaming emphasis should be styled");

        // Settled rendering keeps the markers literal.
        view.set_text("Settled.\n\nNow **bold", false);
        let bold = view.blocks().any(|block| match block {
            Block::Paragraph { runs } => runs.iter().any(|run| run.style.bold),
            _ => false,
        });
        assert!(!bold, "a settled response must not invent a closer");
        assert_eq!(view.blocks().count(), 2);
    }

    /// Manual streaming-frame measurement, mirroring the transcript's worst
    /// case: one long visible body redrawn at the dissolve cadence. Run with
    /// `cargo test --locked -p waku --lib bench_markdown_frame -- --ignored --nocapture`.
    #[gpui::test]
    #[ignore]
    fn bench_markdown_frame(cx: &mut TestAppContext) {
        struct BenchData {
            view: MarkdownView,
            palette: Palette,
            /// `Some((visible_top, visible_height))` renders windowed.
            window: Option<(f32, f32)>,
        }

        struct BenchWindow {
            data: Rc<RefCell<BenchData>>,
        }

        impl gpui::Render for BenchWindow {
            fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
                let data = self.data.borrow();
                let ctx = Ctx::new(
                    "bench",
                    &data.palette,
                    Metrics::BODY,
                    TranscriptSelection::default(),
                );
                let body = match data.window {
                    Some((top, height)) => markdown_windowed(
                        &data.view,
                        &ctx,
                        MessageBodyWindow {
                            visible_top: top,
                            visible_height: height,
                            width: 700.0,
                        },
                    ),
                    None => markdown(&data.view, &ctx),
                };
                div()
                    .w(px(700.0))
                    .child(body.unwrap_or_else(|| div().into_any_element()))
            }
        }

        let data = Rc::new(RefCell::new(BenchData {
            view: MarkdownView::new(),
            palette: Palette::from_theme(&Theme::dark()),
            window: None,
        }));
        let (entity, visual) = cx.add_window_view(|_, _| BenchWindow {
            data: Rc::clone(&data),
        });
        let draw = |visual: &mut gpui::VisualTestContext, entity: &gpui::Entity<BenchWindow>| {
            visual.update(|window, cx| {
                entity.update(cx, |_, cx| cx.notify());
                window.draw(cx).clear(cx);
            });
        };
        let paragraphs = |count: usize| {
            let mut source = String::new();
            for i in 0..count {
                source.push_str(&format!(
                    "Paragraph {i} with several words that need shaping and wrapping across the transcript column.\n\n"
                ));
            }
            source
        };
        let list_items = |count: usize| {
            let mut source = String::new();
            for i in 0..count {
                source.push_str(&format!(
                    "- Item {i} with several words that need shaping and wrapping across the transcript column.\n"
                ));
            }
            source
        };
        // A single top-level block is one window group, so the window cannot
        // bound its frame however tall it grows. Both shapes are timed.
        for (label, source) in [
            ("50 paragraph blocks", paragraphs(50)),
            ("400 paragraph blocks", paragraphs(400)),
            ("one list of 400 items", list_items(400)),
        ] {
            {
                let mut data = data.borrow_mut();
                data.view.set_text(&source, true);
                data.window = None;
            }
            // Two frames so the ledger is filled before it is timed.
            draw(visual, &entity);
            draw(visual, &entity);

            const FRAMES: u32 = 10;
            let start = std::time::Instant::now();
            for _ in 0..FRAMES {
                draw(visual, &entity);
            }
            println!("{label} full per-frame={:?}", start.elapsed() / FRAMES);

            let (total, gap) = {
                let data = data.borrow();
                let heights = data.view.heights.borrow();
                let gap = px(Metrics::BODY.block_gap);
                let total = heights
                    .iter()
                    .filter_map(|(_, height)| *height)
                    .sum::<Pixels>()
                    + gap * (heights.len().saturating_sub(1)) as f32;
                (f32::from(total), f32::from(gap))
            };
            data.borrow_mut().window = Some(((total - 900.0).max(0.0), 800.0));
            draw(visual, &entity);
            let start = std::time::Instant::now();
            for _ in 0..FRAMES {
                draw(visual, &entity);
            }
            println!(
                "{label} windowed per-frame={:?} (gap={gap})",
                start.elapsed() / FRAMES
            );
            data.borrow_mut().window = None;
        }
    }

    fn plan_height(plan: &WindowPlan, heights: &[Pixels], gap: Pixels) -> Pixels {
        let mut height = Pixels::ZERO;
        let mut children = 0usize;
        for (index, group) in plan.groups.iter().enumerate() {
            if let Some(spacer) = plan.spacers[index] {
                height += spacer;
                children += 1;
            }
            for block in group.clone() {
                height += heights[block];
                children += 1;
            }
        }
        if children > 1 {
            height += gap * (children - 1) as f32;
        }
        height
    }

    /// The planner reads nothing but each block's source range, so a test
    /// hands it ranges dressed as blocks.
    fn ranged_blocks(ranges: &[Range<usize>]) -> Vec<TopBlock> {
        ranges
            .iter()
            .map(|range| TopBlock {
                range: range.clone(),
                block: Block::Rule,
            })
            .collect()
    }

    /// The spacers stand in for hidden blocks, so a windowed body has to
    /// measure exactly what the full body would — and it must build every
    /// block the viewport can reach.
    #[test]
    fn window_plan_spacers_reconstruct_the_body_height() {
        let gap = px(10.0);
        let heights = (0..60)
            .map(|index| {
                (
                    index..index + 1,
                    Some(px(((index % 7) as f32 + 1.0) * 24.0)),
                )
            })
            .collect::<Vec<(Range<usize>, Option<Pixels>)>>();
        let measured = heights
            .iter()
            .map(|(_, height)| height.unwrap())
            .collect::<Vec<_>>();
        let ranges = heights
            .iter()
            .map(|(range, _)| range.clone())
            .collect::<Vec<_>>();
        let body = ranged_blocks(&ranges);
        let blocks = body.iter().collect::<Vec<_>>();
        let full = measured.iter().copied().sum::<Pixels>() + gap * (measured.len() - 1) as f32;
        let heights = Rc::new(RefCell::new(heights));

        for (top, height) in [
            (0.0_f32, 200.0_f32),
            (400.0, 200.0),
            (2600.0, 200.0),
            (1_000_000.0, 200.0),
        ] {
            // The volatile tail is the last two blocks; it is always built.
            let plan = window_plan(&heights, &blocks, gap, top, height, measured.len() - 2);
            assert!(
                f32::from(plan_height(&plan, &measured, gap) - full).abs() < 0.01,
                "top={top} groups={:?}",
                plan.groups
            );
            assert_eq!(
                plan.groups.last().map(|group| group.end),
                Some(measured.len()),
                "the volatile tail always closes the window"
            );

            let mut cursor = Pixels::ZERO;
            for (index, block) in measured.iter().enumerate() {
                let block_top = cursor;
                let block_bottom = cursor + *block;
                if f32::from(block_bottom) > top && f32::from(block_top) < top + height {
                    assert!(
                        plan.groups.iter().any(|group| group.contains(&index)),
                        "top={top} hid visible block {index}: {:?}",
                        plan.groups
                    );
                }
                cursor = block_bottom + gap;
            }
        }

        // An unknown height makes the spacer arithmetic a guess, so the plan
        // renders the whole body and lets that pass measure it.
        heights.borrow_mut()[7].1 = None;
        let plan = window_plan(&heights, &blocks, gap, 400.0, 200.0, measured.len() - 2);
        assert_eq!(plan.groups, vec![0..measured.len()]);
        assert_eq!(plan.spacers, vec![None]);
    }

    /// A hidden span of exactly one gap owes a zero-height spacer, not none:
    /// the spacer is what carries the flex column's two surrounding gaps, so
    /// dropping it would shorten the body by a whole gap. Block 0 measures
    /// zero here, which makes the span above block 1 exactly that.
    #[test]
    fn a_zero_height_hidden_span_still_reconstructs_its_gap() {
        let gap = px(10.0);
        let heights = Rc::new(RefCell::new(vec![
            (0..1, Some(px(0.0))),
            (1..2, Some(px(1_000.0))),
            (2..3, Some(px(1_000.0))),
        ]));
        let body = ranged_blocks(&[0..1, 1..2, 2..3]);
        let blocks = body.iter().collect::<Vec<_>>();
        let measured = [px(0.0), px(1_000.0), px(1_000.0)];
        let full = px(2_000.0) + gap * 2.0;

        // A viewport on the second half leaves block 0 outside the window.
        let plan = window_plan(&heights, &blocks, gap, 1_500.0, 100.0, 3);
        assert_eq!(plan.groups, vec![1..3], "{:?}", plan.groups);
        assert_eq!(
            plan.spacers,
            vec![Some(Pixels::ZERO)],
            "the boundary above block 1 dropped a zero-height block"
        );
        assert_eq!(plan_height(&plan, &measured, gap), full);
    }

    /// The height ledger is what makes the spacers exact: appends keep it,
    /// and anything that re-wraps or rewrites the body drops it.
    #[test]
    fn the_height_ledger_survives_appends_and_clears_on_reflow() {
        let mut view = MarkdownView::new();
        view.set_render_width(600.0);
        view.set_text("First.\n\nSecond", true);
        view.heights
            .borrow_mut()
            .extend([(0..1, Some(px(20.0))), (1..2, Some(px(30.0)))]);
        // Block 0 is settled, block 1 is the mended tail.
        view.volatile_from.set(1 << 16);

        view.set_text("First.\n\nSecond and more", true);
        assert_eq!(
            view.heights.borrow().len(),
            2,
            "an append keeps the recorded heights"
        );
        assert!(
            view.heights.borrow()[0].1.is_some(),
            "a settled block's height survives an append"
        );
        assert!(
            view.heights.borrow()[1].1.is_none(),
            "the volatile block must be measured again"
        );

        view.set_render_width(500.0);
        assert!(
            view.heights.borrow().is_empty(),
            "a reflow drops the ledger"
        );

        view.heights.borrow_mut().push((0..1, Some(px(20.0))));
        view.set_text("Rewritten completely", true);
        assert!(
            view.heights.borrow().is_empty(),
            "a rewrite drops the ledger"
        );
    }

    /// An append that starts a new top-level block is the one append that
    /// costs a full pass. Its range match still holds — the block keeps the
    /// source range it had in the mended tail — but its height is gone, so the
    /// planner has nothing to size a spacer with and builds the whole body for
    /// that frame. The ledger keeps every other entry, so the next frame
    /// windows again.
    #[test]
    fn a_structural_append_costs_one_full_measuring_pass() {
        let gap = px(10.0);
        let mut view = MarkdownView::new();
        view.set_render_width(600.0);
        view.set_text("A\n\nB\n\nC\n\nD\n\nE\n\nF", true);
        let recorded = view
            .top_blocks()
            .map(|top| (top.range.clone(), Some(px(1_000.0))))
            .collect::<Vec<_>>();
        assert_eq!(recorded.len(), 6, "fixture must parse to six blocks");
        let volatile = view.parser.display_tail_start();
        assert_eq!(volatile, 5, "only the last block is the mended tail");
        view.volatile_from.set(block_ordinal_base(volatile));
        *view.heights.borrow_mut() = recorded;

        // A viewport on the last block builds it and the volatile block only;
        // everything above them is a spacer.
        let blocks = view.top_blocks().collect::<Vec<_>>();
        let plan = window_plan(&view.heights, &blocks, gap, 5_000.0, 100.0, volatile);
        assert_eq!(plan.groups, vec![4..6], "{:?}", plan.groups);

        // Appending a seventh block promotes block 5 out of the tail: it keeps
        // its range and loses its height, so this frame builds all seven.
        view.set_text("A\n\nB\n\nC\n\nD\n\nE\n\nF\n\nG", true);
        let blocks = view.top_blocks().collect::<Vec<_>>();
        assert_eq!(blocks.len(), 7, "the append started a new top-level block");
        let plan = window_plan(&view.heights, &blocks, gap, 5_000.0, 100.0, 6);
        assert_eq!(plan.groups, vec![0..7], "{:?}", plan.groups);
        assert_eq!(plan.spacers, vec![None]);
    }

    /// Images and formulas size themselves after their first frame, so a body
    /// holding one cannot stand in for a hidden block with a remembered
    /// height: it must render fully every pass.
    #[test]
    fn async_content_disables_windowing() {
        let mut view = MarkdownView::new();
        view.set_text("plain prose", true);
        assert!(!view.has_async_blocks());

        view.set_text("plain prose\n\n$$x^2$$", true);
        assert!(view.has_async_blocks(), "display math");

        let mut image = MarkdownView::new();
        image.set_text("![alt](pic.png)", true);
        assert!(image.has_async_blocks(), "image");

        let mut math = MarkdownView::new();
        math.set_text("inline $x^2$ here", true);
        assert!(math.has_async_blocks(), "inline math");

        let mut code = MarkdownView::new();
        code.set_text("```txt\n$a^2$\n```", true);
        assert!(!code.has_async_blocks(), "math in code stays literal");
    }

    /// Inline math can arrive inside an existing block without moving a block
    /// boundary, so the async scan cannot be skipped when the block count
    /// holds: `see $x` is one literal block and `see $x$` is one math block,
    /// and only the scan sees the difference.
    #[test]
    fn async_content_is_rescanned_on_a_pure_append() {
        let mut view = MarkdownView::new();
        view.set_text("see $x", true);
        assert!(!view.has_async_blocks(), "an unclosed $ stays literal");
        view.set_text("see $x$", true);
        assert!(
            view.has_async_blocks(),
            "inline math arrived on a pure append"
        );
    }

    /// A live selection parks spans and a drag anchor in the frame registry,
    /// which is the same geometry a search reveal reads back: a windowed body
    /// must keep the full walk while one exists.
    #[test]
    fn a_live_selection_disables_windowing() {
        let selection = TranscriptSelection::default();
        let palette = Palette::from_theme(&Theme::dark());
        let ctx = Ctx::new("row", &palette, Metrics::BODY, selection.clone());
        assert!(!ctx.has_selection(), "nothing is selected yet");

        selection
            .selection
            .borrow_mut()
            .begin(TextKey::new("row", 0), 3);
        assert!(ctx.has_selection(), "a drag anchor");

        selection.selection.borrow_mut().clear();
        assert!(!ctx.has_selection(), "a cleared selection");
    }

    /// The container height leads the streaming body: it grows at the body's
    /// smoothed rate so the row never reports a line step, and it never dips
    /// below the body. Without the dissolve there is no veil holding appended
    /// text invisible, so the same call releases the clip instead of pinning
    /// the reader's view at the previous frame's height.
    #[test]
    fn the_container_height_leads_the_body() {
        let mut view = MarkdownView::new();
        view.set_text("hello", true);
        view.body_height.set(Some(px(100.0)));
        let start = Instant::now();
        view.advance_clip(true, start);
        assert!(view.clip_height().unwrap() >= px(100.0));

        view.body_height.set(Some(px(200.0)));
        view.advance_clip(true, start + Duration::from_millis(10));
        let lead = view.clip_height().expect("a container height");
        assert!(lead > px(100.0) && lead <= px(244.0), "{lead:?}");

        view.advance_clip(false, start + Duration::from_millis(20));
        assert_eq!(
            view.clip_height(),
            None,
            "without the veil the body reports its real height"
        );
    }

    /// The clip is a height at one wrapping, and a retained one cuts a body
    /// that has since grown past it. Settling and a reflow both drop it.
    #[test]
    fn settling_and_reflowing_release_the_container_height() {
        let mut view = MarkdownView::new();
        view.set_render_width(600.0);
        view.set_text("hello", true);
        view.body_height.set(Some(px(100.0)));
        view.advance_clip(true, Instant::now());
        assert_eq!(view.clip_height(), Some(px(100.0)), "streaming clips");

        // Settling drops the veil, so the row reports the body's real height
        // instead of a clip the tail may have grown past.
        view.set_text("hello", false);
        assert_eq!(view.clip_height(), None, "settling releases the clip");

        view.set_text("hello", true);
        view.body_height.set(Some(px(100.0)));
        view.advance_clip(true, Instant::now());
        assert_eq!(view.clip_height(), Some(px(100.0)), "streaming clips");
        view.set_render_width(500.0);
        assert_eq!(view.clip_height(), None, "a reflow releases the clip");
    }

    /// Mending re-partitions the tail as it settles. A height measured for
    /// the block that used to own an index must never size the block that
    /// owns it now, or the spacer arithmetic drifts by whole blocks.
    #[test]
    fn a_repartitioned_block_never_borrows_another_blocks_height() {
        let gap = px(10.0);
        let heights = Rc::new(RefCell::new(vec![
            (0..10, Some(px(100.0))),
            (10..20, Some(px(50.0))),
        ]));
        // The same indices now describe different source ranges.
        let body = ranged_blocks(&[0..5, 5..20]);
        let blocks = body.iter().collect::<Vec<_>>();
        let plan = window_plan(&heights, &blocks, gap, 0.0, 100.0, 1);
        assert_eq!(
            plan.groups,
            vec![0..2],
            "a misaligned settled block forces a measuring pass"
        );

        // The volatile region's range changes every frame; that alone must
        // not drop the window, or a stream would re-measure everything.
        let volatile = ranged_blocks(&[0..10, 10..99]);
        let volatile_blocks = volatile.iter().collect::<Vec<_>>();
        let plan = window_plan(&heights, &volatile_blocks, gap, 0.0, 100.0, 1);
        assert_eq!(plan.groups, vec![0..2], "still one range to build");
    }

    /// A windowed pass must lay out at exactly the height the full pass did,
    /// and must not re-lay-out the blocks its window hid.
    #[gpui::test]
    fn windowed_body_measures_the_same_as_the_full_body(cx: &mut TestAppContext) {
        struct WindowedBody {
            view: Rc<MarkdownView>,
            palette: Palette,
            window: Rc<Cell<Option<(f32, f32)>>>,
            height: Rc<RefCell<Vec<(Range<usize>, Option<Pixels>)>>>,
        }

        impl gpui::Render for WindowedBody {
            fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
                let ctx = Ctx::new(
                    "window",
                    &self.palette,
                    Metrics::BODY,
                    TranscriptSelection::default(),
                );
                let body = match self.window.get() {
                    Some((top, height)) => markdown_windowed(
                        &self.view,
                        &ctx,
                        MessageBodyWindow {
                            visible_top: top,
                            visible_height: height,
                            width: 700.0,
                        },
                    ),
                    None => markdown(&self.view, &ctx),
                };
                // The recorder reads the body column's own bounds: the window
                // root otherwise stretches to the whole viewport.
                let measured = MeasuredBlock {
                    inner: body.unwrap_or_else(|| div().into_any_element()),
                    heights: self.height.clone(),
                    index: 0,
                    range: 0..1,
                };
                div().w(px(700.0)).flex().items_start().child(measured)
            }
        }

        let mut source = String::new();
        for i in 0..60 {
            source.push_str(&format!(
                "Paragraph {i} with enough words to wrap across the transcript column at least once and give the block a real height.\n\n"
            ));
        }
        let mut view = MarkdownView::new();
        view.set_text(&source, true);
        let view = Rc::new(view);
        // The row records the wrap width before every body render; this stands
        // in for that, so the first windowed pass does not read as a reflow.
        view.set_render_width(700.0);
        let window = Rc::new(Cell::new(None));
        let height = Rc::new(RefCell::new(vec![(0..0, None)]));
        let (entity, visual) = cx.add_window_view(|_, _| WindowedBody {
            view: view.clone(),
            palette: Palette::from_theme(&Theme::dark()),
            window: window.clone(),
            height: height.clone(),
        });
        let draw = |visual: &mut gpui::VisualTestContext, entity: &gpui::Entity<WindowedBody>| {
            visual.update(|cxt, cx| {
                entity.update(cx, |_, cx| cx.notify());
                cxt.draw(cx).clear(cx);
            });
        };
        let body_height = |height: &Rc<RefCell<Vec<(Range<usize>, Option<Pixels>)>>>| {
            height.borrow()[0].1.expect("the body was laid out")
        };

        draw(visual, &entity);
        let full = body_height(&height);
        let gap = px(Metrics::BODY.block_gap);
        let measured = view
            .heights
            .borrow()
            .iter()
            .map(|(_, height)| height.expect("a full pass measures every block"))
            .collect::<Vec<_>>();
        assert_eq!(measured.len(), 60);
        let ledger = measured.iter().copied().sum::<Pixels>() + gap * 59.0;
        assert!(
            f32::from(full - ledger).abs() < 0.5,
            "full render {full:?} disagrees with the ledger {ledger:?}"
        );

        let middle = measured[..30].iter().copied().sum::<Pixels>() + gap * 30.0;
        window.set(Some((f32::from(middle), 200.0)));
        draw(visual, &entity);
        let windowed = body_height(&height);
        assert!(
            f32::from(windowed - full).abs() < 0.5,
            "windowed {windowed:?} does not fill the full height {full:?}"
        );

        // Stamp a height no layout would produce (smaller, so the window
        // does not slide up to include the block). If the hidden block were
        // rebuilt, the stamp would be overwritten.
        view.heights.borrow_mut()[0].1 = Some(px(1.0));
        draw(visual, &entity);
        let expected = view
            .heights
            .borrow()
            .iter()
            .map(|(_, height)| height.unwrap())
            .sum::<Pixels>()
            + gap * 59.0;
        let stamped = body_height(&height);
        assert!(
            f32::from(stamped - expected).abs() < 0.5,
            "the hidden block was rebuilt: {stamped:?} instead of {expected:?}"
        );
        assert_eq!(
            view.heights.borrow()[0].1,
            Some(px(1.0)),
            "the hidden block's remembered height survived"
        );
    }

    #[test]
    fn column_widths_are_content_proportional_and_floored() {
        let header = vec![runs_of("id"), runs_of("a much longer description column")];
        let widths = column_widths(&header, &[], 2);
        assert!(widths[1] > widths[0], "wider content gets a wider column");
        // The floor survives the fill, and the fractions still sum to one.
        let floor = 0.55 / 2.0;
        assert!(
            widths.iter().all(|width| *width >= floor - 1e-6),
            "every column keeps its floor: {widths:?}"
        );
        assert!((widths.iter().sum::<f32>() - 1.0).abs() < 1e-4);

        // A column that is merely narrow, not starved, stays proportional.
        let balanced = column_widths(&[runs_of("aaaa"), runs_of("bbbbbb")], &[], 2);
        assert!((balanced[0] - 0.4).abs() < 1e-3, "{balanced:?}");

        // An empty table falls back to even columns.
        let even = column_widths(&[], &[], 3);
        assert!(even.iter().all(|width| (width - 1.0 / 3.0).abs() < 1e-6));
    }
}
