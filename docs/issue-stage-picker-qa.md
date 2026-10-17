# Issue inspector stage navigation

## Defect and scope

Issue [#103](https://github.com/Plintash/doki/issues/103) was confirmed against
main `d30ee29c4b24616e8be833e1613b2d409689e18d` by tracing the real inspector
control and shared movement path. A custom Review stage used the same Doing
lifecycle as Implement, but the inspector showed only the lifecycle label and
three lifecycle destinations. Selecting that active lifecycle replaced Review
with the canonical Implement stage. This is source evidence, not a new native
visual reproduction.

The existing status control now shows the issue's saved stage name and lists
its exact owning project's stages in saved order. It works independently of
All projects, a selected project filter, and same-path project aliases. The
menu uses the existing bounded native collection primitive and retains the
existing keyboard/focus/dismissal behavior. Long names are bounded to the
header's available space; their full text remains the control's accessible
name and menu entry.

Menu activation re-resolves the current project/stage instead of applying a
captured configuration. A deleted stage or reassigned project requires reopening
the menu. A stale row from a previously viewed issue cannot mutate it after
navigation. Its menu handle belongs to the inspector visit, so navigation and
Back/reopen also discard the old cached rows. Unbound, unavailable-project and preconfiguration records retain
lifecycle controls. Legacy missing/invalid stage identities display their
owner's canonical lifecycle stage.

All movement still goes through `move_task_to_stage`, including the existing
completion confirmation, runtime-stop, persistence and rollback boundary.
Moving between stages does not create a session or call a provider. Sessions,
workspace bindings and prior invocation snapshots are not changed by the menu.

## Automated coverage

Seven `app::board_header::tests` regressions cover:

- Actual custom label, saved order, and distinct selection of two Doing stages
- Renamed/reordered stages without mutating the issue
- Exact owner selection with a same-path alias first in the catalog
- Canonical fallback for absent/stale stage identity
- Unbound, missing-project and empty-board lifecycle fallback
- Deleted targets, reassigned projects and newly available configuration
- Active movement preserving issue/workspace identity and terminal destination
  resolution without bypassing completion acknowledgment

The native menu's existing test suite covers focus, keyboard navigation,
dismissal and virtualized long collections. These primitive tests and the new
source/model tests do not establish a rendered acceptance pass for this control.

## Verification record

Local verification on 2026-10-17 used Rust 1.96.0 on Linux and a dedicated
Cargo target. The branch includes main `808611c` (including PRs #85, #100 and
#101). The final stale-stage message uses the existing translation system in
English, Simplified Chinese and Japanese.

- `cargo test --locked -j2 --config 'profile.dev.debug=0' -p waku --lib
  app::board_header::tests -- --test-threads=1`: **7 passed**
- `cargo test --locked -j2 --config 'profile.dev.debug=0' -p waku --lib --
  --test-threads=1`: **728 passed, 2 ignored, 0 failed**, repeated after the
  final localization edit
- Changed-file `rustfmt --check` and `git diff --check`: **passed**
- `python3 scripts/tests/test_preview.py`: **12 passed**
- Independent source review: **no remaining actionable findings** after fixing
  cached menu ownership; localization and integration delta were re-reviewed

Hosted checks for pre-localization checkpoint `d5a6d387` independently passed
all 728 app tests, including these seven regressions, on Ubuntu. That overall
[Tests run](https://github.com/Plintash/doki/actions/runs/39045973392) failed later
on nine existing OpenCode-v2 prerequisite tests, tracked in
[#88](https://github.com/Plintash/doki/issues/88). Its
[macOS preview](https://github.com/Plintash/doki/actions/runs/39045973389) built,
verified and uploaded an Apple Silicon app archive. That packaging result does
not establish native interaction or final-head acceptance. Current-head checks
and any later results are recorded on [PR #105](https://github.com/Plintash/doki/pull/105).

## Native acceptance checklist

Use an isolated provider-free database and a fresh exact-source debug app:

1. Save Plan → Implement → Review → Archive. Open a Review issue from the project
   Board and All projects. The header says Review and checks only Review.
2. Use mouse and keyboard to move Review → Plan → Implement → Review. Return to
   Board each time; confirm the same issue/worktree/sessions remain attached.
3. Escape, outside-click, repeated opening, Back/reopen and narrow-window/long
   stage-name checks must preserve focus and readable controls.
4. Rename/reorder a stage and reopen the inspector menu. Delete a custom stage
   or reassign an unstarted issue while a menu snapshot exists; stale targets
   must not restore removed/foreign configuration.
5. Select Archive for idle history and for a connected/busy fixture. Existing
   confirmation/stop/save rules must still apply. Cancel and retry, then reopen
   into a chosen active stage without starting a provider.
6. Restart the fixture; verify stage/workspace/session identity persists.

No native screenshots, keyboard acceptance, live-provider result, release
acceptance or merge approval is claimed by this source change alone.
