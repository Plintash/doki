# Flooded PTY drop: isolated teardown-order candidate

This branch tests a minimal teardown-order candidate for the intermittent
direct-drop failure carried by PR95. It must not be merged with its diagnostic
replacement for the ordinary CI workflow. PR95 and PR96 stay held pending
candidate verification.

## Observed failures

- macOS pull-request job [115998515196](https://github.com/Plintash/doki/actions/runs/38606359447/job/115998515196)
- macOS push job [115998699489](https://github.com/Plintash/doki/actions/runs/38606353229/job/115998699489)

Both report `terminal drop did not finish` from the flooding process-group
fixture. The attach-boundary test passes in the newer runs. Idle, exited-shell,
and HUP-ignoring non-flooded teardown controls pass. No drop-worker panic is
logged, but the old `recv_timeout(...).is_ok()` discarded the distinction
between timeout and channel disconnection.

## Captured hosted result

The baseline diagnostic [job 116026402067](https://github.com/Plintash/doki/actions/runs/38614620073/job/116026402067)
failed on its first isolated flood attempt. The worker began promptly; HUP and
KILL both succeeded; the reader joined without panic after about 253 ms of
teardown. All 81 samples of the drop worker were in Alacritty `Pty::drop` ->
`Child::wait` -> `__wait4`, after our own drop body had returned. The trace did
not advance during evidence collection. The shell was still exiting; its
pipeline child was already a zombie. This captures the user-space wait and
rules out worker scheduling, reader join and signal-hook unregister as the
blocked phase for this run.

## Kernel dependency remains inferred

The captured baseline order was: signal HUP, poll the existing 250 ms
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
and does not identify the exact kernel wait inside the captured shell exit.
The user-space `Child::wait` location, unlike the kernel explanation, is now
confirmed by the native sample.

## Evidence collection

All trace state and observation calls are compiled only under `cfg(test)`.
Milestones timestamp worker start, signals and their return values, grace,
reader stop/join, and PTY destruction. The baseline records the implicit-field
teardown boundary; the candidate records explicit PTY destructor entry/return.
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
`diagnostic/terminal-drop-drain-order`. It builds core tests, repeats the exact
flood regression 100 times in fresh processes, and stops at the first failure.
If all pass, it runs neighboring server tests with normal runner parallelism.
The independently broken PR95 attach-boundary fixture is excluded from that
second pass; PR96 already carries its repair. All execution is provider-free.
Logs, including emitted native-stack evidence, are uploaded on success/failure.

## Baseline diagnostic preparation checks

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

Those preparation checks were Linux-only; the subsequent baseline hosted
failure and stack capture are recorded above.

## Minimal candidate

`DaemonTerminal` now stores its sole PTY owner in an `Option`. Drop takes it
exactly once, retains the existing HUP/grace/KILL sequence while the shell PID
still pins the original group, then explicitly destroys the PTY while the
reader is still allowed to drain. Only after that destructor/reap returns does
it set the stop flag and join the reader. No signal is sent after the child is
reaped, and implicit field destruction sees `None`, preventing a second drop.

The diagnostic fixture, its original 3 s verdict, output batching and retention
are unchanged. The exact fixture source blob remains
`3191547f80b7bb5a4dd3b15059676a695b30df63`. The workflow differs only in its
isolated candidate branch trigger.

This is not a general bounded-wait guarantee: a kernel-stuck process or an
already failed reader can still prevent useful progress. The candidate avoids
intentionally stopping a functioning reader before the child wait. Existing
EOF/error handling and one-time reader join remain in place.

### Candidate Linux checks

- Core test build and non-test core check passed with the established
  constrained-cloud override (`CARGO_PROFILE_DEV_OPT_LEVEL=0`, one build job)
- 29 terminal-related tests passed
- 56 server tests passed, one opt-in real-Pi test ignored, both serialized and
  with normal parallelism; Linux runs included the attach-boundary fixture
- 100 exact flood repetitions passed; each trace showed explicit PTY destruction
  returning before the reader stop/join. Total process wall time ranged from
  269 to 282 ms, median 275 ms
- Formatting, whitespace, workflow YAML and embedded Python syntax checks passed
- Independent Spec and Standards reviews cleared the candidate scope and code

Native candidate validation is still pending. These Linux passes do not
establish macOS success or authorize replacing the held production PRs.
