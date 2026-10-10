# Proposal

## Why

The desktop window is still treated as the source of truth for live state: on
macOS, Cmd-W only hides it (`configure_main_window_close_behavior` calls
`hide_window` and returns `false`), because the window holds things the app
believes cannot be rebuilt — in practice, the right panel's in-process
terminals and browsers. Keeping the window alive is what forces the fullscreen
workaround: `orderOut` on a fullscreen `NSWindow` strands an empty black Space,
so hiding has to leave fullscreen first, which makes Cmd-W feel slow.

That trade-off belonged to the pre-daemon Waku. Doki now has a daemon that
already owns the durable parts: tasks, transcripts, live provider runtimes and
in-flight turns, composer drafts, settings, skills, attachments. The only
genuinely live desktop state left is the terminal PTY, and the daemon already
implements a PTY for its web and mobile clients. Once the window can be
rebuilt, closing it should be a native AppKit close — which also deletes the
fullscreen black-screen problem outright, the way Rio and other single-window
apps never see it.

## What Changes

- **BREAKING (behavioral)**: On macOS, Cmd-W and the close button destroy the
  window instead of hiding it. The app stays in the Dock; activating it builds
  a fresh window. Non-macOS behavior is unchanged (last window closes the app).
- Reopening rebuilds the window from daemon state plus a small, explicit
  desktop snapshot. Restoration policy: persist window frame/display, theme and
  layout preferences, panel visibility and widths, selected project and task,
  composer drafts (already daemon-side), and per-task right-panel *descriptors*
  only (terminal ids, file/diff paths). Deliberately not restored: browser
  tabs and page state, editor cursor/scroll/selection, file-tree expansion,
  transcript scroll position, transient overlays and animations. Browser
  restoration is dropped entirely; the surface stays experimental.
- Unsaved file editors block window close with a confirmation, and are never
  silently discarded.
- The desktop's right-panel terminal moves onto the daemon-owned PTY. The
  daemon keeps a bounded output ring and replays it on attach so a reopened
  window restores the terminal grid (emulation, selection, search and scroll
  stay client-side). The duplicate in-process PTY in `src/terminal.rs` is
  retired, leaving the daemon as the single PTY owner.
- The startup path is restructured so the first frame does not wait on daemon
  spawn, connect, or state load: the window opens immediately with skeleton
  content and hydrates when the daemon answers, guarded by a generation
  counter. Closing and reopening a window must feel instant.
- Startup and reopen latency are instrumented and measured before any
  optimization; the budget is set from the baseline. If profiling shows GPUI
  window/renderer initialization dominates, a minimal patch carried on the
  project's GPUI fork is in scope.

## Capabilities

### New Capabilities

- `window-lifecycle`: a disposable desktop window over daemon state — native
  close, activation rebuilds, fullscreen close, single-window/Dock behavior,
  unsaved-work guard, and the explicit restore/drop policy for desktop state
  across a rebuild.
- `daemon-terminals`: daemon-owned terminal PTYs with bounded scrollback and
  attach/replay, shared by every client; the desktop renders and emulates but
  does not own the process.
- `startup-latency`: first paint independent of daemon readiness, a measured
  reopen budget, and the instrumentation that keeps both honest.

### Modified Capabilities

None. Existing specs cover providers and message annotations; no requirement
there changes.

## Impact

- `src/platform.rs`: delete `hide_window`'s fullscreen workaround and the
  macOS hide-on-close callback; add window rebuild helpers.
- `src/lib.rs`: window creation moves into a reusable opener (`run`, reopen,
  notification activation).
- `src/app.rs`: `Waku::new` splits into "open skeleton" and "hydrate from
  daemon"; quit-time saves move to window-close saves where needed.
- `src/terminal.rs`, `crates/waku-core/src/terminal.rs`,
  `crates/waku-protocol/src/protocol.rs`: terminal ownership and the
  attach/replay protocol; new `AttachTerminal` (or equivalent) command.
- `crates/waku-client`, `crates/waku-core/src/daemon.rs`: backend terminal
  lifecycle per session and per runtime.
- `src/app/right_panel.rs`, `src/app/sessions.rs`: right-panel surface
  descriptors gain persistence; dirty-editor close guard.
- `Cargo.toml` only if the GPUI fork needs a carried patch; the existing
  `egoist/zed` `waku-webview` dependency stays the default.
- Interim fix note: the `fix/fullscreen-close-black-screen` branch's
  `willExit`-based hide is superseded by this change; it need not land.
