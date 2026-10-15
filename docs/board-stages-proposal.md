# Flexible Board stages: prototype and implementation proposal

Status: design proposal and provider-free native prototype, not a shipped workflow.
Base: `432df6d92cbbda6195d1ffdafd74444a7e81e695`.

## User outcome

Users arrange work as Plan → Implement → Review → Archive, add or reorder stages,
and choose a different harness for each stage. Review is one use of the same
stage mechanism. Moving backwards, repeating a stage, and running another
review must remain ordinary operations. An issue keeps its identity, brief,
workspace, and conversation history throughout.

## Prototype boundary

The first prototype demonstrates column organization and concise stage settings
in the native Board, using synthetic data and no provider/model calls. Any
in-memory configuration or movement is explicitly described as nonpersistent
in the acceptance report; product controls do not need mock labels. A screenshot
alone does not establish persistence, automatic execution, or provider support.

Proposed scope is a selected project's Board. Each project's columns have their
own IDs and settings. The existing All Projects view remains a lifecycle
aggregate for this first scope; it must not collapse identically named custom
stages from different projects into a shared executable column. This is a
proposal to validate with the prototype, not a silent account-wide preference.

The compact header shows stage title, count, and optional harness indicator;
add-stage and stage-settings affordances do not add explanations to every card.
Settings expose name, harness, model (provider default allowed), instructions,
and a bounded handoff choice. Manual start is the initial behavior. Moving a
card alone does not send a prompt, spend tokens, cancel work, or edit files.

## Smallest durable model

- Keep `TaskState` as internal lifecycle compatibility: Backlog, Doing, Done.
  It no longer defines all visible columns. Existing Done stop/acknowledgment,
  completion timestamps, archive, and reopening semantics remain authoritative.
- Add ordered project-owned columns with stable UUIDs, name, lifecycle category,
  and optional execution defaults. The lifecycle category is an implementation
  concern, not another user-facing workflow editor.
- Add an optional column ID to tasks. Existing tasks resolve to canonical
  Backlog/Doing/Done columns during migration without changing task/session IDs,
  worktrees, timestamps, or historical results. Example Plan/Implement/Review/
  Archive columns must not silently rename existing users' boards.
- Store only existing provider/model identifiers and instructions in defaults.
  Do not introduce a provider registry or assume every harness supports a native
  review API. No model catalogs or subprocess work in Board rendering.
- Add stage provenance to sessions: source column ID, configuration revision or
  applied configuration snapshot, and optional prior result/source-session links.
  Renaming/reordering a stage never rewrites historical conversations.

Column configuration is a default for a new invocation, not a command to mutate
an existing provider conversation. A compatible unsent draft can be reopened;
a started session can be continued only under its existing provider/workspace
constraints and an explicit user action. Changing harness creates an associated
session on the same issue and concrete workspace. Never copy provider cursors.
A repeated start must preserve edited drafts rather than duplicating or replacing
them. A later stage settings edit affects future drafts, not a running session.

## Generic stage execution and review

One explicit Start action prepares a reviewable draft from the issue brief,
stage instructions, and selected handoff. The existing Send action starts the
provider. Handoff choices should initially be small: issue brief only, or issue
brief plus a selected prior result. Include source links and bounded excerpts;
load cold transcripts before claiming handoff is ready. Do not copy complete
provider-private conversations into another harness.

Review-specific scope and findings are task/run data, not a Codex column type.
Preserve current review target, result-message ID, status, correction provenance,
and same-worktree invariants. Codex native review/start remains an adapter
capability. Another provider requires a scoped prompt and explicit successful
final-result settlement; idle or arbitrary assistant text must never be called
a completed review. Finishing any stage does not automatically move the card.

Archive is the terminal column. Enter it through the existing task-completion
stop/acknowledgment boundary, including detached work and cancel/failure paths.
Do not unlink sessions, delete worktrees, or destroy results when archiving.
Returning to an active stage clears terminal state and retains all associations.
If completion confirmation is cancelled, the card remains in its original stage.

## Persistence and integration work before shipping

1. Extend model, generated protocol/TypeScript types, SQLite schema and migrations,
   daemon load/save, and client reconciliation together. Task saves are currently
   merge-by-ID with newest `updated_at`, and the daemon owns durable Board data.
2. Define versioned column updates/reordering and explicit removal behavior so a
   stale client cannot resurrect deleted columns or overwrite newer settings.
   For a first release, prohibit deleting populated or terminal columns unless
   an explicit destination and the existing terminal safeguards are satisfied.
3. Replace enum-indexed three-element list storage with per-column-ID virtualized
   list state. Keep filtering/search and count computation outside row builders.
4. Route all card/context-menu/inspector/status moves through one stage boundary;
   same-lifecycle moves still change column ID and revision. Preserve pending
   completion cancellation and session workspace-binding behavior.
5. Implement the generic Start draft path and capability-aware review adaptation;
   retain old persisted reviews and their explicit results/provenance.
6. Update semantic automation targets to stable stage IDs and add provider-free
   fixtures. Retain old targets where needed for existing baseline scenarios.

## Acceptance gates

- Add/rename/reorder columns; empty name validation; cancel and repeated clicks;
  keyboard access to all controls; visible focus; menu dismissal and navigation
- Same-name stages in different projects retain independent settings; filtering,
  project switch and All Projects never misplace or hide an issue silently
- Move forward/backward/repeat without starting a provider or changing issue ID,
  worktree, existing session provider/model, or transcript
- Archive success, stop rejection, timeout and cancel; reopening; no loss of
  results, links, or workspace; no terminal bypass from custom drop targets
- Settings provider/model validity; provider-default model; cold-context loading;
  draft reuse with edits; incompatible harness creates new associated session
- Review explicit scope, actionable/empty results, failure/interruption/replay;
  no native-review support claimed for ordinary prompt providers
- SQLite old/new migration and restart, daemon synchronization and stale snapshot
  races; generated types and applicable Rust checks
- Actual native screenshots plus semantic scenarios, no paid model calls;
  independent Specification and Standards/Fowler reviews of the final diff

## Future user-level delegation boundary

The same stable stage IDs, configuration, issue placement, draft/run state, and
explicit actions should be observable and operable through the existing daemon
and semantic RPA surface. Manual UI and automation must use the same move/start/
archive rules. This leaves room for a long-lived role accountable to the user to
steer task-level roles later, without making a session the required controller.
It does not authorize an autonomous supervisor, background stage advancement,
new execution framework, or broader model/data access in this prototype. First
make the user's manual workflow correct and inspectable; later delegation can
choose from those same explicit actions under the user's permissions.

## Design-review requirements before durable implementation

Two concrete compatibility gaps were identified during independent read-only
review and are explicit implementation gates:

- Project aliases and moves: the current project filter includes legacy IDs with
  the same path (`projects::project_path_ids`); preserve that visibility without
  rewriting issue/session project bindings. Column ownership must not be inferred
  from a matching title. Missing or foreign stage IDs must remain visible in a
  safe lifecycle fallback and must not supply execution defaults. Reassigning an
  issue's workspace to another project resets its current stage to that board's
  canonical lifecycle destination, while historical session stage provenance
  stays unchanged. The final alias-board configuration ownership rule needs to
  be settled with durable schema work; the prototype is display-only and does
  not claim to resolve shared execution configuration across legacy aliases.
- Atomic placement: column ID, lifecycle, ordering, completion timestamp and
  revision are one mutation. Completion failure/save rollback restores original
  column as well as state/position. Explicit reopening to a stage uses that
  destination; generic Restore/continued-conversation reopening uses the board's
  canonical active column. It must never leave a Doing issue in Archive or move
  a failed Review→Archive attempt into Implement. Cover same-lifecycle moves,
  stale save failure, generic restore, alias IDs, and cross-project workspace
  changes in acceptance tests.

This review did not validate UI pixels or persistence implementation; those are
separate gates on the eventual diff.

## Future teams and factory boundary

Do not enforce column = role = session as a one-to-one identity. The issue is the
work unit; a stage describes an objective and expected outputs. One attempt can
later coordinate multiple sessions or a team, while preserving per-session
provider provenance and a single inspectable attempt outcome. Keep output/source
links, acceptance evidence, and any explicit dependencies machine-readable rather
than encoding them only in a column title or transcript prose. This is an
extensibility constraint, not a request to build team scheduling, swarm execution,
a dependency engine, or a software/intelligence-factory framework now. The first
prototype and durable slice remain manual stages plus their explicit defaults.

## Implementation slices after prototype approval

### Durable manual organization

The selected implementation stores a revisioned ordered `ProjectBoard` on the
existing Project record, plus `Task.stage_id`. This reuses the existing Project catalog and LoadTaskState path, with one
explicit SaveProjectBoard compare-and-swap action rather than a separate catalog
service. Ordinary task/session snapshot saves preserve the authoritative board. The board
stores stable stage IDs, names, lifecycle category and optional harness/model/
instructions/handoff defaults. Whole-board edits carry an explicit expected revision; conflicting edits must
be reported, not silently presented as saved. A dedicated action distinguishes
stale explicit edits from harmless old project snapshots, including edits more
than one revision behind. SQLite
migration preserves existing issue/session/workspace identities. Canonical stage
IDs are deterministic per project and lifecycle. Existing boards retain their
Backlog/Doing/Done naming; the prototype's Plan/Implement/Review/Archive remains
an example of user customization rather than a forced migration.

### Generic manual invocation

After durable organization passes restart/migration/reconciliation acceptance,
connect a stage's explicit action to the existing session-draft mechanism.
An unconfigured or unavailable harness must not silently select another. Apply
selected defaults to a new associated session in the same workspace; preserve
compatible edited unsent drafts on repeat actions. Moving a card never invokes
this action automatically. The user reviews the prompt and uses normal Send.

Persist invocation provenance separately from mutable column settings: stable
stage ID, applied configuration snapshot, invocation identity, and optional
explicitly selected source session/message IDs. Existing session turns supply
execution and result status. The identity must not enforce one session per stage
or one session per invocation, leaving future teams possible without building
them now. A selected source result must belong to the same issue/workspace and
have a successful completed turn; cold details must load before handoff. Source
IDs survive as provenance even when source content is later removed; removed or
unavailable content blocks a new handoff rather than inventing a replacement.

Review is supported through the same instructions/context draft mechanism with
any configured ordinary prompt-capable harness. A stage named Review never
implies native review/start support or automatic approval. Existing explicit
native-review sessions and their findings/correction links remain readable and
usable; provider-native review remains an adapter capability. This first generic
invocation slice does not silently reinterpret old review results or claim that
a successful ordinary response proves code approval.
