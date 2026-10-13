# Tasks

## 1. Landed foundation

- [x] 1.1 Add the automation taps to our fork (`Plintash/zed#waku-rpa`) and pin the workspace to that fork; verified by `cargo test -p waku --test rpa_spike` and the full suite against the fork.
- [x] 1.2 Land the spike that proved the loop end to end: an app-local endpoint, snapshot over the frame's tree, semantic actions, and `scripts/rpa.ts`; verified by driving a real Pi turn and the subagent fan-out. P1 reworks this transport onto the daemon; the executor logic and instrumentation carry over.

## 2. P0 — the instance explains itself

- [ ] 2.1 Add the diagnostic event message and per-instance sequence to `waku-protocol`, bump the protocol version, and verify with encode/decode tests plus a resumed-client test that a cursor replays without gaps.
- [ ] 2.2 Emit structured events from the app for turn lifecycle (submitted, started, streaming, tools, settled/failed), actions, and driver events, with task/turn/message correlation; verify tests assert the correlation fields and that default-level records cover a whole turn.
- [ ] 2.3 Aggregate in the daemon: a bounded in-memory stream plus rotating NDJSON files under the instance data directory, fed by the app's events, the daemon's own records, and provider stderr attributed to its process; verify tests for bounds, ordering, attribution, and expiry of old cursors.
- [ ] 2.4 Teach both client libraries (Rust `waku-client` and `packages/waku-client`) to consume the stream, resume from cursors after reconnect, and surface expiry; verify tests reconnect mid-stream and see each record once, in order.
- [ ] 2.5 Add the query surface over the daemon (`logs`, `timeline`, `counters`, `verbosity`) and expose it in `scripts/rpa.ts`; verify tests for filtering, merged ordering, timeline reconstruction of a settled and a stuck turn, and counters that move during a stream.
- [ ] 2.6 Put health in every snapshot and attach a failure bundle — observed state, tree diff, relevant recent events, counters — to failed waits, actions, and calls; verify each failure path's bundle in a test.
- [ ] 2.7 Verify against the real trial: reproduce a subagent fan-out run and explain a deliberately failed wait from one command, with no rebuild and no temporary probe; write the commands into `docs/rpa.md`.

## 3. P1 — drive it

- [ ] 3.1 Add the automation protocol to `waku-protocol`: session/attach, calls, subscriptions, async waits, scenario operations, and the daemon→app executor requests; bump the version and verify tests for routing envelopes and rejection of an absent executor.
- [ ] 3.2 Extend both client libraries with the incoming-request handler and automation APIs; verify Rust and TypeScript tests that a call, a subscription and a resume all work through the same client.
- [ ] 3.3 Move the executor onto the daemon: the app registers on connect with its instance and build identity, answers routed snapshot/act/type/key calls using the spike's execution rules, and retires the discovery endpoint behind a temporary flag; verify an integration test drives the app through the daemon and that the old endpoint is no longer needed.
- [ ] 3.4 Implement runtime sessions: cursors and diffs over UI/model/diagnostic streams, subscriptions with on-demand UI pushes, and async waits resolved by events; verify tests for act→diff, a wait that does not block another call, no UI push while unsubscribed, and two independent clients.
- [ ] 3.5 Derive the snapshot's state from the serialized model plus enumerated UI-only fields, with selectable sections; verify that adding a model field makes it observable without touching the bridge.
- [ ] 3.6 List and target instances from the runtime and the CLI, and add disposable scratch instances with their own data directory and seeded database; verify targeting picks the named checkout and a scratch run leaves the working instance untouched.
- [ ] 3.7 Implement scenario runs in the runtime with ordered steps, abort, results and failure bundles; commit a smoke scenario; verify a failing step stops the run and carries its bundle.
- [ ] 3.8 Build the CLI and the `waku.rpa` REPL bindings on the shared protocol, with reconnect and a wait for a fresh build; verify bindings exist without Computer Use and a watcher relaunch is followed by a reconnect.
- [ ] 3.9 Instrument shared primitives and the features under test with ids, roles and labels, add the debug warning for interactive elements without identity, and pin core ids in the smoke scenario; verify a deliberately removed id fails the smoke run.
- [ ] 3.10 Update `AGENTS.md` and `docs/rpa.md` with the validation loop (build, wait for the watcher, run the scenario, read the failure bundle, screenshot only when needed); verify the documented commands run as written.
- [ ] 3.11 Verify the phase acceptance: the subagent fan-out is a committed scenario with zero temporary probes, a client picks a specific checkout in one call, and a release build registers no executor and exposes no queries.

## 4. Later changes

- Scripted provider scenarios for permission, error and long-stream edge states.
- Record/replay: turn a driven session into a scenario and replay it deterministically.
- A scenario library that runs unattended, including any headless app mode it needs.
- Scheduling, distributed execution and cross-device driving over the same runtime.
- Offscreen screenshots and visual overlays for the cases that must be looked at.

## Workflow follow-up

- Archive the change after the project's review requirements are satisfied.
- Re-run the comfort trial on the new surface and record what still needs to change.
