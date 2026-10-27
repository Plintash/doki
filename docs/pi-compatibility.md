# Pi compatibility

Doki uses the official `@earendil-works/pi-coding-agent` CLI's NDJSON RPC mode.
The compatibility matrix is **Pi 1.0.4 and Pi 1.2.0**, with every installed
`@earendil-works/*` dependency pinned to the same version. Other 1.0.x/1.1.x
patches have not been runtime-tested here. There is no hard-coded version gate,
new provider dialect, or dependency on Pi's removed experimental server stack.
Oh My Pi remains a separate existing flavor; these changes do not certify a new
Oh My Pi release.

## Upstream contract

Inspected official tags:

- [Pi 1.0.4](https://github.com/earendil-works/pi/tree/v1.0.4)
- [Pi 1.2.0 release](https://github.com/earendil-works/pi/releases/tag/v1.2.0),
  commit `06a466935536f8c9bcacb2d5efc0fc021be1bdff`
- [RPC implementation](https://github.com/earendil-works/pi/blob/v1.2.0/packages/coding-agent/src/modes/rpc/rpc-mode.ts)
- [Session lifecycle](https://github.com/earendil-works/pi/blob/v1.2.0/packages/coding-agent/src/core/agent-session.ts)

The RPC mode and wire-type source files are byte-identical across these tags
(Git blobs `1c0995d36ec752155f1d13b019728f9343bb263c` and
`7fc71516aebb8b68ea019f1df4eef966eeaa17c7`). Session behavior changed underneath
that wire interface:

1. Pi 1.1+ includes `aborted` on `agent_settled`. Doki preserves it as the
   turn's interrupted flag. Missing/false remains false for the 1.0.4 contract;
   Oh My Pi's `agent_end` is unchanged.
2. Pi 1.2 runs `before_agent_start` for extension-triggered custom messages.
   An extension command can return `disposition: "handled"` before
   `agent_start`, while `get_state.isStreaming` is already true. Doki checks
   state when a handled command appears idle, keeps that startup in one turn,
   and accepts steering/cancellation against the busy run.
3. A custom startup with invalid prompt-section configuration can emit
   `extension_error` (`event: "send_message"`) and become idle without
   `agent_start` or `agent_settled`. For a startup already observed as busy,
   Doki rechecks state and finishes failed only after Pi confirms idle. Queued
   messages are returned through the usual settlement path. A failure that
   precedes the first busy observation is kept with the acknowledged command
   awaiting that observation. Errors have no upstream request ID: if that
   command was superseded, the error remains out-of-band rather than failing
   the newer submission. Ordinary hook errors and uncorrelated idle errors do
   not terminate or create a run.
4. A request the provider rejects is finalized through `message_end`, not
   `message_update`: the final assistant message carries `stopReason: "error"`
   and the provider's `errorMessage`, and the stream's error case emits no
   `message_update` at all. A rejection Pi does not retry — 401/403, an unknown
   or disabled model, exhausted quota, an invalid request — therefore ends the
   run with no content. Doki keeps that reason and fails the turn at
   settlement, so an answerless "completed" turn cannot be the whole record.
   A message that still carried answer text settles as the answer it is, and a
   retry cycle keeps its own reporting: its start clears the held reason, and
   its end reports recovery or `finalError` exactly once.

State responses are applied by the reader before the writer resumes. A stale
response cannot finish a newer submission. A timed-out observation reports a
transport error, does not invent a completion, and retains correlation for a
late reply. A permanently unresponsive process cannot be certified as idle.

Launch flags, native session cursors, model selection, slash commands, queue
format, text/thinking/tool frames, and extension UI replies retain their shared
1.0.4/1.2.0 contracts. The bundled JavaScript bridge uses `registerTool`,
`executionMode: "sequential"`, TypeBox schemas, and `session_shutdown`; it does
not use the changed hook or removed experimental APIs and needs no API rewrite.

## Provider-free checks

These checks use isolated empty HOME/configuration, an environment with no
provider credentials, and official installed packages. Pi is not placed on the
normal PATH. The startup fixture checks Pi's `unknown/unknown` placeholder
before triggering a gated custom run. It either cancels before releasing the
gate or deliberately fails local prompt-section validation. Neither path can
resolve a real provider. No authenticated inference, real tool-using model run,
or pi-subagents workflow was performed.

Covered by `driver/pi/compatibility_tests.rs`:

- Real Doki adapter handshake and command catalog on both versions
- Extension custom messages, title updates, confirm-dialog response and local
  command completion on both versions
- Upstream rejection of cloning an unsaved extension-only session
- Resume/clone/resume of an explicit native-format saved user-message fixture
- Actual process exit after driver shutdown, including resumed sessions
- Pi 1.2 held custom startup, cancellation, one-turn ownership and aborted settle
- Pi 1.2 failed startup without a settlement event, both held and ungated,
  with local failure recovery

A separately opt-in server test covers confirmed runtime shutdown and detached
runtime lookup. Unit fixtures cover both abort formats, stale and late state
responses, reader/writer acknowledgement ordering, failed observations, queued
input surviving custom-startup failure, and a provider rejection reported only
by the final assistant message — on both flavors.

`scripts/test-pi-extension.mjs` loads the actual bundled extension through each
version's official loader and directly executes its registered tools against
Doki's real JavaScript REPL. It verifies exact dependency cohorts before import
and covers schemas, persistent bindings, top-level await, text, images,
metadata, errors, reset, safe defaults, and child-process shutdown. This is an
extension API test, not a model or native UI test.

## Reproduce

Install each official version in its own directory with install scripts disabled.
Pin `@earendil-works/pi-coding-agent` in `dependencies`, and all of its Pi-family
transitive packages in npm `overrides`. The 1.0.4 cohort includes `chord`; 1.2.0
removed it. Retain `package-lock.json`. A CLI-only exact pin can still resolve
newer Pi-family dependencies through upstream caret ranges; that is not a
valid 1.0.4 baseline. The extension harness rejects mixed or unpinned cohorts.
Node must meet the upstream `>=22.19.0` requirement.

```sh
# Repeat for the fully pinned 1.0.4 and 1.2.0 installations.
WAKU_TEST_PI_BINARY=/absolute/pi-install/node_modules/.bin/pi \
  cargo test --locked -p waku-core --lib \
  official_pi_handshake_dialog_clone_resume_and_shutdown_without_a_model \
  -- --ignored --exact \
  driver::pi::compatibility_tests::official_pi_handshake_dialog_clone_resume_and_shutdown_without_a_model

# Pi 1.2.0 only; never run this fixture with a configured provider profile.
WAKU_TEST_PI_BINARY=/absolute/pi-1.2.0/node_modules/.bin/pi \
  cargo test --locked -p waku-core --lib \
  official_pi_12_custom_startup_cancel_and_failure_settle_without_a_model \
  -- --ignored --exact \
  driver::pi::compatibility_tests::official_pi_12_custom_startup_cancel_and_failure_settle_without_a_model

WAKU_TEST_PI_BINARY=/absolute/pi-1.2.0/node_modules/.bin/pi \
  cargo test --locked -p waku-core --lib \
  official_pi_12_fast_custom_startup_failure_is_not_successful -- --ignored

WAKU_TEST_PI_BINARY=/absolute/pi-install/node_modules/.bin/pi \
  cargo test --locked -p waku-core --lib \
  confirmed_shutdown_real_pi_exits_without_a_model_prompt -- --ignored

cargo build --locked --bin waku_js_repl
node scripts/test-pi-extension.mjs target/debug/waku_js_repl \
  /absolute/pi-1.0.4 1.0.4 /absolute/pi-1.2.0 1.2.0
```

This work is based on the frozen Board/RPA integration commit `5c7c944`, with
protocol 16, issue Task/session separation, isolated workspace bindings and
completion/shutdown rules unchanged. Real-model acceptance tasks 2.7 and 3.11
remain open. Native macOS execution and authenticated provider behavior are
not established by these Linux protocol checks.

## Final verification record

All 19 final gates passed on the clean, frozen production commit
`4b82b279324e93c1bacac99a93aed779a4cc705f`, tree
`5843b850a70a123d576e4e9609360ba5e3cf1944`, on 2026-10-14 (Linux).
The runner checked the same commit and clean working tree before every phase
and after the last phase. This record is a subsequent documentation-only change.

- Rust: 644 core, 625 app, 104 protocol, 46 client, 25 REPL, and 2 targeted
  GPUI tests passed. The normal core run left 25 opt-in tests ignored and
  filtered 9 unrelated live OpenCode-service tests; the app left 2 ignored.
- TypeScript: 380 tests passed with 1,366 assertions, plus client/web/mobile
  and RPA script type checks.
- Formatting, generated protocol export check, and debug/release workspace
  checks passed.
- Official Pi 1.0.4 and 1.2.0 each passed the adapter lifecycle test and the
  confirmed runtime-shutdown test. Pi 1.2.0 also passed the held startup
  cancellation/failure test and the separate ungated startup-failure test.
- The actual bundled extension and real JavaScript REPL passed 8 checks per
  version, 16 total, using the two exact official dependency cohorts.
- Independent Standards and Spec reviews iterated to zero actionable findings
  on that exact production commit. The final quality pass found no unnecessary
  compatibility abstraction, retry/polling loop, or speculative recovery.

The recovery package preserves the gate commands, logs, pinned installation
manifests and lockfiles, upstream ordering traces, and review record. No UI
rendering code changed, and this Pi-only pass did not rerun native visual QA.
The frozen Board/RPA evidence remains separate. These results do not close
the real-model or native macOS acceptance limits above.
