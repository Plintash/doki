# Spec Delta

## MODIFIED Requirements

### Requirement: Approvals are one-shot

Waku SHALL answer a permission request with the one-shot approval or the
rejection the service accepts, using the current route's reply envelope for
that request's own id, and MUST NOT write the durable "always" answer into the
service's saved-permission store, which the user's own terminal shares. A
durable choice SHALL live only in the driver's own state. A reply the service
refuses for any reason other than the request no longer existing MUST leave the
request answerable rather than consuming it, so a refused reply cannot leave a
turn blocked with no card to answer; a reply answered as "no such request"
SHALL be treated as settled, because another client or the ended turn already
resolved it.

#### Scenario: Approving once does not persist

- **WHEN** the user approves a request once and a matching request arrives later
- **THEN** the user is asked again

#### Scenario: A durable choice does not leak into the service

- **WHEN** the user chooses a durable approval for a session
- **THEN** no saved-permission rule is written into the service's global store

#### Scenario: The service accepts the reply

- **WHEN** Waku answers a pending permission request
- **THEN** the reply is accepted by the current service rather than rejected as a malformed payload, and the request stops blocking the turn

#### Scenario: A refused reply is offered again

- **WHEN** the service refuses a permission reply for a reason other than the request no longer existing
- **THEN** the approval is offered to the user again instead of being consumed

#### Scenario: A stale request is settled without a second question

- **WHEN** the service answers a reply by saying the request no longer exists
- **THEN** the request is cleared without being offered again

## ADDED Requirements

### Requirement: A denial is a first-class answer

Waku SHALL answer a denial with the rejection the current service accepts and
MUST NOT present the service's internal interrupt reason as the turn's outcome
or as transcript text. A denial the service turns into an aborted turn SHALL
end the turn as a user stop: the session returns to idle, the turn is marked
interrupted rather than failed, and the declined call's own row names the
decline. The abort the service raises for that step MUST NOT surface as a
provider error. Where the service accepts an explanation with a rejection,
Waku SHALL offer one, and a denial carrying that explanation SHALL leave the
turn running so the agent can act on it. A provider-side interruption SHALL be
distinguishable from a failure so clients present it as a stop.

#### Scenario: A plain denial ends the turn as a stop

- **WHEN** the user denies a permission with no explanation and the service aborts the turn
- **THEN** the session returns to idle with the turn marked interrupted, no provider interrupt reason appears as the turn's text, and the declined call's row says the user declined it

#### Scenario: The denial's own abort is not an error

- **WHEN** the service marks the declined step aborted after a denial
- **THEN** no provider error is surfaced for it, because the stop and the declined row already say what happened

#### Scenario: A denial with an explanation keeps the turn alive

- **WHEN** the user denies a permission with an explanation the service accepts
- **THEN** the explanation reaches the agent and the turn continues instead of being aborted

#### Scenario: A provider-side stop is not a failure

- **WHEN** a provider ends a turn because the user stopped it
- **THEN** the turn is reported as interrupted rather than as a failed turn
