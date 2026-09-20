//! Re-anchoring the annotations a sent message carried.
//!
//! A sent annotation is a stored record: its element ordinal and byte range
//! were taken from the reply as it rendered when the annotation was created,
//! and that reply is rendered again after a reload, after an app update that
//! changes how text is normalized, or after any other reparse. Verifying or
//! re-finding the quote is real work — a full markdown parse of the reply — so
//! it never runs on a frame. The app resolves a session's sent anchors in one
//! background pass per signature (see `sync_sent_annotation_resolution`),
//! caches the ranges per target message, and a render only reads that cache. A
//! miss draws no mark; the quote and comment still live on the message and
//! still show in its indicator.
//!
//! This module is pure: it borrows `md::render`'s pure element walk and
//! `md::annotation`'s resolver, and holds no gpui state, so the pass and its
//! tests need no window.

use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use uuid::Uuid;

use super::transcript::{EMPTY_TRANSCRIPT_FINGERPRINT, mix, mix_uuid};
use crate::md;
use crate::model::{AgentSession, AnnotationTarget, MessageAnnotation};

/// One stored anchor, as the pass needs it: where it was, and what it quoted.
pub(super) struct StoredAnchor {
    pub ordinal: usize,
    pub range: Range<usize>,
    pub quote: String,
}

/// A session's resolved sent annotations plus the signature they were resolved
/// for. The signature is written before the background pass starts, so an
/// unchanged session never schedules a second pass.
#[derive(Default)]
pub(super) struct SentAnnotationResolution {
    signature: Option<u64>,
    /// Resolved marks per annotated reply, in the order the anchors were
    /// gathered. Shared because a render clones the list per frame.
    marks: HashMap<Uuid, Rc<Vec<md::render::AnnotationMark>>>,
}

impl SentAnnotationResolution {
    pub(super) fn signature(&self) -> Option<u64> {
        self.signature
    }

    /// Adopt a new signature and drop the ranges resolved for the old one. The
    /// pass fills `marks` back in when it lands.
    pub(super) fn reset(&mut self, signature: u64) {
        self.signature = Some(signature);
        self.marks.clear();
    }

    /// The resolved marks for one reply, if the current signature resolved it.
    pub(super) fn marks(&self, message_id: Uuid) -> Option<Rc<Vec<md::render::AnnotationMark>>> {
        self.marks.get(&message_id).cloned()
    }

    /// Store one reply's resolved marks, but only while the signature still
    /// holds — a superseded pass must not overwrite newer state.
    pub(super) fn insert(
        &mut self,
        signature: u64,
        message_id: Uuid,
        marks: Vec<md::render::AnnotationMark>,
    ) {
        if self.signature == Some(signature) {
            self.marks.insert(message_id, Rc::new(marks));
        }
    }
}

/// The message id a sent annotation points at, when it is a message-span
/// annotation (the only kind today).
pub(super) fn annotated_message_id(annotation: &MessageAnnotation) -> Option<Uuid> {
    match &annotation.target {
        AnnotationTarget::MessageSpan { message_id, .. } => Some(*message_id),
    }
}

/// The anchor a sent annotation stores, for the reply it targets.
pub(super) fn stored_anchor(annotation: &MessageAnnotation) -> StoredAnchor {
    match &annotation.target {
        AnnotationTarget::MessageSpan {
            ordinal,
            span,
            quote,
            ..
        } => StoredAnchor {
            ordinal: *ordinal,
            range: span.start..span.end,
            quote: quote.clone(),
        },
    }
}

/// Every sent anchor that targets `message_id`, in session order.
pub(super) fn anchors_targeting(session: &AgentSession, message_id: Uuid) -> Vec<StoredAnchor> {
    session
        .messages
        .iter()
        .flat_map(|message| message.annotations.iter())
        .filter(|annotation| annotated_message_id(annotation) == Some(message_id))
        .map(stored_anchor)
        .collect()
}

/// The replies in this session that some sent annotation targets, and the
/// content the pass must reparse for each. Recomputed only when the signature
/// moves, never on a frame.
pub(super) fn resolution_jobs(session: &AgentSession) -> Vec<(Uuid, String, Vec<StoredAnchor>)> {
    let mut jobs: Vec<(Uuid, String, Vec<StoredAnchor>)> = Vec::new();
    for annotation in session
        .messages
        .iter()
        .flat_map(|message| message.annotations.iter())
    {
        let Some(message_id) = annotated_message_id(annotation) else {
            continue;
        };
        if jobs.iter().any(|(existing, _, _)| *existing == message_id) {
            continue;
        }
        let Some(target) = session
            .messages
            .iter()
            .find(|message| message.id == message_id && !message.streaming)
        else {
            continue;
        };
        jobs.push((
            message_id,
            target.visible_content().to_owned(),
            anchors_targeting(session, message_id),
        ));
    }
    jobs
}

/// A cheap value that changes exactly when a sent annotation, or the reply it
/// targets, changes. Only sent annotations and their target replies feed it, so
/// an ordinary streamed token elsewhere cannot re-run the pass. No allocation:
/// the per-frame check is a fold over the records plus a length read per
/// target.
pub(super) fn sent_annotation_signature(session: &AgentSession) -> u64 {
    let mut hash = mix_uuid(EMPTY_TRANSCRIPT_FINGERPRINT, session.id);
    for source in &session.messages {
        for annotation in &source.annotations {
            hash = mix_uuid(hash, source.id);
            hash = mix_uuid(hash, annotation.id);
            if let Some(target) = annotated_message_id(annotation) {
                hash = mix_uuid(hash, target);
                // The stored ranges are relative to the target's text, so a
                // rewrite or an append there must re-run the pass too. A
                // still-streaming reply is skipped: its text moves every
                // commit and it cannot carry a sent annotation yet, so hashing
                // its length would only schedule pointless passes.
                if let Some(reply) = session
                    .messages
                    .iter()
                    .find(|message| message.id == target && !message.streaming)
                {
                    hash = mix(hash, reply.visible_content().len() as u64);
                }
            }
            let AnnotationTarget::MessageSpan {
                ordinal,
                span,
                quote,
                ..
            } = &annotation.target;
            hash = mix(hash, *ordinal as u64);
            hash = mix(hash, span.start as u64);
            hash = mix(hash, span.end as u64);
            hash = mix(hash, quote.len() as u64);
        }
    }
    hash
}

/// Re-anchor one reply's stored anchors against its current content.
///
/// Returns the marks in input order, dropping any anchor whose quote can no
/// longer be located. Unnumbered by construction: a sent annotation has left
/// the composer, so it carries no draft number to paint.
pub(super) fn resolve_sent_annotations(
    content: &str,
    anchors: &[StoredAnchor],
) -> Vec<md::render::AnnotationMark> {
    if anchors.is_empty() {
        return Vec::new();
    }
    let texts = md::render::markdown_element_texts(content);
    let elements = texts
        .iter()
        .map(|(ordinal, text)| md::annotation::ElementText {
            ordinal: *ordinal,
            text: text.as_str(),
        })
        .collect::<Vec<_>>();
    anchors
        .iter()
        .filter_map(|anchor| {
            let stored = md::annotation::Anchor {
                ordinal: anchor.ordinal,
                range: anchor.range.clone(),
            };
            md::annotation::resolve(&stored, &anchor.quote, &elements).map(|resolved| {
                md::render::AnnotationMark {
                    ordinal: resolved.ordinal,
                    range: resolved.range,
                    number: None,
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Message, MessageRole, ProviderKind, TextSpan};

    fn anchor(ordinal: usize, range: Range<usize>, quote: &str) -> StoredAnchor {
        StoredAnchor {
            ordinal,
            range,
            quote: quote.to_owned(),
        }
    }

    /// One assistant reply carrying one sent annotation that targets it.
    fn session_with_annotation(reply: &str, streaming: bool) -> (AgentSession, Uuid) {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::default());
        let mut reply_message = Message::new(MessageRole::Assistant, reply);
        reply_message.streaming = streaming;
        let reply_id = reply_message.id;
        session.messages.push(reply_message);
        let mut carrier = Message::new(MessageRole::User, "fix 1");
        carrier.annotations.push(MessageAnnotation {
            id: Uuid::new_v4(),
            target: AnnotationTarget::MessageSpan {
                message_id: reply_id,
                ordinal: 0,
                span: TextSpan { start: 0, end: 3 },
                quote: "the".to_owned(),
                block: reply.to_owned(),
            },
            comment: None,
        });
        session.messages.push(carrier);
        (session, reply_id)
    }

    const SOURCE: &str = "the retry helper drops the error\n\n\
                          second paragraph keeps the timeout\n";

    #[test]
    fn a_span_that_still_matches_resolves_to_itself() {
        let marks = resolve_sent_annotations(SOURCE, &[anchor(0, 4..9, "retry")]);
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].ordinal, 0);
        assert_eq!(marks[0].range, 4..9);
        // Sent marks are never numbered.
        assert_eq!(marks[0].number, None);
    }

    #[test]
    fn a_quote_that_shifted_is_found_by_search() {
        // The stored offsets are wrong, but the quote is still in element 0.
        let marks = resolve_sent_annotations(SOURCE, &[anchor(0, 0..5, "retry")]);
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].ordinal, 0);
        assert_eq!(marks[0].range, 4..9);
    }

    #[test]
    fn a_fragment_in_the_second_block_resolves_to_its_ordinal() {
        let marks = resolve_sent_annotations(SOURCE, &[anchor(0, 0..7, "timeout")]);
        assert_eq!(marks.len(), 1);
        // Second top-level block: ordinal stride 1 << 16.
        assert_eq!(marks[0].ordinal, 1 << 16);
    }

    #[test]
    fn a_quote_that_is_gone_draws_no_mark() {
        let marks = resolve_sent_annotations(SOURCE, &[anchor(0, 0..4, "nomatch")]);
        assert!(marks.is_empty());
    }

    #[test]
    fn a_repeated_quote_stays_near_its_stored_offset() {
        let source = "alpha beta alpha beta\n";
        // Stored in the middle, nearer the first occurrence.
        let marks = resolve_sent_annotations(source, &[anchor(0, 3..8, "alpha")]);
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].range, 0..5);

        // And nearer the second one.
        let marks = resolve_sent_annotations(source, &[anchor(0, 14..19, "alpha")]);
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].range, 11..16);
    }

    #[test]
    fn several_anchors_resolve_independently_and_keep_order() {
        let marks = resolve_sent_annotations(
            SOURCE,
            &[
                anchor(0, 0..5, "retry"),
                anchor(0, 0..4, "nomatch"),
                anchor(1 << 16, 0..7, "timeout"),
            ],
        );
        assert_eq!(marks.len(), 2);
        assert_eq!(marks[0].range, 4..9);
        assert_eq!(marks[1].ordinal, 1 << 16);
    }

    #[test]
    fn the_signature_moves_only_for_a_sent_annotation_or_its_reply() {
        let (mut session, _) = session_with_annotation("the answer", false);
        let base = sent_annotation_signature(&session);
        // Unrelated churn — another message appearing while a turn streams —
        // must not schedule another pass.
        session
            .messages
            .push(Message::new(MessageRole::Assistant, "unrelated"));
        assert_eq!(sent_annotation_signature(&session), base);
        // Rewriting the annotated reply must: the stored ranges are relative
        // to its text.
        session.messages[0].content.push_str(" updated");
        assert_ne!(sent_annotation_signature(&session), base);
    }

    #[test]
    fn a_streaming_or_missing_target_is_never_a_job() {
        // A reply still streaming cannot carry a sent annotation, so it is not
        // reparsed while its text moves every commit.
        let (streaming, _) = session_with_annotation("the answer", true);
        assert!(resolution_jobs(&streaming).is_empty());

        let (mut session, reply_id) = session_with_annotation("the answer", false);
        assert_eq!(resolution_jobs(&session).len(), 1);
        // A rewind deletes the reply; the annotation stays and simply draws no
        // mark, so there is no job to run.
        session.messages.retain(|message| message.id != reply_id);
        assert!(resolution_jobs(&session).is_empty());
    }
}
