//! Re-anchoring a message annotation against a fresh rendering.
//!
//! An annotation stores the element ordinal and byte range it was taken from,
//! plus a snapshot of the quoted text. That anchor can move: markdown
//! normalization changes between app versions, a message re-parses into a
//! different block shape, and an element can disappear entirely. Storing
//! `prefix`/`suffix` context around the quote would buy little — the stored
//! offset is already the strongest positional prior — so this module verifies
//! the stored range, falls back to finding the quote nearest that offset, and
//! gives up cleanly when the text is gone. Giving up is a supported outcome: the
//! annotation keeps its quote and simply draws no mark.
//!
//! Pure and gpui-free so it can be unit-tested. The caller supplies the
//! message's elements in document order — the same order the renderer registers
//! them in, and the same ordinals the find bar matches against.

use std::ops::Range;

/// One painted text element's flattened text, addressed by the ordinal the
/// renderer assigns it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ElementText<'a> {
    pub ordinal: usize,
    pub text: &'a str,
}

/// An annotation's span: an element ordinal and a byte range into that element's
/// flattened text. The same shape carries the stored anchor and the anchor
/// resolved against the current rendering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Anchor {
    pub ordinal: usize,
    pub range: Range<usize>,
}

/// Resolve a stored anchor against a message's current elements.
///
/// Verification comes first: when the element still holds the quote at the
/// stored range, that range is the answer. Otherwise the quote is searched for
/// across the message, preferring the occurrence nearest the stored offset —
/// same ordinal first, then nearest start, then earliest. `None` means the quote
/// can no longer be located, so no mark should be drawn.
pub fn resolve(anchor: &Anchor, quote: &str, elements: &[ElementText<'_>]) -> Option<Anchor> {
    // An empty quote matches at every boundary, so it is not a locator.
    if quote.is_empty() {
        return None;
    }

    // Clamp against the element, not the quote: after a normalization change the
    // stored offsets can land mid-character in the new text.
    if let Some(text) = element_text(elements, anchor.ordinal) {
        let range = clamp_range(text, &anchor.range);
        if text.get(range.clone()) == Some(quote) {
            return Some(Anchor {
                ordinal: anchor.ordinal,
                range,
            });
        }
    }

    // A repeated quote stays pinned where it was rather than jumping to the
    // first occurrence. Equal candidates keep the one already found, which
    // walks document order.
    let mut best: Option<((usize, usize, usize), Anchor)> = None;
    for element in elements {
        for start in element.text.match_indices(quote).map(|(start, _)| start) {
            let key = (
                element.ordinal.abs_diff(anchor.ordinal),
                start.abs_diff(anchor.range.start),
                start,
            );
            if best.as_ref().is_none_or(|(best_key, _)| key < *best_key) {
                best = Some((
                    key,
                    Anchor {
                        ordinal: element.ordinal,
                        range: start..start + quote.len(),
                    },
                ));
            }
        }
    }
    best.map(|(_, found)| found)
}

/// What a fresh selection should do against the annotations already staged for
/// the same message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MergeDecision {
    /// The selection overlaps staged annotations in the same element: the
    /// annotation at `index` grows to cover `range`, and the `absorbed` ones are
    /// dropped. The caller re-snapshots the quote from the element text, since
    /// the union covers more than the selection did.
    Extend {
        index: usize,
        absorbed: Vec<usize>,
        parts: Vec<Anchor>,
    },
    /// The selection is exactly a staged annotation: reveal that card instead of
    /// staging a second one.
    Duplicate { index: usize },
    /// Nothing to merge; stage a new annotation.
    New,
}

/// Decide how a new selection joins the annotations already staged for one
/// message. `staged` is in creation order, each entry the full set of element
/// ranges that annotation covers, and the returned indices address it.
///
/// A selection and a staged annotation overlap when any part of one shares an
/// element ordinal with and overlaps a part of the other; the union keeps one
/// range per element.
pub fn merge(selection: &[Anchor], staged: &[Vec<Anchor>]) -> MergeDecision {
    if selection.is_empty() {
        return MergeDecision::New;
    }
    // An exact match wins over widening: re-selecting the same span asks to look
    // at that annotation, not to grow it.
    if let Some(index) = staged
        .iter()
        .position(|staged| same_parts(staged, selection))
    {
        return MergeDecision::Duplicate { index };
    }

    // Widen transitively: a selection can reach across two staged passages that
    // do not touch each other, and merging only the first would leave the staged
    // set overlapping itself.
    let mut index = None;
    let mut absorbed = Vec::new();
    let mut parts = selection.to_vec();
    loop {
        let mut grew = false;
        for (position, candidate) in staged.iter().enumerate() {
            if index == Some(position)
                || absorbed.contains(&position)
                || !overlaps_any(&parts, candidate)
            {
                continue;
            }
            if index.is_none() {
                index = Some(position);
            } else {
                absorbed.push(position);
            }
            parts = union_parts(&parts, candidate);
            grew = true;
        }
        if !grew {
            break;
        }
    }

    match index {
        Some(index) => MergeDecision::Extend {
            index,
            absorbed,
            parts,
        },
        None => MergeDecision::New,
    }
}

/// Whether two part sets address exactly the same ranges.
fn same_parts(a: &[Anchor], b: &[Anchor]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut a = a.to_vec();
    let mut b = b.to_vec();
    let key = |anchor: &Anchor| (anchor.ordinal, anchor.range.start, anchor.range.end);
    a.sort_by_key(key);
    b.sort_by_key(key);
    a.iter().zip(b.iter()).all(|(a, b)| {
        a.ordinal == b.ordinal && a.range.start == b.range.start && a.range.end == b.range.end
    })
}

/// Whether any part of one set overlaps a part of the other in the same element.
fn overlaps_any(a: &[Anchor], b: &[Anchor]) -> bool {
    a.iter().any(|a| {
        b.iter().any(|b| {
            a.ordinal == b.ordinal && overlaps(&a.range, &b.range)
        })
    })
}

/// The union of two part sets: one range per element, overlapping and touching
/// ranges within an element merged, sorted by ordinal then start.
fn union_parts(a: &[Anchor], b: &[Anchor]) -> Vec<Anchor> {
    let mut parts = a.to_vec();
    parts.extend(b.iter().cloned());
    // Merge within each element until stable: unioning two ranges can bridge a
    // third that neither touched before.
    loop {
        let mut merged = Vec::with_capacity(parts.len());
        let mut changed = false;
        for part in parts.drain(..) {
            if let Some(existing) = merged.iter_mut().find(|existing: &&mut Anchor| {
                existing.ordinal == part.ordinal
                    && existing.range.start <= part.range.end
                    && part.range.start <= existing.range.end
            }) {
                existing.range.start = existing.range.start.min(part.range.start);
                existing.range.end = existing.range.end.max(part.range.end);
                changed = true;
            } else {
                merged.push(part);
            }
        }
        parts = merged;
        if !changed {
            break;
        }
    }
    parts.sort_by_key(|part| (part.ordinal, part.range.start));
    parts
}

fn element_text<'a>(elements: &[ElementText<'a>], ordinal: usize) -> Option<&'a str> {
    elements
        .iter()
        .find(|element| element.ordinal == ordinal)
        .map(|element| element.text)
}

fn overlaps(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

/// Clamp a stored range into `text` and snap it down to char boundaries. Mouse
/// hit-testing lands on boundaries already; this guards a range that was taken
/// from an older rendering.
fn clamp_range(text: &str, range: &Range<usize>) -> Range<usize> {
    let mut start = clamp_to_boundary(text, range.start);
    let end = clamp_to_boundary(text, range.end);
    if end < start {
        start = end;
    }
    start..end
}

fn clamp_to_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND_BLOCK: usize = 1 << 16;

    fn anchor(ordinal: usize, range: Range<usize>) -> Anchor {
        Anchor { ordinal, range }
    }

    #[test]
    fn keeps_a_span_that_still_matches() {
        let elements = [ElementText {
            ordinal: 0,
            text: "the caller never learns the write failed",
        }];
        let stored = anchor(0, 11..19);
        assert_eq!(resolve(&stored, "never le", &elements), Some(stored));
    }

    #[test]
    fn finds_a_span_that_shifted_by_one_character() {
        let elements = [ElementText {
            ordinal: 0,
            text: "alpha beta alpha beta",
        }];
        // Offsets moved by one, so the stored range no longer holds the quote.
        let stored = anchor(0, 12..17);
        assert_eq!(
            resolve(&stored, "alpha", &elements),
            Some(anchor(0, 11..16))
        );
    }

    #[test]
    fn a_repeated_quote_resolves_to_the_nearest_occurrence() {
        let elements = [ElementText {
            ordinal: 0,
            text: "alpha beta alpha beta",
        }];
        // Stored in the middle, nearer the first occurrence.
        let stored = anchor(0, 3..8);
        assert_eq!(resolve(&stored, "alpha", &elements), Some(anchor(0, 0..5)));

        // And nearer the second one.
        let stored = anchor(0, 14..19);
        assert_eq!(
            resolve(&stored, "alpha", &elements),
            Some(anchor(0, 11..16))
        );
    }

    #[test]
    fn the_stored_ordinal_wins_over_a_nearer_offset() {
        let elements = [
            ElementText {
                ordinal: 0,
                text: "target right here",
            },
            ElementText {
                ordinal: SECOND_BLOCK,
                text: "far away, then target later",
            },
        ];
        // The quote lives in both elements and the other element holds the
        // closer offset; the stored ordinal still decides.
        let stored = anchor(SECOND_BLOCK, 0..6);
        assert_eq!(
            resolve(&stored, "target", &elements),
            Some(anchor(SECOND_BLOCK, 15..21))
        );
    }

    #[test]
    fn falls_back_to_another_element_when_the_stored_one_is_gone() {
        let elements = [
            ElementText {
                ordinal: 0,
                text: "rewritten in an older block",
            },
            ElementText {
                ordinal: SECOND_BLOCK,
                text: "no trace of the quote",
            },
        ];
        let stored = anchor(SECOND_BLOCK, 0..8);
        assert_eq!(
            resolve(&stored, "rewritten", &elements),
            Some(anchor(0, 0..9))
        );
    }

    #[test]
    fn loses_a_quote_that_is_no_longer_there() {
        let elements = [ElementText {
            ordinal: 4,
            text: "the message was rewritten",
        }];
        assert_eq!(
            resolve(&anchor(4, 4..12), "dropped the error", &elements),
            None
        );
        assert_eq!(
            resolve(&anchor(99, 0..3), "the", &elements).map(|a| a.ordinal),
            Some(4)
        );
    }

    #[test]
    fn an_empty_quote_is_not_a_locator() {
        let elements = [ElementText {
            ordinal: 0,
            text: "anything at all",
        }];
        assert_eq!(resolve(&anchor(0, 3..3), "", &elements), None);
    }

    #[test]
    fn clamps_offsets_that_land_mid_character() {
        // Byte 1 starts 'é', so a stored range at byte 2 snaps down to it.
        let elements = [ElementText {
            ordinal: 0,
            text: "héllo wörld",
        }];
        assert_eq!(
            resolve(&anchor(0, 2..6), "éllo", &elements),
            Some(anchor(0, 1..6))
        );

        // A quote in a multibyte-only element resolves without panicking.
        let wide = [ElementText {
            ordinal: 0,
            text: "世界",
        }];
        assert_eq!(
            resolve(&anchor(0, 1..4), "界", &wide),
            Some(anchor(0, 3..6))
        );
    }

    #[test]
    fn merges_an_overlapping_selection_into_the_staged_annotation() {
        let staged = [vec![anchor(0, 10..20)]];
        assert_eq!(
            merge(&[anchor(0, 15..25)], &staged),
            MergeDecision::Extend {
                index: 0,
                absorbed: Vec::new(),
                parts: vec![anchor(0, 10..25)],
            }
        );
    }

    #[test]
    fn an_exact_reselection_reveals_instead_of_widening() {
        let staged = [vec![anchor(0, 10..20)], vec![anchor(0, 10..20)]];
        assert_eq!(
            merge(&[anchor(0, 10..20)], &staged),
            MergeDecision::Duplicate { index: 0 }
        );
    }

    #[test]
    fn touching_spans_and_other_elements_stay_separate() {
        let staged = [vec![anchor(0, 10..20)]];
        // Adjacent, not overlapping: two passages, two annotations.
        assert_eq!(merge(&[anchor(0, 20..30)], &staged), MergeDecision::New);
        // Same range, different element: one anchor cannot cover both.
        assert_eq!(
            merge(&[anchor(SECOND_BLOCK, 10..20)], &staged),
            MergeDecision::New
        );
        assert_eq!(merge(&[anchor(0, 0..5)], &[]), MergeDecision::New);
    }

    #[test]
    fn absorbs_every_staged_span_the_union_reaches() {
        let staged = [vec![anchor(0, 10..20)], vec![anchor(0, 30..40)]];
        assert_eq!(
            merge(&[anchor(0, 15..35)], &staged),
            MergeDecision::Extend {
                index: 0,
                absorbed: vec![1],
                parts: vec![anchor(0, 10..40)],
            }
        );
    }

    #[test]
    fn the_first_overlapping_span_in_creation_order_is_kept() {
        let staged = [vec![anchor(0, 10..20)], vec![anchor(0, 25..30)]];
        assert_eq!(
            merge(&[anchor(0, 28..35)], &staged),
            MergeDecision::Extend {
                index: 1,
                absorbed: Vec::new(),
                parts: vec![anchor(0, 25..35)],
            }
        );
    }

    #[test]
    fn a_multi_element_selection_is_one_annotation() {
        // A drag across two elements is one selection; a staged annotation
        // starting in the first element absorbs the second part.
        let staged = [vec![anchor(0, 10..20)]];
        assert_eq!(
            merge(&[anchor(0, 10..20), anchor(SECOND_BLOCK, 0..5)], &staged),
            MergeDecision::Extend {
                index: 0,
                absorbed: Vec::new(),
                parts: vec![anchor(0, 10..20), anchor(SECOND_BLOCK, 0..5)],
            }
        );
    }
}
