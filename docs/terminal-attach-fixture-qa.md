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

## Verification status

Four standalone tests of the exact extracted helper passed: split marker
assembly, completion after the three-second default under an explicit allowance,
unchanged default failure at three seconds, and bounded explicit-timeout failure
when the marker never arrives. These use real channel/base64 dependencies with
minimal event wrappers; they do not replace the full integration test.
Formatting and whitespace checks passed, and independent Spec and Standards
reviews cleared the source change.

Hosted verification of the repair is pending. The temporary focused workflow
is retained only while collecting that evidence and must be removed before a
final mergeable patch is recommended. No issue closure or production throughput
fix is claimed on the basis of the diagnostic run alone.
