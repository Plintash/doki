## ADDED Requirements

### Requirement: Portable semantic recording
The daemon SHALL record explicitly supported session calls and waits in order,
export versioned replay steps with explicit parameters and earlier-result identity
bindings, and reject unsupported or frame-local targets before executing them.
Recorded free text SHALL have no saved default. Failure and cancellation SHALL
be inspectable, bounded, and stop further recording effects.

#### Scenario: Replay into an equivalent fresh target
- **WHEN** a client records session creation and a later operation on its identity
- **THEN** replay uses the identity returned by its own creation step and reaches
  equivalent semantic state without using the original generated identity

### Requirement: True no-window execution
An explicitly enabled debug daemon executor SHALL execute its advertised backend
operations with no GUI process, display server or virtual display, and SHALL
reject UI-only actions, sections and waits explicitly.

#### Scenario: No display is available
- **WHEN** DISPLAY and WAYLAND_DISPLAY are absent and a backend-only job starts
- **THEN** the job completes through the daemon and no GUI executor is launched

### Requirement: Daemon-owned unattended jobs
The daemon SHALL execute bounded ordered programs after their caller disconnects,
stop at first failure or cancellation, retain terminal results, and remove only
the scratch processes/data it owns.

#### Scenario: A program fails
- **WHEN** a program fails or times out
- **THEN** later programs do not start, failure evidence is retained, and owned
  scratch processes/data are cleaned up

### Requirement: Durable local scheduling
The daemon SHALL persist bounded one-shot and recurring local schedules, persist
claims before dispatch, prevent overlapping executions of a schedule, expose its
next due time and last result, and support disabling future execution.

#### Scenario: Restart after an interrupted execution
- **WHEN** the daemon restarts with a persisted active claim
- **THEN** it records that execution as interrupted without silently retrying it,
  and any recurring schedule advances to a future original-cadence slot

#### Scenario: A recurring run is late
- **WHEN** multiple recurrence slots elapsed while the daemon was unavailable
- **THEN** due work is coalesced without a catch-up storm or overlapping execution
