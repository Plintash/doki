# Agent information verification

This checkpoint implements `complete-agent-observability`. It builds on the
combined Board/RPA source and verified Pi retry/annotation fixes. It does not
close the real-model fan-out gates 2.7/3.11 in `add-rpa-bridge`.

## What an agent can establish

- `describe` explains the available calls, argument contracts, identity domains,
  read bounds and effects. `inspect` returns compact selected-conversation,
  provider, pending runtime-choice and cached workspace information.
- Ordinary actions, failure evidence, tree and pointer-state inspection do not
  need the selected transcript. Explicit full-history snapshots still fail
  honestly when they exceed the existing 4 MiB observation bound.
- `transcript --conversation ID QUERY_JSON` pages indexes and bounded UTF-8 text
  for live or unloaded persisted conversations without navigating, hydrating,
  changing the draft, saving the model or starting a provider.
- Pi and Codex are both represented through the shared provider catalog. Cached
  availability is not authentication proof: the fixture reports providers
  disabled and `authentication: notChecked`.
- Selection/comment/editor overlays expose semantic controls and current text;
  runtime approvals expose their actual choices and target control IDs. Reading
  these does not authorize answering an approval or starting a model.

## Exact source and gates

The full debug matrix ran against `ce601ebad0833806afe37b5be0f3ec448ff1ef1c`
(tree `e7c33d4f5ecd33e03526b8bbb2b234a26fb83ab8`). The subsequent `a6f7484`
changes only the native fixture's expected trailing-newline normalization.
The final product corrections are `e558614` and
`cf160493c09dbd1f19139b2e3415de315e36ce1d`
(tree `13660831181154a64fb530f1ffae92873855ca55`): opening the annotation
editor closes its source preview, and chip hover/activation cannot reopen it
while editing. This prevents the preview's outside-click handler from discarding
a Save. A source regression and native replay cover it.

Full debug results:

- Core: 655 passed, 25 pre-existing ignored; only the nine named OpenCode
  live-service prerequisite tests listed in [P1 QA](rpa-p1-qa.md) excluded
- App: 634 passed, 2 pre-existing ignored, serial isolated profile
- Protocol: 110 passed; Rust client: 46 passed; native QuickJS REPL: 26 passed
- GPUI integrations: 2 passed; JavaScript: 394 passed, 1,478 assertions
- Generated bindings, TypeScript checks, workspace check, formatting, native
  build and strict OpenSpec validation passed

Release-profile workspace checking and four core release tests passed on
`a6f7484`, including rejection of the new transcript-read command. The actual
release tests used `LTO=false`, 16 codegen units and disabled debug assertions.
No new release GUI/REPL executable acceptance is claimed; prior native release
proof remains the separate [P1 checkpoint](rpa-p1-qa.md).

On final product source `cf160493`, the full serial app suite passed **635 tests**,
2 ignored; debug and release workspace checks and formatting passed. All four
steps ran with a clean, unchanged source tree. The extra app test covers preview
reset/hover/activation guards; native behavior is verified below.

## Native diagnosis

The cloud Linux fixture runs the real `scripts/dev.ts` watcher with software
Vulkan and its already-configured stable external daemon. It uses a disposable
profile and explicitly disabled provider executables. No model call or prompt
submission was made. Native snapshots identify the intended checkout, process,
instance, database and build rather than assuming a rebuilt binary was loaded.

The combined daemon, workflow and new agent-information suites passed **9 tests,
182 assertions** on build `72c30ac9fb8fff84-18de44933b13a45f` before the final
annotation correction. The new information test establishes:

- A selected 5,309,999-byte collapsed command output coexists with an 8,364-byte
  discovery reply; compact reads and action failure evidence remain usable
- Live reads identify their live source; an unopened conversation stays
  `detailLoaded: false` and reads from persistence
- A 220,000-byte Unicode message and command output each reconstruct exactly
  across bounded byte slices
- Selected conversation, route and unsent draft remain exactly unchanged
- Explicit oversized full-session reads return `observation_too_large`
- A missing UI target returns inspectable failure-time selected/draft state and
  diagnostic evidence despite the large selected history
- Workspace queries report cached `notRequested`, `notRepository` and `error`
  states; the registered review wait begins pending and completes. This proves
  final-state coverage, not an independent asynchronous event-only transition

The same three native suites then passed again on final product source
`cf160493`: **9 tests, 182 assertions**, build
`babf9da95576dce7-18de463ac39f90c4`. Its discovery reply was **8,366 bytes**; all
large-history, persisted-read, draft-preservation and failure-evidence checks
passed again. Scratch instances were removed by the suites.

## Annotation diagnosis

A screenshot-grounded native text drag was followed by semantic controls for
Add to chat, comment entry, staging, preview Edit, editing, Save/Cancel and clear.
The first run exposed a real bug: the preview remained open behind its editor,
so its outside-click handler closed the editor before Save could retain text.
The compact annotation state and input value made that failure inspectable.

On the final build, editor input and exact text were visible in semantic state;
physically hovering the composer annotation chip left `previewVisible: false`.
Save followed by reopening retained `Agent-readable annotation comment
(verified)` exactly. Adding unsaved text, cancelling and reopening preserved
that saved comment. Clear left annotation count zero, draft empty, selection
and toolbar false, comment closed, editor null and preview hidden. The final
editor screenshot was captured after reopening the saved comment.

After acceptance, title-bar X shutdown triggered a panic in the unchanged pinned
GPUI Linux X11 client (`client.rs:1546`, `RefCell already mutably borrowed`).
The watcher exited and the fixture runner stopped its daemon and removed
`running.json`. This is a separate teardown defect, not a clean-shutdown claim;
it did not occur during the acceptance scenarios. No GPUI fork change is
included in this checkpoint.

Screenshots are synthetic fixture data only. They illustrate native surfaces;
the state/behavior assertions, not pixels alone, establish their effects.

## Reproduction and evidence

Use the existing authorized isolated daemon connection as in [RPA usage](rpa.md).
Do not print its token or run against a working draft. The live suite creates
and cleans its own scratch instances; the huge-history fixture intentionally
marks only its newly-created workspace as unavailable Git state.

```sh
WAKU_RPA_DAEMON_TEST=1 WAKU_RPA_WORKFLOW_TEST=1 WAKU_RPA_AGENT_INFO_TEST=1 \
  bun test scripts/rpa-daemon-live.test.ts scripts/rpa-workflow-live.test.ts \
  scripts/rpa-agent-info-live.test.ts
cargo test -p waku --lib -- --test-threads=1
CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
  cargo test -p waku-core --release --lib bounded_transcript_tests -- --test-threads=1
```

Cloud evidence retained under `/workspace/shared/doki-toolchains/`:
`agent-info-final-gates.json`, `agent-info-release-gates.json`,
`agent-info-delta-gates.json` and their named logs;
`agent-info-final-native-combined.log`; native describe/inspect JSON;
`agent-info-final-annotation-acceptance.json` and its state/tree/action JSON;
`agent-info-final-annotation-editor.png` and the earlier comment screenshot;
and `agent-info-final-watcher.log` including the teardown traceback. These are local QA
artifacts, not a claim that a recipient can open cloud filesystem paths.

## Review and boundary

Independent Standards/Fowler and Spec reviewers found no actionable source
issues through `cf16049`. Their independent evidence reviews also found no new blocker. Source and
evidence clearance does not expand the acceptance boundaries below.

No paid-provider acceptance, macOS run, recording/replay, unattended/headless
suite, scheduling or cross-device execution is included in this checkpoint.
Those remain separate work. Diagnostics still have their documented retention
and disconnected-stream limits; cached observations are not fresh external I/O.
