# task-digest — Spec Delta

## ADDED Requirements

### Requirement: A task carries a Waku-owned objective that never rewrites its title

A task SHALL carry one objective — a statement of what will be true when the task
is done — owned by Waku, persisted with the task, and delivered with the task
list entry so a client can render it without fetching the transcript. The
objective MUST NOT modify the task's title: the title stays the stable anchor a
user recognizes the task by, while the objective may change as the work changes.

#### Scenario: A digest does not rename a task

- **WHEN** an objective is generated or replaced for a task
- **THEN** the task's title and provider title are unchanged

#### Scenario: A list entry carries the objective

- **WHEN** a client lists tasks and an objective exists
- **THEN** the objective arrives with the list entry, without hydrating the transcript

#### Scenario: A store written before the field existed still loads

- **WHEN** a task record without the field is loaded
- **THEN** it decodes with no objective and no migration error

### Requirement: What the user or the provider owns outranks the digest

Where a task has a goal the user set, that goal SHALL be the objective. Where the
provider streams plan steps for the live turn, those steps SHALL be preferred as
the row's step over any generated text. The objective SHALL be resolved in one
place before it reaches clients, so two clients cannot display different
objectives for the same task, and a generated objective SHALL only be stored when
the goal is absent.

#### Scenario: A user-set goal wins

- **WHEN** the user has set a goal on a task and a generated objective also exists
- **THEN** every client's row and card show the user's goal

#### Scenario: A cleared goal releases the field

- **WHEN** the provider reports that the task's goal was cleared
- **THEN** the objective falls back to the generated one, or to none

#### Scenario: Provider plan steps win for the step line

- **WHEN** a provider whose plan events carry step text reports a new step for the live turn
- **THEN** the row shows that step rather than generated text

#### Scenario: A plan event without step text falls back

- **WHEN** a provider emits a plan event that carries no step text
- **THEN** the row falls back to the task's objective for a busy task, or keeps today's content

### Requirement: The objective states an outcome, the step states the means

The objective SHALL describe the outcome the task is trying to reach, and MUST
NOT be written in terms of implementation detail such as file paths, file
extensions, symbol names, or tool names. The parser SHALL enforce this: an
objective carrying a path separator, a file extension, or a symbol-like token
SHALL be rejected and the previously stored objective kept. The step shown on a
row MAY name files, symbols, or tools.

#### Scenario: An implementation-shaped objective is rejected

- **WHEN** a generated objective names a file such as `src/app/sidebar.rs` or a symbol such as `render_sidebar_session_item`
- **THEN** it is not stored, and the previous objective remains

#### Scenario: The objective survives a change of means

- **WHEN** the objective describes an outcome and the agent changes which files it edits
- **THEN** the objective still describes the task correctly without regeneration

### Requirement: Generation stays out of the task's own conversation

Generating an objective MUST NOT add a message, turn, or instruction to the
task's own conversation, and MUST NOT be routed through the live session's own
protocol. The work SHALL be done by a separate one-shot invocation.

#### Scenario: The task's own conversation is untouched

- **WHEN** an objective is generated for a task
- **THEN** the task's transcript contains no generation request and its context usage does not grow because of it

#### Scenario: A generation request is not conversation content

- **WHEN** a generation runs
- **THEN** the task's transcript contains no generation request, as an assistant message or otherwise, and its context usage does not grow because of it

### Requirement: Generation runs inside the task's own provider process

Generation SHALL run inside the provider process that already serves the task,
through that provider's extension surface. It MUST NOT start a second provider
process, MUST NOT invoke a provider command line, and MUST NOT create a session.
The trigger SHALL be a command whose response reports it was handled rather than
started, and that report SHALL be verified before a result is accepted.

#### Scenario: No second process and no new session

- **WHEN** an objective is generated for a task
- **THEN** no additional provider process is started and no new session appears in the provider's session store

#### Scenario: The trigger creates no turn

- **WHEN** the daemon sends the generation trigger for a task
- **THEN** the provider reports the command as handled, no run starts, and the task's transcript gains no entry because of it

#### Scenario: Generation works while the turn is streaming

- **WHEN** a trigger is sent while the task's turn is still running
- **THEN** it executes without being queued as steering or as a follow-up

### Requirement: The result travels on the session's own event stream

The result SHALL be published as a provider entry that is not sent to the model,
SHALL reach attached clients as a session event, and SHALL carry a format version.
A client SHALL ignore an unknown version or a malformed result rather than
storing it.

#### Scenario: The result reaches the client without being sent to the model

- **WHEN** the extension publishes the result
- **THEN** the client receives it as a session event, and the entry is excluded from the model's context

#### Scenario: An unknown version is ignored

- **WHEN** a result carries a version the client does not know, or is malformed
- **THEN** nothing is stored and the previous objective is kept

### Requirement: Generation follows settled work and is never back-filled

Generation SHALL be triggered by turn settlement, not by tool activity, file
changes, or the passage of time, and a task SHALL have at least one settled turn
to be eligible. No generation SHALL be started for a turn that settled before
this behavior existed, no back-fill pass over stored tasks SHALL exist, and
resuming or opening a stored task MUST NOT schedule one. The mechanism SHALL NOT
be relied on to describe work in flight.

#### Scenario: A settled turn triggers generation

- **WHEN** a turn settles and the task stays quiet for the quiet period
- **THEN** an objective is generated for that task

#### Scenario: Existing history is not back-filled

- **WHEN** the app runs for the first time with this behavior and the store already holds settled tasks
- **THEN** no generation is scheduled for their previous turns

#### Scenario: Tool activity alone does not generate

- **WHEN** a task streams many activities but no turn settles
- **THEN** no generation is started

### Requirement: Generation is paced

After a turn settles, the task SHALL be given a quiet period of 15 seconds
before generation starts, and the task MAY be regenerated at most 6 times per
hour. Each generation SHALL have a 60-second timeout.

#### Scenario: A rapid back-and-forth does not multiply calls

- **WHEN** the user sends four prompts in quick succession, each settling a turn
- **THEN** the objective is generated once, after the last turn settles

#### Scenario: A long session stays within the budget

- **WHEN** a task settles turns continuously for an hour
- **THEN** at most 6 generations run for it

### Requirement: An unstable objective is worse than a stale one

When a newly generated objective says the same thing as the stored one, the
stored text SHALL be kept. Regeneration SHALL therefore decide whether the task's
objective changed rather than restating it, and the comparison SHALL be the same
overlap check that suppresses an objective repeating the title.

#### Scenario: Rewording does not churn the objective

- **WHEN** a regeneration produces an objective materially the same as the stored one
- **THEN** the stored objective's text is kept unchanged

#### Scenario: A real redefinition replaces it

- **WHEN** the user redirects the task and the regeneration produces an objective describing different work
- **THEN** the objective is replaced

#### Scenario: An objective that repeats the title is not stored

- **WHEN** a generated objective overlaps the task's title above the comparison threshold
- **THEN** it is rejected as a title repeat rather than stored

### Requirement: Generation is silent and never blocks work

Generation SHALL run outside the UI thread and outside a turn's delivery path,
and SHALL fail silently: a failure or timeout leaves the previous objective in
place, MUST NOT surface a provider error, and MUST NOT delay a prompt, a turn, or
a task activation. A task whose objective cannot be generated SHALL render as if
it had none.

#### Scenario: A failing side call changes nothing

- **WHEN** the invocation fails or times out
- **THEN** the previous objective is retained, no error toast or transcript entry appears, and the row and card keep rendering

#### Scenario: Prompts are never delayed

- **WHEN** a generation is in flight and the user submits a prompt
- **THEN** the prompt is delivered without waiting for it

### Requirement: A save never clears what the sender does not carry

The objective, the blocked-since time, the blocked reason, and the turn and
changed-file counts SHALL be treated as daemon-owned: a task-state save whose
payload omits them MUST NOT clear or downgrade the stored values, following the
existing preservation rule for daemon-captured checkpoints. A newer value from
the client that owns the change SHALL still win.

#### Scenario: A stale client cannot erase an objective

- **WHEN** a client saves a task projection created before an objective was generated
- **THEN** the stored objective is preserved rather than cleared

#### Scenario: An older build cannot erase it either

- **WHEN** a client that does not know the field at all saves the task
- **THEN** the stored objective survives

### Requirement: A generated objective reaches clients

A generated objective SHALL be persisted by the daemon and published as a new
task-catalog revision, and every client SHALL see it with that revision or the
next task-state load, without a restart. Publication SHALL be coalesced per
generation and SHALL NOT be emitted when the stored objective is unchanged, and
updating an objective MUST NOT change the task's recency or its position in any
list.

#### Scenario: A digest reaches the open window

- **WHEN** an objective is generated while the window is open
- **THEN** the affected row and card show it after the next catalog revision, without the user restarting or opening the task

#### Scenario: Churn is bounded

- **WHEN** generation produces the same objective as the stored one
- **THEN** no catalog revision is emitted

#### Scenario: Recency is untouched

- **WHEN** an objective is updated
- **THEN** the task's sort position and its relative time label are unchanged
