# Issue-driven Board workflow

## Scope

This feature completes the Board already present at base commit
`c291e58f1711b68a21c3056223e9a6123cb0dcdf`. It makes an issue the durable home
for project context and multiple coding sessions, while preserving standalone
sessions. It does not introduce agent automation or a separate approval stage.

Product intent comes from the repository owner's requested workflow:

- “具体的session能和task/issue绑定 issue可能对应多个session”
- “issue完成 相应的session归档 … issue需要绑定workspace/project 也需要展示worktree”
- “board的初心是做一个类似trello的东西”
- The owner subsequently requested aggregate and per-project views, useful
  status directly on cards, fewer non-decision steps, and a composer worktree
  control. New issue work should use isolated worktrees by default.

Historical GitHub issues are supporting context, not additional requirements.

## User flow

1. Board is the primary organization view. It has the existing three columns:
   To do, In progress, Done. Working sessions can loop and refine until the
   user finishes the issue; no separate review/acceptance state is imposed.
2. One compact header contains All projects / project scope, issue search,
   Running and Needs attention filters, and a prominent New issue action.
   Project scope and search survive opening an issue or session and returning.
   Debug database paths and idle autosave messages do not occupy this header.
3. New issue creates an actual issue, including when it has no conversations.
   In a project-scoped Board, it defaults to that project. Its detail view
   supports title, description, project/default workspace, and linked sessions.
4. Starting a session from an issue inherits its title/description and project.
   New issue work defaults to an isolated worktree. Further sessions may reuse
   the issue's assigned concrete worktree. Existing sessions and explicit
   workspace choices are not silently moved. The composer clearly exposes
   Create worktree; toggling it only changes draft intent. Actual creation
   happens on first submission through existing preparation infrastructure.
   Failed preparation retains the draft; a created path is reflected in the
   session and issue. No extra confirmation is needed for ordinary creation.
5. A session can be attached to an issue in the same project, opened, continued,
   or detached. Unsent issue drafts keep their own persisted composer content;
   a standalone New session must never reuse an issue's draft. A draft that
   cannot safely be detached explains that limit instead of discarding text.
6. Cards show project context in aggregate view, worktree, session count, and
   aggregate live running/attention signals. One session waiting for input must
   not be hidden by another newer/running session. Attention actions open the
   specific conversation; permission, input, and failure are distinguished
   when the current runtime reports them. Available visible progress may be
   summarized, but idle does not imply successful completion. Do not invent
   commit, PR, CI, or throughput facts not present in the existing model.
7. Completing an issue archives all its conversations out of the active
   sidebar, without deleting their history. Running work requires the existing
   stop confirmation and must settle before completion; failed stops keep the
   issue active. Completed issue details and the archive retain access to
   transcripts. Reopening restores linked sessions. Explicit new work can
   reopen an issue; mere provider idle/turn-complete events cannot.
8. Standalone and unfiled sessions remain reachable. Task grouping must not
   hide unassigned conversations, drafts, or orphaned task references.

## Reliability and integration

- Descriptions, issue bindings, workspace metadata, and issue-owned drafts
  survive save/restart. Existing databases migrate without dropping history.
- Consecutive issue mutations cannot merge away because of second-resolution
  timestamps. Older catalog snapshots cannot unfile newer local bindings.
- A failed database transaction must not advance saved-state caches or leave
  a supposedly committed in-memory task snapshot behind.
- User-visible issue saves are acknowledged asynchronously; failures remain
  retryable. Frame rendering performs no filesystem, network, subprocess, or
  blocking IPC work.
- Completion/reopening invariants apply in the daemon, not only one desktop
  view. Shared TypeScript consumers remain compatible with generated protocol
  types, and an incompatible protocol is rejected explicitly.
- Use native GPUI components, existing Theme tokens, focus/keyboard conventions,
  and virtualized long lists. Do not introduce a new UI framework/token system.

## Verification

Use an isolated, clearly marked fixture database and projects, never the
owner's active application data. Verify the native app's empty issue, project
selection, inherited draft, existing-session attachment, completion, archived
transcript, reopen, persistence after restart, filters, and worktree selection.
Capture real native screenshots at useful milestones. Synthetic data is not
proof of a real provider run. If a real provider test is performed, use Pi
1.0.x; Pi1.2 adaptation is later work.

Run the relevant Rust core/client/protocol/UI tests, generated-binding checks,
TypeScript tests/typechecks, formatting, and workspace build. Distinguish test
results, static review, synthetic native UI checks, and actual provider checks.

Review the committed nonempty change against the fixed base above on separate
standards and specification axes, repair actionable findings, and review again.
