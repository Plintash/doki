//! Paint-only paced dissolve for newly appended streaming Markdown text.
//!
//! The complete text enters layout immediately. Only the colors of newly
//! appended byte ranges animate, so the fade cannot change shaping, wrapping,
//! selection offsets, or row height.
//!
//! The reveal is paced like lobehub/streamdown's: each appended grapheme is
//! born one pace after the grapheme before it, and every birth starts its own
//! fade. A stream commit therefore reads as a wave travelling through the new
//! text instead of one uniformly dimmed block. Births chain across commits, so
//! the wave stays continuous however the provider chops its deltas, and the
//! chain is capped one commit gap plus one fade ahead of the frame, so a burst
//! can never park more than one dissolve of text in the invisible queue.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::time::{Duration, Instant};

use gpui::TextRun;
use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};

/// How long one grapheme takes to dissolve in.
pub const VEIL_FADE_MS: f32 = 180.0;
/// Fastest and slowest per-grapheme cadence. The fast end is the rate a
/// sustained burst drains at; the slow end keeps a sparse drip from landing as
/// one step.
const VEIL_MIN_PACE_MS: f32 = 2.0;
const VEIL_MAX_PACE_MS: f32 = 18.0;
/// Inter-commit gap fed into the pace, clamped so provider jitter cannot
/// stretch or collapse the wave.
const VEIL_GAP_MIN_MS: f32 = 16.0;
const VEIL_GAP_CLAMP_MS: f32 = 160.0;
/// Opacity bands that adjacent graphemes are merged into before painting.
/// Without them a second of backlog would split the runs at every grapheme;
/// sixteen bands are finer than the eye reads on a 180 ms fade.
const VEIL_OPACITY_LEVELS: f32 = 16.0;

pub type VeilSpan = (Range<usize>, f32);

/// Streamdown's dissolve curve, `cubic-bezier(0.33, 0, 0.67, 1)`: a slow
/// start and a soft landing, so text never pops to half opacity on the frame
/// it lands.
pub fn veil_opacity(progress: f32) -> f32 {
    let progress = progress.clamp(0.0, 1.0);
    if progress <= 0.0 {
        return 0.0;
    }
    if progress >= 1.0 {
        return 1.0;
    }
    // `x(t)` is monotone for these control points, so a bisection inverts it
    // exactly enough at this scale; `y(t)` then evaluates the curve.
    let (mut low, mut high) = (0.0_f32, 1.0_f32);
    for _ in 0..24 {
        let mid = 0.5 * (low + high);
        if bezier_x(mid) < progress {
            low = mid;
        } else {
            high = mid;
        }
    }
    bezier_y(0.5 * (low + high))
}

fn bezier_x(t: f32) -> f32 {
    let inverse = 1.0 - t;
    3.0 * inverse * inverse * t * 0.33 + 3.0 * inverse * t * t * 0.67 + t * t * t
}

fn bezier_y(t: f32) -> f32 {
    let inverse = 1.0 - t;
    3.0 * inverse * t * t + t * t * t
}

fn millis_between(earlier: Instant, later: Instant) -> f32 {
    later.saturating_duration_since(earlier).as_secs_f32() * 1_000.0
}

fn millis(ms: f32) -> Duration {
    Duration::from_secs_f32(ms / 1_000.0)
}

/// One queued grapheme: the bytes it covers and the instant its fade starts.
#[derive(Clone, Debug)]
struct Unit {
    range: Range<usize>,
    birth: Instant,
}

#[derive(Debug, Default)]
struct ElementVeil {
    previous: String,
    units: Vec<Unit>,
    /// Birth the next appended grapheme would take if the wave is running.
    next_birth: Option<Instant>,
    last_append: Option<Instant>,
}

fn common_prefix(a: &str, b: &str) -> usize {
    let mut prefix = a
        .as_bytes()
        .iter()
        .zip(b.as_bytes())
        .take_while(|(left, right)| left == right)
        .count();
    while prefix > 0 && !b.is_char_boundary(prefix) {
        prefix -= 1;
    }
    prefix
}

fn is_grapheme_boundary(text: &str, index: usize) -> bool {
    // An index inside a character is trivially not a cluster boundary, and the
    // cursor panics on one rather than answering.
    if !text.is_char_boundary(index) {
        return false;
    }
    let mut cursor = GraphemeCursor::new(index, text.len(), true);
    matches!(cursor.is_boundary(text, 0), Ok(true))
}

/// Longest shared prefix that is also a grapheme boundary in the new text.
/// The common prefix of a Markdown rewrite can otherwise land between a base
/// character and its combining mark, and the retained span would split a
/// cluster the shaper wants whole.
fn grapheme_prefix(previous: &str, text: &str) -> usize {
    let mut prefix = common_prefix(previous, text);
    // Stepping back one byte at a time can land inside a multi-byte character,
    // which `is_grapheme_boundary` rejects, so the loop never asks the cursor
    // about a mid-character index.
    while prefix > 0 && !is_grapheme_boundary(text, prefix) {
        prefix -= 1;
    }
    prefix
}

impl ElementVeil {
    fn seed(&mut self, text: &str) {
        self.previous.clear();
        self.previous.push_str(text);
        self.units.clear();
        self.next_birth = None;
        self.last_append = None;
    }

    /// Queue the appended tail as one grapheme per pace, chained after
    /// whatever is still fading in. The cap is what keeps a burst from
    /// spending seconds invisible: once the queue would run more than one
    /// commit gap plus one fade ahead, the rest is born at the cap and lands
    /// together.
    fn append(&mut self, text: &str, prefix: usize, now: Instant) {
        let gap_ms = self
            .last_append
            .map_or(VEIL_GAP_CLAMP_MS, |last| millis_between(last, now))
            .clamp(VEIL_GAP_MIN_MS, VEIL_GAP_CLAMP_MS);
        let count = text[prefix..].graphemes(true).count().max(1) as f32;
        let pace = millis((gap_ms / count).clamp(VEIL_MIN_PACE_MS, VEIL_MAX_PACE_MS));
        let cap = now + millis(gap_ms + VEIL_FADE_MS);

        let mut chain = self.next_birth.unwrap_or(now);
        let mut offset = prefix;
        for grapheme in text[prefix..].graphemes(true) {
            let birth = chain.max(now).min(cap);
            let end = offset + grapheme.len();
            self.units.push(Unit {
                range: offset..end,
                birth,
            });
            offset = end;
            chain = birth + pace;
        }
        self.next_birth = Some(chain);
        self.last_append = Some(now);
    }

    fn advance(&mut self, text: &str, now: Instant) -> Vec<VeilSpan> {
        if text != self.previous {
            // A streaming Markdown reparse can replace delimiter characters
            // with styled text. Preserve the common prefix and re-veil only
            // the changed tail instead of flashing the whole block.
            let prefix = grapheme_prefix(&self.previous, text);
            self.units.retain_mut(|unit| {
                unit.range.end = unit.range.end.min(prefix);
                unit.range.start < unit.range.end
            });
            if prefix < text.len() {
                self.append(text, prefix, now);
            }
            self.previous.clear();
            self.previous.push_str(text);
        }

        // Units settle in birth order; drop the ones that already landed.
        let settled = self
            .units
            .iter()
            .position(|unit| millis_between(unit.birth, now) < VEIL_FADE_MS)
            .unwrap_or(self.units.len());
        self.units.drain(..settled);

        let mut spans: Vec<(Range<usize>, u8)> = Vec::new();
        for unit in &self.units {
            let opacity = if unit.birth > now {
                0.0
            } else {
                veil_opacity(millis_between(unit.birth, now) / VEIL_FADE_MS)
            };
            let level = (opacity * VEIL_OPACITY_LEVELS).round() as u8;
            match spans.last_mut() {
                Some((range, last)) if *last == level && range.end == unit.range.start => {
                    range.end = unit.range.end;
                }
                _ if level as f32 >= VEIL_OPACITY_LEVELS => {}
                _ => spans.push((unit.range.clone(), level)),
            }
        }
        spans
            .into_iter()
            .map(|(range, level)| (range, level as f32 / VEIL_OPACITY_LEVELS))
            .collect()
    }

    fn is_fading(&self) -> bool {
        !self.units.is_empty()
    }
}

/// Fade state for one Markdown body, keyed by the renderer's stable text
/// element ordinal.
#[derive(Debug, Default)]
pub struct RowVeil {
    elements: HashMap<usize, ElementVeil>,
    seen_this_frame: HashSet<usize>,
    seeding: bool,
}

impl RowVeil {
    /// Existing content is adopted at full opacity on the first render. This
    /// is used when attaching to a response that was already streaming.
    pub fn seeded() -> Self {
        Self {
            elements: HashMap::new(),
            seen_this_frame: HashSet::new(),
            seeding: true,
        }
    }

    pub fn begin_frame(&mut self) {
        self.seen_this_frame.clear();
    }

    pub fn finish_seeding(&mut self) {
        self.seeding = false;
    }

    pub fn finish_frame(&mut self) {
        // A windowed body may not build every block a frame: a dissolve still
        // in flight in an off-screen tail block must keep its units, or the
        // fade would restart from the block's full text when the window
        // returns to it. Settled unseen elements are droppable.
        self.elements
            .retain(|element, veil| self.seen_this_frame.contains(element) || veil.is_fading());
        self.finish_seeding();
    }

    pub fn advance(&mut self, element: usize, text: &str, now: Instant) -> Vec<VeilSpan> {
        self.seen_this_frame.insert(element);
        if self.seeding && !self.elements.contains_key(&element) {
            let mut veil = ElementVeil::default();
            veil.seed(text);
            self.elements.insert(element, veil);
            return Vec::new();
        }
        self.elements.entry(element).or_default().advance(text, now)
    }

    pub fn is_fading(&self) -> bool {
        self.elements.values().any(ElementVeil::is_fading)
    }
}

/// Split runs at veil boundaries and multiply only paint colors by the
/// current opacity. The text and total run lengths remain byte-identical.
pub fn apply_veil(runs: Vec<TextRun>, spans: &[VeilSpan]) -> Vec<TextRun> {
    if spans.is_empty() || spans.iter().all(|(_, opacity)| *opacity >= 1.0) {
        return runs;
    }

    let mut output = Vec::with_capacity(runs.len() + spans.len() * 2);
    let mut position = 0;
    for run in runs {
        let start = position;
        let end = start + run.len;
        position = end;
        let mut cuts = vec![start, end];
        for (range, _) in spans {
            if range.start > start && range.start < end {
                cuts.push(range.start);
            }
            if range.end > start && range.end < end {
                cuts.push(range.end);
            }
        }
        cuts.sort_unstable();
        cuts.dedup();

        for interval in cuts.windows(2) {
            let (piece_start, piece_end) = (interval[0], interval[1]);
            let mut piece = run.clone();
            piece.len = piece_end - piece_start;
            if let Some(opacity) = spans
                .iter()
                .find(|(range, _)| range.start <= piece_start && piece_end <= range.end)
                .map(|(_, opacity)| *opacity)
                .filter(|opacity| *opacity < 1.0)
            {
                piece.color = piece.color.opacity(opacity);
                piece.background_color = piece.background_color.map(|color| color.opacity(opacity));
                if let Some(underline) = &mut piece.underline {
                    underline.color = underline.color.map(|color| color.opacity(opacity));
                }
                if let Some(strikethrough) = &mut piece.strikethrough {
                    strikethrough.color = strikethrough.color.map(|color| color.opacity(opacity));
                }
            }
            output.push(piece);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TextRun, font};

    fn at(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    fn run(len: usize) -> TextRun {
        TextRun {
            len,
            font: font("Test"),
            color: gpui::white(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }
    }

    fn advance(veil: &mut ElementVeil, text: &str, now: Instant) -> Vec<VeilSpan> {
        veil.advance(text, now)
    }

    #[test]
    fn a_commit_lands_as_a_wave_not_a_block() {
        let start = Instant::now();
        let mut veil = ElementVeil::default();
        // Four graphemes with no prior commit: the 160 ms default gap gives
        // the 18 ms pace, so on the commit frame everything is still dark.
        assert_eq!(advance(&mut veil, "one ", start), vec![(0..4, 0.0)]);

        // Halfway through the wave the oldest grapheme leads and the newest
        // trails: one gradient rather than two uniform blocks.
        let spans = advance(&mut veil, "one ", at(start, 100));
        assert!(spans.len() > 1, "expected a gradient, got {spans:?}");
        assert_eq!(spans.first().unwrap().0.start, 0);
        assert_eq!(spans.last().unwrap().0.end, 4);
        assert!(
            spans.windows(2).all(|pair| pair[0].1 > pair[1].1),
            "expected a descending gradient, got {spans:?}"
        );

        assert!(advance(&mut veil, "one ", at(start, 400)).is_empty());
        assert!(!veil.is_fading());
    }

    #[test]
    fn the_wave_keeps_its_cadence_across_commits() {
        let start = Instant::now();
        let mut veil = ElementVeil::default();
        advance(&mut veil, "one ", start);
        // The next commit arrives 100 ms later. Its graphemes chain after the
        // first commit's queue instead of restarting the wave at full
        // opacity, so the appended tail is still dark on arrival.
        let spans = advance(&mut veil, "one two", at(start, 100));
        assert_eq!(spans.last().unwrap().0.end, 7);
        assert_eq!(*spans.last().unwrap(), (4..7, 0.0));
    }

    #[test]
    fn a_burst_is_born_within_one_gap_and_fade() {
        let start = Instant::now();
        let mut veil = ElementVeil::default();
        let burst = "a".repeat(1_000);
        advance(&mut veil, &burst, start);

        // Nothing is scheduled later than one commit gap plus one fade: a
        // burst lands as fast-revealed text, never as seconds of invisible
        // backlog.
        let cap = start + millis(VEIL_GAP_CLAMP_MS + VEIL_FADE_MS);
        assert!(veil.units.iter().all(|unit| unit.birth <= cap));
        // The queue is still spread into a wave rather than landing as one
        // step, so the capped tail rides a continuous dissolve.
        let distinct = veil
            .units
            .iter()
            .map(|unit| unit.birth)
            .collect::<HashSet<_>>()
            .len();
        assert!(
            distinct > 100,
            "expected a spread wave, got {distinct} births"
        );

        // And the whole burst is fully painted one fade after the cap.
        assert!(
            advance(
                &mut veil,
                &burst,
                cap + millis(VEIL_FADE_MS) + Duration::from_millis(1)
            )
            .is_empty()
        );
        assert!(!veil.is_fading());
    }

    #[test]
    fn grapheme_clusters_are_never_split() {
        let start = Instant::now();
        let mut veil = ElementVeil::default();
        // A combining mark belongs to the grapheme before it; no span may
        // start or end between the two bytes.
        let spans = advance(&mut veil, "e\u{301}x", start);
        assert!(
            spans
                .iter()
                .all(|(range, _)| range.start != 1 && range.end != 1)
        );

        // A rewrite whose shared prefix ends inside the cluster re-veils the
        // whole cluster instead of splitting it.
        let mut rewrite = ElementVeil::default();
        advance(&mut rewrite, "e", start);
        let spans = advance(&mut rewrite, "e\u{301}x", at(start, 100));
        assert!(
            spans
                .iter()
                .all(|(range, _)| range.start != 1 && range.end != 1)
        );
    }

    #[test]
    fn seeded_rows_do_not_refade_existing_content() {
        let start = Instant::now();
        let mut veil = RowVeil::seeded();
        assert!(veil.advance(0, "already here", start).is_empty());
        veil.finish_seeding();
        assert_eq!(
            veil.advance(0, "already here plus", at(start, 100)),
            vec![(12..17, 0.0)]
        );
    }

    #[test]
    fn markdown_rewrites_keep_the_common_prefix() {
        let start = Instant::now();
        let mut veil = ElementVeil::default();
        advance(&mut veil, "intro **bol", start);
        // The rewrite keeps the shared prefix's history and queues only the
        // changed tail; nothing is faded past the new text's end.
        let spans = advance(&mut veil, "intro bold", at(start, 100));
        assert!(spans.iter().all(|(range, _)| range.end <= 10));
        let tail = spans
            .iter()
            .find(|(range, _)| range.start >= 6)
            .expect("the rewritten tail should be queued");
        assert_eq!(tail.0.start, 6);
        assert_eq!(spans.last().unwrap().0.end, 10);
    }

    #[test]
    fn veil_splits_runs_without_changing_layout_lengths() {
        let runs = vec![run(4), run(6)];
        let faded = apply_veil(runs.clone(), &[(2..8, 0.5)]);
        assert_eq!(
            faded.iter().map(|run| run.len).collect::<Vec<_>>(),
            vec![2, 2, 4, 2]
        );
        assert_eq!(faded.iter().map(|run| run.len).sum::<usize>(), 10);
        assert!(faded.iter().all(|run| run.font == runs[0].font));
        assert_eq!(faded[0].color.a, 1.0);
        assert_eq!(faded[1].color.a, 0.5);
    }

    /// A rewrite's shared prefix can end on a character boundary that is not
    /// a grapheme boundary — `é` followed by a combining mark, say. Walking
    /// back one byte at a time then lands inside a multi-byte character, and
    /// the boundary check would slice-panic instead of answering `false`.
    #[test]
    fn grapheme_prefix_never_splits_a_multibyte_character() {
        let text = "é\u{0301}";
        assert_eq!(grapheme_prefix("é", text), 0);

        // The shared prefix backs off to the cluster's start: the combining
        // mark binds to `b`, so the prefix is 1, not 2.
        assert_eq!(grapheme_prefix("ab", "ab\u{0301}c"), 1);
        assert_eq!(grapheme_prefix("abc", "abc"), 3);
        assert_eq!(grapheme_prefix("界", "界界"), "界".len());
    }

    #[test]
    fn dissolve_curve_eases_in_and_out() {
        assert_eq!(veil_opacity(0.0), 0.0);
        assert_eq!(veil_opacity(1.0), 1.0);
        assert!((veil_opacity(0.5) - 0.5).abs() < 0.02);
        // A slow start is the difference between dissolving and flashing.
        assert!(veil_opacity(0.25) < 0.2, "{}", veil_opacity(0.25));
        assert!(veil_opacity(0.75) > 0.8, "{}", veil_opacity(0.75));
        let mut previous = -1.0;
        for step in 0..=64 {
            let value = veil_opacity(step as f32 / 64.0);
            assert!(value >= previous);
            previous = value;
        }
    }
}
