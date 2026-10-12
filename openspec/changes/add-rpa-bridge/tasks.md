# Tasks

## 1. GPUI fork foundation

- [x] 1.1 Add the automation taps to our fork (`Plintash/zed#waku-rpa`): forced tree building without a screen reader, a snapshot with stable node ids, bounds and provenance, and public action dispatch; switch the workspace off `egoist/zed`. Verified by `cargo test -p waku --test rpa_spike` and the full `cargo test -p waku` suite passing against the fork.
- [ ] 1.2 Expose a debug-only offscreen frame capture in the fork (`render_to_image` outside `test-support`), and verify with a test that a window's frame renders to a non-empty PNG.
- [ ] 1.3 Add a debug-only warning in the fork when an interactive element with click or focus handlers is built without an accessibility role while a11y is active; verify by a test that the warning fires once per element and stays silent for instrumented elements.
- [ ] 1.4 Rebase the fork branch on the pinned upstream commit and push; verify `cargo test -p waku --test rpa_spike` passes from a clean `Cargo.lock` fetch.

## 2. Bridge endpoint and protocol

- [ ] 2.1 Create the app-side `rpa` module: request/response envelope, protocol version constant, structured error shape (code, target, nearest ids, state), and unit tests for encoding, decoding and version-mismatch refusal.
- [ ] 2.2 Implement instance discovery: write `<data_dir>/rpa.json` (address, token, pid, checkout, database, build) on startup and remove it on exit, with `WAKU_RPA_ENDPOINT`/`WAKU_RPA_TOKEN` overrides; verify by tests for liveness checks, stale files, and override precedence.
- [ ] 2.3 Implement the loopback server: ephemeral port, bearer-token auth, newline-delimited JSON, background socket thread feeding a channel drained on the foreground executor; verify with an integration test that an unauthenticated request is refused and an authenticated request round-trips without blocking frames.
- [ ] 2.4 Wire the tree lifecycle: attach/detach keeps the window's automation tree built only while at least one client is attached, and expose `snapshot` over the tree portion; extend `tests/rpa_spike.rs` to prove detaching stops building and reattaching resumes it.
- [ ] 2.5 Assemble the state portion of `snapshot` (instance, window, route, tasks, active session, composer, panels, toasts, pending permissions) from live view state, merging it with the tree; verify with a test that a constructed app state appears field-for-field and that long message bodies are not embedded.
- [ ] 2.6 Implement `act`: click, focus, set text, key chord and scroll, targeting by node handle or semantic id, with stale-generation refusal and errors naming the target and nearest ids; verify one test per action and one for the error content.
- [ ] 2.7 Implement `invoke` verbs for task create/open, prompt submit/cancel, permission answer, settings page, panel toggles and model selection, each routing through the existing app handler; verify that a verb and its control produce the same state.
- [ ] 2.8 Implement `wait` conditions (node appears/disappears, label/value match, session or turn state, frame drawn, event match) with timeouts; verify resolution without sleeps and that a timed-out wait returns state, tree diff and events.
- [ ] 2.9 Implement `run` as an ordered batch state machine that yields between steps, stops at the first failure, and reports per-step results plus a final snapshot; verify ordering, stop-on-failure and evidence with a batch test.
- [ ] 2.10 Implement `shot` over the fork's offscreen capture, including the disclosure field for windows with native child surfaces; verify a PNG is returned with non-zero pixels.
- [ ] 2.11 Implement `describe` and relaunch awareness: instance/build identity in every response, `waitForRestart` until a new pid/build appears; verify with a test that a stale discovery file is not acted on and a new instance is reported.
- [ ] 2.12 Gate bridge startup behind the debug-only clamp so release builds compile out the endpoint; verify `cargo build --release -p waku` succeeds and write the docs/rpa.md protocol, discovery and troubleshooting sections with commands that run as written.

## 3. Agent-facing surface

- [ ] 3.1 Add `waku.rpa` bindings to the REPL bootstrap over one native bridge call; verify with the existing `js_repl` test module that bindings exist, errors map to actionable messages, and image output works for `shot`.
- [ ] 3.2 Decouple the REPL attachment from Computer Use for debug builds (driver config for Pi, Codex, OpenCode and the other MCP-capable providers) and pass `WAKU_RPA_CONFIG` through to provider processes and the MCP environment; verify with a driver-level test that a debug build with Computer Use disabled still registers the server, and update the REPL tool description.
- [ ] 3.3 Build `bun scripts/rpa.ts` with snapshot/act/wait/run/shot/logs verbs over the same client library; verify each documented command against a live debug app and write the CLI section of docs/rpa.md.

## 4. Instrumentation and conventions

- [ ] 4.1 Add the semantic id convention (`area.control[.<domain-id>]`) with core ids as constants in one module and document it in docs/rpa.md; verify a unit test keeps the constants unique and matching the documented shape.
- [ ] 4.2 Instrument the shared primitives (`icon_button`, `MenuChip`, `toggle_switch`, text fields, menu items, list rows) with roles, labels and values so call sites inherit identity; verify with a test that each primitive produces a node with role and label.
- [ ] 4.3 Instrument the core panes: composer (input/send/model picker), sidebar task rows, transcript rows (ids carry turn and message identity), settings pages, permission and new-project dialogs, command palette and toasts; verify the smoke scenario below finds every pinned id.
- [ ] 4.4 Commit `scenarios/smoke.mjs` pinning the core ids and run it via the CLI against a booted app; verify it fails on a deliberately removed id and passes on the current app.
- [ ] 4.5 Update AGENTS.md with the validation loop (build → wait for watcher → run scenario → read failure dumps → screenshot only when visual) and the rule that a new interactive control ships with identity; verify the steps read as executable instructions with the real commands.

## 5. Diagnostics

- [ ] 5.1 Create the `waku-diagnostics` crate: event schema with correlation fields, bounded in-memory ring, rotating NDJSON writer on a background thread, and a reloadable level/target filter; verify unit tests for bounds, rotation and filter reload.
- [ ] 5.2 Wire the app: `tracing` subscriber, lifecycle spans for turns (submitted, started, streaming, tools, settled/failed) and bridge requests with task/turn correlation; verify tests that records carry the expected ids and fields.
- [ ] 5.3 Wire the daemon and drivers: emit the same records, capture provider stderr attributed to its process, and poll the runtime filter file; verify a daemon test asserting correlation ids and provider attribution.
- [ ] 5.4 Implement `logs`, `timeline`, `counters` and `verbosity` queries merging app and daemon/provider records by time, level, source and correlation; verify tests for filtering, merged ordering and timeline reconstruction of a settled and a stuck turn.
- [ ] 5.5 Add the health summary to every snapshot and the failure dumps to failed waits, actions, batches and in-process UI tests; verify the failure of each carries state, tree diff, relevant events and counters.

## 6. Integration checks

- [ ] 6.1 Run the end-to-end live check on the dev-watcher build: a scenario opens a task, submits a real prompt, waits for streamed output, asserts session state, and captures a screenshot; verify the documented single command reproduces it.
- [ ] 6.2 Verify release gating on a release build: no discovery file is written, no endpoint listens, and an agent session gets no `waku.rpa` bindings.
- [ ] 6.3 Check the attached-client cost: with a client attached during streaming, counters show the frame and commit cadence within the documented bounds, and the smoke scenario reports the measured numbers.

## Workflow follow-up

- Archive the change after the project's review requirements are satisfied.
- Verify the archived specs and the AGENTS.md instructions agree.
