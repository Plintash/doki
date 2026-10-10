# task-archive — Spec Delta

## ADDED Requirements

### Requirement: Archiving hides a task without destroying it

Archiving SHALL record that a task is put away and remove it from the desktop
task lists. Archiving MUST NOT delete the task's messages, its stored transcript
detail, its checkpoints, its worktree, or its provider session. The archive
state SHALL persist, and a store written before the field existed SHALL load
with every task unarchived.

#### Scenario: An archived task leaves the list

- **WHEN** the user archives a task
- **THEN** the task disappears from the project view and from the status view's active sections

#### Scenario: Nothing is deleted by archiving

- **WHEN** a task is archived
- **THEN** its messages, transcript detail, checkpoints, and worktree still exist on disk and in the store

#### Scenario: The other clients keep the field

- **WHEN** a task is archived and listed by `apps/web` or `apps/mobile`
- **THEN** those lists still include the task and its archive field, which they pass through rather than drop or clear

#### Scenario: An older store loads unarchived

- **WHEN** a store written before archiving existed is opened
- **THEN** every task loads as unarchived and no task disappears

### Requirement: Archiving is a recorded action, not a field a save can drop

Archive and unarchive SHALL be applied as explicit actions against the stored
task, and the stored archive time SHALL NOT be cleared or set by an ordinary
task-state save from a client that is not performing the action. A client that
does not know the field MUST NOT clear it.

#### Scenario: A stale save does not unarchive

- **WHEN** a client holding a pre-archive projection saves the task
- **THEN** the task stays archived

#### Scenario: An older build does not clear it

- **WHEN** a client that does not know the field saves the task
- **THEN** the archive time survives

### Requirement: The archive can be seen and undone

The sidebar SHALL provide a persisted way to reveal archived tasks. Unarchiving
SHALL return a task to the active sections immediately. A task's row context menu
SHALL offer the action for that task and the inverse for an archived one, the
command palette SHALL offer the same action for the task it is showing, and the
placement of revealed tasks is defined by the sidebar's archived section.

#### Scenario: Unarchive returns the task

- **WHEN** the user unarchives a task
- **THEN** the task is back in the section its status implies, without relaunching

#### Scenario: The action is reachable from the row

- **WHEN** the user opens a task's row context menu
- **THEN** it offers archiving, and offers unarchiving for a task that is already archived

#### Scenario: The action is reachable from the palette

- **WHEN** the user opens the command palette while a task is showing
- **THEN** it offers archiving that task, and offers unarchiving it when it is already archived

#### Scenario: The reveal choice survives a restart

- **WHEN** the user turns archived-task visibility on, quits, and reopens the app
- **THEN** archived tasks are still revealed

### Requirement: An archived task stays reachable

A task that is archived MUST NOT become unreachable: it SHALL still be found by
the client's task search, and it SHALL stay in the catalog that search reads even
while list rendering hides it. Selecting an archived task from search SHALL open
it, and the list SHALL keep showing the active task even when archived tasks are
hidden.

#### Scenario: Search still finds it

- **WHEN** the user searches for an archived task by a word from its title
- **THEN** the task appears in the results

#### Scenario: Hiding is a rendering concern only

- **WHEN** a task is archived
- **THEN** it remains in the catalog the search and palette read

### Requirement: Archiving states what it does not do

The archive action SHALL be described as putting the task away. It MUST NOT
claim or imply that disk space was freed, and this change introduces no
storage-reclaiming action.

#### Scenario: No false promise of space

- **WHEN** the user archives a task
- **THEN** no message claims that disk space was reclaimed
