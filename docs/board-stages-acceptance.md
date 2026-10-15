# Native Board stages prototype acceptance

Historical checkpoint: `09cc465f0aa0d042533819100ef55ac12fc9938b`.
This report describes the original nonpersistent prototype, before the durable
organization and generic manual invocation implementation. Its test totals and
limitations do not describe later commits.

## Scope

Opt-in debug-only selected-project Board, enabled with `WAKU_BOARD_STAGES_PROTOTYPE=1`.
Configuration and organization prototype only: Plan / Implement / Review / Archive are example stages; the active stages can be added, named and reordered. Harness, model, instructions and bounded handoff choices are runtime-only settings. They are not connected to Start/Send. Existing issue execution and compact inspector are unchanged. No scheduler, provider calls, automatic stage starts, migrations, or publication.

Stage IDs/configuration/active-stage associations are not persistent. Existing lifecycle transitions remain real and can persist to the disposable synthetic fixture database: active drops route to Doing, Archive uses the existing completion stop/acknowledgment boundary, and reopening uses the existing lifecycle behavior. Issue IDs, session providers/models, transcripts and workspace associations are retained.

All Projects keeps its original Backlog / Doing / Done lifecycle aggregate. Archive is fixed terminal; no rename/reorder/delete control is offered. Up to eight columns are supported in this prototype. Harness choices are configuration intent; disabled harnesses are labeled unavailable. Model overrides are free text, not validated against provider catalogs. Handoff is configured only; no result is collected or transmitted.

## Native environment

- Checkout: `/workspace/scratch/8b607ff4f3f0/doki-reviewer-role`
- Native Linux GPUI app, managed by `scripts/dev.ts 'Board Stages QA'`
- Isolated fixture: checkout `temp/app.db`, seeded through `scripts/issue-board-fixture.py`
- All provider binaries disabled; no real accounts/credentials copied
- Native runner/semantic bridge/captures: `/workspace/shared/doki-stage-qa/`
- Shared toolchain: `source /workspace/shared/doki-toolchains/env.sh`
- Native launch: `bash /workspace/shared/qstage` in the cloud desktop Terminal
- Semantic actions: `python /workspace/shared/doki-stage-qa/request-ui.py --script /workspace/shared/doki-stage-qa/acceptance.ts`

## Capture caption

Native configuration/organization prototype using synthetic issues. Column settings and stage placement are in-memory only; existing issue lifecycle controls still operate on the disposable fixture. Stage execution is not implemented; moving a card does not start an agent.

## Validation

Latest native build: `0f7322e66f452f2c-18de662a4a42f226`. Final native semantic acceptance passed after the square-button correction (`acceptance.json`).

Verified natively:
- Escape from name, model, instructions, and harness button dismisses settings while keeping Board open
- Add/Save, Add/Cancel, Add/Escape, empty-name validation, rename, model, instructions, harness and handoff configuration
- Reorder both edges with usable focus; Cancel restores original ordering
- Forward/backward keyboard stage movement using Alt+Left / Alt+Right; issue identity retained
- Project-specific defaults and All Projects lifecycle aggregate
- Full observable session objects, including transcripts and workspace/provider/model bindings, unchanged by settings/movement tests
- Restart resets the five-column edited configuration to four defaults, proving its nonpersistent boundary (`restart-check.json`)
- An unstarted issue with no sessions enters Archive through existing completion path; archived lifecycle survives restart
- Configure button bounds are exactly 28×28 in the 1178×814 window, with a long stage title, and in a native 980×680 window. Titles truncate instead of compressing controls (`button-geometry-*.json`)

Screenshots inspected from the actual GPUI window:
- Corrected full-width settings: `final-native.jpg`, Library `libfile_ffdbce496164819181bfb52cc0daa108`, version 1
- Narrow long-name stress case: `narrow-long-name.jpg`, Library `libfile_98255bc553208191a517b9f342fdc7e7`
- Earlier preliminary screenshot is superseded by the corrected full-width capture

Final full app library test rerun after the square-button correction passed: 682 passed, 0 failed, 2 ignored (`app-tests.log`). Formatter and diff checks passed. Three new pure state tests cover canonical stage identity after reorder, terminal rollback preserving prior active placement, and workspace-project changes invalidating stale placement. The rollback test is direct state simulation, not a native stop/acknowledgment test.

Exact final test command (native fixture stopped first):

```sh
source /workspace/shared/doki-toolchains/env.sh
WAKU_RPA_DATA_DIR=/workspace/shared/doki-stage-qa/unit-data \
  cargo test --locked --offline -j1 \
  --config 'profile.dev.package.waku.opt-level=0' \
  -p waku --lib -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

The low-memory override is test-only; native watcher builds retain the normal optimized app profile. No concurrent app build/test/fixture runs were used.

Not newly validated natively: busy Archive cancellation, stop rejection/timeout, detached-running work, real provider execution, durable stage migration/synchronization, or cross-machine behavior. Existing completion code remains authoritative and its existing unit tests are part of the app suite; that does not constitute new native proof of those paths. No macOS build or full-workspace suite was run.

## Review

Independent Specification review: zero remaining findings in stated scope. Independent Standards/Fowler/Matt Pocock source review: zero remaining findings after focus, Escape and square-button fixes. Native checks confirmed those fixes. The initial Escape test caught propagation to global Board close; a scoped action binding resolved it. User screenshot feedback caught the narrow ellipsis text button; the existing native icon-action primitive resolved it.

## Cleanup and publication

The owned native watcher and daemon were stopped before the final test run; `temp/issue-board-qa/cleanup.json` confirms cleanup and the running marker was removed. The existing desktop Terminal tabs were preserved. No caches or other workers’ databases were removed. This is a local checkpoint only; nothing was pushed or published.
