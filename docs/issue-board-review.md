# Issue Board review record

Review method: [Matt Pocock's two-axis code review](https://github.com/mattpocock/skills/blob/main/skills/engineering/code-review/SKILL.md).
Standards and specification are reviewed independently. Repository rules override
code-smell heuristics; heuristics do not justify speculative abstractions.

Fixed base: `c291e58f1711b68a21c3056223e9a6123cb0dcdf`.
Feature specification: [issue-board-spec.md](issue-board-spec.md).

## Round 1

Reviewed commit: `40c71a7` (`Complete issue-driven board and isolated session workflow`).
Diff: `git diff c291e58f1711b68a21c3056223e9a6123cb0dcdf...40c71a7`.

### Standards

1. Inspector rows and attachment choices used `session.title` rather than
   `display_title()`, violating the provider-title precedence documented in
   `docs/titles.md`. Use the shared display title resolver.
2. With the sidebar hidden, dialog dismissal targeted the removed
   `board-new-task` control, and returning to Board targeted hidden sidebar
   focus. Restore focus to the mounted New issue control instead.
3. Inspector row rendering rebuilt and sorted complete session-derived menu
   choices even with menus closed, contrary to `AGENTS.md`'s frame-work rule.
   Prepare those choices only while their native menu is open.
4. Judgment: the `start_task_session(new_worktree: bool)` override had no true
   caller after workspace intent became explicit. Delete the unused parameter
   and branches rather than retaining a hypothetical path.

Summary: 3 documented-standard findings and 1 judgment finding. The most severe
standard issues were unreachable keyboard focus and unnecessary per-frame work.

### Specification

1. Foreground stop optimistically set Idle and immediately allowed Done;
   cancel/close notifications did not prove process shutdown. Completion must
   wait for explicit exit evidence, and timeout/rejection must leave the issue
   active without treating missing observation as success.
2. A stale hydration response could overwrite a just-changed project/workspace
   while preserving the newer revision. Both hydration paths must retain newer
   catalog metadata while loading transcript detail.
3. Submitting/queueing/streaming could reset an advanced binding revision to wall
   time, allowing a stale catalog to unfile the session. Normal mutations must
   preserve the existing revision; metadata edits must still advance it.
4. Existing explicit goal activation could start work on an archived issue
   without reopening it. Reopen for active-goal starts, but not refresh, pause,
   clear, or passive provider events.
5. The draft's only Detach menu action was disabled, making the existing
   explanatory message unreachable. Activation should explain why the unsent
   draft remains attached rather than silently doing nothing.

Summary: 5 specification findings. The most severe were unconfirmed foreground
shutdown and stale hydration changing the execution workspace.

### Native QA findings

These were found by running the actual application rather than by compilation:

- Duplicate hover styles in composed header controls caused a first-frame GPUI
  assertion. Let the shared control own its hover/pressed behavior.
- The worktree checkbox needed a focusable handle and a scoped keyboard action.
- The editor and session rows had separate centering containers with mismatched
  edges. Use one shared width/padding container for the entire inspector.

Fixes are being validated with another committed review round and native UI
checks. This record does not mark pending fixes or model execution as passed.

## Round 2

Reviewed commit: `bd75a7c` (`Fix issue lifecycle, revision races, and native board interactions`).
Diff: `git diff c291e58f1711b68a21c3056223e9a6123cb0dcdf...bd75a7c`.

### Standards

1. Hidden-sidebar inspector dialog dismissal still selected the overview's
   unmounted New issue control. Select the mounted inspector Back control.
2. Project, workspace, attachment, and overflow-attention menus were unbounded
   collections. Use an opt-in virtualized native collection menu with bounded
   height, lazy opening snapshots, and conventional keyboard navigation.

Summary: 2 documented-standard findings; no additional actionable smell.
The most severe was an unbounded collection making later choices inaccessible.

### Correctness follow-through

The standards reviewer also noticed two correctness edges, recorded separately
rather than treating them as standards violations:

- A cold catalog's placeholder provider settings must not replace hydrated
  settings. Only a loaded local detail can own newer provider-setting edits.
- OpenCode uses a shared service, so closing its adapter is not process-exit
  evidence. Confirm its owned session has no active or queued work; retain the
  adapter on rejection, including replacement and rewind teardown.

Completion follow-through additionally keeps a rejected Done save active while
preserving concurrent title/description edits, and rejects stale rollback
acknowledgments after a newer reopen/completion. The daemon retains the shutdown
evidence barrier across a desktop restart with an optimistic Idle snapshot.

### Native checks

- Worktree checkbox mouse, Space, Enter, Tab, and Shift-Tab passed, with separate
  persisted-state reads confirming planned worktree versus Local intent.
- Inspector sections share edges at client widths 980, 1178, and 1264 with the
  sidebar shown or hidden. Hidden-sidebar dialog dismissal and Back focus pass.
- Fresh fixture verification retains seven sessions/eight messages, all issue
  states and workspace intentions, zero provider runtimes, and unchanged Git
  worktree lists. No model prompt was sent.

The next full two-axis committed review and final collection-menu native pass
are pending. Test and screenshot details are in [issue-board-qa.md](issue-board-qa.md).

## Round 3

Reviewed commit: `6587b56` (`Bound issue pickers and confirm completion persistence`).
Diff: `git diff c291e58f1711b68a21c3056223e9a6123cb0dcdf...6587b56`.

### Standards

No remaining actionable documented-standard violations or judgment-based code
smells. The reviewer checked the earlier focus, collection virtualization,
title-precedence, closed-menu work, and unused-parameter fixes. Shared-service
shutdown retains the user's OpenCode process and runs off the UI thread.

Summary: 0 Standards findings.

### Specification

1. P1: the local preparation barrier covered normal submission but omitted goal
   startup. Runtime attachment misses could mark a legitimately preparing goal
   orphaned, allow completion, then let the prepared goal reopen and run. Cover
   local preparation registries in completion and attachment/orphan guards.
2. P1: the daemon accepted a current-cursor incoming Idle projection over its
   Working snapshot when no close had been requested, allowing Done while the
   runtime remained live. Require daemon-owned settlement evidence for linked
   live runtimes rather than client-written Idle.

Summary: 2 Specification findings; both violate the required completion boundary.
These findings are being repaired and will receive another committed review.

### Automated UI finding

The 10,000-item collection-menu regression found that End did not expose the
last row. The native list's keyboard scrolling must pass before acceptance;
the assertion is retained rather than relaxed.

### Follow-through

The revised completion path checks submission, goal, and fork preparation in
both completion and attachment/orphan handling. One asynchronous preflight
joins missing controls, including Idle catalog entries, and automatically
continues the original action. Absence is scoped to that attempt; membership
changes, lost controls, stale callbacks, and closed windows cannot silently
complete it. The daemon independently rejects Done while any linked runtime
remains live. The old second-click attachment probe path was removed.

The collection list now uses measured variable row heights and logical-index
scrolling for unpainted destinations. Pinned GPUI clears initial height hints
when its width is established, so pixel-offset reveal alone cannot jump to a
never-painted distant row. The long-list target, visibility, and bounded-render
assertions remain; the test enters keyboard mode before measuring a jump so
its accounting excludes GPUI's preceding mouse-to-keyboard redraw.

The fixes are awaiting the next committed two-axis review and fresh tests.

## Round 4

Reviewed commit: `7b71217` (`Close completion startup races and fix distant menu navigation`).
Diff: `git diff c291e58f1711b68a21c3056223e9a6123cb0dcdf...7b71217`.

### Standards

No actionable documented breaches or judgment-based smells. Shared preparation
guards, attempt-scoped completion evidence, background attachment, and pinned
GPUI logical-index scrolling satisfy the relevant repository guidance.

Summary: 0 Standards findings.

### Specification

1. P1: a newly attached driver's forwarding thread can still be delivering
   replay after the attachment callback. Draining the currently queued UI
   events does not establish that the runtime is quiet. An Idle catalog could
   therefore bypass the required stop confirmation before its live runtime is
   closed.

Summary: 1 Specification finding, limited to the stop-confirmation boundary.
The earlier preparation and daemon-settlement findings are resolved.

The narrow follow-through uses the existing confirmation whenever a linked
runtime is resident, in addition to known busy/background work. Copy describes
connected or running conversations rather than pretending every connection is
busy. Idle history with no runtime still completes directly. No replay-readiness
protocol, timing heuristic, or provider redesign is introduced.

The committed checkpoint passes all 117 focused application tests, including
the 10,000-row menu regression. The confirmation adjustment awaits its final
tests and committed review.

## Round 5

Reviewed commit: `b1d7af06388fd2a160f3392961d97539bd18c19c`
(`Confirm connected runtime shutdown before issue completion`).
Diff: `git diff c291e58f1711b68a21c3056223e9a6123cb0dcdf...b1d7af0`.

### Standards

No actionable documented violations or judgment-based code smells. The
confirmation decision and count share one clear predicate, obsolete logic is
removed, and the revised copy accurately describes connected or running
conversations. Earlier standards fixes remain intact.

Summary: 0 Standards findings.

### Specification

No actionable specification findings. Every linked resident runtime requires
the existing stop confirmation, with matching count and wording. Idle history
without a runtime retains one-click completion. The reviewer found no additional
missing requirements, scope creep, or incorrect implementation in this rereview.

Summary: 0 Specification findings.

Both results are independent read-only reviews; runtime and automated test
evidence is recorded separately in [issue-board-qa.md](issue-board-qa.md).
