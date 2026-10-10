# Proposal

## Why

Typing Pi's own `/compact` in a Pi session compacts nothing: the text is sent
to the provider as an ordinary prompt, so the model receives a user message
that says `/compact`. The command is not offered in the composer's palette
either, because Pi's `get_commands` RPC returns only extension commands,
prompt templates and skills — its built-in slash commands are the interactive
TUI's own dispatch — and `parse_pi_commands` correctly mirrors that list.

Pi's RPC does expose the operation: `{"type": "compact", "customInstructions"?
"…"}` runs `session.compact()`, which emits `compaction_start` / `compaction_end`
on the same stream Waku already reads and answers with the summary,
`tokensBefore` and `estimatedTokensAfter`. Waku has no path to it, and it
shows none of the compaction Pi performs on its own.

## What Changes

- Pi sessions offer `/compact [focus]` in the composer: the daemon's command
  catalogue adds the built-in beside the commands the provider reports, so
  autocompletion resolves it, while a project, user or skill command of the
  same name still wins the name.
- A submitted `/compact` for a Pi session never reaches the model. The Pi
  driver recognises the prompt at the transport boundary and writes the
  provider's `compact` RPC instead, with the focus text as
  `customInstructions`; a compaction that races a live turn is refused as
  undelivered rather than aborting the run Pi's `session.compact()` would
  abort. Only Pi is affected: Oh My Pi keeps its current behaviour.
- Compaction is visible in the transcript: `compaction_start` opens a
  "Compacting context" activity, `compaction_end` completes it as
  "Compacted context" or fails it with the provider's own reason, and the
  summary is kept on the row. The context meter follows
  `estimatedTokensAfter`. Pi's automatic (threshold and overflow) compaction
  shows through the same row.
- The turn a `/compact` was submitted in settles when the compaction ends,
  without the answerless-turn fallback line: the activity row is the record.

## Capabilities

### Modified Capabilities

- `pi-provider`: a new requirement covers recognising and running the
  provider's compaction command, offering it in the composer, refusing to
  abort a live turn for it, and surfacing both manual and automatic
  compaction.

## Impact

- `crates/waku-protocol/src/composer.rs`: a parser for the compact
  invocation shared by the daemon and the client.
- `crates/waku-core/src/composer_complete.rs`: the Pi command catalogue.
- `crates/waku-core/src/driver/pi.rs`: prompt routing, the `compact` RPC
  write, compaction stream events, and the RPC answer that arrives after
  them.
- `src/app/runtime.rs`, `src/app/streaming.rs`, `src/app.rs`: the settling
  turn is recorded and marked so it does not gain a synthetic reply.
- `locales/*.yml`, `docs/providers.md`: new copy and the provider notes.
