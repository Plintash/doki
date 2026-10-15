# Durable Board stages and generic manual drafts

Base: `09cc465f0aa0d042533819100ef55ac12fc9938b`, the previously accepted
configuration-only prototype. This report replaces its nonpersistence limitations
only for the implementation and checks explicitly described below.

## Implemented boundaries

- Project-owned ordered stages and defaults persist through SQLite and daemon
  catalog sync. Explicit configuration saves use compare-and-swap revisions;
  ordinary task/session snapshots cannot overwrite a newer board configuration.
- Existing projects retain Backlog/Doing/Done and deterministic canonical IDs.
  Users can add/reorder active columns and rename terminal Done to Archive.
  Lifecycle remains internal; terminal completion uses existing stop/ack rules.
- Issue stage identity moves atomically with lifecycle and ordering, preserving
  issue/session/workspace identity. Repeated optimistic moves roll back to the
  acknowledged placement; newer authoritative catalog data supersedes rollback.
- Exact project IDs own defaults. Same-path alias issues remain visible; a custom
  foreign-column drop reports an error instead of applying another owner's
  defaults. All Projects retains its lifecycle aggregate.
- Explicit Prepare creates/reopens a reviewable provider-neutral session draft.
  Moving cards never starts providers. Normal Send remains the execution action.
  Configured stages replace competing Pi/Codex start controls; old review sessions
  and provenance remain accessible through the issue's conversations.
- Optional handoff requires explicit selection of a successful final visible
  assistant result from the same issue/workspace. Preparation and first Send
  validate source identity. Cold details load before selection; visible excerpts
  are bounded, and invocation/config/source provenance persists separately.
- First brief-only drafts support planned worktrees through existing first-Send
  materialization. Unconfigured/unavailable harnesses and unknown explicit model
  overrides block rather than silently selecting another provider/model.
- No autonomous stage advancement, scheduler, team/supervisor implementation,
  provider-private context copying, or native-review capability assumption.

## TypeScript compatibility checks

Project factories in web/mobile now provide an empty initial board; the daemon
assigns canonical stages. Generated protocol types were refreshed, not hand-edited.

Passed against current generated types:
- Client typecheck
- Web typecheck
- Mobile typecheck
- Client tests: 70 passed, 0 failed
- Web library tests: 137 passed, 0 failed
- Mobile library tests: 128 passed, 0 failed

The shared `node_modules/@waku/client` symlink resolves to the original checkout.
Web/mobile verification therefore used temporary tsconfig path overrides pointing
at this worktree's client sources and generated types. Without that override,
TypeScript correctly complained that the old checkout's Project lacked board;
that was a worktree resolution mismatch, not a current-source type failure.
No shared dependency symlink was modified.

Commands and logs are preserved under
`/workspace/shared/doki-stage-qa/durable/`:

```sh
source /workspace/shared/doki-toolchains/env.sh
bun run --cwd packages/waku-client check
node_modules/.bin/tsc --noEmit -p /tmp/stages-web-tsconfig.json
node_modules/.bin/tsc --noEmit -p /tmp/stages-mobile-tsconfig.json
bun test packages/waku-client/src
bun test --tsconfig-override /tmp/stages-web-tsconfig.json apps/web/src/lib
bun test --tsconfig-override /tmp/stages-mobile-tsconfig.json apps/mobile/src/lib
```

## Core, migration and protocol verification

- Focused optimized persistence suite: 73 passed, including pre-0010 schema
  migration, restart/custom defaults/order and session invocation provenance
- Focused CAS suite: 2 passed, including stale configuration, deletion and reorder
- Final broad core suite: 758 passed, 0 failed, 25 ignored; exactly nine live
  OpenCode-service tests filtered because the required CLI is absent
- Protocol suite: 134 passed; generated TypeScript export/check current

The initial unfiltered core run had 757 passed and 10 failures: nine live-service
fixtures failed before launching because OpenCode v2 was not installed; one old
roundtrip expectation omitted the newly assigned canonical stage ID. The latter
was corrected while preserving full equality checks for issue identity, workspace,
state and timestamps, and passes in the final run. No provider/model calls were
made, no live-service CLI installed, and no authenticated tests enabled.

The final broad core rerun used a command-line test-only
`profile.dev.package.waku-core.opt-level=0` override to bound memory/code-generation
cost. Cargo.toml and the native app build profile are unchanged. Exact run output
and skip names are preserved in the durable QA logs.

## App and source review gates

Final current-source full app library rerun: 699 passed, 0 failed, 2 ignored,
including editor-measurement cache and stage-aware draft-title regressions. All stage defaults,
source-selection/draft invariants, coalesced movement rollback, remote reconciliation
and legacy draft-exclusion tests pass. The first app run had 696 passed and one
failure because new RPA verbs lacked discovery metadata; adding the accurate
argument/effect descriptions resolved that failure in the full rerun.

Formatter and diff checks pass. Independent Specification and Standards/Fowler
reviews report no remaining concrete source findings. The Standards review caught
and verified fixes for multi-move rollback across queued saves, including the case
where another unrelated save was already in flight. The final review also verified
the authoritative remote-task reconciliation hook and compact native controls.
These source reviews do not substitute for native runtime acceptance.

App test command (native fixture stopped):

```sh
source /workspace/shared/doki-toolchains/env.sh
WAKU_RPA_DATA_DIR=/workspace/shared/doki-stage-qa/unit-data \
  cargo test --locked --offline -j1 \
  --config 'profile.dev.package.waku.opt-level=0' \
  -p waku --lib -- --test-threads=1
```

## Native build resource limitation

The normal optimized native app build was terminated by SIGKILL after roughly
seven minutes, without a Rust compiler error. Resource pressure was suspected
from low available memory; OOM was not independently established. The successful
app unit suite does not establish a normal-profile native build pass.

The native retry succeeded in 40.97 seconds with a fixture-only Cargo wrapper supplying
`--config 'profile.dev.package.waku.opt-level=0'`. Product Cargo.toml and normal
profiles are unchanged. The resulting screenshot/interaction evidence proves
functional behavior and layout under that validation profile, not normal-profile
performance or a release/macOS build.

## Native behavior and restart evidence

Initial cloud-native build `f813994ec5bcbae6-18de697c02c4431d` ran against
protocol 20. Fifteen semantic checks passed, including durable configuration,
conflict recovery, lifecycle movement, explicit draft preparation/reuse and
selected-result handoff. The same fixture was restarted and retained the exact
catalog, board revision,
defaults, issue placement and prepared session IDs (`restart.log`/`restart.json`).
A read-only database audit also confirmed all three invocation snapshots exactly
matched their pre-restart values, with no model override and zero stored messages.
Normal and narrow Board screenshots were inspected: compact cards, fixed-size
Configure controls and horizontal column overflow remain intact.

The final synthetic Amp audit records 20 version probes and six blocked
`skill list --json` discovery attempts during draft selection. No Send or model execution occurred. All provider
credentials and real provider execution remained disabled.

Pixel review identified indistinguishable generic “Unsent draft” history labels.
The minimal stage-aware label was independently source-reviewed and verified
in fresh native build `4f50baca4b5dc5f8-18de69e03d950b36`: historical draft names
come from their saved invocation, ordinary drafts retain their existing title,
and the inspector still has one compact preparation action.
`final-generic-prepare-panel.png` records the corrected UI. At 980px, horizontal
scrolling exposes the terminal column with its full 28×28 Configure control.

Final same-build screenshots: `final-normal.png` (1178×814),
`final-generic-prepare-panel.png`, and `final-narrow-scrolled.png` (980×814).
Board and inspector are available as Library images
`libfile_7982671436788191878ba203b1cacaa2` and
`libfile_73c89920de7c81919916a0a8615e5db7`, both version 0.
The fixture watcher and daemon were stopped and their active marker removed.

Native validation covers preparation only, not authenticated first Send or
provider inference. Busy-provider stop/error behavior is covered by existing
and new unit tests, not a live provider scenario in this fixture.

Final app output is preserved in `durable-app-tests-final.log`. Source, native
fixture and Cargo/display ownership were released after the passing run.
Changes are ready for coordinated publication; this report does not claim a
remote push, hosted CI result or release acceptance.
