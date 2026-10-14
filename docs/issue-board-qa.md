# Issue-board runtime QA fixture

The fixture uses the actual debug GPUI app, the actual TCP/WebSocket daemon,
and the daemon's embedded database migrations. It is synthetic test data only.

## Isolation and startup

Debug builds use the compile-time `<checkout>/temp/app.db` path in both
`waku-core` and `waku-client`; changing `HOME` alone does not change it, and
`WAKU_DB_PATH` is not supported. `scripts/issue-board-fixture.py` refuses any
pre-existing `temp/` directory unless it has this fixture's exact ownership
marker. It also refuses a symlinked directory/database and refuses to reseed
when a recorded app/daemon run or a non-fixture database row is present.
A filesystem lock prevents concurrent fixture operations across process namespaces.

All projects, worktrees, home/config/cache/runtime directories, settings, logs,
and screenshots stay under the fixture-owned `temp/`. No existing developer
history is copied. The runner drops inherited credential environment variables,
disables all providers, and sets every provider binary override to a nonexistent
fixture path. Its shell-environment probe uses a local no-profile wrapper. Every
conversation is idle, with no resume cursor, queued prompt, or provider runtime.
The conversations are labeled Pi, but do not launch or install any Pi version.

Build the latest app and daemon before seeding:

```sh
source /workspace/shared/doki-toolchains/env.sh
cargo build --locked --workspace
python3 scripts/issue-board-fixture.py prepare
python3 scripts/issue-board-fixture.py seed
python3 scripts/issue-board-fixture.py verify
```

`seed` briefly launches the official daemon with `--bind 127.0.0.1:0` and
`--parent-pid`, waits for every checked-in migration, then stops it before writing
fixture rows. It requires the issue-description migration. `verify` starts a
fresh daemon, reads all three issues/five sessions over authenticated loopback
RPC, hydrates every transcript, and confirms that attaching each session finds
no provider runtime. It also checks the unsent issue-bound draft and its saved text. It requires the Python `websockets` package; this QA
execution environment already provides it.

For this cloud desktop, use its supported native Terminal and retain its normal
DISPLAY=:0 and XAUTHORITY. The local lavapipe driver was required for actual
painting; the system-default renderer produced X11 shared-memory errors. Set
these only for the fixture launch (they are preserved by the runner):

```sh
export LIBGL_ALWAYS_SOFTWARE=1
export VK_ICD_FILENAMES=/workspace/shared/doki-toolchains/native/usr/share/vulkan/icd.d/lvp_icd.json
export VK_DRIVER_FILES="$VK_ICD_FILENAMES"
```

When a working native display is available, run:

```sh
python3 scripts/issue-board-fixture.py run
```

The runner prints process IDs, connects the app to its explicit loopback daemon,
and retains ownership until the app closes or the runner receives Ctrl-C/SIGTERM.
It cleans up only those child processes. Do not launch `scripts/dev.ts`: its
normal first-run inheritance behavior is intentionally bypassed here.

## Deterministic contents

IDs use `9e5e` fixture prefixes and timestamps are anchored to 2026-10-13 12:00
UTC. The manifest at `temp/issue-board-qa/manifest.json` records exact IDs.

- Orion: an ordinary Git checkout plus `feature/issue-navigation` worktree
- Atlas docs: a second ordinary Git checkout
- Backlog: “Clarify empty-state guidance”, with a description and zero conversations
- Doing: “Keep issue context across conversations”, with two Pi conversations and a durable unsent follow-up draft in the Orion worktree
- Done: “Publish contributor checklist”, with a retained Pi conversation in Atlas docs
- Unfiled: “Unfiled documentation question”, a Pi conversation in Atlas docs

Each started conversation contains one completed synthetic turn and two messages.
The unsent follow-up has zero turns/messages, is_draft=1, and a session-keyed
composer draft to verify restart persistence.
No fixture process can discover or start an installed provider through its
configured binary overrides. Do not send a prompt, enable a provider, open a
terminal, or choose an external project while performing this UI-only pass.

## Screenshot and interaction milestones

Capture actual app pixels only. If Linux display startup is unavailable, record
that blocker; source code, mockups, and fixture RPC checks are not visual QA.

1. Board overview: issue cards group by Backlog/Doing/Done, show readable descriptions/counts/context, and the unfiled conversation stays separate
2. Empty issue detail: open “Clarify empty-state guidance”; verify title, description, zero-conversation state, and a clear new-conversation action
3. Populated issue detail: open “Keep issue context across conversations”; verify Orion, the worktree branch, and its related conversations/draft without automatically opening a chat
4. Draft inheritance: click new conversation from the populated issue; verify the issue binding, Orion project, and worktree are inherited before entering or sending a prompt
5. Return and archive: navigate back to the issue, open the completed issue and archived transcript, and verify completion retained its discussion
6. Actions: exercise a synthetic issue title/description edit, a state move, and restoration from Done if exposed; verify those mutations survive an app restart
7. Keyboard: navigate the issue list/detail controls with Tab/Enter/Escape; confirm a visible focus indicator and no trapped focus

Store captures in `temp/issue-board-qa/screenshots/`, preferably named
`01-board.png`, `02-empty-issue.png`, `03-issue-detail.png`,
`04-inherited-draft.png`, `05-completed-issue.png`, and `06-restored-issue.png`.
Logs are in `temp/issue-board-qa/logs/`. The successful RPC snapshot is
`temp/issue-board-qa/verified-state.json`. `running.json` exists only while this
runner owns live processes. Stop the app/runner before reseeding.

## Recorded Linux runtime pass (2026-10-13)

The actual GPUI app painted on the supported cloud desktop using the local
lavapipe ICD. Screenshots 01–08 are app-window captures, not reconstructed UI.
The running build was refreshed after migrations 0005 and 0006 were embedded.

Verified:

- The real daemon applied migrations, decoded all fixture rows, hydrated the four started transcripts, and returned no provider runtimes when each session was attached
- Migration 0006 backfilled all three Orion worktree session projections; branch labels were correct immediately after a fresh app launch
- Both the zero-conversation issue and populated issue opened as issue details rather than chats
- The empty issue required a project before enabling New session; keyboard Down/Enter selected Orion from the project menu
- New session on the populated issue resumed its existing unsent draft, retaining its custom text and concrete worktree
- New session on the newly assigned empty issue created a new durable issue-bound draft, copied its full title/description into the composer, and retained Orion's local workspace
- The completed issue kept its archived transcript readable; Reopen restored its conversation into the active sidebar, and Complete hid it again without deleting it
- A title edit and a multiline description edit saved to SQLite, with visible saving feedback
- The message count remained eight throughout these actions; no prompt was submitted

Extra captures: `07-new-issue-draft.png` shows the freshly inherited prompt;
`08-archived-conversation.png` shows the retained completed transcript.

The pass intentionally mutates only the synthetic fixture. After exercising
New session, the original backlog issue has a durable unsent draft and is In
progress. The completed issue was renamed with a ` QA` suffix and its description
ends with `QA checked.` to make restart persistence easy to verify. The baseline
`verify` command asserts the freshly seeded five-session fixture; it is not a
post-interaction assertion for this six-session state. After stopping the runner,
use `python3 scripts/issue-board-fixture.py verify-current` to verify the mutated
fixture via a fresh daemon, including zero runtimes and the unchanged eight-message
count. The seeder refuses to
silently erase the newly generated UUID.

One environment-specific issue remains outside the board feature: closing the
native Linux window triggered an upstream GPUI X11 `RefCell already mutably
borrowed` panic (exit 101). The runner still cleaned up its daemon and lock.
The saved log is `temp/issue-board-qa/logs/app-close-panic.log`. Ordinary board,
detail, draft, and archive interactions were usable. No upstream renderer source
was changed for this test.

## Isolated Pi 1.0.4 initialization smoke test

Separately from the provider-disabled UI fixture, the official Pi CLI 1.0.4 was
installed in an isolated cloud prefix with install scripts disabled. A bare CLI
pin initially resolved transitive Pi-family components to 1.2.0, so the final
manifest pins all eight Pi-family packages at 1.0.4 before execution. The CLI
reports `pi --version` as `1.0.4`.

Real RPC `get_state` and `get_commands` requests both succeeded in an empty,
isolated HOME and working directory with no inherited credentials and with
tools, extensions, MCP, and context discovery disabled. State reported
`isStreaming=false`, zero messages, and zero pending messages; stderr was empty
and no model prompt was sent. The binary is not on the normal PATH, so the
provider-disabled UI fixture remains inactive.

This verifies executable and RPC initialization only. Authenticated model
execution and end-to-end provider jobs were not exercised. Evidence:
`/workspace/shared/doki-toolchains/pi-rpc-smoke.json`; installation manifest:
`/workspace/shared/doki-pi-1.0.4/package.json`.

A separate real-provider lifecycle test also passed against the final core:
`server::tests::confirmed_shutdown_real_pi_exits_without_a_model_prompt`.
It used a temporary database/cwd and an environment-cleared empty-HOME wrapper
to start the pinned Pi 1.0.4 RPC process, then obtained a positive
`CloseSessionAndWait` acknowledgment and `AttachSession(None)`. This proves
confirmed process shutdown without any model prompt; it does not establish
authenticated model execution. Evidence:
`/workspace/shared/doki-toolchains/test-pi-shutdown.log`.

## Redesign runtime pass (2026-10-13, 19:11 UTC build)

The initial redesigned header failed before its first frame because a shared
button and its caller both registered GPUI hover styling. The duplicate was
removed and the rebuilt app painted correctly. Evidence of the original
failure is retained in `logs/app-header-hover-panic.log` inside the QA directory.

Verified in the real app:

- The compact single header, three board columns, descriptions, latest visible progress, and compact conversation rows render
- Choosing Orion filters out Atlas issues; opening an issue and returning to Board retains Orion scope
- A nonmatching search returns zero cards; clearing it restores the scoped cards
- Running and Needs attention filters select correctly and show zero cards for this intentionally idle fixture
- New issue from the Orion-scoped board inherits Orion and a planned New worktree default
- The new issue's first unsent draft has Create worktree checked; unchecking changes the durable draft workspace to Local, and returning to the issue then reopening New session preserves that choice without duplicating the draft
- On the original materialized-worktree draft, checking and unchecking restores the issue's concrete `feature/issue-navigation` worktree and preserves its unsent text
- These controls created no directories or worktrees: Orion still has two Git worktrees, Atlas one; the project root still has exactly the original three directories
- SQLite still has exactly eight messages; no provider prompt was submitted

This pass added the synthetic issue `Check isolated worktree planning` and its
unsent draft, resulting in four issues and seven sessions. Screenshot
`09-worktree-choice.png` records the initial checked choice. Screenshots 01 and
03 were refreshed for the redesign, although 03 predates the subsequent shared
alignment correction and must not be used as final alignment evidence.

Keyboard activation remains under investigation in this build: Tab/Shift+Tab
visibly focuses the checkbox, but bare Space and Enter did not change its
durable workspace. Mouse activation works. A later build and separate settled
UI/DB checks are required before marking keyboard operation as passed.

## Final interaction pass (2026-10-13, 19:27 UTC build)

- The checkbox keyboard defect is fixed: after Tab/Shift+Tab focus, Space changes NewWorktree to Local and Enter changes Local back to NewWorktree. Separate settled SQLite reads confirmed both transitions, with eight messages throughout
- The previous Local choice survived process restart before the keyboard test
- Title, description, section header, and conversation rows share the same edges at native client widths 980, 1178, and 1264 pixels, with sidebar both shown and hidden
- From a hidden-sidebar issue detail, Ctrl+N then Escape restores focus to the visible Board button; Enter returns to the board and focuses New issue
- Manual state selection moved the synthetic planning issue to To do; its new description saved normally. The final board therefore shows one To do, two In progress, and one Done issue
- Final native screenshots 01, 03, and 05 replace the earlier Library versions. Local captures 10–12 preserve the narrow and hidden-sidebar alignment checks

These tests retain the original eight transcript messages and never submit a
provider prompt. Checked worktree state remains only a plan; all original Git
worktree lists and the three project directories remain unchanged.

## Bounded-menu and attach/detach checkpoint (6587b56)

The real native three-row project/workspace menus passed Home/End navigation
and Enter selection; Escape dismissal restored visible chip focus. Workspace
Home/Enter changed the synthetic planning issue and its unsent draft to Local;
End/Enter restored NewWorktree. The one-row Attach menu remained readable with
Home, End, Page Up, and Page Down, reopened by Enter after dismissal, and
accepted a mouse selection. This is short-menu interaction coverage, not
evidence about 10,000-row rendering or scrolling.

Attaching the synthetic started unfiled Atlas conversation to the Done Atlas
issue automatically reopened the issue with a toast and restored both
conversations into the active sidebar. Complete archived both. Detach from
the archived row returned the conversation to Unfiled while leaving the issue
Done. Both original question/answer texts remained visible; SQLite retained
two messages for that conversation and eight overall. A fresh daemon
`verify-current` passed after clean runner shutdown. Screenshot 13 records the
temporary two-conversation attached state; the final fixture is restored to
one retained Atlas conversation plus the original unfiled conversation.

## Async completion preflight pass (7b71217)

With no resident runtimes, one Board Reopen action restored the Atlas issue.
One Board context-menu Complete action then completed it after asynchronous
preflight, without another click. A single detail Reopen followed by a single
detail Complete also finished correctly. Separate settled screenshots and
SQLite reads confirmed the transitions and active/archive sidebar changes;
all eight messages remained intact. No confirmation dialog was required for
this no-runtime fixture. This does not exercise stopping a live model job.

The final resident-runtime confirmation predicate build was retested at
20:02 UTC: one Reopen and one Complete click on the Atlas issue succeeded
without an unnecessary confirmation or repeated click. The issue returned to
Done with its conversation archived. After clean runner shutdown, the final
fresh-daemon `verify-current` passed with four issues, seven sessions, eight
messages, and zero provider runtimes. The fixture app and daemon are stopped.

A further fresh-process check opened the archived Atlas transcript after the
Attach-before-hydration change. Both original user and assistant messages
rendered correctly, and the archived conversation stayed out of the active
sidebar. This verifies retained-history access, not provider execution.

## Final automated acceptance (b1d7af0)

All checks below passed against the final Rust/source inputs. Source hashes
were checked before and after the run to exclude edits during compilation.

- Protocol: 91 tests
- Core: 118 focused tests: persistence 64, real Git worktrees 2, daemon 16,
  OpenCode owned-session shutdown 1, server 34, and the separately enabled
  real Pi 1.0.4 shutdown test 1
- Client: 32 tests
- Application: 120 unique focused tests, including issue lifecycle, workspace
  choice/draft recovery, hydration revisions, sidebar reachability, text input,
  long detail lists, and the 10,000-row menu keyboard/virtualization regression
- TypeScript shared client, Web, and Mobile: 323 tests across 47 files,
  1,017 assertions, zero failures; all three typechecks pass
- Workspace build and check, Rust formatting, `git diff --check`, and generated
  TypeScript bindings freshness all pass

This is 361 focused Rust tests, not an unfiltered full-workspace test run.
The server suite normally ignores the real-provider test; it was run separately
against the isolated pinned Pi installation rather than counted as a skipped
acceptance check.

Reproduction entry points:

```sh
source /workspace/shared/doki-toolchains/env.sh
cargo build --locked --offline --workspace
cargo check --locked --offline --workspace
cargo fmt --all -- --check
bun run --cwd packages/waku-client check
bun run --cwd apps/web typecheck
bun run --cwd apps/mobile typecheck
bun test packages/waku-client/src apps/web/src apps/mobile/src
```

The cloud verification prefix retains the exact test runners, per-suite logs,
and `verification-summary.json`, which lists the 120 unique application test
names. The TypeScript run is in `test-typescript-final.log`. Test compilation
uses `CARGO_INCREMENTAL=0` after an interrupted incremental cache was recovered.
Existing unused-code/import and ts-rs attribute warnings remain.

Coverage limits: native interaction used isolated synthetic data on Linux;
macOS was not exercised. Real Pi executable/RPC initialization and process
shutdown were verified, but no authenticated model execution or model prompt
was performed. The Linux window-close issue documented above remains outside
this Board change. Independent final Standards and Specification reviews both
have zero actionable findings; see [issue-board-review.md](issue-board-review.md).
