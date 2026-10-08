# Proposal

## Why

Pi's RPC mode already reports everything a client needs to be a full host, and
Waku listens to about half of it. The half it misses is the half
`npm:pi-subagents` lives in: a parent session that is woken by its own
background children.

The cost is on record. On 2026-09-18 a pi session driven from Doki
(client session `16d28cb3`, provider session `01a0b29d`) was orchestrating
subagent workflows, and both directions failed:

**Inbound.** Every prompt sent while the agent was busy came back
`Agent is already processing. Specify streamingBehavior ('steer' or 'followUp')
to queue the message.` — three times in a row between 16:13 and 16:19. The
client stored that provider error as an **assistant message**, while the
provider's own session log recorded the user's messages as **absent**: the user
was reading their own words in a transcript the agent never saw. The messages
are gone, not delayed.

**Outbound.** The same provider session carries **24 `custom_message` records no
client ever rendered** — 16 `subagent-incremental-child-notify`, 7
`subagent-notify`, 1 `subagent_control_notice` — the "Workflow child
completed/failed" and "Background task completed" notifications that are the
only signal that a background run finished. The user's words at the time were
"我没看到你被唤醒过" ("I never saw you wake up"). Nothing was broken on Pi's
side; the client had no surface for them.

This is not version drift. Pi's RPC surface is byte-identical between 0.99.1 and
1.0.0 (`rpc-types.d.ts`, `docs/json.md` and the RPC command reference are
unchanged), and Waku never read `data.disposition`, `queue_update`, or
`role: "custom"` in the first place. The parked branch `fix/pi-streaming-behavior`
noticed one symptom — a prompt racing the tail of a turn — and answered it with
an unconditional `streamingBehavior: "followUp"`, which the provider ignores
while idle but which also removes any way to retract the message afterwards.

## What Changes

**Inbound — a message is delivered, queued, or visibly refused.**

- A prompt sent while the agent is streaming is queued with the transport's own
  queueing behavior instead of being refused, and a refusal that still happens
  never leaves a user message the provider never saw.
- The prompt answer's `data.disposition` decides the turn: `handled` (an
  extension command consumed it) settles immediately because no run will start,
  `queued` waits for the queue to drain, `started` runs. The
  `data.agentInvoked` test is deleted — Pi never sends that field.
- Settlement no longer depends on receiving a prompt answer at all: Pi answers a
  prompt submitted inside its settle window with nothing.
- A steer is sent only to a live run. Pi queues a steer unconditionally, so one
  that arrives when the agent is idle is parked and spliced into the *next*
  turn's boundary — the "message appears in the middle of a later turn" shape.
- Stop retracts what it can: the provider's queue is cleared before the abort,
  and the abort is not subject to the 10 s control timeout, which Pi's
  idle-waiting abort routinely exceeds.
- The queue the provider reports (`queue_update`) becomes the truth the client
  shows.

**Outbound — work the agent starts on its own is visible.**

- A run Pi starts with no prompt from Waku (an extension waking the session
  through `sendMessage(..., {triggerTurn: true})`) opens a turn and streams like
  any other. Today Pi is missing from the list of providers allowed to do that,
  so the turn is dropped and every delta with it.
- `role: "custom"` session messages are decoded: their `<customType>`, text and
  `display` flag become a transcript row (or a surface with a home, for
  pi-subagents' child notifications), and `display: false` markers are not
  rendered.
- Extension UI records reach the user instead of being auto-cancelled or
  dropped: `notify` (with its `notifyType`), `setStatus`, `setWidget`,
  `setTitle`, `set_editor_text`, and the dialog methods (`select`, `confirm`,
  `input`, `editor`) as answerable requests rather than instant cancellations.

**Verification.** The transport is exercised against a real `pi --mode rpc`
process, and the acceptance path is the product one: `npm:pi-subagents` running
a background workflow whose completion wakes the parent.

## Capabilities

### New Capabilities

- `pi-provider`: what Waku's Pi transport must do — deliver every prompt it
  accepts, settle a turn on the signal the provider actually sends, keep its own
  view of the session identical to the provider's, give agent-initiated runs and
  extension messages a home, and answer extension UI requests instead of
  cancelling them.

### Modified Capabilities

(none)

## Impact

- **Code**: `crates/waku-core/src/driver/pi.rs` (prompt/steer/cancel paths, the
  inbound stream, extension UI records, custom messages);
  `crates/waku-protocol/src/model.rs` and `driver_wire.rs` if the custom-message
  and queue surfaces need new driver events; `src/app/streaming.rs` (the
  agent-initiated-turn allowlist, delivery failures); the app surfaces that
  present queue state and extension status.
- **Docs**: `docs/providers.md` — the Pi section's per-turn, inbound-stream,
  cancel and steer paragraphs, plus a new paragraph on extension surfaces.
- **Supersedes**: the parked `fix/pi-streaming-behavior` commit, whose
  unconditional `followUp` is replaced by disposition-driven delivery.
- **Acceptance**: `npm:pi-subagents` — `/subagents` progress and failures
  visible, a completing background child waking the parent with its
  notification in the transcript, and a prompt sent while the agent is busy
  queued and delivered rather than lost.
