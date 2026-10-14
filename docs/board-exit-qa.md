# Board detail exit and issue terminology QA

Verified on 2026-10-14. Final code: `9bf39c7`, based on main `8d10896`
(including the searchable attachment picker and dropdown layout fixes).

## Reproduction and fix

The reported recording was inspected frame by frame before changing the code.
Returning to Board displayed Saving at 1.841667 seconds and removed it at
1.891667 seconds: three frames, approximately 50 ms. The filter controls moved
with that transient label. There was no blank-screen transition to animate.
The private recording and its frames are not included in the repository.

The same unchanged-detail Back action reproduced in the cloud native app.
Running moved from x=806 to x=857, and Needs attention from x=896 to x=947:
a 51 px jump. New issue stayed at x=1082.

Back now flushes only a pending editor change. Explicit saves consume the
debounce marker, retiring its timer without losing newer edits. Genuine saving
and failure feedback use a fixed-width slot beside the flexible header spacer.
The full error remains available through the retry control's accessible label,
tooltip, and error toast; its visible Retry label fits all three locales.
No animation or delayed navigation was introduced.

Board-facing creation, grouping, action, and hint text consistently uses Issue
in English and Chinese and 課題 in Japanese. Internal task identifiers and
shared standalone-session terminology are unchanged.

## Native acceptance

The real GPUI app was rebuilt and launched by `scripts/dev.ts` as
`Doki Debug — Board Exit QA`, using an isolated provider-disabled fixture on
the cloud Linux desktop. Final build: `b904b5d4d1f01297-18de59126b834149`.

The checked-in native workflow suite passed all four scenarios with 194
assertions. The new exit scenario covers:

- Repeated unchanged Back, keyboard Back, and cancelling an uncreated issue
  without entering the saving state
- Editing and immediately going Back while the scratch database write lock
  keeps the real save observable; the title is checked in SQLite after success
- Edits during an in-flight save, with the newest title durably preserved
- A retired-debounce unit regression plus a native negative pending-state wait
  after acknowledgment; this is not a count of all SQL writes
- A deliberate save failure, stable filter geometry, and successful Retry
- Returning with the sidebar hidden, focus on New issue, and Enter activation
- Scratch cleanup without changing the working app's data or UI state

On the final build, unchanged Back reports `savePending=false` immediately.
Running remains at x=857, Needs attention at x=947, and New issue at x=1082:
zero displacement. Genuine pending and failed states preserve the same bounds.

Direct app-window light and dark captures were inspected. English Retry,
Chinese 重试, and Japanese 再試行 were also inspected in the failure state;
all fit without overlapping or moving the header controls. The saved fixture
edit survived the watcher rebuild/relaunch used for main integration.
macOS was not exercised in this cloud-only verification. No model was invoked.

The fixture was closed normally after acceptance. Its watcher exited with
code 0; the daemon stopped and the running marker was removed.

## Automated checks

The full product checks passed before main integration (`102d81d`). Only the
upstream dropdown change and native test assertion correction followed, so the
final integration reran the affected app suite and workspace checks rather
than repeating unchanged core tests.

- Final application suite: 658 passed, 2 existing ignored
- Protocol: 117 passed; client: 46 passed
- Core: 733 passed, 25 existing ignored, 9 existing live OpenCode tests excluded
  from this provider-free run
- JavaScript REPL: 26 passed; GPUI RPA integration tests: 2 passed
- TypeScript suites: 400 passed, 1,516 assertions
- Client, web, mobile, and RPA TypeScript checks passed
- Native app/daemon build, debug and release workspace checks, Rust formatting,
  generated protocol type freshness, and whitespace checks passed
- Independent specification and engineering-standards source reviews cleared
  the fix, including the compact localized retry treatment

The first core run stalled in the existing
`server::tests::websocket_terminal_flood_delivers_bounded_batches` test. After
interrupting that run, the exact test passed in 0.13 seconds and the unchanged
full serial core command passed in 44.99 seconds. No extra skip was added.

An initial native harness assertion incorrectly included several RPC and paint
round trips in a 300 ms debounce deadline. It now checks the actual unsaved
state immediately after editing and issues Back without an intervening wait.
The first whole-workflow run also observed startup window geometry settling;
the final four-scenario run on the settled rebuilt app passed.

Reproduction uses a fresh application test profile, so saved desktop window
preferences cannot leak into the GPUI test platform:

```sh
WAKU_RPA_DATA_DIR=/tmp/doki-board-exit-unit-profile \
  cargo test --locked --offline -p waku --lib -- --test-threads=1
cargo check --locked --offline --workspace
cargo check --locked --offline --workspace --release
cargo fmt --all -- --check
```

For native acceptance, follow `docs/rpa.md` to configure the fixture's existing
authenticated daemon connection, then run:

```sh
WAKU_RPA_WORKFLOW_TEST=1 bun test scripts/rpa-workflow-live.test.ts
```
