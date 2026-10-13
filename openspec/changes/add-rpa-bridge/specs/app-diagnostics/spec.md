# Spec Delta

## Purpose

Makes the running system explain itself: structured, correlated events recorded
continuously, retained under a bound, adjustable at runtime, and queryable
together across processes, so a complex problem is diagnosed from the system's
own records instead of by adding temporary logging and rebuilding.

## ADDED Requirements

### Requirement: Structured, correlated events

App, daemon, and provider output SHALL be recorded as structured events —
timestamp, level, source, message, and fields — and an event that belongs to a
task, turn, message, permission request, or provider process SHALL carry that
identity so related records can be collected without guessing. The daemon SHALL
aggregate the events of its instance from the app, itself, and provider
processes into one ordered stream.

#### Scenario: A turn's events are identifiable

- **WHEN** a prompt is submitted and its turn runs
- **THEN** every event produced for it carries the task and turn identity

#### Scenario: Provider output is attributed

- **WHEN** a provider process writes a diagnostic line
- **THEN** it is recorded with the provider and process it came from

#### Scenario: The app's events reach the instance stream

- **WHEN** the app records an event
- **THEN** the daemon's aggregated stream carries it in order with the daemon's own records

### Requirement: Sequenced, replayable stream

Every event SHALL carry a monotonic sequence number in its instance's stream,
and a client SHALL be able to read events after a cursor, so no observation is
lost across a dropped or resumed connection.

#### Scenario: Reconnect resumes from the cursor

- **WHEN** a client reconnects with its last seen sequence
- **THEN** it receives the events recorded since, once and in order

#### Scenario: An expired cursor is refused

- **WHEN** a client asks for a sequence older than retention
- **THEN** the answer says the cursor expired instead of returning a silent gap

### Requirement: Always-on bounded retention

Each process SHALL retain recent events in bounded memory and append them to
rotating NDJSON files under the instance data directory. Retention MUST be
bounded by size or count, older records SHALL be dropped, and records SHALL be
written by a background writer so a slow disk does not delay a frame.

#### Scenario: Retention is bounded

- **WHEN** a session produces far more events than the retention bound
- **THEN** older records are dropped and memory and disk use stay within the bound

#### Scenario: A slow disk does not stall frames

- **WHEN** records are being written while the app is drawing frames
- **THEN** frames continue to be drawn

#### Scenario: Files are the ground truth

- **WHEN** the app is hung or not answering
- **THEN** its records are still readable directly from its files

### Requirement: Runtime verbosity

Verbosity SHALL be adjustable at runtime per process and per source, without a
rebuild or restart. At the default level, the lifecycle milestones needed to
explain a session MUST still be recorded.

#### Scenario: Turning up one subsystem

- **WHEN** a client raises the level for one source in the running app
- **THEN** that source's detailed events start appearing while other sources keep their level

#### Scenario: Turning it back down

- **WHEN** the client restores the default filter
- **THEN** detailed events stop being recorded

### Requirement: Timeline reconstruction

The daemon SHALL reconstruct the ordered lifecycle of a task or turn —
submitted, provider started, streaming, tool activity, settling, settled or
failed — with timings, from the recorded events.

#### Scenario: A settled turn's timeline

- **WHEN** a turn has finished
- **THEN** its timeline lists the phases in order with their timings

#### Scenario: A stuck turn is explainable

- **WHEN** a turn has not settled
- **THEN** the timeline shows the last phase reached and when it last changed

### Requirement: Live counters

The runtime SHALL expose live per-process counters and gauges that answer
performance questions — frames drawn, render and pump commits, queue depths,
event backlog, and request round-trip latency — without a rebuild.

#### Scenario: Counters move while streaming

- **WHEN** a reply is streaming and the client reads the counters twice
- **THEN** it receives a current value for every named counter, and the frame and commit counters are higher on the second read

#### Scenario: Perf questions are answerable in place

- **WHEN** a client asks for counters
- **THEN** it receives current values attributed to the process that reported them

### Requirement: One merged diagnostic view

The daemon SHALL answer one query that returns records from the app, itself,
and provider output together, filtered by time, level, source, and correlation
identity, so one call can explain a failure that spans processes.

#### Scenario: A cross-process failure is one query

- **WHEN** a prompt fails because the provider refused it
- **THEN** the client sees the app's and the daemon's records for that turn in one ordered result

### Requirement: Health in every snapshot

Every semantic snapshot SHALL include a health summary — daemon connection
state, last error, provider and session state, pending permission requests, and
backlog or frame counters — that names state rather than dumping records, so a
first look answers whether anything is wrong.

#### Scenario: A broken connection is visible immediately

- **WHEN** the app's link to the daemon has dropped
- **THEN** the health summary names the failure without any extra probing query

#### Scenario: Health stays a summary

- **WHEN** health is included
- **THEN** it names the daemon link, the last error, session and provider state, pending permissions, and counters as state, not records

### Requirement: Failure dumps

A failed wait, action, or test SHALL carry the evidence needed to diagnose it:
the observed state, the semantic tree diff, the relevant recent events, and the
counters at failure time.

#### Scenario: A failed step carries its evidence

- **WHEN** a step fails
- **THEN** the result includes the state, relevant events, and counters captured at that moment

#### Scenario: A failing test dumps the same evidence

- **WHEN** a UI test fails on a snapshot assertion
- **THEN** the failure prints the snapshot and the relevant events rather than a bare comparison

### Requirement: Debug-only diagnostics

Recording to files, runtime verbosity changes, and diagnostic queries SHALL be
debug-build behavior. Release builds MUST NOT write diagnostic event files or
expose the queries.

#### Scenario: Release builds stay quiet

- **WHEN** a release build runs
- **THEN** it writes no diagnostic event files and exposes no query surface
