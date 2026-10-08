# pi-provider

## Purpose

Drive Pi's RPC mode as the session transcript's single source of truth: deliver
every prompt the user sends, settle a turn on the signal the provider actually
emits, keep the client's view of the queue and the tree identical to the
provider's, and give the work an extension starts — a background child waking the
session, a status line, a question — a place to land.

## ADDED Requirements

### Requirement: A prompt is delivered, queued, or visibly refused

Waku SHALL hand every prompt to Pi in a form the running session accepts: while
the agent is streaming, the prompt SHALL carry the transport's queueing behavior
instead of letting the provider refuse it. A prompt the provider refuses before
acceptance MUST NOT remain in the transcript as a user message the provider
never saw, and the refusal SHALL be presented as that message's delivery
failure rather than as an assistant reply.

#### Scenario: A prompt sent while the agent is busy is delivered later

- **WHEN** the user sends a message while Pi is still streaming a turn
- **THEN** the provider queues it on the running turn, the message is delivered when that turn reaches its boundary, and it appears in the transcript once

#### Scenario: A refused prompt is not left looking delivered

- **WHEN** a prompt is refused before acceptance
- **THEN** the user message is marked undelivered and the provider's error text is not stored as an assistant message

#### Scenario: An idle session runs the prompt immediately

- **WHEN** the user sends a message while the agent is idle
- **THEN** a run starts for it with no queueing step in between

### Requirement: The prompt answer settles only what it reports

Waku SHALL read what the provider says happened to a prompt — started, queued, or
handled by an extension — and MUST NOT depend on receiving that answer at all: Pi
answers a prompt submitted inside its settle window with no response. A prompt an
extension consumed SHALL settle without waiting for a run that will never start.

#### Scenario: An extension command does not hang the turn

- **WHEN** a submitted prompt is consumed by an extension command and no run starts
- **THEN** the turn settles at once, with no waiting spinner

#### Scenario: Nothing is left parked across a settlement

- **WHEN** a run ends while the provider still holds a message the user sent — the case a stop creates, because the provider drains its queue at a normal turn boundary but not across an abort
- **THEN** the turn settles once and at once, and the held message is retracted with its text handed back to the user instead of waiting in the provider's queue to be spliced into a later turn

#### Scenario: A missing answer does not stall the session

- **WHEN** the provider accepts a prompt but never writes a response for it
- **THEN** the turn still settles when the run finishes

### Requirement: The client's queue is the provider's queue

Waku SHALL treat the queue the provider reports as the truth about what is
pending, and SHALL present a queued message as queued rather than delivered. A
settlement MUST NOT leave a message parked in that queue: a message the provider
still holds when a run ends SHALL be retracted and its text handed back to the
user, rather than left to arrive inside a later turn.

#### Scenario: The pending list follows the provider

- **WHEN** a message is queued and the provider reports its queue
- **THEN** the client shows that message as pending, and shows it as delivered only once it has left the queue

#### Scenario: Sending while busy does not claim delivery

- **WHEN** the user sends a message into a streaming turn
- **THEN** the transcript marks it pending until the provider reports it delivered

### Requirement: Steering is offered only to a live run

Waku SHALL send a steering message only while a run is live. When no run is live,
the message SHALL take the prompt path. A steering message SHALL NOT be reported
as delivered merely because the provider accepted it into a queue.

#### Scenario: A steer that misses the turn is not parked

- **WHEN** a steer arrives after the run it was meant for has settled
- **THEN** it is delivered as a prompt for the next turn rather than waiting in the provider's steering queue

#### Scenario: A steer into a live turn is delivered there

- **WHEN** the user steers a run that is streaming
- **THEN** the provider delivers the message at the turn's boundary and the transcript shows it inside that turn

### Requirement: Stop retracts what it can and never waits on the transport

Stopping a turn SHALL remove the messages the user has queued with the provider
before it aborts the run, and the text so removed SHALL be returned to the user
rather than discarded. The abort MUST NOT be subject to the short control timeout
that other requests use, because the provider answers it only once the session is
idle.

#### Scenario: A stopped message does not run afterwards

- **WHEN** the user sends a message into a streaming turn and then stops the turn
- **THEN** the queued message does not run, and its text is available to the user again

#### Scenario: A slow abort is not reported as an error

- **WHEN** the provider takes longer than the control timeout to reach idle after an abort
- **THEN** no transport error is reported for the stop

### Requirement: A run the agent starts on its own has a transcript home

When Pi starts a run that no Waku prompt opened — an extension waking the session
— Waku SHALL open a turn for it and stream its output exactly as for a prompted
turn. Only the provider's own run-start signals MAY open such a turn; a message
the extension appends without starting a run MUST NOT.

#### Scenario: A completion wake streams into the transcript

- **WHEN** a background subagent finishes and wakes the parent session
- **THEN** the wake's reply and its tool activity appear in the transcript and the turn settles when the run does

#### Scenario: An appended message alone opens no turn

- **WHEN** an extension appends a message without triggering a run
- **THEN** no turn is opened for it

### Requirement: Extension messages are surfaced at their place in the session

Waku SHALL decode the provider's extension messages — their kind, text and
display flag — and SHALL place each where the session tree has it: a message
about work that outlives the turn belongs to the client's background-work
surface, and every other message to the transcript. Such a message is rendered
on that surface whether or not the provider marked it for display, because the
provider places it there itself and its status is what a row would have said; it
MUST NOT also add a conversation row. A message of any other kind adds a
conversation row only when the provider marked it for display.

#### Scenario: A child's completion is visible

- **WHEN** a background subagent child completes or fails
- **THEN** the client shows that outcome where it shows detached work, named after the child

#### Scenario: A message not meant for the conversation adds no row

- **WHEN** the provider appends a message marked as not for display
- **THEN** no conversation row is added for it, and it is not duplicated into a second place

### Requirement: Extension UI requests reach the user, and none is cancelled on their behalf

Waku SHALL present the provider's extension UI requests: notifications with their
severity, status updates, widgets, window titles and editor text SHALL reach the
user's surfaces, and answerable dialogs SHALL be answerable, with a cancellation
sent only when the user cancels or the provider's own timeout expires. A dialog
MUST NOT be cancelled silently at the moment it arrives.

#### Scenario: A failed subagent is not silent

- **WHEN** an extension reports a failure through a notification
- **THEN** the user sees it, with its severity

#### Scenario: Live progress reaches the UI

- **WHEN** an extension publishes a status update
- **THEN** the session shows it until the extension clears it

#### Scenario: A dialog waits for the user

- **WHEN** an extension asks a question that needs an answer
- **THEN** the request is shown and the provider receives the user's answer, or a cancellation if the user dismisses it, and never an immediate cancellation

### Requirement: The transport is verified against a live Pi

The behaviours above SHALL be covered by tests that drive a real `pi --mode rpc`
process, and the acceptance path SHALL be a `pi-subagents` workflow whose
background child completion wakes the parent session.

#### Scenario: The live suite covers delivery and surfaces

- **WHEN** the transport's tests run against an installed Pi
- **THEN** a busy session accepts a queued prompt, a self-started run streams, an extension message is classified, and an extension UI request is answered

#### Scenario: The acceptance run uses the product path

- **WHEN** a background subagent workflow completes
- **THEN** the parent session is woken, the completion is visible in the client, and the reply it produced is in the transcript
