# Shutdown acknowledgment failure

## Scope and contract

Investigation of [#44](https://github.com/Plintash/doki/issues/44) found a
separate daemon-ownership bug: an authenticated, permitted `Shutdown` must
signal daemon shutdown even when its acknowledgment cannot be delivered.
A denied shutdown must not set that flag. Authentication and the
`allow_shutdown` gate remain unchanged.

The client writes `Shutdown` and immediately leaves its connection loop. The
server previously tried to write `ShuttingDown` before setting the shutdown
flag. A failed write returned from the connection handler early, leaving the
server's accept loop running and its owner waiting in `server.join()`.

The fix accepts the shutdown before attempting the acknowledgment. Write
errors still propagate to the existing connection-error reporting path.
No protocol, timeout, PTY, child process, or output-retention changes are made.

## Deterministic reproduction

The regression uses the real connection handler and a real loopback socket.
After the authenticated handshake, a Ping/Pong barrier drains subscription
setup messages. The fixture shuts down only the server socket's write side,
then sends `Shutdown` through the still-working client-to-server direction.
This deterministically fails the reply without relying on a client reset
(which could discard the shutdown request itself).

Before the fix, the accepted-shutdown test failed with `shutdown == false`;
the denied-shutdown control passed. After the fix, both pass. Test socket reads
and completion waits are bounded so a regression reports a failure rather
than wedging the test process.

## Linux verification, 2026-10-14

Base: `0437fb6e1cc3b00565a4a35021bb712737ea83d6`.
Tested server source blob: `d9c546c9bb2dd37dd3c777ea3d16d2f3c468735f`.

- Both acknowledgment-failure tests: passed
- Terminal-related tests: 24 passed
- Server tests, serialized: 51 passed, 1 opt-in real-Pi test ignored
- `cargo fmt --all -- --check` and `git diff --check`: passed
- 100 baseline coalescing-test runs passed on Linux before the production fix

Build command: `CARGO_PROFILE_DEV_OPT_LEVEL=0 CARGO_BUILD_JOBS=1 cargo test
--locked --offline -p waku-core --lib --no-run`. The usual optimized dev test
build was killed with SIGKILL in the constrained cloud environment. The reduced
optimization build retained debug assertions and succeeded; no source/build
configuration was changed to get it through. Focused binaries ran with an outer
timeout and serialized tests. No provider/model calls or GUI testing occurred.

## Limits

This proves the shutdown-acknowledgment bug and its fix, not the exact blocked
stack of either hosted macOS coalescing-test stall. No macOS execution was
available. The original #44 flooding-pipeline/PTY-destructor problem remains
open; this change must not close that issue or claim a bounded PTY child wait.
The separate #91 attach-boundary failure is also not claimed fixed.
