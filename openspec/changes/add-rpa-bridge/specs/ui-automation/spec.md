# Spec Delta

## Purpose

Gives a local client a semantic view of the running debug app and the ability
to operate it the way a user would, without pixels, system accessibility APIs,
or coordinates, so feature work and regressions can be verified
programmatically.

## ADDED Requirements

### Requirement: Semantic identity for interactive controls

In debug builds, every interactive control SHALL expose a stable semantic
identity — an id and a role, plus a label or value when it has one — through
the window's accessibility tree. The id SHALL stay stable across frames while
the control exists, and a control that represents a domain object SHALL include
that object's identity rather than a position.

#### Scenario: Controls appear with identity

- **WHEN** a window with interactive controls is inspected through the bridge
- **THEN** each control appears with its id, role, and label or value

#### Scenario: Ids survive ordinary repaints

- **WHEN** the same window is inspected on a later frame
- **THEN** the same controls carry the same ids

#### Scenario: Dynamic rows are distinguishable

- **WHEN** a list shows two tasks
- **THEN** each task's row exposes its own identity

### Requirement: On-demand accessibility tree

A debug window SHALL build its accessibility tree every frame while at least
one automation client is attached, even though no assistive technology is
connected, and SHALL NOT send those tree updates to the platform adapter.
While no client is attached, the tree MUST NOT be built, so ordinary frames pay
no accessibility cost.

#### Scenario: Attaching builds the tree

- **WHEN** a client attaches to a window that had no tree
- **THEN** subsequent frames build the tree and the snapshot contains the window's controls

#### Scenario: Detaching stops building

- **WHEN** the last client detaches
- **THEN** later frames build no tree and the snapshot is unavailable

#### Scenario: The system sees nothing

- **WHEN** automation keeps the tree built
- **THEN** no tree update is delivered to the platform accessibility adapter

### Requirement: Execution model

Input SHALL be dispatched without holding a view lease, and a call that needs
the current frame SHALL draw it on demand (after a full refresh); a bridge call
MUST NOT abort the app or block the UI thread.

#### Scenario: An occluded window still answers

- **WHEN** the window is fully occluded and a snapshot is requested
- **THEN** the executor draws a frame on demand and answers with a complete tree

#### Scenario: Dispatch does not re-enter the view

- **WHEN** an action dispatches input while the workspace is rendered
- **THEN** the app stays alive and the action's effect is observable

### Requirement: Debug-only availability

The executor, the agent bindings, and the tree-for-automation mode SHALL exist
only in debug builds. Release builds MUST NOT register an executor or attach
the bindings, regardless of settings or environment.

#### Scenario: Release build exposes nothing

- **WHEN** a release build starts
- **THEN** no executor registers and agents get no `waku.rpa` bindings

#### Scenario: Debug build exposes the bridge

- **WHEN** a debug build starts
- **THEN** its executor is available to the daemon's clients

### Requirement: Daemon-mediated execution

A debug app SHALL expose its UI executor to the daemon it is connected to, and
automation calls SHALL reach it through the daemon; clients MUST NOT need a
separate endpoint, publication file, or token to drive the app.

#### Scenario: Calls arrive through the daemon

- **WHEN** any authenticated daemon client asks for a snapshot
- **THEN** the daemon routes it to the connected app and returns the app's answer

#### Scenario: No executor, a clear failure

- **WHEN** no app executor is connected for the target instance
- **THEN** the call fails naming the instance and the reason

#### Scenario: A dead executor is never used

- **WHEN** the app exits while a client holds a session
- **THEN** the runtime reports the executor gone rather than answering from stale state

### Requirement: Semantic snapshot

On request the bridge SHALL return the current frame's semantic tree — node
handle, id, role, label or value, state, and bounds — for what is on screen,
plus the app's observable state. The state SHALL be derived from the serialized
model plus explicitly enumerated UI-only fields. The tree MUST reflect what is
rendered; off-screen or virtualized content is not reported.

#### Scenario: Snapshot answers what is shown

- **WHEN** a task with an open transcript is active
- **THEN** the snapshot names the task, the session status, the composer text, and the controls on screen

#### Scenario: Virtualized content is honest

- **WHEN** a long transcript is scrolled to its middle
- **THEN** the snapshot reports the rows that are rendered, not the whole transcript

#### Scenario: State and tree agree

- **WHEN** the composer is disabled because a turn is preparing
- **THEN** the snapshot's state says so and the composer's control reports disabled

#### Scenario: A new model field is observable

- **WHEN** the model gains a field and the app serializes it into the state
- **THEN** the snapshot carries it without any new bridge code

### Requirement: Action answers carry their effect

An action SHALL answer with the cursor of the frame it produced and the
controls and state that changed, so observing an action's effect does not need
a second call.

#### Scenario: The answer names what changed

- **WHEN** a client clicks a control that switches the route
- **THEN** the answer carries the new frame cursor and names the changed route

### Requirement: Semantic actions

The bridge SHALL operate a control identified by its node handle or semantic
id: activating it, focusing it, entering text, sending a keyboard chord, and
scrolling it. Operations MUST take the same path as real user input, MUST NOT
require coordinates, and MUST fail with an actionable error naming the target
when it is unknown, ambiguous, disabled, or off screen.

#### Scenario: Click by id runs the real handler

- **WHEN** a client clicks a control by its semantic id
- **THEN** the handler the user's click would run runs, producing the same state

#### Scenario: Keyboard input reaches a target

- **WHEN** a client focuses the composer and sends a chord or text
- **THEN** the composer receives it exactly as if typed

#### Scenario: Unknown targets are reported, not guessed

- **WHEN** a client acts on an id that does not exist
- **THEN** the error names the requested id and the closest known ids

### Requirement: High-level verbs

The bridge SHALL offer verbs for workspace operations the UI already performs,
including creating and opening tasks, submitting and cancelling a prompt,
answering a permission request, opening a settings page, toggling panels, and
choosing provider or model. A verb MUST invoke the same app behavior as the
corresponding control and MUST NOT maintain a parallel implementation of it.

#### Scenario: A verb lands in the same state as the control

- **WHEN** a client opens a settings page through a verb and, separately, through its control
- **THEN** both produce the same route, state, and snapshot

#### Scenario: Verbs cover the setup a scenario needs

- **WHEN** a scenario creates a task, submits a prompt, and answers a permission request
- **THEN** every step is available as a verb

### Requirement: Deterministic waits

The bridge SHALL provide waits for a control appearing or disappearing, its
label or value matching, a session or turn reaching a state, a frame being
drawn, and a diagnostic event matching. Waits MUST be resolved by observing the
app rather than by sleeping, MUST accept a timeout, and MUST return a failure
that includes the last observed state and the relevant recent events rather
than only reporting the timeout.

#### Scenario: Waiting for a turn to start

- **WHEN** a prompt is submitted and the client waits for the turn to be working
- **THEN** the wait resolves when the state changes, without a fixed sleep

#### Scenario: A failed wait explains itself

- **WHEN** a wait times out
- **THEN** the failure carries the current state, the tree diff observed during the wait, and the relevant recent events

### Requirement: Agent-facing bindings and CLI

The JavaScript REPL server attached to a coding-agent session SHALL expose the
bridge as `waku.rpa` whenever the session runs in a debug build, independently
of the Computer Use setting. A command-line client SHALL provide the same verbs
for callers without that server. Both SHALL be clients of the same daemon
protocol, so local and remote callers see one surface.

#### Scenario: Bindings exist without Computer Use

- **WHEN** an agent session runs in a debug build with Computer Use disabled
- **THEN** its REPL exposes `waku.rpa` and can snapshot the app

### Requirement: Attached clients identify and reconnect

A client SHALL report the instance and a build identity that distinguishes two
builds of one checkout, reconnect after an app relaunch, and offer a wait for a
fresh build after a rebuild.

#### Scenario: A relaunch is observable

- **WHEN** the dev watcher rebuilds and relaunches the app
- **THEN** a waiting client reconnects to the new instance and reports its new build identity

#### Scenario: Two builds are distinguishable

- **WHEN** the same checkout is rebuilt and relaunched
- **THEN** the build identity a client reports is different from the one it reported before

### Requirement: Identity conventions and coverage

The project SHALL document the naming convention for semantic ids and the rule
that a new interactive control carries identity before it is testable. Debug
builds SHALL report interactive controls that lack identity, and the repository
SHALL keep a smoke scenario that fails when a pinned core control disappears
from the snapshot.

#### Scenario: New controls are covered by construction

- **WHEN** a feature is built from the instrumented shared primitives
- **THEN** its controls appear in the snapshot without bridge changes

#### Scenario: Missing identity is visible

- **WHEN** an interactive element lacks an id or role in a debug build
- **THEN** the app reports it as a warning rather than failing silently

#### Scenario: Core controls are pinned

- **WHEN** the smoke scenario runs against a booted app
- **THEN** it fails if a pinned core control is missing
