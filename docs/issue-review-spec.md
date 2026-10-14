# Issue-centered Pi development and Codex review

Base: `045a120eadb142ee5d63ab549ab93c0cace65e70` (PRs 75–78 merged).

## Outcome

An issue remains the home for its brief, workspace and conversations. Pi is the
normal development path; Codex provides an explicit review step using its native
app-server review workflow. No fixed review board column or automatic issue
completion is imposed.

## Workflow

- The issue offers Develop with Pi and Review with Codex. Both prepare a visible
  draft; sending it is the existing action that starts a provider. Repeated
  clicks reopen the matching unsent draft without replacing user edits.
- Review scope is explicit: all uncommitted changes (including untracked files),
  changes against a named base branch, one specified commit, or an exact base..head commit range. Branch and commit
  scopes require a nonblank reference. The selected scope is visible in the
  composer and is included by the transport submission, separately from editable
  issue/context text.
- A review uses the issue's existing concrete checkout/worktree. A planned but
  uncreated worktree must be developed first, or changed to an existing workspace.
  Review does not create a different worktree, copy a Pi provider cursor, or
  silently change an existing session's workspace.
- The fresh Codex conversation contains the issue title/description and available
  final development response as context. Codex receives a native inline
  `review/start` with a custom target containing the explicit scope and brief.
  This uses the existing adapter; it is not a second generic prompt adapter.
- Review status and a bounded result preview are visible on the issue card and
  detail. Full output remains in the review transcript. Only actual review output
  plus successful provider settlement establishes Review complete; idle, failed
  startup, interruption and a normal answer do not establish a completed review.
  Completed does not mean approved, and never moves an issue to Done.
- Fix with Pi prepares a new Pi draft in the reviewed workspace, under the same
  issue, containing the issue brief, review target and full findings. It leaves
  other conversations and their unsent drafts intact. Repeated clicks reopen the
  same correction draft. Develop with Pi reuses only an ordinary unsent Pi draft
  matching the issue's current project and workspace; correction drafts stay
  under their explicit Fix with Pi action. Their source-review identity lives on
  the correction session itself, so rerunning or removing the original review
  cannot silently turn an old correction into a movable development draft.
  Another review can follow after fixes.
- Review metadata, result links, correction links and drafts survive restart and
  catalog/hydration races. Existing state migrates without losing history.
- Existing permissions/authentication and provider errors remain visible. No
  model run is required for provider-free protocol and native acceptance.

## Verification

Independent specification and Standards/Fowler review apply to the committed
nonempty diff. Run relevant Rust model/persistence/driver/UI tests, generated
protocol and TypeScript checks, formatting and debug/release workspace checks.
Use only synthetic isolated data and a provider-free app-server fixture for
native screenshots and end-to-end lifecycle checks. Do not present that as a
real authenticated Codex review, macOS acceptance, cross-device acceptance or CI.

## Codex option semantics

Explicit Doki model selections also set thread-local `config.review_model`,
since Codex otherwise prefers its configured review model. This changes neither
user-global configuration nor ordinary turn models. An unspecified model
preserves Codex's configured review default. After native review, changing the
model, effort or service tier reopens/resumes the existing Codex thread before
the next review, applying those options to native review as well.
