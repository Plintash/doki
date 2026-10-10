# Design

## Where the command is recognised

The obvious shape is the one `/fast` and `/goal` have: the app notices the
typed command and calls a dedicated `DriverControl` method. The desktop's
`DriverControl` is not the daemon's, though — it is a proxy that forwards a
`waku_client::Command` over the daemon connection, and `Command` is part of
the generated wire surface shared with the web and mobile clients. Adding a
`compact` command would therefore add a protocol variant and regenerate
every client to serve one provider's maintenance command.

A prompt already crosses that boundary with everything needed. `/compact
[focus]` is a provider command rather than a Waku control: it belongs in the
transcript the way OpenCode's native commands do, and the prompt path already
routes those (`submit_prompt` dispatches registered native commands). So the
driver recognises the submitted text at the transport boundary, exactly where
Pi's own TUI dispatches its built-ins — before extension commands and prompt
templates — and writes the RPC instead. Because the interception lives in the
transport, every client of the daemon gets it, including one that only knows
how to send a prompt.

The recognition is `strip_prefix('/')`, name `compact`, optional focus text.
`resolved_submission` has already run: a project, user or skill command named
`compact` either expanded to its template body or resolved to `/skill:compact`,
so it cannot be mistaken for the built-in. The catalogue injection keeps the
same precedence — it is pushed as `Builtin` before dedup, so a more specific
scope still owns the name.

## Why run when the agent is idle

Pi's `session.compact()` calls `abort()` first. That is the TUI's semantics:
pressing enter on `/compact` mid-run stops the run and compacts. Waku's
submit path queues a message while its session is busy instead of sending it,
so the normal flow never races; the driver still guards the boundary. When a
prompt does arrive while a run is live, it refuses the compaction — the run is
not aborted on the user's behalf by a maintenance command — and reports the
submission as undelivered with a reason, which is the same settlement a
refused prompt gets.

## Visibility

`compaction_start` and `compaction_end` already ride the `session.subscribe`
stream the RPC forwards. The driver maps them onto the activity surface the
app already renders, reusing the existing copy:

- `compaction_start` opens a row titled `activity.compacting_context`, with an
  id unique to that compaction so consecutive compactions do not overwrite
  each other.
- `compaction_end` completes it. A result becomes `activity.compacted_context`
  with the summary kept as the row's output; a failure becomes
  `activity.compaction_failed` with the provider's `errorMessage` as its
  detail; an aborted compaction just completes the row, because the stop path
  already said what happened.
- `estimatedTokensAfter` refreshes `DriverEvent::UsageUpdated`; a missing
  window keeps the current one, which is how every other partial update reads.

Automatic compaction (threshold or overflow) is the same two events without a
manual request behind them, so it gets the same row. It does not settle any
turn: the run that triggered it is already open and settles through its own
`agent_settled`.

A manual compaction has no run to settle it. Its `compaction_end` therefore
emits `TurnFinished` itself — success even when the compaction failed, because
a failed maintenance command must not paint the task red; the failed row
carries the reason, and an aborted compaction is reported as interrupted.

## The turn that submitted it

The turn `/compact` was submitted in has a user message and, by design, no
assistant reply: the command runs maintenance, not an agent turn. The app
would otherwise push its answerless-turn line ("Turn completed") under it. The
hand-off point — where the resolved prompt leaves for the transport — records
the turn id on the runtime when the prompt is a compact invocation for Pi, and
`TurnFinished` skips that fallback for exactly that turn. Any other settlement
path (a refused prompt, a dead process, a stop) keeps its normal behaviour.

## The RPC answer

The `compact` request carries an id, and its answer arrives after
`compaction_end` (the handler awaits the same operation). It exists for the
case the events cannot describe: a build whose RPC does not know the command
answers with an error and emits no events at all, which would leave the turn
spinning. The driver therefore reads the answer as well: when no
`compaction_end` settled the request first, the refusal settles that
submission as undelivered. When the end event did settle it, the answer adds
nothing.

## Out of scope

Oh My Pi's registry carries a built-in `compact` for its own TUI, but its RPC
surface has not been verified to expose the command, and its flavour split
stays intact. This change is Pi's own compact path.
