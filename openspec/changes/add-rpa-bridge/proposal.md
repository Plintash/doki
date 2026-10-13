# Proposal

## Why

Every non-trivial feature in the desktop app can currently only be verified by a
human looking at a running window: an agent has to fall back to Computer Use,
reading pixels or the system accessibility tree, which is slow, brittle, and
blind to the app's own state. Feature work cannot be closed out end to end, and
regressions cannot be replayed.

The groundwork exists: our GPUI fork builds a real AccessKit tree every frame,
with roles, labels, values, bounds, focus and per-node actions, plus a dispatch
path that runs the same listeners a screen reader would. What is missing is a
runtime that can observe and operate the app, and make the app explain itself
when something goes wrong.

That runtime belongs in the daemon, not in a new process and not only in the
app. The app is a client of a per-checkout daemon that already owns the
long-lived machinery — sequenced events, replay cursors, subscriptions,
reconnection, and Rust and TypeScript clients. The daemon is also where future
responsibilities land: scripted providers, scenario runs, scheduling,
distributed execution, cross-device clients. The app keeps only what only it
can hold: the UI's truth and its execution.

## What Changes

- **A daemon-hosted automation runtime.** The daemon routes UI calls to the
  connected app executor and keeps the long-lived state: automation sessions,
  cursors over UI/model/diagnostic streams, subscriptions, async waits,
  instance targeting, and scenario runs. The app registers as an executor over
  its existing daemon connection; release builds register nothing.
- **Diagnostics as a daemon hub.** App, daemon, and provider events land in one
  sequenced, correlated stream per instance, with bounded retention, runtime
  verbosity, health summaries, merged queries, a reconstructed turn timeline,
  live counters, and failure bundles attached to failed operations.
- **The driving surface.** Interactive controls gain stable semantic identity
  through the accessibility tree; the executor answers snapshots and actions on
  demand, draws its own frame when occluded, and derives app state from the
  serialized model so a field the model gains is observable without new bridge
  code.
- **Tooling on the shared protocol.** The CLI and the agent REPL bindings are
  clients of the same daemon protocol, so local and remote callers see one
  surface; instance listing and targeting, disposable scratch instances,
  scenario runs, a committed smoke scenario, and the conventions that keep
  coverage from drifting.
- **Phases.** P0 makes the system explain itself; P1 makes it drivable; P2
  makes it more automatic (scripted providers, record/replay, scheduling,
  distributed and cross-device execution). Each phase lands its own tests and
  documentation.

The previous `egoist/zed` dependency already moved to our fork
(`Plintash/zed`, branch `waku-rpa`), which carries the automation taps.

## Capabilities

### New Capabilities

- `automation-runtime`: the daemon's runtime — executor registration and
  routing, automation sessions and cursors, subscriptions and pushed events,
  async waits, instance listing and targeting, disposable scratch instances,
  scenario runs, and debug-only, remote-ready availability.
- `ui-automation`: the app side of the surface — semantic identity for
  controls, the on-demand accessibility tree and execution model, the semantic
  snapshot and actions, high-level verbs, waits, screenshots, the agent
  bindings and CLI, and the conventions that keep coverage growing.
- `app-diagnostics`: structured correlated events from app, daemon, and
  providers; one sequenced, replayable stream per instance; bounded retention
  and runtime verbosity; timeline, counters, merged queries; health; failure
  bundles.

### Modified Capabilities

None.

## Impact

- **`waku-protocol`**: automation messages (client↔daemon calls, daemon→app
  executor requests, UI/model/diagnostic events) join the shared wire contract,
  with a protocol version bump.
- **`waku-client` (Rust) and `packages/waku-client` (TypeScript)**: an incoming
  request handler and automation client APIs, so both existing clients can
  drive and observe.
- **`waku-core` / `waku-daemon`**: the automation runtime, the diagnostics hub,
  executor routing, and the scenario state.
- **`waku` app**: the executor handler replacing the spike's local endpoint,
  the semantic instrumentation, and the UI/model event push.
- **Tooling**: `scripts/rpa.ts` becomes a daemon client; the REPL exposes
  `waku.rpa` over the same protocol; docs and `AGENTS.md` describe the
  validation loop.
- **GPUI fork**: the automation taps stay; the offscreen capture used by
  screenshots comes later.
- **Compatibility**: debug-only surface; release builds neither register an
  executor nor expose queries. No migration is required.
