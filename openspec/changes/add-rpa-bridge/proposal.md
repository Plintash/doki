# Proposal

## Why

Every non-trivial feature in the desktop app can currently only be verified by
a human looking at a running window: an agent has to fall back to Computer Use,
reading pixels or the system accessibility tree, which is slow, brittle, and
blind to the app's own state. There is no way to ask the app what it is showing
or to act on a control by anything other than guessing coordinates, so feature
work cannot be closed out end to end, and regressions cannot be replayed.

The groundwork for a fix already exists: our GPUI fork builds a real
AccessKit tree every frame, with roles, labels, values, bounds, focus and
per-node actions, plus a dispatch path that runs the same listeners a screen
reader would. What is missing is exposing that tree to a local client on
demand, giving the app's controls semantic identities, and making the app's
runtime observable enough to debug what a snapshot cannot explain.

## What Changes

- **Semantic identity for controls.** Interactive elements in the app carry a
  stable id, a role, and a label/value where meaningful; the window's
  accessibility tree becomes the single source of truth for what is on screen
  and what can be operated. Shared primitives are instrumented first so new
  call sites inherit identity by construction.
- **A debug control endpoint inside the app.** A loopback, token-authenticated
  endpoint owned by the debug app serves a semantic snapshot (tree + app
  state + health), semantic actions (click, focus, set text, key, scroll),
  high-level verbs (new task, send, cancel, answer permission, open settings,
  …), waits that synchronize on conditions instead of sleeps, ordered batch
  runs, and offscreen screenshots. Each runs the same app code path the real
  UI runs.
- **An agent-facing surface.** The existing `waku_js_repl` MCP gains `waku.rpa`
  bindings, attached whenever the REPL is attached rather than only for
  Computer Use, plus an equivalent CLI for humans and agents without that
  server. Discovery is per instance: the app publishes its endpoint/token in
  the checkout's data directory, and the provider plumbing hands that location
  to the tooling it launches.
- **Systematic diagnostics.** The app, the daemon, and provider output emit
  structured, correlated events retained in bounded memory and rotating
  NDJSON files; verbosity is adjustable at runtime per target; queries can
  reconstruct a task's or turn's timeline, report counters, and merge the
  sources into one view. Snapshots carry a small health summary, and failures
  attach the relevant state and events instead of timing out blind.
- **Conventions and enforcement.** The id scheme, the event schema, the
  workflow for validating a feature, and the rule that a new interactive
  control ships with semantic identity are documented where the agents and
  humans that follow them will look.

Release builds keep none of this: the endpoint, the bindings and the
tree-for-automation mode are debug-only, clamped the way Computer Use already
is.

## Capabilities

### New Capabilities

- `ui-automation`: semantic identity for interactive controls, the on-demand
  accessibility tree, the debug control endpoint and its actions/waits/batch
  runs/screenshots, the `waku.rpa` bindings and CLI, instance discovery, and
  the naming conventions that keep all of it working as the app grows.
- `app-diagnostics`: structured correlated events across app, daemon and
  providers; bounded retention and runtime verbosity; timeline, counters and
  merged queries; health in the snapshot; failure dumps.

### Modified Capabilities

None.

## Impact

- **GPUI fork** (`Plintash/zed`, branch `waku-rpa`): already carries the three
  automation taps — forced tree building, a snapshot with stable node ids and
  bounds, and public action dispatch — validated by the spike on this branch.
  The fork replaces the previously pinned `egoist/zed` branch.
- **App** (`src/`): a new `rpa` module (protocol, server, client), window and
  view instrumentation, `src/js_repl.rs` bindings, startup wiring, and the
  settings/driver plumbing that attaches the REPL and passes the discovery
  location to agents.
- **Daemon and core** (`crates/waku-core`, `crates/waku-daemon`): diagnostic
  emission points, the NDJSON sink, provider stderr capture, and carrying the
  discovery location into provider environments.
- **Docs**: a new `docs/rpa.md` (or equivalent) plus an `AGENTS.md` section
  describing the validation loop.
- **Tests**: integration tests proving the endpoint and dispatch, a smoke
  scenario pinning the core controls, and unit tests for the protocol and
  snapshot assembly. Existing tests and release behavior are unaffected.
- **Compatibility**: debug builds only; no wire protocol change is planned
  unless diagnostics merging over the daemon connection proves it necessary.
