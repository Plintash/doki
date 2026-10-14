# Project registration and picker deduplication

Issue: <https://github.com/Plintash/doki/issues/81>.
Final base: `8d10896a633c3830f816561e833f82de7cdc9db0` (PR #82 merged).
The reviewed deduplication changes carried forward from `2cc04ad` without conflicts;
native verification uses the corrected shared dropdown layout.

## Identity boundary

- Re-registering a local directory compares the selected record first, then
  existing records. Exact paths reuse their ID; successful canonicalization
  also recognizes local symlink and parent-component aliases.
- An unresolved path keeps exact-path identity. Remote daemon paths are never
  canonicalized against the desktop filesystem. Catalogs remain daemon-scoped.
- No repository-root inference, basename match, case folding, or worktree-root
  merging is performed. Distinct directories and worktrees remain distinct.
- A local registration remains in memory across an older daemon catalog until
  a catalog acknowledges its ID. After acknowledgment, catalog omissions are
  authoritative again. This protection tracks only registrations made by the
  current client; it is not a permanent union of project catalogs.
- No stored records are removed, migrated, renamed, or rebound. Existing stored paths
  and project IDs remain untouched. Re-adding an issue's current project is a
  no-op, preserving its Local or materialized worktree choice.
- Composer, empty-state, issue, and Board-filter project menus collapse old
  exact-path duplicate rows. Their selected ID wins over other registrations.
  Legacy aliases whose stored paths differ are not guessed equivalent by the
  renderer. Canonicalization is limited to the explicit registration action.
- Board filtering includes every historical ID for the chosen exact path.
  That ID set is computed once in the existing per-frame cache; each issue
  performs a set lookup. Picker projection is lazy and performs no I/O.

## Review

Method: independent Standards and Specification passes using
[Matt Pocock's two-axis code review](https://github.com/mattpocock/skills/blob/main/skills/engineering/code-review/SKILL.md).

Initial review found one Standards issue (per-issue project catalog scans) and
two Specification issues (draft transfer escaping the composer scope, and an
omitted empty-state project picker). All were repaired. The reviewer then
reported zero remaining Standards and zero Specification findings. Existing
draft/navigation semantics are retained; this change does not add a new draft
transfer behavior. A subsequent independent pass reviewed the pending-registration
catalog race fix and also reported zero findings on both axes.

The final QA fixture and native script were re-reviewed after correcting a
fixture persistence assumption. Both review axes again reported zero findings.

## Verification

Verified on 2026-10-14 in an isolated cloud Linux checkout, using Rust 1.96.0
and Bun 1.4.3. The watcher launched the actual GPUI app with a dedicated
provider-disabled daemon, fixture database, and HOME. No prompt was submitted.

- `cargo fmt --all -- --check` and `git diff --check`: pass.
- `cargo test --locked --offline -p waku --lib -- --test-threads=1`:
  661 passed, 0 failed, 2 ignored, including all six new project identity,
  pending-catalog race, and binding-preservation regressions.
- `cargo check --locked --offline --workspace` and the same command with
  `--release`: pass.
- Existing RPA/semantic regression suites: 47 passed, 338 assertions.
- The focused native test passes strict standalone TypeScript checking.
- `scripts/rpa-project-dedup-live.test.ts` with
  `WAKU_RPA_PROJECT_DEDUP_TEST=1`: 1 passed, 22 assertions; a second run against
  the same app also passed. It verifies composer, empty-state, issue, and
  Board-filter choices; selected draft identity/text/workspace; inclusion of
  both legacy issue bindings in the collapsed filter; unchanged catalog and
  durable conversation bindings in memory and SQLite.
- Fresh native executor: `6e8ce8ff-6542-4689-a37a-9ae82172c9c4`, build
  `0b16fd76b938c5e7-18de59865985633d`, protocol 18. The checkout and database
  matched the fixture before the run. Corrected shared dropdowns from PR #82
  were present in composer, issue, and Board screenshots and visually inspected.
- The fixture contains four project records but three directory identities.
  Two remaining choices intentionally have the same display name but different
  paths; the independent worktree also remains a separate choice.
- Clean quit verified watcher exit 0, stopped watcher/daemon, and removal of
  the running marker.

### Test corrections and boundaries

The first native run passed the picker and in-memory assertions but failed
its final SQL comparison because the fixture had seeded an unstarted standalone
session as durable history. Existing persistence intentionally prunes those
transient IDs; draft text is stored separately. The corrected fixture seeds a
completed static conversation and creates the draft through native `newSession`.
The completed conversation keeps its concrete worktree and project binding;
both editable project controls are checked on the new draft. No product fix was
made for this fixture error. One intervening run lost its executor when the
watcher correctly relaunched after a script edit; verification resumed only
after confirming the fresh executor above.

- Registration aliases and remote-path isolation are helper-test coverage;
  the native script covers the actual pickers/filter and checks both in-memory
  catalog data and read-only SQLite binding columns. It does not claim restart
  or OS folder-dialog coverage.
- macOS native folder-dialog behavior is not verified by cloud Linux QA.

The original private screenshot was inspected locally and is not included in
this repository or the public issue.
