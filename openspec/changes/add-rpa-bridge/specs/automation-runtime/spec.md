# Spec Delta

## Purpose

Owns the long-lived automation runtime: it routes UI calls to the connected app
executor, keeps cursors and subscriptions over the instance's streams, resolves
waits, targets instances and drives scenario runs, so a client can hold a
conversation with a running app instead of issuing isolated calls.

## ADDED Requirements

### Requirement: Executor registration and routing

An app SHALL register as the UI executor with the daemon it is connected to,
naming its instance; the daemon SHALL route automation calls to the executor
that reported the targeted instance.

#### Scenario: A call reaches the app

- **WHEN** an authenticated client calls against an instance whose app is connected
- **THEN** the daemon routes the call to that app and returns its answer

#### Scenario: No executor is connected

- **WHEN** the targeted instance has no connected app
- **THEN** the call fails naming the instance and saying that no executor is connected

### Requirement: Automation sessions

A client SHALL attach with an automation session bound to one instance; the
session SHALL keep its cursors across a disconnected and resumed client, and
several clients MAY hold sessions against one instance without disturbing each
other.

#### Scenario: Reconnect keeps the conversation

- **WHEN** a client reconnects and resumes its session
- **THEN** its cursors are still where it left them

#### Scenario: Two clients are independent

- **WHEN** two clients read the same instance
- **THEN** each has its own cursor and neither consumes the other's events

### Requirement: Cursors and diffs

The runtime SHALL expose the instance's streams — UI frames, model state, and
diagnostics — under monotonic cursors, and a client SHALL be able to ask what
changed since a cursor in any of them.

#### Scenario: An action answers with its effect

- **WHEN** a client acts
- **THEN** the answer carries the resulting frame cursor and the controls and state that changed

#### Scenario: Asking for changes only

- **WHEN** a client asks for the state or tree since a cursor
- **THEN** it receives the diff, not a full dump

### Requirement: Subscriptions and pushed events

A client SHALL subscribe to UI, model, and diagnostic event classes; while a
subscription exists the runtime SHALL push matching events, and the executor
SHALL push UI frame events only while at least one subscription needs them.

#### Scenario: Waiting without polling

- **WHEN** a client subscribes and then waits for a control to appear
- **THEN** the event arrives without the client polling

#### Scenario: No idle cost

- **WHEN** no client subscribes to UI events
- **THEN** the executor sends no per-frame UI events

### Requirement: Async waits

A wait SHALL be registered in the runtime and resolved by observation, not by
the client polling; a client SHALL be able to poll or be notified of its
resolution, and a timed-out wait SHALL carry the observed state and the
relevant recent events.

#### Scenario: Other calls proceed during a wait

- **WHEN** a client starts a wait and then issues another call
- **THEN** the second call is served while the wait is still pending

#### Scenario: A failed wait explains itself

- **WHEN** a wait times out
- **THEN** the failure carries the observed state, the tree diff seen during the wait, and the relevant recent events

### Requirement: Instance listing and targeting

The runtime SHALL list the live instances it knows — checkout, database,
process, build, and whether an executor is connected — and a session SHALL
target one of them.

#### Scenario: Choosing the right checkout

- **WHEN** several checkouts run apps at once
- **THEN** a client can list them and target one by checkout without guessing

### Requirement: Disposable scratch instances

The runtime SHALL be able to start a disposable instance with its own data
directory and seeded database and tear it down with its run, so automated work
does not have to disturb a working instance.

#### Scenario: A scratch run leaves the working app alone

- **WHEN** a scenario runs against a scratch instance
- **THEN** the working instance's tasks, drafts, and window are untouched

### Requirement: Scenario runs

The runtime SHALL register a scenario as a first-class run with ordered steps,
start, abort, and a result; a failing step SHALL stop the run and attach a
failure bundle.

#### Scenario: A run reports its steps

- **WHEN** a scenario finishes
- **THEN** the result names each step's outcome and the final cursor

#### Scenario: A failing step stops the run

- **WHEN** a step fails
- **THEN** later steps do not run and the error carries the step and its failure bundle

### Requirement: Debug-only, remote-ready availability

The automation surface SHALL exist only while the executor is a debug build; it
SHALL be reachable by any authenticated daemon client, local or remote, through
the same messages, with no second mechanism or endpoint.

#### Scenario: Release builds expose no executor

- **WHEN** a release build runs
- **THEN** no executor registers and automation calls are refused

#### Scenario: A remote client sees the same surface

- **WHEN** an authenticated client connects from another host
- **THEN** it can drive the instance through the same calls a local client uses
