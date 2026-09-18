# Message annotations — design

## Context

See `proposal.md` for motivation. The pieces this design leans on already exist:

- Every painted text element in a transcript message registers as
  `TextKey { row: "message-<id>", index }`, where `index` is the ordinal produced
  by `block_ordinal_base(block_ix) + element_index` (`src/md/render.rs`).
- `md::render::markdown_search_matches` walks the same tree in the same order and
  returns `(ordinal, byte_range)` for a byte range inside that element's flattened
  rendered text. The contract is covered by
  `markdown_matches_use_the_renderers_element_ordinals`.
- A selection is `Vec<Span { key, range, text, block_break }>` —
  an annotation anchor minus the comment.
- `MarkdownCtx::with_search_highlights` already paints persistent, non-selection
  ranges inside those elements.
- `apply_pending_transcript_search_reveal` + `reveal_transcript_search_geometry`
  already solve "scroll to a spot inside a message taller than the viewport
  without a one-frame jump".
- `merged_submission` is where prompt text is assembled today; `ComposerSubmission`
  rides prompt, steer, queued, and edit-resend paths. `Command::Prompt` and
  `Command::Steer` carry one string.
- Constraints from `AGENTS.md`: no I/O or heavy work reachable from a frame, pulse
  clock for decoration with `reduce_motion` honored, keyboard operability, and
  markdown-renderer contracts are load-bearing for search and selection.

## Goals / Non-Goals

**Goals**

- Annotation records survive re-render, app-version normalizer changes, and a
  round trip through storage, without a per-frame resolution cost.
- The provider prompt is produced by one client-side function, so every send path
  (prompt, steer, queue, edit-resend) has identical behavior and no provider
  driver knows annotations exist.
- The transcript mark and the composer card stay in sync through create, delete,
  send, and edit-resend.

**Non-Goals**

- Annotating tool activity, diffs, or file spans; annotating user messages.
- A unified `ContextReference` union over attachments, file spans, and DOM
  elements. The seam is defined (`{ target, comment }` with a typed target) but
  only the `message-span` arm is built.
- Resolution/triage state per annotation (open/resolved), threads, or replies.
- Interactive hover panels. The hover preview is read-only; expanding is a click.

## Decisions

**D1. Anchor identity, and re-resolution without prefix/suffix.**
An anchor is `(message_id, ordinal, byte_range)` plus a quote snapshot and an
enclosing-block snapshot. Resolution verifies `flat_text[range] == quote`; on
failure it searches the message for the quote and, when the quote occurs more than
once, takes the hit nearest the previous offset.

Alternatives: storing `prefix`/`suffix` strings for re-anchoring (rejected: the old
offset is already the strongest positional prior, so prefix/suffix add two fields
and a maintenance path for no extra accuracy); storing offsets into the raw
markdown source (rejected: selection, painting, and search all address the
flattened rendered text, and `**bold**` collapses to `bold` in it).

**D2. Record shape.** `MessageAnnotation { id, target: MessageSpan { message_id,
ordinal, range, quote, block }, comment: Option<String> }`. The target is typed and
switchable even though it has one arm today; `comment` is separate from any
captured text so the projection can label them distinctly.

Alternatives: a single free-text payload (rejected: the projection could not label
quote vs. instruction, and the UI could not reveal or trim one without the other);
a full context-record union now (rejected as speculative: attachments serialize as
`@path` mentions through a different path and storage format, so unifying today
would only reconcile two formats for one consumer).

**D3. Projection lives in the client, at the `merged_submission` seam.** One
function takes the user's text plus staged records and returns the prompt string.
The daemon, `waku-protocol` commands, and all provider drivers are unchanged, so
the feature works identically under every harness.

Alternatives: an `annotations` field on `Command::Prompt` rendered per driver
(rejected: ten drivers times one rendering decision, no user-visible gain); writing
quotes to a temp file and attaching it (rejected: turns conversation context into
workspace pollution).

**D4. What the prompt carries.** A framing header, the user's own text verbatim,
then numbered entries that match the numbers in the UI: a source locator, the
quote, an optional `comment` line, and — only when the selection is a fragment of
its block — a `context` window centred on the quote. Offsets and ordinals are
internal coordinates and are never sent to a model.

Alternatives: quote only (rejected: a mid-word selection such as a three-character
fragment identifies nothing on its own, and once a reply leaves the model's context
window the quote is the only pointer left); the whole source reply (rejected:
redundant while the reply is in context, expensive when it is not); a JSON blob
per annotation (rejected: no readability gain, and models read labelled fields
better).

**D5. Creation is an explicit action.** `secondary-alt-a` and a context-menu entry
create an annotation from the current selection. Because selection is mouse-only in
the transcript today, creation is mouse-driven in this change; every other part of
the surface (open, review, delete, jump) is keyboard-complete.

Alternative: create on selection end (rejected: makes `select → cmd-C` leave a
stray annotation).

**D6. Marks are gutter badge plus wash, never inline.** An inline badge would
reflow a shaped paragraph and break the ordinal/range contract that selection and
search both depend on. The badge is a numeral, so the mark is never colour-only.

**D7. Lifetimes.** Staged records live on the composer draft (persisted with
`ComposerDrafts`). Sent records live on the user message that carried them. Numbers
are draft-scoped vocabulary shared by the badge, the card, and the prompt; after
send the badge drops its number and the mark stays as a quiet underline that
reveals the quote, comment, and originating message.

Alternative: marks that vanish on send (rejected: reading back "did I already raise
this?" is half the value, and it would leave the sent message as the only trace).

**D8. No annotations while streaming.** The turn's own text is still moving and its
parse can restructure. Refusing creation keeps anchors static, which removes a
re-resolution path from the frame budget entirely. The cost — annotating a settled
prefix mid-turn — stays available as a later relaxation.

**D9. Reveal and flash reuse existing machinery.** Jumping uses the transcript
search reveal path (mount the row if needed, then reveal glyph geometry). The flash
is an overlay lease on the pulse clock at ≤30 Hz, skipped under `reduce_motion`
where the scroll and the badge carry the meaning.

**D10. Overlapping selections extend the existing annotation** rather than stacking
a second badge on the same passage. Identical re-selection is a no-op that reveals
the existing card.

## Risks / Trade-offs

- **Hover preview cannot host buttons yet** → v1 preview is read-only; expanding the
  card list is an explicit click or `enter`. Resolving pointer-path/bridge behavior
  is left to a separate exploration.
- **Normalizer changes across app versions can move a range** → quote search with a
  positional prior; worst case the mark disappears while the annotation still ships
  its quote.
- **Provider-side session logs keep only the projected text** → annotation records
  live in Doki's store, so a session resumed from a provider-native conversation
  degrades to the plain projected text. The transcript remains the source of truth.
- **Prompt growth** → per-quote cap (~4 KB / 12 lines) and a context-window cap
  (~800 chars) with an explicit truncation marker.
- **Renumbering after delete can invalidate a typed "fix 2"** → numbers are creation
  order and never reordered by document position; the accepted trade-off is that
  deleting a card renumbers the rest, and cards stay visible while typing.
- **Assistant output quoted back is partly redundant with the model's own context**
  → caps keep the redundancy cheap, and the locator plus numbering is the part the
  model cannot reconstruct.
- **`apps/web` and `apps/mobile` do not render annotations** → additive optional
  field; no behaviour change for those clients.

## Migration Plan

1. Protocol and storage first: optional `annotations` on `Message`, staged
   annotations on the draft payload, one additive migration, and fingerprint input,
   verified by a migration test.
2. Creation, composer staging and card list, transcript marks.
3. Projection on every send path, verified by unit tests over the serializer's
   format, caps, and fragment-context rule.
4. Reveal, flash, sent-message indicator, edit-resend restore.
5. Rollback: older builds ignore the added field and column; no reverse migration
   is required.

## Open Questions

- The settled mark's exact visual weight (faint wash vs. underline vs. dot) is a
  later visual call; it does not change the record or the projection.
- Whether the sent-message indicator expands inline or in a popover is deferred; the
  record shape already supports both.
