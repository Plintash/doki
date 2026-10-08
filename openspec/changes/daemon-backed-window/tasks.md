# Tasks

## 1. Daemon terminal retention

- [x] 1.1 Add terminal attach/replay to the wire protocol: an `AttachTerminal` command returning the retained output snapshot and current size, plus tests that the generated bindings round-trip the new fields
- [x] 1.2 Add a bounded output ring to the daemon terminal with tests proving drops only happen past the cap and memory stays bounded
- [x] 1.3 Coalesce daemon terminal output into bounded batches (interval or byte threshold) with tests that a fast producer yields far fewer events than writes
- [x] 1.4 Replay the retained ring on attach, including at least one test that a client attaching after output reconstructs the same bytes
- [x] 1.5 Reject writes from a client bound to a superseded runtime with a test covering the replacement path
- [ ] 1.6 Verify terminal lifecycle: explicit close terminates the shell, task removal disposes its terminals, and a detached terminal keeps running — via daemon unit/integration tests

## 2. Desktop terminal on the daemon stream

- [ ] 2.1 Add a desktop terminal session that attaches to the daemon terminal, feeds replayed bytes into the existing Alacritty emulator, and forwards input and resize; verify with daemon-backed tests for attach, input, and resize
- [ ] 2.2 Switch the right-panel terminal surface to the daemon session and delete the desktop-owned PTY spawn path; verify a shell survives a simulated window teardown and reattaches with its grid intact
- [ ] 2.3 Persist the terminal identity in the right-panel surface descriptor and restore the surface when that daemon terminal still exists, degrading to no surface when it does not — with serialization tests for both cases
- [ ] 2.4 Document the terminal output cadence and batching rule in `docs/performance.md`, verified by re-running the flood scenario and confirming the client stays responsive

## 3. Window factory and app-scope services

- [x] 3.1 Move the daemon supervisor, updater, and notification handling out of window-scoped state into an application global; verify the app still builds and existing startup tests pass
- [x] 3.2 Extract a reusable main-window opener from `run` that builds the window, creates the `Waku` entity, and restores the persisted placement; verify a normal launch is indistinguishable from today
- [x] 3.3 Route `on_reopen` and notification activation through the opener with a single-window guard; verify Dock activation with a window open focuses it and creates no second window

## 4. Close, rebuild, and restore policy

- [ ] 4.1 Remove the macOS hide-on-close path so Cmd-W closes the window through AppKit, and delete the fullscreen `orderOut` workaround; verify the committed fullscreen black-screen reproduction no longer occurs
- [ ] 4.2 Extend persisted desktop state with the selected project/task and per-task right-panel descriptors, with round-trip tests for the new fields
- [ ] 4.3 Hydrate a rebuilt window from daemon state plus the desktop snapshot, with generation guards for superseded and window-closed loads covered by tests
- [ ] 4.4 Add the unsaved-editor confirmation to the close path, with tests for cancel keeping the window and buffer and confirm discarding them
- [ ] 4.5 Write the restore/drop policy tests: restored task, layout, and terminal; dropped browser, scroll, cursor, and file-tree state
- [ ] 4.6 Update `CHANGELOG.md` with the close/rebuild behavior and verify the release-note extraction picks the entry up

## 5. Startup and reopen latency

- [ ] 5.1 Add startup/reopen milestone tracing behind an opt-in flag and verify a launch and a reopen each emit the full milestone set
- [x] 5.2 Move daemon spawn/connect and initial state loads off the window-open path so the first frame paints skeleton content; verify with a deliberately delayed daemon
- [ ] 5.3 Record cold-launch and reopen baselines in `docs/performance.md` and set the budgets the harness enforces
- [ ] 5.4 Add a repeatable latency harness that launches the debug app and checks the recorded budgets, and verify a deliberately slowed build fails it
- [ ] 5.5 Profile window/renderer initialization; if it dominates the reopen budget, carry the minimal GPUI fork patch and record the profile and decision in the design or performance doc

## 6. Integration validation

- [ ] 6.1 Run the full acceptance pass on the dev-watcher build: fullscreen close, close during streaming, close with a running terminal, reopen restores task/layout/terminal, dirty guard, browser not restored
- [ ] 6.2 Confirm daemon unavailability degrades gracefully (window opens and reports the failure) without a blank or hung frame
- [ ] 6.3 Capture screenshots or a short recording of close, reopen, and the fullscreen case for the pull request

## Workflow follow-up

- Retire the `fix/fullscreen-close-black-screen` branch as superseded.
- Archive the change once the project's review requirements are satisfied.
