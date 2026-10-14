# Local automation acceptance

## Scope and source

- Product source checkpoint: `258312ee57cd1f407651319ef505a298d4c2f5f2`
- Recording CLI harness correction: `7ef3bb848220b82563ee97ed85e0d0fa46982ec5`
- Protocol version: 18; recording file format: 1
- Final verified daemon build: `daemon-4986719b51e3905b` (2026-10-14 UTC)
- Linux cloud, provider-free isolated databases and process trees
- No user schedules, OS scheduler entries, credentials, or paid model calls

Independent Standards and Spec reviews of the product checkpoint both returned
zero actionable findings. Review fixes cover atomic draft text writes, recording
admission ordering, interrupted-program evidence, bounded startup handshakes,
CLI validation before background effects, and serialized durable-result retries.

## Runtime checks

`scripts/rpa-headless-live.test.ts` drives an actual daemon and actual CLI
subprocesses. The final exact-source run passed all three scenarios (122 assertions).

1. Record creation, rename, draft update, state wait and snapshot; export without
   original UUID/text defaults; replay in a fresh scratch target; compare semantic
   title/draft state and prove the generated identity changed. Resume the detached
   attachment from another client while the daemon owns execution.
2. Run ordered suites after CLI exit, with fresh targets per program. Exercise
   first failure, overall timeout, cancellation, retained step evidence and owned
   cleanup. Later programs never execute after failure.
3. Create future and due schedules; observe one-shot completion; kill/restart the
   controller during a claimed run; verify interruption without duplicate replay,
   retained future timing, disable and removal.

For the owned daemon process, the test checks `/proc` environment for absent
`DISPLAY` and `WAYLAND_DISPLAY`, verifies the exact daemon executable, and checks
that no GUI or display child exists. UI actions and UI waits fail explicitly.
Unrelated cloud desktop processes are outside this isolated process tree.

```sh
cargo build --locked --offline -p waku-daemon
daemon_bin=$(realpath "${CARGO_TARGET_DIR:-target}/debug/waku-daemon")
env -u DISPLAY -u WAYLAND_DISPLAY \
  WAKU_RPA_HEADLESS_TEST=1 \
  WAKU_RPA_DAEMON_BIN="$daemon_bin" \
  bun test scripts/rpa-headless-live.test.ts
```

## Local gates

- Rust protocol: 116 tests passed
- Rust client: 46 tests passed
- CLI and TypeScript client: 80 tests, 410 assertions passed
- TypeScript client check and strict CLI/test type checks passed
- Rust formatting, whitespace and strict OpenSpec validation passed
- Core: 733 passed, 25 existing ignored, 9 live-service tests excluded
- Native REPL binary: 26 tests passed, including the shared automation bindings
- Debug and release workspace checks passed
- Release disabled-automation surface regression: 1 test passed
- Exact-source daemon-only CLI acceptance: 3 tests, 122 assertions passed

The REPL gate targets `--bin waku_js_repl`, not the app library. Release
workspace checking uses the normal release profile; the focused release test
uses `CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16`.

The core gate excludes only these existing live-service tests; no provider
credentials are supplied to make them pass:

```sh
cargo test --locked --offline -p waku-core --lib -- --test-threads=1 \
  --skip driver::opencode::tests::opencode_session_against_the_adopted_service \
  --skip opencode_api::tests::live_catalogues_settle_a_cold_location \
  --skip opencode_api::tests::live_service_answers_the_startup_path_and_the_moved_routes \
  --skip opencode_service::tests::adopts_the_registered_service \
  --skip opencode_service::tests::live_service_identifies_itself_through_the_production_probe \
  --skip opencode_session::discovery_smoke::discovers_models_from_the_adopted_service \
  --skip opencode_session::tests::discovers_the_real_model_and_agent_catalog \
  --skip opencode_session::tests::lists_real_root_sessions_across_workspaces \
  --skip slash_command_catalog::tests::opencode_builtin_catalog_against_a_real_service
```

Controlled-clock schedule/store tests cover non-overlap, coalescing, anchored
cadence, restart interruption, disable/re-enable, capacity, corruption, atomic
write failure, exclusive ownership, overflow and completion retry races.

## Limits

Ad-hoc recordings/results remain daemon-memory state unless the client exports a
file. Schedules and compact last-result summaries survive restart; full evidence
is bounded in memory. Hard crashes may leave disposable scratch directories.
Backend-only capabilities are explicit; arbitrary UI workflows are not headless.

CI was intentionally not run. Existing real-provider acceptance gates 2.7/3.11
remain open. This does not establish macOS/Windows GUI behavior, live Pi/Codex
completion, cross-device execution, or clean native titlebar shutdown.
