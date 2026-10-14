# Linux native window-close verification

Fixes [#73](https://github.com/Plintash/doki/issues/73). Product source:
`5a09f9bcbf3da8ffd005e923f1acf0fc588e8d89` (tree
`b489410262f6a73eff551ed00c2c6897e85bd9e5`), based on main
`9522d3f9fa49fcdf88f82086de9a53b04235172e`.

## Minimal reproduction and cause

On Linux X11, start the native app through `scripts/dev.ts`, wait for its
workspace to hydrate, and click the window manager's titlebar X. The initial
Board is sufficient: no conversation, annotation, agent-information scenario,
or model call is required. This independently reproduced the reported
`RefCell already mutably borrowed` panic on unchanged main. Its native identity
was build `babf9da95576dce7-18de4c9e8b919412` in the isolated teardown checkout.

In the unchanged pinned GPUI revision
`4d2f3b3f3a8bb1f892c9c5c73e07047a9b3c61db`, X11
`client.rs:821` invokes `window.should_close()` while holding a mutable borrow
of the client state. Doki's synchronous hook reaches
`request_window_close -> persist_window_state -> capture_window_state ->
window.display -> X11Client::displays`, whose borrow at `client.rs:1546`
panics. The should-close callback storage is also borrowed during that hook.

The Linux hook now returns false and schedules the existing close decision on
the foreground executor. The task uses the original window context and a weak
root handle, then runs the same unsaved-editor guard and frame persistence
before explicit removal. Ordinary `defer` is insufficient because it can flush
inside the same platform callback. Pending requests coalesce; requests while
the confirmation is already open are ignored so they cannot outlive Cancel.
The non-Linux native close answer and the GPUI dependency pin are unchanged.

## Verification

All final product checks used the source above, with no intervening product
edits:

- Full serial application suite: **637 passed, 2 pre-existing ignored**
- Debug and release `cargo check --locked --offline --workspace`: passed
- `cargo fmt --all -- --check` and `git diff --check`: passed
- Native watcher build and fresh executor identity: verified
- Independent Standards/Fowler and Specification reviews: no actionable findings

New GPUI tests cover callback deferral, repeated requests, connecting/failed
windows, and a queued close that must not remove a replacement window. Existing
unsaved-editor guard tests still pass. These mock-platform tests protect
scheduling and lifetime; the real X11 before/after run establishes the original
platform failure and its correction.

Native acceptance on build `7b2e138c8a781576-18de4d18bfa650d8`:

1. Clean hydrated Board: move the native window, then click titlebar X. The app
   and watcher exit **0**, with no panic. The saved floating frame is exactly
   the observed client frame: x 5, y 109, width 1280, height 860.
2. Relaunch the same product build. Open the fixture's Atlas docs README and
   append an unsaved marker. Titlebar X presents the existing confirmation.
3. Click titlebar X twice while that confirmation is open, then click Cancel.
   The dialog stays dismissed, the editor text is retained byte-for-byte, and
   keyboard focus returns to the editor. The on-disk file hash is unchanged.
4. Open Settings with that dirty editor behind it. Titlebar X still presents
   the confirmation; Escape dismisses it and leaves Settings open.
5. Request close again and choose Discard and Close. The app and watcher exit
   **0**, with no panic; the file's original hash remains unchanged. Both clean
   and discard runs remove the runner's live marker and stop its owned fixture
   daemon after the app exits.

The watcher propagates the app's exit code (`scripts/dev.ts`); the fixture
records that code rather than inferring success from the window disappearing.
Screenshots establish the confirmation and dismissal surfaces; semantic reads
establish exact editor text and focus, and disk reads establish frame/file
persistence. The fixture uses disposable data, software Vulkan, and disabled
provider executables. No paid model was invoked.

## Evidence and boundaries

Cloud QA artifacts are retained under `/workspace/shared/doki-toolchains/`:
`teardown-baseline-watcher.log`, baseline/final/reopened describe JSON,
`teardown-app-tests.log`, workspace/release/format logs,
`teardown-clean-close-{watcher.log,exit.json,state.json}`,
`teardown-observed-frame.json`, dirty/cancel tree JSON and the after-cancel
snapshot, `teardown-dirty-settings-confirmation.png`,
`teardown-dirty-close-{watcher.log,exit.json}`, and
`teardown-final-acceptance.json`. These are local evidence paths, not shared
artifact URLs.

The first harness startup encountered a stale protocol-18 daemon from another
completed worktree. It was stopped and restarted with this checkout's matching
protocol-17 binary before either reproduction or acceptance. No daemon protocol
change belongs to this fix.

This is Linux X11 debug-native acceptance. Release was checked, not launched;
macOS, Windows, and Wayland were not exercised. No macOS impact is established.
CI remains intentionally deferred. This change neither updates GPUI nor claims
to fix the upstream callback's borrowing contract for other applications.
