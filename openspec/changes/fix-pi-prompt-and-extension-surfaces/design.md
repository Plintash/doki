# Design

## Context

Pi's RPC mode is four record families on one NDJSON pipe (`docs/rpc.md` from the
installed CLI is the authoritative reference): commands in, one `response` per
command, session events out, and an extension UI subprotocol that travels both
ways. Everything in this change already exists on that pipe. Waku's driver reads
a slice of it.

What the provider already sends and Waku ignores, verified against the installed
1.0.0 (`dist/core/agent-session.js`, `dist/modes/rpc/rpc-mode.js`) and against
0.99.1, which is byte-identical in `rpc-types.d.ts`, `json.md` and
`rpc-commands.md`:

| Provider record | What it means | Waku today |
| --- | --- | --- |
| `prompt` answer `{data: {disposition: started \| queued \| handled}}` | what happened to the prompt | tests `data.agentInvoked`, which Pi never sends (`pi.rs:1291`) |
| `steer`/`follow_up` answers `{disposition: queued}` | the message is in a queue, not delivered | treated as delivered (`pi.rs:757`) |
| `queue_update {steering, followUp}` | the complete current queue | unhandled (`_ => {}`) |
| `clear_queue` | removes queued messages and returns their text | never sent |
| `abort` | answers once the session is idle | sent with the 10 s control timeout (`RPC_TIMEOUT`) |
| `message_start`/`message_end` with `role: "custom"` | extension messages: `<customType>`, text, `display` | unhandled — only `role: "assistant"` is read |
| `extension_ui_request` (`notify`, `setStatus`, `setWidget`, `setTitle`, `set_editor_text`) | fire-and-forget status surfaces | dropped |
| `extension_ui_request` (`select`, `confirm`, `input`, `editor`) | answerable dialogs | auto-cancelled (`pi.rs:1520`) |

Two provider behaviours that decide the approach:

- `session.prompt()` reads `streamingBehavior` **only inside its streaming
  branch**; while idle the option is ignored and the prompt runs normally. A
  constant value is therefore safe, and no racy `get_state` probe is needed.
- The run loop drains steering and follow-up queues before it emits
  `agent_settled` (`_runBeforeSettleBoundary` → `agent.hasQueuedMessages()`), so
  a queued message and the run that was already going settle **once**. `queued`
  therefore belongs to the turn that is already open, not to a new one.

And one on Waku's side: a `TurnStarted` with no active turn is only given a
transcript home for Codex, Claude and OpenCode (`src/app/streaming.rs:350`), and
every `TextDelta`/`RichActivity` is gated on an active, busy turn
(`session_accepts_turn_output`, `src/app/streaming.rs:901`). A run Pi starts on
its own is therefore dropped whole — which is what makes a pi-subagents
completion wake invisible today.

## Goals / Non-Goals

**Goals**

- A prompt Waku hands to Pi is delivered, queued, or reported as refused; it is
  never silently absent from the provider's session while looking delivered in
  the client's.
- Work the agent starts on its own is visible: its turn streams, and the
  notification that caused it appears where the user will see it.
- Extension UI records reach the user: every `notify`/`setStatus`/`setWidget`
  and every dialog has an answer, and none is cancelled on the user's behalf.
- Acceptance is the product path: `npm:pi-subagents` drives a background
  workflow whose completion wakes the parent.

**Non-Goals**

- Version gating on Pi. The RPC surface did not change between 0.99.1 and 1.0.0;
  the defects are ours and are live on both.
- `ctx.ui.custom()` overlays, `onTerminalInput`, `setToolsExpanded` and
  `getEditorText`. Pi's own RPC mode no-ops these (`rpc-extension-ui.md`), so
  there is nothing to receive.
- Oh My Pi's permission system. Wiring it to a `Permission` event is a separate
  change; this one must not regress Oh My Pi, whose `agentInvoked` signal stays
  honoured alongside `disposition`.

## Decisions

**D1 — Every prompt carries `streamingBehavior: "followUp"`.** It is ignored
while the agent is idle, so one constant covers both cases: a normal prompt
starts a run, and a prompt racing the tail of a turn is queued instead of
refused. Rejected: probing `get_state.isStreaming` first (racy — the state can
change between the probe and the write) and retrying after a refusal (the
refusal is only readable after the prompt was already rejected).

**D2 — Delivery is read from `data.disposition`.** `started` runs, `queued`
waits inside the current turn, `handled` means no run will start for this
prompt. Oh My Pi's `data.agentInvoked: false` keeps its meaning as an additional
`handled` signal; it is no longer the only test.

One case keeps that from being the whole rule, and measuring it changed the
design. At a normal turn boundary the provider drains its queue inside the same
run, so the message is answered in that turn and there is one settlement. Across
an **abort** it does not: the run settles with the message still held, no run
starts for it, and the held text is only consumed by the *next* prompt, spliced
in after that turn's first turn-end — the "message in the middle of a later
turn" shape this change exists to prevent. (Pi's own `clear_queue` reference says
an abort continues queued messages; in RPC mode on 1.0.0 that is not what
happens. A live probe of the D1/D2 path is what showed it.)

So a run's end must not leave text parked: on a settlement that still shows a
held message the driver clears the queue and hands the cleared text back to the
app — the same retraction a stop performs — and settles at once. Holding the turn
open instead would hang a stopped turn until the user typed something, which is
worse than the splice it was meant to prevent, and re-running the text would
start a turn behind a stop the user asked for. The decision to clear is made from
the provider's own queue report (D6), so a message the provider already delivered
at its boundary is never touched.

**D3 — Settlement never depends on the prompt answer.** A prompt submitted while
Pi is emitting `agent_settled` is deferred and **no response is ever written**
for it, and a failing prompt's error arrives through the same response path. So
the state machine settles on `agent_settled` (already the case) and uses the
answer only to learn `handled`. A refused prompt settles the turn as a delivery
failure — and the app must present that as the message not being delivered, not
as an assistant reply carrying a raw provider string.

**D4 — A steer is only offered to a live run.** The driver gates on its own
`run_started`; a steer that arrives after the run settled is sent as a prompt
instead. `SteerAccepted` means "accepted into the live turn", which is exactly
what Pi's `disposition: queued` supports — not "delivered".

**D5 — Stop clears the queue, then aborts, and does not wait for the abort.**
`clear_queue` returns the removed text, which lets the composer give the user
their stopped message back. The abort is written without a response waiter: its
answer only arrives once the session is idle (routinely past the 10 s control
timeout), and blocking the writer on it would delay the next prompt behind it.
With the queue cleared and the next prompt carrying `followUp`, an abort still
in flight is harmless.

**D6 — `queue_update` is the queue truth.** Pi reports the complete steering and
follow-up queues on every change, so the client's "queued" indicator is the
provider's own list rather than a guess. Providers without such a signal keep the
app's local follow-up queue.

**D7 — A self-started run opens a turn.** Pi joins the providers allowed to
start a turn with no prompt. The guard stays: only `agent_start`/`turn_start`
open a turn, so a custom message that arrives without triggering a run can never
fabricate one.

**D8 — Extension messages get homes, not a generic dump.**
`customType` classifies the message: pi-subagents' child and background
notifications (`subagent-incremental-child-notify`, `subagent-notify`) map to
`DriverEvent::BackgroundWork` (`BackgroundWorkKind::Subagent`) — the surface that
already exists for "work that outlives the turn", which is what a workflow child
is — and everything else becomes a system line in the transcript, the same shape
Claude and Codex notices take. A message marked `display: false` is stored but
never rendered, so the local projection keeps matching the provider's session
tree (Pi hides those in its own TUI; dropping them outright would make the tree
diverge).

**D9 — Extension UI records map onto existing app surfaces.** `notify` becomes
the app's notice/toast channel with its `notifyType`; `setStatus` a per-session
status line; `set_editor_text` fills the composer; `setTitle` the window title;
`setWidget` a strip beside the composer, which is where the provider itself puts
a widget (`aboveEditor`/`belowEditor`) and where the client's own queued-message
card already lives. It does not belong on the detached-work surface: those
entries are provider work with a key, a status, a stop affordance and a part in
a parked turn's decisions, and a block of text an extension keeps and clears
would masquerade as running work. Dialogs become answerable requests with the
ordinary `cancel` response as the dismissal, and an unanswered dialog is bounded
by the timeout Pi itself sends when it has one. What stays cancelled is what Pi's
RPC cannot carry at all (`custom`, `onTerminalInput`).

**D10 — Verification is a live process, not a fixture.** The driver's live tests
already spawn a real `pi --mode rpc` (`pi_context_usage_against_the_real_rpc`), so
the new behaviour is covered the same way: a busy session that must accept a
queued prompt and one that must not lose it, a custom message that must be
classified, and an extension UI request that must be answered. The end-to-end
acceptance run drives `npm:pi-subagents`: one background workflow whose child
completion must wake the parent and appear.

## Risks / Trade-offs

- **Queueing changes what "sent" means.** A prompt sent while the agent is
  streaming is now answered later, in the same turn, instead of failing. If
  `clear_queue` is skipped, a message the user stopped would still run; D5 makes
  clearing and aborting one action.
- **A self-started turn can surface unexpected output.** If Pi starts a run Waku
  did not ask for, the transcript gains a turn with no user message. The app
  already models that (Codex goal continuation), including `TurnParked` for a
  reply that ends with detached work still running, so the risk is contained to
  Pi's own map of custom messages.
- **Dialogs can hold the provider.** `select`/`confirm`/`input`/`editor` block
  Pi's extension until they are answered. Auto-cancelling was wrong, but
  answering only in the UI risks an unnoticed open dialog; the driver bounds it
  with the timeout Pi supplies and surfaces the request prominently.
- **Live tests need a credentialed Pi and the extension installed.** They must
  stay runnable without either — the existing live tests in `pi.rs` skip rather
  than fail when the binary is absent, and the pi-subagents acceptance run is
  gated the same way.

## Migration Plan

No wire break is intended. If a new driver event is needed for a surface that has
no home, it is added with a default on decode (`bun run protocol:generate`
refreshes the TypeScript bindings; `bun run protocol:check` gates them), and
older clients keep their behaviour.

Order of landing:

1. Inbound delivery (D1–D3) — removes the loss, and is the prerequisite for
   everything else being trustworthy.
2. Queues, steering and stop (D4–D6).
3. Agent-initiated runs and custom messages (D7–D8).
4. Extension UI surfaces (D9).
5. Verification (D10), `docs/providers.md`, `CHANGELOG.md`, and the archive.

## Open Questions

- Does a `handled` prompt deserve a transcript row? Pi sends nothing to the model
  and the extension command usually prints its own output, so the current
  assumption is "no row, settle the turn".
- Should Pi's background children share the right-panel surface Claude's
  background tasks use, or should a subagent workflow get its own grouping? The
  first is the smaller change; the grouping question is deferred until
  pi-subagents' fleet view is the acceptance case.
