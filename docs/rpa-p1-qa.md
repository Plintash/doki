# RPA P1 verification

This is an infrastructure checkpoint for `add-rpa-bridge`, reviewed against
P0 commit `484480e1116346f571672fd0b1144f726df726cc`. The original remote branch
prerequisite is `ada83525d161c563b8d6ad1a48e06e29650d3926`.

## Acceptance boundary

Tasks **2.7 and 3.11 remain open**. Task **3.8** also remains open for
the exact development-watcher relaunch check; its CLI/REPL and explicit-build
replacement portions passed. No model prompt was submitted, no provider
credentials were copied or configured, and the real subagent fan-out scenario
has not been accepted. The committed fan-out recipe is not evidence that it
ran. The pinned Pi 1.0.4 checks cover its extension loader and two RPC commands
(`get_state`, `get_commands`), not model-driven tool execution.

Linux native checks use the supported cloud desktop, an isolated profile with
all providers disabled, software Vulkan, and a stable external daemon. The
working fixture and each disposable scratch instance have separate databases
and profiles. The macOS LaunchServices environment forwarding has a source
guard. Neither the native macOS watcher nor `scripts/dev.ts` on Linux was
executed for the successful replacement test: the isolated fixture automatically
restarted the app after an explicit Cargo build.

A subsequent actual Linux `scripts/dev.ts` attempt completed its daemon, app,
REPL and Computer Use builds, then stopped before launch when its isolated
shell could not download the pinned Cua SDK. The official Debian SQLite CLI
prerequisite was installed, and the pinned SDK was subsequently downloaded and
checksum-verified through the permitted download environment. The Cua host
build/packaging and resulting watcher-triggered relaunch remain unverified:
only about 1 GiB of workspace storage remained, with a 768 MiB safety reserve.
No package substitution or watcher bypass was used. The default RPA database
files were restored byte-for-byte after each isolated attempt.

That attempt also exposed stale cross-worktree local-crate artifacts in the
shared Cargo target after another branch built a different protocol. Forcing
this checkout's workspace source timestamps rebuilt the correct graph and the
native build passed. The earlier regression/release/native results preceded
those integration builds. Prefer a separate target when storage permits;
otherwise force local workspace rebuilds when switching this shared target.

## Results

Verified on Linux against source commit
`b26f21511d44693741385cfbcf1f110dae0fcceb`
(tree `0d31a3e7ffa97d2a2ba7ab6f74d46055ca53a146`), before the
subsequent documentation-only acceptance record:

- Core library: **609 passed**, 21 pre-existing ignored, 9 existing OpenCode v2
  live-service prerequisite tests filtered out; run serially
- App library: **571 passed**, 2 pre-existing ignored; run serially
- Protocol: **97 passed**; Rust client: **45 passed**; generated TypeScript
  bindings are current
- Native QuickJS REPL: **25 passed**; authenticated WebSocket fixture passed
  shared-protocol, private-capability, failure-evidence and redaction checks
- TypeScript client, CLI and identity: **77 passed**; package and standalone
  script type checks passed
- GPUI automation spike and list integration: **2 passed**
- Formatting, release workspace check and strict OpenSpec validation passed
- Actual release-profile core tests: **3 passed**, covering automation and
  diagnostic query/recording gates and diagnostic request timing. LTO was
  disabled and codegen units set to 16 to fit the cloud build budget; release
  `debug_assertions` remained off
- Both an assertions-off native REPL binary and the actual release-profile
  REPL binary rejected injected RPA capability hints, exposed no RPA/native
  capability binding and did not leak the fixture capability. The actual
  release build used the same LTO/codegen overrides as the core gates
- Fresh debug app, daemon and REPL builds passed. Native daemon/scratch
  workflow: **7 passed**, 78 assertions; fresh-build replacement: **1 passed**,
  6 assertions on the same external daemon
- Actual release-profile GUI build passed with the same LTO/codegen overrides.
  Its isolated native window rendered successfully. Across 20 observations
  on its configured external debug daemon it registered **zero executors**;
  it published no legacy endpoint even with `WAKU_RPA_LEGACY_TCP=1`. The app
  remained alive throughout; the owned app and daemon were then stopped

The initial parallel app-library run exposed a pre-existing shared-global
locale race: the persisted Japanese-language window test saw `File` instead
of `ファイル` while another window test changed the global locale. The files
implementing that test and locale storage were unchanged from P0. The full
serial retry passed all 571 tests; parallel stability is not claimed.

The nine filtered tests require an installed/adopted OpenCode v2 service. A
previous unfiltered run failed those prerequisite checks without executing a
model. They are not counted as passing. A prior terminal-flood stall did not
recur in its isolated or full-suite rerun.

The native workflow checks targeting, absence of app-local discovery,
on-demand frames, failure-time diagnostic evidence, scratch isolation,
semantic input, provider-free disabled send, independent client resume,
committed smoke steps, scenario failure retention, automatic teardown and
unchanged working state. The explicit-build fixture relaunch check observes a changed build
fingerprint, PID and instance UUID on the same daemon. It is evidence for
fresh-build selection and reconnect, not watcher-triggered rebuild acceptance.

The final-source captured prototype shows an actual scratch-only draft, `No providers`,
and a disabled send control. It contains no model output. The screenshot
fixture was subsequently removed and its database absence verified.

## Reproduction

Use the repository's supported toolchain and native prerequisites. Do not run
multiple Cargo jobs against the same target directory.

```sh
cargo test -p waku-protocol
cargo run -p waku-protocol --bin export_types -- --check
cargo test -p waku-client
cargo test -p waku-core --lib automation -- --test-threads=1
cargo test -p waku --lib -- --test-threads=1
cargo test -p waku --bin waku_js_repl
cargo test -p waku --test rpa_spike --test rpa_list
cargo fmt --all -- --check
cargo check --workspace --release
bun test packages/waku-client/src scripts/rpa.test.ts scripts/rpa-identity.test.ts
OPENSPEC_TELEMETRY=0 bunx @fission-ai/openspec validate add-rpa-bridge --strict
```

Release executable checks used:

```sh
CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
  cargo test -p waku-core --release --lib respects_build_mode -- --test-threads=1
CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
  cargo build -p waku --release --bin waku --bin waku_js_repl
```

The full-core count excludes only these existing live-service tests via
individual `--skip` filters, rather than skipping all OpenCode unit coverage:

- `driver::opencode::tests::opencode_session_against_the_adopted_service`
- `opencode_api::tests::live_catalogues_settle_a_cold_location`
- `opencode_api::tests::live_service_answers_the_startup_path_and_the_moved_routes`
- `opencode_service::tests::adopts_the_registered_service`
- `opencode_service::tests::live_service_identifies_itself_through_the_production_probe`
- `opencode_session::discovery_smoke::discovers_models_from_the_adopted_service`
- `opencode_session::tests::discovers_the_real_model_and_agent_catalog`
- `opencode_session::tests::lists_real_root_sessions_across_workspaces`
- `slash_command_catalog::tests::opencode_builtin_catalog_against_a_real_service`

For native tests, supply an existing authorized isolated daemon connection in
the environment as described in `rpa.md`. Never paste or print its token.
The scratch test mutates only instances it creates, does not submit a prompt,
and removes owned data.

```sh
WAKU_RPA_DAEMON_TEST=1 WAKU_RPA_WORKFLOW_TEST=1 \
  bun test scripts/rpa-daemon-live.test.ts scripts/rpa-workflow-live.test.ts
```

For replacement acceptance, start the following while the old app is still
connected. Rebuild and relaunch only that app, preserving its external daemon.
The test bounds the wait and verifies the replacement executor with a fresh
snapshot.

```sh
WAKU_RPA_RELAUNCH_TEST=1 bun test scripts/rpa-relaunch-live.test.ts
```

The default app-owned daemon can change address and token on relaunch. This
check does not establish rediscovery across that replacement; refresh the
caller's supplied connection through its existing authorized setup.

## Review and delivery

Standards and Spec are reviewed independently using the Matt Pocock
code-review workflow and the fixed P0 baseline. Repository conventions take
precedence over generic smell preferences. After iterative repairs, both
reviewers cleared the full `484480e...b26f215` diff: zero documented Standards
violations, zero actionable smell findings and zero actionable Spec findings.
The reviews are source review, not a substitute for native or model acceptance.

GitHub publication is blocked by repository write authorization. Source is
committed on the dedicated branch, never merged to main. Compact recovery
archives contain the public-baseline delta bundle, binary patches, a checksum
manifest, recovery instructions and the verified prototype. Restore checks
must establish the exact commit/tree, patch tree equality, valid object graph,
missing-prerequisite rejection and downloadable Library byte equality.
