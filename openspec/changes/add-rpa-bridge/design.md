# Design

## Context

See `proposal.md` for motivation and the three spec deltas for the behavior
contract. What shapes the approach:

- The app is a client of a per-checkout daemon. The daemon already owns the
  session model, provider drivers, persistence, a versioned wire contract, and
  the long-lived machinery the runtime needs: sequenced events, replay cursors
  and resumption, subscriptions, and two client libraries (Rust and
  TypeScript).
- The app is the only place UI truth and UI execution exist: the window, its
  frame, its accessibility tree, and the handlers behind its controls.
- Our GPUI fork carries the automation taps validated by the spike: forced tree
  building without a screen reader, a snapshot with stable node ids and
  bounds, and public action dispatch. The fork is ours (`Plintash/zed`,
  branch `waku-rpa`).
- The spike taught three hard rules: dispatching input while holding a view
  lease aborts the process; an occluded window has no display link, so a frame
  must be drawn on demand; and a cached view emits no accessibility nodes, so
  the observed frame must be a full refresh.
- The spike also showed the cost of poor observability: every wrong assumption
  was diagnosed by a temporary file probe and a rebuild, not by the system
  explaining itself.

## Goals / Non-Goals

**Goals:**

- One runtime for observing and operating the app, hosted where long-lived
  state belongs: the existing daemon, not a new process.
- One protocol, reused by every client, local or remote.
- The system explains failures from its own records before more surface is
  built.
- The app keeps only UI truth and execution; everything else is daemon-side.

**Non-Goals:**

- No new daemon and no second control plane; the daemon gains
  responsibilities, it does not multiply.
- No release surface. Debug builds only.
- No daemon-side mirror of the UI tree; the executor owns it, the daemon
  caches only what a session cursor needs.
- No headless app harness, scripted provider, scheduling, or distributed
  execution in P0/P1; those are P2 and later.
- No arbitrary evaluation inside the app; the surface stays declarative.

## Decisions

### D1 — The runtime lives in the daemon; the app is its executor

Automation sessions, cursors, subscriptions, waits, scenarios, instance
targeting, and diagnostics aggregation live in the daemon. The app registers as
the UI executor over the connection it already has.

- Why: the daemon is the long-lived per-checkout process; it already carries
  the event/replay/subscription machinery and both client libraries; and it is
  where future responsibilities (scripted providers, scheduling, distributed
  and cross-device execution) belong. Keeping the runtime client-side would
  duplicate all of it and strand it in one tool.
- Alternatives: a client-side session runtime (works for one tool, loses
  aggregation and remote reach); a new daemon (extra lifecycle, version skew,
  a second control plane); app-only (no long-lived state, no future).

### D2 — The daemon's existing machinery is the runtime's foundation

Automation events ride the sequenced, replayable event model; sessions resume
from cursors after reconnects; clients are the existing Rust and TypeScript
daemon clients.

- Why: reconnection, replay, cursor expiry and subscriber limits are already
  implemented and tested. Building a parallel stream would be the same work
  twice, with new failure modes.
- Alternatives: a bespoke side channel (rejected: duplicates replay and auth);
  polling snapshots (rejected: misses transitions, costs frames).

### D3 — Protocol evolution: automation in `waku-protocol`, requests in both directions

Automation calls, executor requests and UI/model/diagnostic events join the
shared wire contract; the daemon gains server-initiated requests to the app
executor. Both client libraries gain the matching handler and APIs.

- Why: one surface for native, web, mobile, local and remote clients; the
  server→client direction is the seam later needed for remote executors and
  daemon-to-daemon dispatch.
- Alternatives: proxying the spike's local TCP endpoint (kept only as a
  temporary debug fallback while P1 lands); a web-socket-free side channel
  (rejected).

### D4 — Split of responsibilities

The daemon owns long-lived state and orchestration; the app owns the frame and
its execution. The daemon does not mirror the tree; it may cache the diffs a
session has not yet consumed. UI frame events are pushed only while at least
one subscription needs them.

- Why: trees are large and frame-local; sessions are small and long-lived.
  Pushing only on demand keeps idle frames free.
- Alternatives: mirroring the tree daemon-side (stale by construction,
  expensive); unbounded pushes (idle cost).

### D5 — State is the serialized model plus explicit UI-only fields

The snapshot's state section serializes the app's model (sessions, turns,
messages, subagent runs) and enumerates the genuinely UI-only fields (route,
composer, panels, toasts). Requests can select a subset.

- Why: the fan-out trial showed a model field is already serializable; the
  hand-written JSON was pure duplication that would rot with every feature.
- Alternatives: hand-written per-pane JSON (rejected); scraping the tree for
  state (rejected: loses identity and precision).

### D6 — Execution model as a contract

Input is dispatched without holding a view lease; any call that needs the
current frame draws it on demand after a full refresh; a call never blocks the
UI thread and never aborts the app. The executor serializes its work.

- Why: the spike crashed the app with a double lease and observed stale trees
  from occluded windows; these are properties of driving a live app, not
  implementation details.
- Alternatives: catching panics (masks the bug); only acting on visible
  windows (fails the moment the user switches away).

### D7 — Diagnostics: one sequenced stream per instance, files as ground truth

Every process records structured, correlated events; the daemon aggregates
them into one ordered, bounded, parseable stream; runtime verbosity adjusts
what is recorded; health is part of every snapshot; failures attach a bundle.

- Why: the spike's debugging cost came from the system not explaining itself.
  Ground-truth files also cover the case where the app or the daemon is hung.
- Alternatives: per-process logs queried separately (the state before this
  change); a logging framework per process with no correlation (rejected).

### D8 — The session contract is cursors, subscriptions and async waits

Clients hold a session with cursors over UI, model and diagnostic streams;
they can ask for what changed; waits are registered and resolved by events,
not client polls; an action answer already carries its effect.

- Why: this is what turns isolated RPC calls into a conversation, and it is
  also what makes waits cheap: the daemon resolves them from events it already
  has.
- Alternatives: polling snapshots (costly, lossy); client-side waits (each
  tool re-implements them, and they cannot see frames).

### D9 — Instance targeting and disposable scratch instances

The runtime lists live instances and targets one per session; it can start a
disposable instance with its own data directory and seeded database for a run
that must not disturb a working app.

- Why: driving a live app mutates its tasks, drafts and window; the trial did
  exactly that to the user's instance. Scratch instances make destructive
  testing safe and repeatable.
- Alternatives: always testing against the working instance (pollutes it);
  a manual second checkout (no lifecycle, no seeding).

### D10 — Phases: explain, drive, automate

**P0 — explain itself.** Diagnostic events from the app reach the daemon; the
daemon aggregates app/daemon/provider records into one sequenced stream with
files, cursors, queries (logs, timeline, counters, verbosity), health, and
failure bundles. Acceptance: any failure is explainable in one command from
cross-process records, with no rebuild and no probe.

**P1 — drive it.** The automation protocol and executor routing land; sessions
with cursors, subscriptions, async waits and act→diff; model-derived state;
CLI and REPL on the shared protocol; instance listing and scratch instances;
scenario runs; instrumentation by primitive and by feature under test, with the
missing-identity warning and the smoke scenario. Acceptance: the subagent
fan-out trial is a committed scenario with zero temporary probes; targeting a
specific checkout is one call; release builds register nothing.

**P2 — more automatic.** Scripted providers and record/replay; a scenario
library that runs unattended; scheduling, distributed execution and
cross-device driving. Each is its own change when reached.

### D11 — Instrumentation strategy

Shared primitives are instrumented once; each feature under test adds its own
identity and, where the model does not already carry it, its state. Debug
builds warn about interactive elements without identity, and a committed smoke
scenario pins the core ids.

- Why: a full 231-site sweep would be speculative; the trial showed most
  coverage comes free from primitives and the model, and the rest is small and
  local.
- Alternatives: a full sweep up front (delayed value, large diff); review-only
  (drifts).

### D12 — Debug-only, remote-ready

The executor and the queries exist only in debug builds. The surface is reached
through the daemon's existing authentication, so an authenticated remote client
gets the same calls as a local one with no second mechanism.

- Why: the shape of cross-device support should not be a later retrofit; the
  same messages, different connection.
- Alternatives: loopback-only (fails the stated direction); a separate remote
  protocol (a second surface to keep consistent).

## Risks / Trade-offs

- **Protocol evolution in both directions.** → One versioned addition with a
  handler in each client library and tests for reconnect/resume; the spike's
  endpoint stays as a temporary fallback until P1's end-to-end test passes.
- **A bridge call can abort the app.** → D6 is a contract with integration
  tests reproducing the lease and occlusion cases.
- **The daemon becomes a single point of failure for automation.** → Files
  remain ground truth for diagnostics; the runtime reports an absent executor
  instead of answering stale state.
- **The daemon accumulates too many responsibilities.** → The runtime keeps
  its own module and spec; P2 responsibilities are separate changes with their
  own contracts.
- **Snapshot payload size.** → Model-derived state plus selectable fields and
  cursor diffs; event payloads are small and bounded.
- **Remote surface security.** → Existing daemon auth and origin rules; the
  surface is debug-only.
- **Scope creep across phases.** → Each phase has an acceptance gate; P0 ships
  alone before P1 starts.

## Migration Plan

The spike's local endpoint and discovery file are retired in P1 once the
daemon-routed executor passes the subagent trial; until then they are a debug
fallback behind an environment flag. The protocol version bumps once, and both
client libraries ship the same version. Release behavior does not change.

## Open Questions

- Where scenarios are authored and run (CLI-hosted runner vs daemon-native
  runs) — P1 starts daemon-owned runs with a thin client runner; refine after
  the trial.
- How a scratch instance is seeded (copied project database vs generated
  fixtures) — deferrable; the runtime only needs "its own data directory".
- Whether unattended CI runs need a headless app mode — revisit in P2 if the
  live debug app is not enough.
