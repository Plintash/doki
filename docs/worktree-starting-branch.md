# Worktree starting branch

Open a project’s sidebar menu → **Project settings** → **Worktree** →
**Default starting branch** to configure that project’s optional default.
The dialog is fixed to the project it was opened for; it does not switch the
active conversation or change an issue’s project binding. The current-project
composer menu also opens these settings when the sidebar uses another grouping.
Local and remote-tracking branches are separate choices, stored
as `refs/heads/...` and `refs/remotes/...`; remote-tracking refs are the repository's
existing local Git metadata, not a fresh network read.

Each project has its own choice from all of its local and remote-tracking
branches, including `develop`, `dev`, and `release/1.0`; there is no
`main`/`master` restriction. For example, one project can default to local
`develop` while another defaults to remote `origin/dev`.

For a planned worktree, the composer branch picker chooses a one-off starting
branch. **Project default** clears that override. The effective order is:

1. The draft's explicit starting branch, including an inherited issue branch
2. The project's configured default
3. Automatic: the local branch named by `origin/HEAD`, otherwise that
   remote-tracking ref; without `origin/HEAD`, the current branch or detached HEAD

Branch discovery and worktree creation use the same validated automatic resolver.
A missing explicit ref is an error; it never silently falls back. A project change
applies to a draft without an override when first submitted. An already-created
issue worktree continues to be reused. No directory is created merely by changing
settings or toggling the draft checkbox.

Creation still uses `git worktree add -b waku/<slug> <path> <canonical-ref>`, so
Git's existing upstream-configuration behavior is preserved. It does not fetch,
pull, switch the source checkout, or copy staged, unstaged, or untracked files.
Only committed files from the starting ref are present in the new worktree.

## Regression coverage

- Core real-Git fixtures: distinct local/remote commits, legacy local choices,
  missing/invalid refs, automatic remote fallback, detached/unborn HEAD, dirty
  source/index preservation, no fetch, upstream configuration, subprojects and
  name collisions
- Protocol/persistence: old JSON/database defaults, exact ref round trips,
  override precedence, reset/reopen and independent project settings
- Daemon catalog: setting changes reach other clients; unchanged saves stay quiet
- App helpers: displayed automatic base agrees with the daemon, missing refs stay
  visible, issue draft defaults and inherited explicit branches remain distinct

## Native acceptance

In a freshly rebuilt signed app, open project settings from the sidebar project
menu and the current-project composer menu. Configure two projects differently,
reopen each project’s dialog, and confirm persistence without changing the active
conversation, issue binding, or draft text. Confirm the control is absent from
application General settings. Test repeated Tab/Shift+Tab stays within the dialog,
including from an open dropdown; Escape dismisses the menu before the dialog, and
closing returns focus to the initiating project control. For a draft and a new-worktree issue, try the
project default, local and remote overrides, and reset. Verify both click and
keyboard menu paths, search, Escape/reopen, project switching, and repeated clicks.
With a dirty source checkout, submit and compare the new worktree's commit with
the displayed starting ref; confirm the source checkout and files are untouched.
Delete a selected ref before submission and confirm the draft reports the error
without starting the provider or choosing another base. This checklist is not a
claim that native visual acceptance has run.
