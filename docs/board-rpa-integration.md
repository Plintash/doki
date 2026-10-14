# Board and RPA integration

## Inputs and scope

This branch combines the issue-driven Board at
`12e9e3b18bc9f37d126da194c8266f589657dc78` and the RPA P1 infrastructure at
`83a52a189f530c9c22a712af4aac54c884ff16cf`. Their shared ancestor is
`3c3309c6abb27b71c0930cf2c614ec3717e98693`. Neither input branch is rewritten.

The requirements remain [the Board specification](issue-board-spec.md) and
[the RPA change](../openspec/changes/add-rpa-bridge/proposal.md), with these
integration clarifications:

- An issue (`Task`) is durable project/workspace context and can own many
  conversations (`AgentSession`). They are different UUID domains.
- RPA `tasks` / `task`, `openTask` and `newTask` refer to issues. `sessions`,
  `openSession` and `newSession` refer to conversations. `newSession.taskId`
  uses the existing issue-session creation path, including its persisted
  draft and worktree behavior. UUIDs are validated in their own domain.
- Diagnostics use `sessionId`, including log filters and timelines. Their
  correlation is with the provider conversation/turn, not the issue that may
  later be attached or detached. CLI `--conversation` distinguishes that UUID
  from `--session`, which is the automation attachment.
- Protocol **16** fences both earlier Board-only 13 and RPA-only 15 clients.
  Issue persistence, confirmed shutdown, automation, and diagnostics all use
  the same generated contract.
- Ordinary persistence keeps started sessions and issue-bound drafts. Scratch
  seeding may additionally keep empty unfiled sessions. The combined predicate
  is `include_empty || has_started() || task_id.is_some()`.
- Scratch seeds can supply issues. They validate projects and matching
  bindings, clear references to issues not supplied in the seed, and replace
  concrete foreign worktree paths with local scratch project context. No
  seed grants permission to modify its source checkout.
- Board mutations notify RPA's model observer without doing work from render.
  Both asynchronous issue-save feedback and confirmed provider shutdown stay
  in place. Completing an issue archives conversations and preserves history;
  reopening restores them.
- Board controls expose stable semantic IDs, roles, labels, and disabled
  metadata through the existing GPUI tree. Domain-specific menu entries use
  project/session/workspace identity rather than a mutable row position.

## Verification boundaries

Integration verification uses only this cloud checkout and disposable,
provider-disabled native fixtures. No provider credentials or model prompts
are used. The official pinned Pi **1.0.4** may be exercised for executable/RPC
initialization and confirmed process shutdown. Pi 1.2 adaptation is later work.

The original [Board QA](issue-board-qa.md) and [RPA QA](rpa-p1-qa.md) records
are historical input evidence, not proof that the combined binary passed.
Combined-tree results are recorded below after each actual run.

RPA real-model acceptance tasks 2.7 and 3.11 remain open. The input RPA branch
left the exact development-watcher relaunch portion of 3.8 open. The combined-tree run below closes that specific gate; the historical
input record remains unchanged.

GitHub publication is outside this checkpoint because repository writes were
denied. Changes remain on a locally committed integration branch.

## Combined-tree results

At production checkpoint `1d3ada9ebe625149502372625511af3d973a7643`:

- Workspace debug check and full debug build passed
- Protocol: **104 passed**; Rust client: **46 passed**
- Core: **635 passed**, 22 ignored, 9 explicitly excluded live OpenCode checks
- App: **625 passed**, 2 ignored, serial, using a fresh isolated data profile
- REPL: **25 passed**; GPUI spike/list: **2 passed**
- TypeScript client/Web/Mobile/CLI/source: **380 passed**, 1,366 assertions
- Client/Web/Mobile and standalone automation-script typechecks passed
- Rust formatting, diff whitespace, and generated-binding freshness passed
- Release workspace compile check passed
- Actual debug native daemon/CLI/scratch/Board workflows: **8 passed**, 136
  assertions; all providers disabled, no model prompts
- Official pinned Pi 1.0.4 confirmed-shutdown test: **1 passed**, no model prompt
- Fresh-daemon fixture verification retained all synthetic issue/session rows,
  all eight original messages, and the durable unsent draft; no provider runtime

The headless app suite uses a fresh `WAKU_RPA_DATA_DIR`: an interactive native
fixture can save maximized window state, while GPUI's test window deliberately
does not implement native zoom. This isolates test inputs from the visual-QA
profile instead of bypassing those tests.

The actual `scripts/dev.ts` watcher gate passed on 2026-10-14 UTC. It launched
an isolated provider-disabled app, detected a temporary Rust source edit,
rebuilt and replaced the process, and the CLI verified a new build, PID, and
instance on the same external daemon. Actual native `waku.rpa` REPL snapshots
worked before and after, with no Computer Use configuration and an empty
script-visible environment. The probe ran from 00:08:33 to 00:10:07 UTC.
The temporary edit was restored byte-for-byte; all three original fixture DB
files were restored with matching SHA-256 hashes. The unmodified tree was then
rebuilt. This closes task 3.8 without claiming cross-daemon rediscovery.

The unmodified production tree also passed the full optimized release build
(app, daemon, and REPL; 6 minutes 9 seconds) and actual release-runtime checks:

- The release app visibly opened the Board. Across 20 registry observations it
  stayed alive without registering an automation executor. Even with the legacy
  opt-in flag supplied, it published no legacy endpoint.
- The release daemon reported protocol 16 and rejected both automation discovery
  and attachment with `unavailable`.
- The release REPL exposed neither `waku.rpa` nor its native capability and
  ignored a forged endpoint-environment input. Its script-visible environment
  remained empty.

All owned native QA, watcher, release-app, and daemon processes were stopped.
Actual combined Board, issue-detail, and inherited-draft screenshots are retained
as synthetic provider-disabled fixture evidence, rather than model-run evidence.

## Independent review

Standards and Spec are reviewed separately against the fixed common ancestor
above. Prior independent input reviews remain recorded in their respective
QA documents; integration review must inspect their interaction as well as
these explicit protocol and identity changes.

The first integration pass found actionable gaps in semantic control coverage,
menu IDs/click dispatch, and the asynchronous completion-rollback notification.
They were repaired without adding a parallel UI tree or an alternate activation
path. Regressions cover real GPUI dispatch, identity separation, scratch binding,
protocol rejection, and the no-provider native issue workflow.

The final source checkpoint
`1d3ada9ebe625149502372625511af3d973a7643` received separate read-only Standards
and Spec reviews against `3c3309c...HEAD`. Both reported **no remaining actionable
findings**; the Standards pass also reported no actionable Fowler smells. These
reviews do not substitute for the combined-tree runtime checks above.

### Native regression found during integration

The real no-provider scratch scenario exposed a teardown acknowledgment race:
a terminal run disappeared from discovery before its process and directory
cleanup finished, so a concurrent explicit removal could return too early.
The integration retains these instances privately until cleanup finishes;
concurrent removal and shutdown join the existing teardown lock. Retiring
instances still count toward capacity. A deterministic forced-race regression
checks immediate database absence and child reaping after acknowledgment.

The native issue workflow independently passed issue/conversation ID rejection,
optimistic completion and asynchronous save-failure rollback, archive/reopen
with transcript retention, empty-issue creation, semantic workspace menu
activation, and inherited/standalone draft isolation. The final eight-test
aggregate native run passed after rebuilding the teardown fix.
