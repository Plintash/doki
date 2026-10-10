# Tasks

## 1. The compact invocation, in one place

- [x] 1.1 Add `parse_compact_invocation` to `crates/waku-protocol/src/composer.rs`: bare `/compact`, or `/compact <focus>`, with the focus returned as its own value; verified by unit tests for the bare form, the focus form, whitespace, a different command and a skill path

## 2. The composer offers it

- [x] 2.1 Push Pi's built-in `compact` into the catalogue assembled in `crates/waku-core/src/composer_complete.rs` (scope `Builtin`, hint `[focus]`, no template, description from a new locale key) so it is discoverable and dedup still lets a project, user or skill command of that name win; verified by a catalogue test for a Pi session with no reported commands and with a same-named project command
- [x] 2.2 Add `commands.compact_description` to `locales/app.yml`, `locales/zh-CN.yml` and `locales/ja.yml`; verified by the web i18n parity test (the new keys appear in both translated catalogs; the remaining six missing keys are the pre-existing #19 set on `main`, and the three `activity.compaction_*` keys this feature renders are now translated too)

## 3. The driver runs it instead of the model

- [x] 3.1 Route a `CommandMessage::Prompt` through one function in `crates/waku-core/src/driver/pi.rs`: a Pi-flavour compact invocation with no live run writes `{"type":"compact","customInstructions"?…}` with no id to await, and any other prompt is sent as before; verified by a wire test for the bare and focused forms and a same-wire test that Oh My Pi still prompts
- [x] 3.2 Refuse a compact prompt that arrives while a run is live: no compact RPC is written and the submission settles as undelivered with a reason, because Pi's `session.compact()` would abort the run; a steer carrying the same command is rejected so the client queues it through the prompt path; verified by driver tests for the live-run prompt and for the steer guard (Pi rejects, Oh My Pi still steers)
- [x] 3.3 Write the request without an id and await nothing, like `abort` and `clear_queue`: every outcome the supported RPC reports arrives as an event, so no parallel answer path is added; verified by the wire test asserting the write carries no id and registers no pending response
- [x] 3.4 Add `errors.compact_turn_running` and `errors.compact_queued_until_settled` to the three locale catalogs; verified by the parity test

## 4. Compaction is visible

- [x] 4.1 Map `compaction_start` / `compaction_end` onto `RichActivity` rows with the existing copy: start opens "Compacting context" with a per-compaction id, a result completes it as "Compacted context" with the summary as output, a failure completes it as failed with the provider's reason as detail, and an abort just completes it; verified by driver tests for success and failure (the abort row is completed rather than left live)
- [x] 4.2 Refresh the context meter from the result's `estimatedTokensAfter` without clearing the known window; verified by the same success test asserting `UsageUpdated { context_tokens: Some(32_000), context_window: None }`
- [x] 4.3 A manual compaction settles its turn: `compaction_end` with reason `manual` emits `TurnFinished` (interrupted when aborted, success otherwise), while a threshold or overflow compaction emits none; verified by driver tests for both reasons

## 5. The settling turn gets no synthetic reply

- [x] 5.1 Record the turn on the session runtime when the resolved prompt handed to the transport is a Pi compact invocation, at `finish_submission_preparation` in `src/app/runtime.rs`; verified by `cargo check` and the operator's app validation
- [x] 5.2 Skip the answerless-turn fallback in `TurnFinished` for exactly that recorded turn in `src/app/streaming.rs`, consuming the record either way so a command the provider ignored cannot swallow a later settlement; verified by `cargo check` and the operator's app validation

## 6. Documentation and validation

- [x] 6.1 Note the `/compact` bridge in the Pi section of `docs/providers.md`; verified by the paragraph matching the implemented behaviour
- [x] 6.2 Run `cargo fmt --package waku --package waku-protocol --package waku-client --package waku-core --package waku-daemon -- --check`, `cargo check` and `bun run protocol:check`; verified by all three reporting no changes or errors
- [x] 6.3 Run `cargo test --locked`; verified by the full suite passing with no failures
- [ ] 6.4 Validate in the freshly rebuilt debug app against a Pi session: `/compact` completes from the palette, submitting it compacts without a model turn, the activity row shows while it runs and when it ends, the context meter moves, and a project command named `compact` still wins — **owned by the user**, who runs the app-level checks by hand in the worktree's rebuilt app; driver and catalogue tests cover the same paths headlessly
