# Design

## Context

See `proposal.md` — Why. Current state that shapes the approach:

- macOS close is `configure_main_window_close_behavior` → `hide_window`
  (`orderOut`) → return `false`. The fullscreen workaround exists only because
  ordering out a fullscreen window strands an empty Space.
- The daemon already owns tasks, transcripts, provider runtimes and in-flight
  turns, composer drafts, settings, skills, and attachments. `AttachSession`
  already exists for reconnecting to a live runtime.
- Two terminal implementations exist: `src/terminal.rs` (desktop-owned PTY plus
  Alacritty emulation) and `crates/waku-core/src/terminal.rs` (daemon-owned PTY
  streaming base64 events, used by web/mobile). The daemon terminal has no
  scrollback retention and drops output while nobody is attached.
- `src/lib.rs::run` starts the daemon supervisor and loads task state before
  `open_window`; `Waku::new` also loads drafts and task state synchronously.
- Right-panel surfaces are already tracked per task in memory
  (`right_panel_session_states`) but never persisted.
- `on_reopen` only activates an existing window; there is no window factory.

## Goals / Non-Goals

**Goals:**

- One window factory that both first launch and activation use, with the
  daemon-backed services owned at application scope, not window scope.
- A single PTY owner (the daemon) with bounded scrollback and attach replay, so
  closing a window cannot kill a shell.
- An explicit, tested restore/drop policy for the desktop snapshot.
- Startup and reopen latency measured before any optimization, with a recorded
  budget and a regression gate.

**Non-Goals:**

- External control, inter-agent messaging, agent teams or swarms. The daemon
  stays the state owner and the terminal attach protocol is client-agnostic, so
  this work does not preclude them.
- A daemon that outlives the desktop process. The supervisor still reaps the
  daemon with its parent; terminals therefore survive window rebuilds but not
  app restarts.
- Browser restoration or renewal work. The browser surface stays experimental
  and reopened windows never restore it.
- Multiple simultaneous windows. The invariant stays "at most one main window".

## Decisions

### Close destroys the window; activation rebuilds it

Native `[NSWindow close]` handles fullscreen correctly (AppKit tears the Space
down and orders the window out in one step), removes the `willExit`/`didExit`
observers entirely, and matches how single-window macOS apps behave.

Alternatives: keep the hiding design and only fix the fullscreen timing (the
current branch) — rejected because hiding is what forces the workaround and the
extra animation; quitting the process on close — rejected because it loses
Dock/notification continuity and the warm daemon connection.

The one invariant the rebuild must preserve is "no second window": the factory
checks `cx.windows()` and focuses instead of opening when a window already
exists. All cross-window process state (daemon supervisor, updater, notification
delegate) moves to an app-level global so a rebuilt window reattaches to the
same daemon rather than spawning a new one.

### Terminals move to the daemon before close semantics ship

Ordering matters: closing the window with app-owned PTYs would kill running
shells (dev servers, watchers). The daemon PTY already exists, so the phase
order is: unify terminals first, then flip close semantics.

The daemon owns the PTY and a bounded byte ring; clients keep emulation,
selection, search, and scrolling. Attach returns the ring snapshot plus current
size, then delivers live output. A grid-diff protocol (daemon emulates and
sends cell changes) was rejected: it duplicates the client emulator, makes the
daemon own render state, and adds per-frame protocol surface for no user win.

Alternatives: keep the desktop PTY and accept terminal loss on close —
rejected; persist terminal scrollback to disk — rejected, since the process
itself is the valuable part and app restarts already end it.

Output is coalesced in the daemon reader (flush on an interval or byte
threshold, whichever first) and the ring is capped so a flooding producer
cannot grow memory. The client's existing streaming pump cadence is the
template; terminal flushing is bounded the same way.

### The rebuild restores identities, not view state

Two stores back a rebuilt window:

- **Daemon**: tasks, transcripts, drafts, settings, live runtimes, terminals.
  The rebuild issues `LoadTaskState`, `HydrateSession` for the selected task,
  `AttachSession` for a running turn, and `AttachTerminal` for restored
  terminal surfaces.
- **Desktop `state.json`**: window frame/display, panel visibility and widths,
  theme/language/font size, selected project and task, and per-task right-panel
  descriptors (terminal id, file path, diff source).

Explicitly dropped: browser tabs and page state, editor cursor/scroll/selection,
file-tree expansion, transcript scroll position, overlays, and animation state.
Descriptors are restored only when the referenced daemon object still exists;
missing terminals or files simply leave the surface absent.

Alternatives: persist full right-panel state (rejected: the user asked for
trade-offs and the browser is experimental); persist nothing beyond the selected
task (rejected: terminal and file tabs are cheap identities and make reopen feel
continuous).

### Dirty file editors block close

`on_window_should_close` returns `false`, shows the existing overlay pattern
with a discard/cancel choice, and closes the window only on confirm. Buffer
contents are never persisted. GPUI's close path is asynchronous, so the confirm
handler marks the window for removal rather than calling close re-entrantly.

### Startup paints before the backend is ready

`run` builds the window synchronously with the services global in a `Starting`
state; daemon spawn/connect and the initial loads move to a background task and
hydrate the entity with `cx.notify`. Actions that need daemon data are disabled
or show skeleton state until hydration lands. Generation counters, already the
codebase pattern for scans and saves, guard against late results overwriting
newer state and against results arriving after the window is dropped.

### Measurement precedes optimization

A startup trace records process start, window open, first frame, daemon ready,
tasks hydrated, and interactive. The reopen path is measured separately from
cold launch. Budgets are recorded from the baseline (cold launch and rebuild
have different budgets) and enforced by a harness. Only if the profile shows
GPUI window/renderer initialization dominating does a minimal patch to the
pinned `egoist/zed` fork come into scope, carried the same way the existing
`waku-webview` branch is and dropped when upstream lands.

## Risks / Trade-offs

- Terminal over loopback IPC could regress throughput versus an in-process PTY →
  coalesced batching, bounded ring, and a flood scenario in the harness; the
  emulator and renderer stay local, so only bytes cross the boundary.
- A rebuilt window can flash as panes hydrate → skeleton-first layout and
  budgets measured on the interactive moment, not first paint alone.
- The dirty-editor guard races the macOS close animation → return `false`, ask,
  then remove the window asynchronously.
- Losing browser surfaces on close may annoy someone using them → accepted by
  product direction; the surface is marked experimental and is not restored.
- Terminals do not survive an app restart (daemon is parent-owned) → no
  regression versus today, and the future daemon-detach work is explicitly out
  of scope.
- A GPUI fork patch adds maintenance cost → keep it minimal, documented in
  `Cargo.toml`, and upstream it if possible.
- Notification activation with no window must open one; if AppKit delivers the
  response before the window factory is ready, the selected task is applied when
  the window finishes hydrating → keep the notification tag as the requested
  task until hydration.

## Migration Plan

1. **Phase 1 — daemon terminals**: add scrollback retention, attach/replay, and
   coalescing to the daemon terminal; port the desktop right-panel terminal onto
   it. Window behavior is unchanged in this phase, so it can land alone.
2. **Phase 2 — window lifecycle**: window factory and app-scope services; close
   destroys; activation rebuilds; restore snapshot and dirty guard; delete
   `hide_window`'s fullscreen workaround. This is the behavior switch and the
   point where the interim fullscreen branch is abandoned.
3. **Phase 3 — latency**: startup/reopen instrumentation, budgets, harness, and
   the GPUI spike only if the profile demands it.

Rollback: each phase is independently revertible; phase 2's behavioral change is
the only user-visible one and is called out in the changelog. The current
`fix/fullscreen-close-black-screen` branch is superseded and should be closed.

## Open Questions

- Exact scrollback cap per terminal (bytes and/or lines) — tune against memory
  and reopen replay cost after Phase 1; the spec only fixes "bounded".
- Whether a notification click delivered after the app was fully quit restores
  the referenced task or lands on the last selected task — verify against GPUI's
  reopen ordering during Phase 2 and record the actual behavior.
