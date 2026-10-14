# Terminal attach completion fixture

## Requirement and scope

The attach-boundary test verifies that retained replay and subsequent live
output meet at one exact byte sequence, with consecutive numbered lines and
observable live output after the snapshot. Those assertions and their existing
500 ms observation window are unchanged. See `docs/performance.md`, Terminal
output, for this contract.

After those assertions, the test drains its finite producer to a marker so
teardown begins after the shell finishes writing. That drain is not a specified
222 KB throughput benchmark. It now uses the same 30-second bounded completion
allowance as the terminal output-shape and retained-window fixtures. Every
other caller of `terminal_output_until` retains its three-second default. A
missing completion marker still fails; no production code changes.

## macOS evidence before the repair

Diagnostic run: https://github.com/Plintash/doki/actions/runs/38604106050/job/115990457761

- At 3.017 s into the final marker wait, received and retained sequences both
  equaled 132180; the marker was absent from retained output
- At 5.270 s, both sequences equaled 222012 and the marker had arrived
- All observed live byte-sequence continuity assertions held
- The diagnostic deliberately still failed the original three-second criterion

This run establishes eventual contiguous delivery rather than a missing marker
or an observed attach-boundary gap. It does not identify the cause of macOS
producer/PTY throughput or establish a production performance improvement.
Temporary in-process snapshots and extended diagnostic logging are removed
from the repair.

## Verification

Four standalone tests of the exact extracted helper passed: split marker
assembly, completion after the three-second default under an explicit allowance,
unchanged default failure at three seconds, and bounded explicit-timeout failure
when the marker never arrives. These use real channel/base64 dependencies with
minimal event wrappers; they do not replace the full integration test.
Formatting and whitespace checks passed, and independent Spec and Standards
reviews cleared the source change.

Focused macOS repair verification:
https://github.com/Plintash/doki/actions/runs/38606353237/job/115998135556

- Tested remote head: `166f465954bb55d1be601891264c5b5a7223b272`
- Normal optimized test build completed in 9 min 56 s
- Exact attach-boundary test: 1 passed, 0 failed, 773 filtered out, 6.22 s
- Command: `cargo test --locked -p waku-core --lib server::tests::websocket_terminal_attach_boundary_has_no_duplicates_or_gaps -- --exact --nocapture`

The final cleanup removes only the temporary focused workflow and updates this
record; `server.rs` is unchanged from that hosted passing run. The regular
Tests workflow is unchanged. The focused pass does not claim a green full
workspace suite or a production throughput improvement.
