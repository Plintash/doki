# Design

The runtime still owns sessions and ordered scenarios. A recording is an
explicit session activity, capturing only commands submitted through that
session, never arbitrary mouse/keyboard input. Recorded text becomes a required
input without a saved default; generated identities bind to exact earlier result
JSON pointers. Unsupported/nonportable operations fail before their effect.
An active recording serializes its steps and retains a sanitized failure status.

The no-window executor is a thread in the existing debug daemon. It registers
through the existing authenticated executor contract and operates real backend
session state. It has no GPUI dependency, virtual display or window. Its explicit
capabilities reject UI actions/sections; persisted state follows the same normal
backend rules (including transient, unstarted sessions). Disposable jobs own
both their isolated data and child daemon, and never inherit provider profiles.

Suites execute bounded programs in order, each on a fresh owned scratch instance,
and stop at the first failed program. Caller disconnect does not cancel work.
Cancellation stops future steps and cleans up only the owned scratch. Results
name timeout, interruption and failure distinctly from success.

Local schedules persist a claim before dispatch. A schedule never overlaps
itself. Missed recurrence slots coalesce into one due execution; future slots
remain anchored to the original cadence. A crash-interrupted claim is recorded
as interrupted and is never retried implicitly. A completed or interrupted
one-shot requires a new schedule; recurring jobs continue at a future slot.
Schedules do not start or keep the daemon alive and do not install OS services.
