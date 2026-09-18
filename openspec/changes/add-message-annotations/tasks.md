# Tasks — add-message-annotations

Verification runs against the debug app owned by `bun ./scripts/dev.ts`. Never
rebuild or replace the installed `Doki.app` while the user is working in it.

## 1. Protocol and storage

- [x] 1.1 Add the `MessageAnnotation` record and an additive, optional `annotations` list on `Message` in `crates/waku-protocol/src/model.rs`; verify `cargo test -p waku-protocol` passes and older payloads without the field still deserialize
- [x] 1.2 Carry staged annotations on the composer draft payload in `crates/waku-protocol/src/protocol.rs` and persist them in `crates/waku-core/src/persistence.rs`; verify a save/load round-trip test restores staged annotations
- [x] 1.3 Add one additive migration under `db/migrations/` for the message column, include annotations in the message fingerprint input; verify an existing-database migration test opens, migrates, and reloads a message with annotations
- [x] 1.4 Regenerate TS bindings into `packages/waku-client/src/generated/`; verify `apps/web` and `apps/mobile` typecheck unchanged with the field ignored

## 2. Anchor resolution

- [ ] 2.1 Add a pure resolution module: verify the range when it still matches the quote, otherwise search the message, and prefer the hit nearest the previous offset; unit-test duplicates, ambiguous quotes, multi-byte boundaries, and a quote that is gone entirely
- [ ] 2.2 Implement overlap extension and exact re-selection reveal; unit-test union of overlapping spans and the no-op duplicate case
- [ ] 2.3 Keep resolution off the frame path: resolve once per message content version in a background pass with a generation guard; verify no resolution work runs per frame while typing and streaming

## 3. Creation and composer staging

- [ ] 3.1 Add the create action on a non-empty assistant-message selection, reachable from the message context menu and a keyboard shortcut, and refused while the reply streams; verify the refusal predicate in a test and the happy path in the debug app
- [ ] 3.2 Add the composer label row above the attachment tiles with expand, collapse, and remove; verify visually in the debug app with both annotations and an image attachment staged
- [ ] 3.3 Add the stacked card list with `Selected text:` and an optional `User comment:` field, editing the record on change; verify in the debug app and by asserting the stored record
- [ ] 3.4 Add the read-only hover preview with no interactive controls inside it; verify by hover in the debug app
- [ ] 3.5 Persist staged annotations with the draft; verify by switching sessions and by restarting the debug app

## 4. Transcript marks

- [ ] 4.1 Paint the wash plus a numbered gutter badge from staged records, reusing the existing highlight paint path and the element ordinal contract; verify visually in the debug app and that selection and find still behave
- [ ] 4.2 Keep marks in sync on remove and on extend; verify in the debug app
- [ ] 4.3 Confirm badges are never inline and never colour-only; verify by rendering a staged annotation over a wrapped paragraph

## 5. Prompt projection

- [ ] 5.1 Implement the projection at the prompt-assembly seam: framing header, user text verbatim, numbered entries with quote, optional comment, source locator, fragment context window, and caps with truncation markers; unit-test every scenario in `specs/prompt-context-projection/spec.md`
- [ ] 5.2 Route prompt, steer, queued, and edit-resend paths through the projection; verify a test asserts identical annotation text for the same input across all four
- [ ] 5.3 Confirm no offsets, ordinals, or message ids reach the prompt, and that a directive-like or multi-line quote stays confined to its field; verify by unit test
- [ ] 5.4 Localize the new user-visible labels in `locales/`; verify all locale files gain the keys and the app renders them

## 6. Send, persist, restore

- [ ] 6.1 Write annotations onto the sent user message, show the compact indicator with on-demand expansion, and drop the draft numbering from transcript badges; verify in the debug app
- [ ] 6.2 Restore annotations into the composer on edit-resend, degrading to quote-only when the source message no longer exists; verify both cases in the debug app
- [ ] 6.3 Reload the session after a restart and confirm sent annotations and marks are still there; verify in the debug app

## 7. Reveal, flash, accessibility

- [ ] 7.1 Jump from a card by reusing the transcript search reveal path; verify an off-screen source and one taller than the viewport
- [ ] 7.2 Flash the span through a pulse-clock lease at the decorative-motion cadence and skip it under reduce-motion; verify with the system setting toggled
- [ ] 7.3 Make the label, cards, delete, and the sent-message indicator fully keyboard operable; verify the whole review flow with the keyboard only

## 8. Validation

- [ ] 8.1 Walk both spec files' scenarios in the freshly rebuilt debug app and record the outcome for each
- [ ] 8.2 Re-read `docs/performance.md` and confirm the annotation paths add no I/O, subprocess, or per-frame work, and that the pulse lease parks when no flash is active
- [ ] 8.3 Confirm `openspec validate add-message-annotations --strict` stays green and that no provider driver or daemon command changed
