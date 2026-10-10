# task-sidebar Specification

## Purpose
TBD - created by archiving change add-sidebar-task-triage. Update Purpose after archive.

## Requirements

### Requirement: The sidebar offers three task views

The sidebar SHALL offer a project view, an updated (recency-grouped) view, and a
status view, switchable from the same control, and the chosen view SHALL survive
a restart. Switching views SHALL preserve collapsed state per section identity
in both directions.

#### Scenario: The choice survives a restart

- **WHEN** the user switches to the status view, quits, and reopens the app
- **THEN** the sidebar opens in the status view

#### Scenario: The existing views are still selectable

- **WHEN** the user opens the sidebar options
- **THEN** project, updated, and status are all offered, and the previously chosen one is marked

#### Scenario: Collapsed state is per section

- **WHEN** the user collapses a project section, switches to the status view and back
- **THEN** that project section is still collapsed

### Requirement: The status view groups tasks into three active sections

The status view SHALL group tasks into three sections in this order:

1. **Needs you** — tasks in `Waiting` (a provider question or permission request
   blocks the turn) and tasks in `Failed`.
2. **Running** — tasks in `Connecting`, `Working`, or `Background`.
3. **Recent** — the `Idle` tasks the project view also lists.

A task SHALL appear in exactly one section. A section with no tasks MUST NOT be
rendered, except the trailing archived section defined below.

#### Scenario: Waiting and failed tasks come first

- **WHEN** the sidebar renders the status view and at least one task is `Waiting` or `Failed`
- **THEN** those tasks appear in the first section, above every running and idle task

#### Scenario: Empty sections disappear

- **WHEN** no task is `Waiting` or `Failed`
- **THEN** the "needs you" section is not rendered, and the "running" section becomes first

#### Scenario: Never-started drafts stay out

- **WHEN** a task exists in the client's state but has not started
- **THEN** it appears in no status section, matching the project view's existing rule

### Requirement: The archived section is the only home for archived tasks

The status view SHALL have a trailing **Archived** section, rendered only when
archived-task visibility is on, or — while it is off — for the active task
alone. A task that is archived SHALL appear only in that section and MUST NOT be
mixed into the three active sections, whatever its status.

#### Scenario: An archived task is not mixed in

- **WHEN** an archived task is `Waiting` and archived visibility is on
- **THEN** it appears in the trailing archived section, not in "needs you"

#### Scenario: The active task is never a gap

- **WHEN** the active task is archived and archived visibility is off
- **THEN** the archived section is rendered with that task in it, so the open task is visible and marked, and it lists no other archived task

#### Scenario: Visibility off keeps the archive hidden

- **WHEN** archived visibility is off and several tasks are archived
- **THEN** only the active one, if it is archived, is listed in the trailing section

### Requirement: The status view is flat

The status view MUST NOT nest tasks under projects, and MUST NOT apply the
project view's recency window, batch reveal, or "show more" affordance. Every
task SHALL be reachable by scrolling one list. Each status section SHALL be
labeled and collapsible like a project section, SHALL NOT offer the per-project
"new task" action, and the ordering control SHALL NOT be offered there because
the section orders below are fixed.

#### Scenario: A flat list of every project

- **WHEN** the status view renders tasks from several projects
- **THEN** each task appears once in its section with no project header, and no task is hidden by a per-project window or reveal limit

#### Scenario: No project-scoped affordances

- **WHEN** a status section header is rendered
- **THEN** it carries its label, its count, and a collapse control, and no "new task" button

### Requirement: Section ordering matches what the section is for

The "needs you" section SHALL be ordered by how long the task has been blocked,
longest first. The "running" and "recent" sections SHALL be ordered by
conversation recency, newest first, using the same recency the project view sorts
by — a submitted turn, not a metadata edit such as a rename.

#### Scenario: The oldest blockage is first

- **WHEN** two tasks have been `Waiting` for 4 minutes and 40 minutes
- **THEN** the task waiting 40 minutes is listed first

#### Scenario: A rename does not reorder

- **WHEN** the user renames a task while another task received the newest reply
- **THEN** the renamed task does not move above the task with the newest reply

### Requirement: Blocked duration comes from the task's own record

Blocked duration SHALL come from the task's record of when it entered `Waiting`
or `Failed`, SHALL survive a restart, and MUST NOT be derived at render time. A
task recorded before that field existed SHALL order by its newest turn activity.

#### Scenario: A restart does not reorder

- **WHEN** the app is restarted while two tasks are blocked
- **THEN** their order is unchanged, because the blocked-since time is part of the task record

#### Scenario: A record without the field still orders

- **WHEN** a task was blocked before the field existed
- **THEN** it is ordered by its newest turn activity rather than being dropped or treated as oldest

### Requirement: A task row describes the task's present state

The second line of a task row SHALL be decided in this order, and MUST NOT show
content that claims work which is not happening:

1. a blocked or failed task SHALL show why it is blocked;
2. a task that is currently busy and whose client holds the provider's plan step
   for the live turn SHALL show that step;
3. a busy task with no plan step SHALL show its objective when one is known;
4. a task in any other state SHALL keep the content it has today.

#### Scenario: A plan step replaces the branch while the task runs

- **WHEN** a busy task's provider plan step is held by the client
- **THEN** the second line shows that step instead of a branch name

#### Scenario: A busy task without a plan shows the objective

- **WHEN** a busy task's provider reports no plan step and an objective is known
- **THEN** the second line shows the objective

#### Scenario: An idle row is left as it is

- **WHEN** a task is not busy
- **THEN** the row keeps the content it has today, and no objective, step, or placeholder is substituted into it

#### Scenario: No content is invented

- **WHEN** no step, objective, or known branch applies
- **THEN** the row keeps the content it has today and MUST NOT introduce a placeholder such as "working" or "unknown", and the existing trailing time label SHALL stay

### Requirement: The trailing time label keeps its existing rules

The right-hand label SHALL keep the behavior it has today: a task in
`Connecting` or `Working` shows its elapsed turn time, a `Background` task keeps
its status label, and every other task shows a relative time. This change MUST
NOT alter those rules.

#### Scenario: A background task keeps its label

- **WHEN** a task is in `Background`
- **THEN** its trailing label is the existing status label rather than an elapsed time

#### Scenario: A running task shows elapsed time

- **WHEN** a task's turn has been running for three minutes
- **THEN** the right side of its second line shows the elapsed time

### Requirement: The row never claims a branch the task is not on

The project view MUST NOT present another branch as the task's own: when the
task's worktree branch is unknown, the line SHALL name the project instead of a
branch.

#### Scenario: An unknown branch falls back to the project

- **WHEN** a task's own worktree branch is not known to the client
- **THEN** its second line names the project rather than the project's current branch

### Requirement: A blocked task row shows why it is blocked

A task that is waiting on the user or that failed SHALL show the reason in place
of the step or objective. The reason SHALL be part of the task's stored record,
so it survives a restart and reaches clients that never attached a runtime to
that task. A task whose reason was never recorded SHALL render without one
rather than with a placeholder.

#### Scenario: A blocked task explains itself after a restart

- **WHEN** a task is `Waiting` on a permission request, and the app is restarted
- **THEN** the row still says it is waiting on the user

#### Scenario: A failure recorded by another client

- **WHEN** a task failed on one client and a second client lists it
- **THEN** the second client shows the recorded failure reason

#### Scenario: An unknown reason degrades

- **WHEN** a `Failed` task has no recorded reason
- **THEN** the row renders without a reason and without a placeholder

### Requirement: The status view keeps the project visible

Because the project is not a section heading in the status view, the second line
there SHALL name the task's project, resolved the same way the project view
resolves that task's section: a projectless task shows the no-project name, and a
task whose project is missing from the catalog shows the unknown-project name.

#### Scenario: The project stays visible in a flat list

- **WHEN** the status view renders a busy task whose step is known
- **THEN** the second line identifies the project and shows the step

#### Scenario: Projectless tasks are named

- **WHEN** the status view renders a task with no project
- **THEN** its second line shows the no-project name rather than an empty slot

### Requirement: A row hover reveals the task without switching to it

Hovering a task row SHALL show a tooltip answering "what is this and where does
it stand" without opening the task. The tooltip SHALL contain the title, the
objective when one exists, the blocking reason or step when one applies, the
project, the branch when known, and a facts line with turn count, changed files,
and recency. The card SHALL be one compact stack with no column dividers and no
fixed column widths, with the facts right-aligned into the space left by the
shorter lines.

#### Scenario: Hovering answers the question

- **WHEN** the pointer rests on a task's row
- **THEN** the tooltip shows that task's objective, project, branch when known, and facts, without selecting the task

#### Scenario: The layout stays compact

- **WHEN** the tooltip renders a long objective and short facts
- **THEN** the facts sit at the right edge of the objective's line rather than in a separate column

#### Scenario: An unknown field degrades

- **WHEN** a task's objective has not been generated yet
- **THEN** the tooltip renders its remaining lines and omits the objective line, without blocking, retrying, or spawning work

### Requirement: Showing a tooltip costs nothing

The card SHALL be built by a pure function of values the client already holds —
the task's list entry, its stored objective and reason, its project, and the
cached branch label — and that builder SHALL take no daemon client, store, or
path handle, so a tooltip can never fetch anything. A test SHALL assert that
hovering and focusing a row issue no daemon request.

#### Scenario: Moving across a list starts no work

- **WHEN** the pointer moves across a list of tasks without pausing
- **THEN** no daemon request is issued and no frame-blocking work is started

### Requirement: The tooltip is reachable without a pointer

The same content SHALL be reachable by keyboard: focusing a row SHALL show the
card for that row, and moving focus away SHALL hide it. The card MUST NOT be the
only route to information the user needs to operate a task — the row itself
keeps showing its step, objective, or reason.

#### Scenario: Keyboard focus shows the same card

- **WHEN** the user moves focus to a task row with the keyboard
- **THEN** the same card content is shown for that row

#### Scenario: A row's card and its menu do not fight

- **WHEN** the user focuses a row and then opens its context menu
- **THEN** the card is dismissed by the menu opening, and neither surface is left half-open
