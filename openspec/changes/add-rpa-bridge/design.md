# Design

## Context

See `proposal.md` for motivation and the two specs for the behavior contract.
What shapes the approach:

- The desktop app is a GPUI client of a per-checkout daemon. The window and all
  view state live in the app process; sessions, providers and persistence live
  in the daemon. UI truth is app-side, so a UI surface must live there too.
- Our GPUI fork already carries three automation taps, validated by
  `tests/rpa_spike.rs` on this branch: a window can keep its AccessKit tree
  built without an assistive-technology client (never sending updates to the
  platform), read that tree back with stable `NodeId`s, bounds, focus and
  element provenance, and dispatch an accessibility action down the same path
  a screen reader request takes.
- GPUI's element model already requires ids for stateful elements and its
  accessibility layer already derives roles, labels, values, states and
  actions from them. `div().on_click(...)` registers a `Click` action, and
  `track_focus(...)` registers `Focus`, so dispatch reaches real handlers.
- The agent-facing `waku_js_repl` MCP server, with a persistent QuickJS kernel,
  is already attached to several providers by the daemon-side driver config;
  today that attachment is tied to the Computer Use flag.
- The app and the daemon compute the same per-instance data directory (per
  checkout in debug builds), which is the natural place for a discovery
  publication.

## Goals / Non-Goals

**Goals:**

- One semantic surface for what is on screen and what can be operated, kept
  honest by rendering: only laid-out elements, real state, real bounds.
- One local channel that can observe and operate the app, using the same code
  paths as real input.
- One observability system for all processes, good enough to explain failures
  without editing code to add logging.
- Conventions and enforcement that keep coverage growing with the app instead
  of drifting.

**Non-Goals:**

- No release surface. Release builds expose no endpoint, no bindings, no
  diagnostic files, no tree-for-automation mode.
- No arbitrary code execution or state injection inside the app. The app
  serves declarative verbs; scripting stays in the agent's REPL.
- No remote or cross-machine access; the endpoint is loopback-only.
- No deterministic scripted provider in this change. Waits are event-based so
  a provider substitute can arrive later without changing the surface.
- No change to screen-reader behavior; the accessibility tree gains roles and
  labels because instrumentation is generally useful, but VoiceOver quality
  tuning is separate work.

## Decisions

### D1 — Substrate: GPUI's accessibility tree, not a parallel registry

Instrument interactive elements with standard GPUI accessibility attributes
(`id`, `role`, `aria_label`/values) and treat the per-frame AccessKit tree as
the automation surface, dispatching actions through the accessibility action
path.

- Why: it is the only element registry GPUI already maintains; ids obey the
  framework's stability rules; click/focus actions are derived automatically;
  the dispatch path is the one a screen reader uses and falls back to real
  synthesized input at the node's bounds; bounds, focus and disabled/selected
  states come from the same frame. The instrumentation also advances the
  accessibility work AGENTS.md already treats as a product requirement.
- Alternatives considered: a Waku-owned map of control id → handler
  (duplicates accessibility, and every new control needs two registrations);
  Computer Use over pixels/AX (permissions, brittle, blind to app state);
  coordinate scripting (rejected outright).

### D2 — Own the fork, keep the taps minimal

The three additions live as one commit on `Plintash/zed`, branch `waku-rpa`,
which replaces the previously pinned `egoist/zed` branch in `Cargo.toml`.
Future patch work happens in a local clone (`~/proj/zed-rpa`) and is pushed
there; the branch is rebased onto upstream as needed.

- Why: `egoist/zed` is not under our control; a fork we own lets us add the
  few generic accessors without waiting, and the commit is written to be
  upstreamable (no Waku-specific policy).
- Fork surface: `Window::set_a11y_automation(bool)`,
  `Window::a11y_automation_snapshot()` (stable node ids, bounds, focus,
  provenance), `Window::dispatch_a11y_action(...)`, plus the draw-loop rule
  that an automation-built tree is never sent to the platform adapter. A
  debug-only offscreen `render_to_image` exposure is added for screenshots.
- Alternatives considered: vendoring gpui into the workspace (heavy, no
  easier to update); a Waku-maintained wrapper crate that re-implements the
  taps (impossible — the state is inside `Window`).

### D3 — The control endpoint lives in the app process

A debug-only server owned by the app serves the bridge. UI requests are
applied on the UI thread; socket I/O, decoding and encoding run off it.

- Why: only the app can read view state or run a click handler; the daemon is
  headless and shared across windows.
- Threading: a background accept/read thread feeds a channel; a `cx.spawn`
  task drains it on the foreground executor and applies one request per frame
  turn, mirroring the existing driver event pump pattern. No blocking I/O and
  no synchronous IPC from a frame.

### D4 — Transport: loopback TCP, bearer token, newline-delimited JSON

The endpoint binds `127.0.0.1` on an ephemeral port, requires a random bearer
token on every request, and speaks one JSON object per line.

- Why: it matches the daemon's existing localhost+token convention, is trivial
  to speak from Rust, JS and shell, and needs no dependency.
- Alternatives considered: a Unix socket (awkward on Windows, no benefit
  here); WebSocket (no browser client exists; the daemon's server machinery
  lives in `waku-core`, which the app does not depend on); a deterministic
  port (collides between checkouts).

### D5 — Discovery: an instance file, handed to tooling through the daemon

On startup a debug app writes `<data_dir>/rpa.json` with `{address, token,
pid, checkout, database, build}`; it is removed on exit. The daemon passes
`WAKU_RPA_CONFIG=<path>` (and the path may be overridden by
`WAKU_RPA_ENDPOINT`/`WAKU_RPA_TOKEN` for tests) to every provider process it
launches, in the same places it already passes the Computer Use server paths;
the REPL and the CLI read it lazily, check the pid is alive, and prefer the
explicit environment when present.

- Why: no wire-protocol change; works when the daemon connection is down; the
  file is inspectable while debugging discovery itself; the daemon already
  knows the same data directory and the executable path.
- Alternatives considered: registering the endpoint over the daemon protocol
  (protocol churn and replay semantics for a debug-only concern); a fixed
  port (see D4); requiring the agent to locate the checkout (would leak
  setup work into every scenario).

### D6 — The agent surface is the existing REPL, decoupled from Computer Use

`waku_js_repl` registers with providers whenever a debug build launches them,
whether or not Computer Use is enabled; the Cua bridge inside it stays
Computer-Use-only. The bootstrap exposes `waku.rpa.*` over one native call,
and the tool description documents the surface. `bun scripts/rpa.ts` provides
the same verbs for humans and for providers without an MCP server (for
example Claude sessions today).

- Why: the REPL already exists in the sessions where features are developed,
  it persists across calls, and it can turn images into tool output; a second
  mechanism would be redundant. CLI parity keeps non-MCP providers and
  terminal-driven work first-class.
- Alternatives considered: a new MCP server dedicated to RPA (duplicate
  lifecycle, second kernel); shell-only tooling (higher friction inside agent
  sessions).

### D7 — Protocol: few verbs, ordered batches, structured errors

Requests (`describe`, `snapshot`, `act`, `invoke`, `wait`, `run`, `shot`,
`logs`, `timeline`, `counters`, `verbosity`) sit under one versioned envelope.
`run` executes an ordered batch as a foreground state machine that yields to
the frame loop between steps, stops at the first failure, and returns per-step
results plus a final snapshot. Errors carry a code, the offending target, the
nearest known ids, and the observed state.

- Why: one call should carry a whole interaction; agents should never need to
  poll or sleep; a failure should be diagnosable from its own response.
- Targets are node handles (from the last snapshot) or semantic ids resolved
  against the current tree. Node handles are scoped to a snapshot: the client
  sends ids and a snapshot generation, and the bridge refuses stale handles
  rather than acting on a recycled node.
- Alternatives considered: a generic `eval` in the app (unbounded surface,
  untestable); one step per request (chatty, and waits would have to live
  client-side, where they cannot see frames).

### D8 — Snapshot composition: tree from the frame, state from the views

`snapshot` returns the frame's accessibility tree (nodes with handles, ids,
roles, labels/values, states, bounds), an app-state section assembled from
live view state per pane (instance, window, route, tasks, session, composer,
panels, toasts, permissions), and a health summary. It never reads the
database, and never invents state that is not on screen; long content is not
embedded — the tree carries what is rendered, and message bodies are not
duplicated into the state section.

- Why: a derived snapshot cannot go stale the way a hand-maintained mirror
  would; the tree is already honest about virtualization; keeping bodies out
  bounds payload size during streaming.
- Ownership: each pane owns its state section, added where the pane already
  builds its view state, so a feature that adds observable state adds it in
  one place.

### D9 — Instrumentation conventions, enforced

Ids follow `area.control[.<domain-id>]` (`composer.send`,
`sidebar.task.<uuid>`, `settings.page.computer_use`); static ids used by
scenarios are declared as constants in one module so the scenario and the view
cannot drift apart. Shared primitives (`icon_button`, `MenuChip`,
`toggle_switch`, list rows, the text fields, menu items) carry roles and
labels so call sites inherit them. In debug builds the fork logs once per
element when an interactive element with no accessibility role is built while
automation or assistive technology is active, and the repository keeps
`scenarios/smoke.mjs`, which fails when a pinned core id disappears.

- Why: coverage is cheap only if it comes from the primitives; the warning
  makes a missing identity visible at the moment the feature runs, and the
  smoke scenario makes an accidental removal fail a committed check.
- Alternatives considered: a lint over source text (cannot see dynamic ids);
  review-only (drifts).

### D10 — Diagnostics: `tracing` plus one shared recorder

A new `waku-diagnostics` crate owns the event schema, a bounded in-memory
ring, a rotating NDJSON writer on a background thread, a runtime-reloadable
level/target filter, and the query API. The app, the daemon and the driver
layer emit `tracing` events and spans (already in the dependency graph via
GPUI) with correlation fields; provider stderr lines are captured as events
attributed to their process.

- Why: `tracing` gives spans, structured fields and a reloadable filter
  without inventing a logging framework; a shared crate keeps app and daemon
  records identical.
- Queries: the app-side bridge merges its own ring with the daemon's and
  providers' files, so no daemon protocol change is needed. If that proves
  insufficient, a debug-only daemon command is the follow-up.
- Verbosity: the app applies a reload handle; the daemon polls a small filter
  file under the data directory (no protocol, survives daemon restarts).
- Bodies: events carry ids, states and timings; prompt/message bodies are
  recorded only at debug level, keeping default records low-volume and free of
  user content.

### D11 — Health and failure evidence are part of the surface

Every snapshot includes the health summary. Failed waits, actions, batches and
in-process UI tests attach a failure dump: observed state, tree diff since the
batch began, relevant events (filtered by the session/turn/source involved)
and counters.

- Why: the answer to "something is wrong" should not require knowing which
  query to run; it should be attached to the thing that failed.

### D12 — Gating and safety

All of it is `debug_assertions`-only and clamped the way Computer Use is: the
endpoint is compiled out of release builds (not merely disabled), the REPL
only exposes the bindings in debug builds, and discovery is never written by a
release process. The token file is user-only permissions, the endpoint is
loopback, and requests can only reach the surfaces the bridge declares.

## Risks / Trade-offs

- **Instrumentation coverage drifts as features land.** → Shared primitives do
  most of the work; the debug warning surfaces gaps the moment a feature runs;
  the smoke scenario pins core controls; AGENTS.md makes identity a review
  condition.
- **Fork maintenance.** → Exactly one isolated commit, written to be
  upstreamable, rebased onto upstream when the fork is synced; the spike test
  fails loudly if a rebase drops a tap.
- **Accessibility-tree cost while attached.** → The tree exists only while a
  client is attached; expensive label computation is gated on
  `Window::is_a11y_active`, the documented GPUI idiom; a counters check lives
  in the smoke scenario.
- **Snapshot payloads grow with streaming transcripts.** → The tree reports
  rendered nodes only, and bodies stay out of the state section.
- **Two debug app instances of one checkout.** → Discovery is last-writer-wins;
  responses name pid, build and database, and clients verify them before
  acting.
- **Protocol drift between REPL and app.** → One version constant in the
  envelope; both ends refuse a mismatched major version with the expected one
  in the error.
- **Diagnostics in the hot path.** → Records are enqueued to a ring and a
  writer thread; no formatting on the UI thread beyond the enabled check.

## Migration Plan

None required: the surface is new, debug-only, and nothing in the app depends
on it at runtime. Rollback is reverting the change; the fork dependency can
stay or be repinned independently of app behavior.

## Open Questions

None blocking. The scripted provider for fully deterministic end-to-end
scenarios is deliberately deferred; if it arrives, it plugs into the existing
waits and verbs without changing the specs.
