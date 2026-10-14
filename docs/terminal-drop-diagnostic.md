# Flooded PTY drop: diagnostic branch only

This branch captures evidence for the intermittent direct-drop failure carried
by PR95. It is not a production fix and must not be merged as a replacement for
the ordinary CI workflow. PR95 and PR96 stay held pending the evidence.

## Observed failures

- macOS pull-request job [115998515196](https://github.com/Plintash/doki/actions/runs/38606359447/job/115998515196)
- macOS push job [115998699489](https://github.com/Plintash/doki/actions/runs/38606353229/job/115998699489)

Both report `terminal drop did not finish` from the flooding process-group
fixture. The attach-boundary test passes in the newer runs. Idle, exited-shell,
and HUP-ignoring non-flooded teardown controls pass. No drop-worker panic is
logged, but the old `recv_timeout(...).is_ok()` discarded the distinction
between timeout and channel disconnection.

## Candidate requiring phase evidence

The production order remains unchanged: signal HUP, poll the existing 250 ms
grace without reaping, signal KILL, stop/join the reader, then implicitly drop
Alacritty's `Pty`. Alacritty 0.26.0 waits for the child in its destructor before
its master-file field closes.

Apple's published XNU source makes a post-KILL drain dependency plausible:
[session-leader exit](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_exit.c#L2093-L2140)
calls `ttywait` before terminal revocation, and
[`ttywait`](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/tty.c#L1683-L1730)
waits while output remains queued and the terminal stays connected. Stopping
the reader before the child finishes exiting could therefore leave the wait
blocked. This source analysis is not a stack capture from the failing runner
and does not establish that either observed failure followed that path.

## Evidence collection

All trace state and observation calls are compiled only under `cfg(test)`.
Milestones timestamp worker start, signals and their return values, grace,
reader stop/join, and the boundary immediately before implicit field teardown.
The reader's hot path stores its current phase, without printing each read.
The test distinguishes a returned drop panic, disconnected channel, and timeout.
The original 3 s completion result is frozen before diagnostics; late completion
cannot change a failure into a pass.

Only on failed completion, collect fixture/test process state and, on macOS,
a 1 s `/usr/bin/sample` of the test process without elevation. Diagnostic child
processes have 2 s/5 s budgets plus at most 200 ms to reap after termination.
Evidence printed to the test log is capped at 256 KiB per file. Missing tools or
denied sampling retain the phase evidence. No cached fixture PID/PGID is
signaled by these diagnostics; the existing in-group watchdog is unchanged.
The extra timing can affect the race, so a green instrumented run alone does
not disprove the candidate.

The branch-scoped workflow replaces ordinary full-workspace CI only on
`diagnostic/terminal-drop-phases`. It builds core tests, repeats the exact
flood regression 100 times in fresh processes, and stops at the first failure.
If all pass, it runs neighboring server tests with normal runner parallelism.
The independently broken PR95 attach-boundary fixture is excluded from that
second pass; PR96 already carries its repair. All execution is provider-free.
Logs, including emitted native-stack evidence, are uploaded on success/failure.

## Linux preparation checks

- Test-only core build passed with the established constrained-cloud override:
  `CARGO_PROFILE_DEV_OPT_LEVEL=0 CARGO_BUILD_JOBS=1 cargo test --locked --offline -p waku-core --lib --no-run`
- Non-test `cargo check --locked --offline -p waku-core --lib` passed with the same override
- 29 terminal-related tests passed
- 56 server tests passed, one opt-in real-Pi test ignored; the local run included
  the attach-boundary fixture, which passed on Linux
- 100 exact flood repetitions passed in fresh processes: total process wall time
  ranged from 269 to 284 ms, median 274 ms
- The stalled diagnostic-tool test passed in 30 ms, honoring its 20 ms tool budget
- Formatting, diff whitespace, workflow YAML parsing and embedded Python syntax passed
- Independent review cleared the diagnostic after correcting the EIO reader subphase

No native macOS execution occurred in preparation. A successful Linux or
instrumented run does not establish the failed macOS path; hosted evidence is
still pending.
