# Add message annotations

## Why

Replying to one sentence of a long agent reply is a manual copy-paste dance today:
select the text, copy, paste it into the composer, retype its meaning. The agent
receives an unattributed paste, and the transcript keeps no record of what was
pointed at. Codex already ships the interaction users want — pick several spans of
the last reply, attach an optional note to each, send them together — and Doki
should own the same interaction natively.

The reason to build it as anchored records rather than string concatenation: the
anchor is what lets the transcript mark the annotated span, jump back to it, and
re-anchor after the message is re-rendered. A string-only payload gives up all of
that on the first render change.

## What Changes

- New annotation record anchored to a span of an assistant reply: source message,
  rendered-text element ordinal, byte range, quote snapshot, enclosing-block
  snapshot, optional comment.
- Create annotations from a transcript text selection through an explicit action
  (context menu and keyboard shortcut). Plain selection never creates one.
- Composer gains an annotation label row above the attachment tiles: a count chip
  that expands into a stacked card list (`Selected text:` plus an optional
  `User comment:`), one card per annotation, each removable.
- Hovering the label shows a read-only preview of the stacked annotations.
  Clicking a card (or pressing `enter` on it) scrolls the transcript to the marked
  span and briefly highlights it.
- Transcript marks: a numbered badge in the message gutter plus a wash over the
  annotated range. Once sent, the badge drops its number and the mark stays,
  revealing the quote, its comment, and the message it was sent with.
- Annotations persist on the sent user message and show as a compact indicator
  under it. Editing and resending that message restores its annotations into the
  composer; when the source reply is gone the cards degrade to quote-only.
- Staged annotations persist with the composer draft, so switching sessions or
  restarting the app does not lose them.
- One client-side projection turns annotation records into provider prompt text:
  numbered quotes and comments matching the numbers the user sees, a source
  locator, and a context window when the selection is a fragment of its block.
  `Command::Prompt` and `Command::Steer` keep carrying a single string, so no
  daemon command and no provider driver changes.
- Creating annotations is disabled while the reply is streaming.
- No breaking changes: the message field is additive and optional.

## Capabilities

### New Capabilities

- `message-annotations`: anchored annotations on transcript messages — creation
  from a selection, composer staging and presentation, transcript marks,
  jump-and-highlight, sent-message records, draft persistence, and restore on
  edit-resend.
- `prompt-context-projection`: the single client-side path that serializes
  annotation records (and later other staged context records) into provider
  prompt text — framing, numbering, source locators, caps, and the rule that
  structured records never reach the daemon as structured data.

### Modified Capabilities

None: `openspec/specs/` is empty, this is the first capability set for the app.

## Impact

- `crates/waku-protocol/src/model.rs`: `Message` gains an optional `annotations`
  list; `protocol.rs` carries staged annotations with saved composer drafts.
- `packages/waku-client/src/generated/`: TS bindings regenerate; the field is
  additive and `apps/web` / `apps/mobile` ignore it for now.
- `db/migrations/`: one additive migration; `crates/waku-core/src/persistence.rs`
  gains the column, its fingerprint input, and a migration test.
- `src/app/composer.rs`: the prompt-assembly seam becomes the projection entry
  point; the composer card gains the label row and card list.
- `src/app/transcript_view.rs`: marks, jump targets, reveal and flash.
- `src/md/render.rs`: annotation wash and gutter badge, reusing the existing
  ordinal contract and the search-highlight paint path.
- `src/app/drafts.rs` and `locales/*.yml`.
- Out of scope for this change: annotating tool activity, diffs, or file spans;
  annotating user messages; a unified context-record union across attachments.
