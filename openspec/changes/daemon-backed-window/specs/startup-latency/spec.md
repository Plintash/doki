# Spec Delta

## Purpose

Keeps the first frame independent of backend readiness and makes window rebuild
latency a measured, enforced property instead of an accident.

## ADDED Requirements

### Requirement: First paint does not wait on the daemon

The main window SHALL open and paint its first frame before the daemon is
spawned or connected, before task state is loaded, and before provider probing
completes. Pending data MUST render as skeleton or empty state rather than
delaying the frame.

#### Scenario: Cold launch with a slow daemon

- **WHEN** the app launches and the daemon takes time to become ready
- **THEN** the window is already visible with skeleton content, and the real
  task list appears when state arrives

#### Scenario: Daemon is unavailable

- **WHEN** the daemon fails to start or connect
- **THEN** the window still opens and reports the failure instead of hanging on
  a blank frame

### Requirement: Window rebuild meets a measured latency budget

Activating the app with no window SHALL produce an interactive window within a
budget recorded in the repository. The budget MUST be derived from a measured
baseline, not assumed, and a regression gate MUST fail when the budget is
exceeded.

#### Scenario: Reopen after close

- **WHEN** the user closes the window and activates the app again
- **THEN** an interactive window appears within the budget and hydrates the
  selected task

#### Scenario: Budget regression is caught

- **WHEN** a change makes reopen slower than the recorded budget
- **THEN** the latency harness reports the regression

### Requirement: Late hydration cannot overwrite newer state

State arriving from the daemon after a newer request has superseded it MUST be
discarded, and results addressed to a window that has since closed MUST NOT be
applied.

#### Scenario: Superseded load

- **WHEN** two state loads are issued and the older one finishes last
- **THEN** the newer load's result stays in place

#### Scenario: Closed window

- **WHEN** a window closes while its hydration is in flight
- **THEN** the late result is dropped and no stale window state is written

### Requirement: Startup and reopen milestones are recorded

The app SHALL record timestamps for process start, window open, first frame,
daemon ready, task state hydrated, and interactive, and SHALL expose them for
local inspection when tracing is enabled.

#### Scenario: Trace a launch

- **WHEN** the app starts with tracing enabled
- **THEN** it writes the milestone timestamps for that launch

#### Scenario: Trace a reopen

- **WHEN** the user closes and reactivates the window with tracing enabled
- **THEN** the milestones for the rebuild are written
