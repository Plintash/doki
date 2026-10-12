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

### Requirement: Debug-only availability

The control endpoint, the agent bindings, and the tree-for-automation mode
SHALL exist only in debug builds. Release builds MUST NOT listen on the
endpoint, publish discovery, or attach the bindings, regardless of settings or
environment.

#### Scenario: Release build exposes nothing

- **WHEN** a release build starts
- **THEN** no endpoint is opened, no discovery is published, and agents get no `waku.rpa` bindings

#### Scenario: Debug build exposes the bridge

- **WHEN** a debug build starts
- **THEN** the endpoint is available to local clients

### Requirement: Instance discovery and authentication

A debug app SHALL publish its endpoint, a bearer token, and its instance
identity (checkout, database, process id, build identity) under its instance
data directory, SHALL bind the endpoint to loopback only, and SHALL require the
token on every request. The publication MUST be removed or superseded when the
process exits, so a client never talks to a dead instance.

#### Scenario: A client finds the instance that hosts it

- **WHEN** several checkouts run debug apps at once
- **THEN** a client resolves the instance that launched it and no other

#### Scenario: Requests without the token are refused

- **WHEN** a client connects without the published token
- **THEN** the endpoint refuses the request and discloses no state

#### Scenario: A dead instance is never used

- **WHEN** the app exits and a client rereads discovery
- **THEN** the stale publication is not offered as a live instance

### Requirement: Semantic snapshot

On request the bridge SHALL return the current frame's semantic tree — node
handle, id, role, label or value, state (focused, disabled, selected, …), and
bounds — for what is on screen, plus the app's observable state: instance and
build identity, window, route, tasks, the active session, composer contents,
panels, toasts, and pending permission requests. The tree MUST reflect what is
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

### Requirement: Semantic actions

The bridge SHALL act on a control identified by its node handle or semantic id:
click, focus, set text or value, send a keyboard chord, and scroll. Actions
MUST take the same path as real user input, MUST NOT require the caller to
choose coordinates, and MUST fail with an actionable error naming the target
when it is unknown, ambiguous, disabled, or not on screen.

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

### Requirement: Batch runs

The bridge SHALL accept an ordered batch of steps and execute them against the
app, reporting each step's outcome. A batch SHALL stop at the first failing
step by default and SHALL return that step's failure evidence together with the
results of the steps that ran.

#### Scenario: One call drives a whole interaction

- **WHEN** a client submits a batch that opens a task, types a prompt, sends it, and waits for a reply
- **THEN** the batch runs the steps in order and returns their results

#### Scenario: A failing step is identifiable

- **WHEN** a step in the middle of a batch fails
- **THEN** the result names that step and carries its evidence, and later steps do not run

### Requirement: Screenshots on demand

The bridge SHALL capture the window's current frame as a PNG on request,
rendered from the app's own frame rather than the screen, so it requires no
screen-recording permission. Content composited by native child views MAY be
absent, and the response SHALL disclose it when the window contains such a
surface.

#### Scenario: A screenshot matches the frame

- **WHEN** a client requests a screenshot
- **THEN** it receives the current frame's pixels without any desktop capture permission

#### Scenario: The native-view limitation is disclosed

- **WHEN** the window contains an embedded browser surface
- **THEN** the response marks that the capture does not include it

### Requirement: Agent-facing bindings and CLI

The JavaScript REPL server attached to a coding-agent session SHALL expose the
bridge as `waku.rpa` whenever the session runs in a debug build, independently
of the Computer Use setting. A command-line client SHALL provide the same verbs
for callers without that server. Both SHALL report the instance and build
identity they are attached to, reconnect after an app relaunch, and offer a
wait for a fresh build after a rebuild.

#### Scenario: Bindings exist without Computer Use

- **WHEN** an agent session runs in a debug build with Computer Use disabled
- **THEN** its REPL exposes `waku.rpa` and can snapshot the app

#### Scenario: A relaunch is observable

- **WHEN** the dev watcher rebuilds and relaunches the app
- **THEN** a waiting client reconnects to the new instance and reports its new build identity

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
