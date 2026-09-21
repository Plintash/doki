# opencode2-provider

## Purpose

Drive the OpenCode 2 background service the user already runs — adopting its
process, speaking its current HTTP and event API, and reading its
location-scoped catalogues — without ever becoming its owner.

## ADDED Requirements

### Requirement: Adopt the user's own service, never own it

Waku SHALL discover OpenCode 2 through the service descriptor the CLI publishes
in the user's state directory, and SHALL read that descriptor read-only: the
service watches the file and self-terminates when it changes, so repairing or
rewriting it would kill the user's daemon. Teardown SHALL stop at cancelling
Waku's own subscription and MUST NOT signal the process. A catalogue read MUST
NOT start a service; only a task may start one, and starting one SHALL be
idempotent against a healthy incumbent so concurrent Waku processes converge on
the same service.

#### Scenario: Repeated tasks reuse the running service

- **WHEN** a second task starts on a machine where a service is already running
- **THEN** it attaches to that same process and no service process is restarted or replaced

#### Scenario: Refreshing a picker starts nothing

- **WHEN** a catalogue is refreshed with no service running
- **THEN** no OpenCode process is started and the picker falls back to its cached catalogue

#### Scenario: Finishing a task leaves the daemon alive

- **WHEN** the last session using the service ends, is stopped, or is deleted
- **THEN** the service process is still running

### Requirement: Identify the service before driving it

Waku SHALL identify a service through the current identity route and SHALL
accept it only when the process it reports matches the descriptor it was found
through. The CLI SHALL be taken as `opencode2`, or as `opencode` when that
binary reports a 2.x version, because the second name is also OpenCode 1's. A
machine with only OpenCode 1 MUST NOT advertise it as OpenCode 2. Waku MUST NOT
branch on the reported version: either the routes answer or the build is
refused with an error that names the service.

#### Scenario: A descriptor for another process is not adoption

- **WHEN** the descriptor names a process other than the one the service reports
- **THEN** the service is rejected and no session is created against it

#### Scenario: A build whose routes moved is refused visibly

- **WHEN** the identity route does not answer
- **THEN** the provider reports the failure instead of starting a session or silently degrading

#### Scenario: OpenCode 1 is not offered as OpenCode 2

- **WHEN** only OpenCode 1's `opencode` binary is installed
- **THEN** the OpenCode 2 provider reports itself as not installed

### Requirement: Sessions are client-minted and location-exact

Waku SHALL mint the session id itself and SHALL be subscribed to the event
stream before creating the session, so no first event can be emitted before a
subscriber exists. The workspace directory SHALL be canonicalized once and
SHALL be the same string for creation and for every location-scoped read,
because the service compares locations by exact equality and resolves neither
alternative spelling.

#### Scenario: A session's first event is never lost

- **WHEN** a session is created
- **THEN** no event belonging to it can be emitted before Waku's subscription exists

#### Scenario: One spelling is used for create and every read

- **WHEN** a session is created in a workspace
- **THEN** the location string sent with it is the canonicalized path and is the string reused by every catalogue and session read for that workspace

### Requirement: The execution outcome settles the turn

The turn SHALL be settled by the session's execution outcome, and settling
SHALL be idempotent so a second message delivered into the same turn still
settles exactly once. Waku MUST NOT wait for an idle event the service does not
emit, which would pin a finished turn to a running state.

#### Scenario: A finished turn leaves the working state

- **WHEN** an execution outcome arrives
- **THEN** the turn finishes once and the session returns to idle

#### Scenario: A steered message does not settle a turn twice

- **WHEN** a message is steered into a running turn and its execution outcome arrives
- **THEN** the turn is finished exactly once

### Requirement: Steering is a delivery of a message, promoted at a step boundary

Waku SHALL steer by delivering a message with the steering delivery, or by
changing the delivery of a message that is already pending; delivery MUST NOT be
treated as a separate verb. A message SHALL never be spliced between an
assistant's tool call and that call's result, because the service promotes
pending deliveries at a step boundary.

#### Scenario: Steering reaches the running turn

- **WHEN** the user sends while a turn is running and the transport can steer
- **THEN** the message is delivered into that turn and the outcome is reported back to the app

#### Scenario: No message appears between a tool call and its result

- **WHEN** a tool call and its result are adjacent in the transcript
- **THEN** no message injected during the turn appears between them

#### Scenario: Changing the delivery of a pending message

- **WHEN** the delivery of a pending message is changed
- **THEN** the service adopts the new delivery rather than enqueuing the message a second time

### Requirement: One event stream, demultiplexed by session

Waku SHALL read one server-wide event stream and route each frame to the
subscriber owning the session the frame names. Frames belonging to the service's
terminal-control family, and frames for sessions Waku does not own, SHALL be
dropped rather than rendered.

#### Scenario: Frames reach exactly one session

- **WHEN** frames for two sessions arrive on the shared stream
- **THEN** each is delivered only to its own session's transcript

#### Scenario: Terminal-control frames are not transcript content

- **WHEN** the service emits a terminal-control frame
- **THEN** nothing from it appears in the transcript

### Requirement: Approvals are one-shot

Waku SHALL answer a permission request with the one-shot approval or the
rejection the service accepts, and MUST NOT write the durable "always" answer
into the service's saved-permission store, which the user's own terminal shares.
A durable choice SHALL live only in the driver's own state.

#### Scenario: Approving once does not persist

- **WHEN** the user approves a request once and a matching request arrives later
- **THEN** the user is asked again

#### Scenario: A durable choice does not leak into the service

- **WHEN** the user chooses a durable approval for a session
- **THEN** no saved-permission rule is written into the service's global store

### Requirement: Catalogues settle a cold location and never narrow silently

Models, agents, skills and commands SHALL be read from the service's catalogue
routes for the location being asked about, and a catalogue Waku treats as global
SHALL be read unscoped rather than pinned to whichever workspace asked first. A
location the service has not opened yet
publishes empty and fills in a moment later, so a read SHALL first wait for that
location's registries to report ready, bounded so that a build without that
readiness surface still answers. A location whose registries are already ready
SHALL answer on its first read even when its catalogue is empty. A catalogue
that cannot be read SHALL be reported rather than silently dropped, because an
empty result is indistinguishable from a route that moved. Skills SHALL be
offered beside commands, since the current API no longer marks which skills are
user-invocable.

#### Scenario: A fresh workspace still lists its models

- **WHEN** a catalogue is requested for a location the service has not opened
- **THEN** the result is that location's catalogue, not the empty answer published while it was still loading

#### Scenario: A ready location with an empty catalogue answers at once

- **WHEN** a catalogue is requested for a location whose registries are ready and whose catalogue has no entries
- **THEN** the empty answer is returned without waiting out the budget

#### Scenario: A route that moved is visible

- **WHEN** a catalogue route answers an error
- **THEN** the provider reports the failure instead of presenting an empty catalogue

#### Scenario: Skills are offered beside commands

- **WHEN** the composer palette is populated for a workspace
- **THEN** the workspace's skills and the service's commands are both offered, and a skill the service refuses is not offered as a usable entry

### Requirement: Rewind and branch use the resident service

Waku SHALL create a fork with a boundary through the adopted service rather than
by starting a second OpenCode process, so two processes never contend for the
same local resources.

#### Scenario: Branching starts no second process

- **WHEN** the user branches a session
- **THEN** the fork is created through the service already in use

### Requirement: Computer Use attaches to one session through the current routes

Enabling Computer Use SHALL register the runtime server under the current
experimental routes and attach it to the session as an instruction entry, and
disabling it SHALL remove both. A failure to attach SHALL be reported rather
than leaving the session believing Computer Use is available.

#### Scenario: Computer Use is scoped to its session

- **WHEN** Computer Use is enabled for one session
- **THEN** that session's instruction entry names the runtime server and other sessions are unaffected

#### Scenario: A failed attachment is visible

- **WHEN** the runtime server cannot be registered or attached
- **THEN** the failure is reported to the app instead of being treated as enabled

### Requirement: The provider is verified against a private live service

The provider's tests SHALL drive a service the harness starts on an ephemeral
port with its own data, state and configuration directories, reached through the
same discovery path production uses, and MUST NOT use whatever service the
developer happens to be running or write into the user's state directory. A test
that asserts a catalogue SHALL read a location nothing has opened, so the staged
publication of a cold location is exercised rather than assumed.

#### Scenario: A moved route fails the suite

- **WHEN** a route the provider depends on stops answering
- **THEN** the provider's test suite fails

#### Scenario: The developer's own service is untouched

- **WHEN** the suite runs on a machine that already has a service running
- **THEN** that service is neither used, nor signalled, nor registered into
