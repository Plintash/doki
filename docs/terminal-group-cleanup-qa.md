# Terminal-owned process-group cleanup

This follow-up to [#44](https://github.com/Plintash/doki/issues/44) covers
original-process-group cleanup and the captured flooded-terminal teardown
ordering defect. It does not promise a universal bounded child wait.

## Reproduced ownership defect and contract

A terminal's shell can ignore SIGHUP and leave a child in its original process
group. On Linux, the previous destructor killed/reaped only the shell and left
that child running. It also skipped termination entirely if the shell had
already exited. Both cases were reproduced using a `sleep 120` child inheriting
ignored SIGHUP; the orphan remained sleeping with PPID 1 after terminal drop.

Closing the terminal must terminate remaining members of the shell's original
process group, whether its shell is still running or already exited. Signaling
must not race reuse of a reaped shell PID. Output must keep draining through
the existing graceful-termination interval and the final child reap, rather
than stopping the reader while a killed session leader is still exiting.

## Implementation

- Alacritty creates the direct shell using `setsid()`, making its PID the
  original session/process-group ID
- The output reader observes child exit using `waitid(..., WNOWAIT)` rather
  than reaping through Alacritty's child-event channel
- The terminal is the sole PTY owner; the reader holds only its cloned output
  file and the child PID. Drop does not need to acquire a shared PTY lock
- Drop sends SIGHUP to the original shell group, gives the direct shell the
  existing 250 ms grace, then SIGKILLs remaining group members even if the
  shell has already exited
- Drop takes the sole PTY owner from an `Option` and explicitly destroys it
  while the reader can still drain. Alacritty reaps the child before closing
  its master; only afterward is the reader stopped/joined. The emptied owner
  prevents a duplicate destructor, and all group signals precede reaping

A naturally exited shell intentionally remains waitable until the terminal
owner closes. This retains its exit status/PID (a zombie, not a running shell),
which prevents group-ID reuse before the terminal finishes signaling. Final
drop must reap it; a regression test checks both sides of that ownership bound.
Output batching and retained-buffer limits are unchanged.

## Verification

Original baseline: `0437fb6e1cc3b00565a4a35021bb712737ea83d6`.
Final validation is stacked on the separately reviewed PR94 acknowledgment fix,
local commit `d4a08f373585005029d04d91bac461b24a28a608`.
Original process-group-fix source blobs:

- `terminal.rs`: `c7b3bb0c2b58ae08055c23b2d84bb8b7973895c5`
- `server.rs`: `2219f841b8fcc116e8eb59a32ecd1cb272d7048c`

- Before the production change, both active-shell and exited-shell orphan
  regressions failed: the HUP-ignoring child was still running after drop
- After the change, both cases pass
- Regression coverage includes closing an actively flooding `yes x | cat`
  pipeline after receiving at least 64 KiB, and verifying that its child stops
- Exit-event coverage verifies the direct child is still waitable after the
  reader reports exit (polling for waitability because EOF can precede it), and
  is reaped once the terminal closes

Original group-fix Linux results: 28 terminal-related tests passed; 55 server
tests passed, one opt-in real-Pi test ignored; 100 repeated flood-delivery
fixtures passed.
Formatting and diff whitespace checks passed. No full-workspace result is
claimed.

Before stacking PR94, the full server suite timed out after the flood test
logged a reset-by-peer acknowledgment failure. An isolated retry stalled on
its first iteration: process/thread-state inspection found the test waiting,
the server sleeping in a timer, no remaining output-reader thread, and an
already-exited zombie shell. That is consistent with the separately reproduced
shutdown-flag race rather than blocked PTY teardown; it is not a captured stack
or an explanation proven for the earlier hosted macOS stalls. After stacking
the acknowledgment fix, the full server suite and 100 flood retries completed.

The tests use real PTYs, real shells, and bounded completion channels. A
five-second in-group watchdog prevents failures from leaking test children:
it signals its own current process group, never a cached PGID after the terminal
has reaped its leader. Child-state inspection distinguishes a terminated zombie from a
running orphan and fails on unexpected `ps` errors. Independent review caught
and prompted repair of the original fixture's post-reaping PGID cleanup and
the EOF/waitability test race. The repaired source passed all counts above again.
A separate owned-group probe also verified that the watchdog terminates an
orphan after its direct shell has been killed and reaped.

The isolated unchanged-source flood probe completed 30 Linux runs in roughly
254 ms per drop, so it did not reproduce the reported macOS hang. Its separate
orphan probe failed before and succeeded after the candidate. Syscall tracing
was unavailable because the executor denied ptrace; no blocked stack was
captured. At that stage, no native macOS execution or paid provider calls
occurred.

Build uses the existing shared dependencies, with
`CARGO_PROFILE_DEV_OPT_LEVEL=0 CARGO_BUILD_JOBS=1 cargo test --locked --offline
-p waku-core --lib --no-run` to fit the constrained cloud memory. Production
source/build configuration is unchanged by that verification override.

## Captured macOS failure and order-change verification

The isolated [baseline diagnostic](https://github.com/Plintash/doki/actions/runs/38614620073/job/116026402067)
failed on its first flood attempt under the unchanged three-second verdict.
The worker started promptly, HUP/KILL succeeded, and the reader joined without
panic after about 253 ms. All 81 native samples of the drop worker were in
Alacritty `Pty::drop` -> `Child::wait` -> `__wait4`, after the reader had ended.
The shell was still exiting and its pipeline child was a zombie.

That establishes the user-space blocked wait. The precise kernel dependency
remains inferred: Apple's published
[session-leader exit path](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_exit.c#L2093-L2140)
drains its controlling terminal before revocation. The order change keeps a
functioning reader available through Alacritty's final wait instead of stopping
it immediately after SIGKILL delivery.

The independently reviewed [candidate macOS run](https://github.com/Plintash/doki/actions/runs/38618460416/job/116039504396)
passed all 100 exact flood repetitions with the same three-second verdict.
Every trace showed PTY destruction return before reader stop/join. Drop-worker
duration was 258–751 ms (median 299 ms); PTY destruction itself took at most
39 ms. The diagnostic-branch server suite passed 55/55 with one opt-in Pi test
ignored; this includes the diagnostic-tool timeout test and deliberately
excludes the independently repaired attach-boundary fixture.
The candidate was an optimized hosted build and made no model/provider calls.

Diagnostic traces, sampling helpers and the focused workflow stay on separate
historical diagnostic branches. This clean production patch retains the
original group/flood/exit-ownership regressions and ordinary CI unchanged.
Clean production source: `terminal.rs` blob
`d16a734cb63069f68c4260608134c162c3e35e80`; original `server.rs` regression blob
`2219f841b8fcc116e8eb59a32ecd1cb272d7048c` and ordinary CI are unchanged.
Independent Spec and Standards reviews verified equivalence to the successful
candidate's production behavior after removing test-only diagnostics.

Fresh Linux clean-patch checks passed: 28 terminal tests; 55 server tests with
one opt-in Pi test ignored, both serialized and with normal parallelism;
100 exact flood repetitions; test build and non-test core check; formatting
and diff whitespace. The native run above tested the same production behavior
with test-only instrumentation. No full-workspace green result is claimed.

## Remaining limits

This covers the shell's original group. It does not promise termination of
jobs that moved into a separate process group or detached session. It does not
replace Alacritty's unbounded `Child::wait()` and cannot make a kernel-stuck
process reap within a deadline. An already failed reader may no longer drain;
this fix removes the intentional stop before the child's final wait while
the reader is functioning. The captured stack and successful candidate verify this ordering
case, not every possible #44 failure. The separate shutdown-reply race fixed
in PR94 and the attach-boundary failure #91 remain separate changes.
