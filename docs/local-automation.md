# Local recording and unattended automation

This debug-only extension uses the same authenticated daemon, shared protocol
and executor contract as native RPA. It does not install services, cron jobs or
another control plane. Cross-device execution remains deferred.

## Start a no-window target

A debug daemon can expose backend automation with `--automation-headless`.
It needs the normal `WAKU_DAEMON_TOKEN` environment capability; clients use that
existing capability plus `WAKU_DAEMON_ADDRESS`. Do not print or save the token in
commands, recordings or reports. This mode has no GPUI, window, virtual display
or display environment requirement.

```sh
bun scripts/rpa.ts scratch create --headless --json
bun scripts/rpa.ts --instance SCRATCH_ID describe --json
```

Headless scratch starts only an isolated child daemon. It does not inherit
provider keys, profiles, settings or the working database. An empty seed gets
one isolated project. `describe` reports the exact build and capabilities:

- Read: describe, bounded snapshot, persisted transcript
- Invoke: newSession `{projectId?}`, openSession `{id}`, renameSession `{id,title}`,
  setDraft `{id,text}`
- State sections: session, sessions, projects, selection, draft (`state` selects all)
- Waits: backend state and diagnostics; UI controls/frame waits fail explicitly
- UI actions, typing, keys, screenshots, Board and model submission are unavailable

Normal persistence rules still apply: an unstarted standalone session is
transient; its draft is durable under its project key. Renaming a started
session persists its title. Selection is process-local and does not start a
provider. This is not a claim of no-window coverage for arbitrary UI workflows.

## Record explicit semantic operations

```sh
bun scripts/rpa.ts --instance SCRATCH_ID record start "draft workflow" --json
# Use the returned attachment sessionId for every recorded operation:
bun scripts/rpa.ts --session SESSION_ID invoke newSession '{}' --json
bun scripts/rpa.ts --session SESSION_ID invoke setDraft '{"id":"RETURNED_CONVERSATION_ID","text":"A disposable draft"}' --json
bun scripts/rpa.ts --session SESSION_ID wait --state '/draft/text="A disposable draft"' --json
bun scripts/rpa.ts --session SESSION_ID snap --sections sessions,draft --json
bun scripts/rpa.ts --session SESSION_ID record status --json
bun scripts/rpa.ts --session SESSION_ID record stop --json > replay.json
```

Recording captures only the supported calls and waits submitted to that
attachment. It does not capture arbitrary human input or every client's actions.
Only one recorded operation may be in flight; finish the wait before the next
recorded operation. The daemon continues recording across CLI disconnects within
the normal attachment lifetime. `record cancel` discards the recording. A failed
recording blocks further recorded effects until explicitly stopped or cancelled;
its status retains sanitized failure information, and detailed evidence remains
on the original failed operation. Stopping a failed recording exports only the
successful prefix and does not change its failed status.

Export format version 1 contains semantic steps plus exact JSON-pointer bindings.
Typed text, draft/title strings and string equality values become required inputs
without saved defaults. `record status` names each required input and its step/
field uses. A created conversation UUID binds to the earlier creation result;
pre-existing seed UUIDs become required inputs. Neither is silently reused as a
portable generated identity. This is exact value replacement, not string
interpolation or arbitrary evaluation.

Frame-local node handles, UUID-bearing semantic IDs/pointers, transcript/full
model captures, frame/diagnostic waits, send controls/submission chords and
unsupported invoke verbs are rejected before their effect while recording.
Recorded typing excludes control characters; use headless setDraft for multiline
text. This conservative subset is discoverable from explicit error messages and
is not a guarantee that arbitrary prompts or arbitrary controls are secret-safe.

Replay with a fresh equivalent target and an explicit input JSON object:

```sh
bun scripts/rpa.ts --instance FRESH_SCRATCH_ID replay replay.json --parameters inputs.json --json
bun scripts/rpa.ts scenario status RUN_ID --session SESSION_ID --json
```

Replay validates the full document and required inputs before dispatch. Missing,
unused or structurally invalid inputs fail before side effects. Target-state checks
and executor-specific value limits are validated per step; earlier successful
steps are not rolled back if a later step fails. Result references must point
to an earlier step. Inputs are at most 1 MiB; the replay document is at most
1 MiB and 128 steps. Recording documents persist only when the client explicitly
writes their JSON output; daemon recording state is not crash-durable.

## Run unattended suites

```sh
bun scripts/rpa.ts job start rpa/jobs/headless-smoke.json --json
bun scripts/rpa.ts job status RUN_ID --json
bun scripts/rpa.ts job wait RUN_ID --json
bun scripts/rpa.ts job abort RUN_ID --json
# Start and wait in one CLI process, with truthful terminal exit status:
bun scripts/rpa.ts job run rpa/jobs/headless-smoke.json --json
```

A job contains 1–8 programs, each either `{type:"scenario",scenario:...}` or
`{type:"replay",replay:...,parameters:{...}}`. It runs programs in order, each on
a fresh scratch target with the same seed, and stops at the first failure.
`headless:true` selects daemon-only execution; false requires the existing owning
app executable and a usable display. The default overall timeout is five
minutes, configurable from 1 ms through 30 minutes; startup and execution count
against it. A wait/action can have a shorter deadline of its own.

The daemon owns the worker after `job start` exits. `job run`/`job wait` report
nonzero on failure/cancellation. A CLI wait timeout does not implicitly cancel
its daemon-owned job. Abort requests cancellation; inspect until terminal to
confirm cleanup. No model, login or approval question is answered automatically.

At most four scratch instances can execute concurrently. Extra concurrent starts
fail explicitly with capacity evidence rather than forming an unbounded queue.
At most 16 jobs and 32 MiB of full job evidence are retained per daemon lifetime;
completed entries can be evicted with an explicit unknown-job error. Terminal
results include completed programs, first-failure evidence and cancellation/
timeout reason. Normal completion, failure, timeout and cancellation tear down
only the owned scratch processes/data. Ad-hoc job results are memory-only;
schedules retain the compact durable result described below.

## Durable local schedules

A schedule file contains `name`, `job`, `firstRunAtMs` (UTC Unix milliseconds),
and `intervalMs` (null for one-shot, otherwise at least 1000 ms). The job shape
is the same as above, including any explicit replay parameters.

```sh
bun scripts/rpa.ts schedule create schedule.json --json
bun scripts/rpa.ts schedule list --json
bun scripts/rpa.ts schedule status SCHEDULE_ID --json
bun scripts/rpa.ts schedule disable SCHEDULE_ID --json
bun scripts/rpa.ts schedule enable SCHEDULE_ID --json
bun scripts/rpa.ts schedule cancel SCHEDULE_ID --json
bun scripts/rpa.ts schedule remove SCHEDULE_ID --json
```

Schedules are held in the daemon data directory, with a private locked file and
atomic replacement. Explicit replay parameters are stored with a scheduled job;
do not put passwords, tokens or provider credentials in them. At most 32 schedule
definitions are retained, each job limited to 1 MiB. A corrupt or unavailable
store fails schedule/job setup visibly while ordinary daemon use remains
available; it is never silently reset to an empty schedule list.

- A durable claim precedes execution. A schedule never overlaps itself
- Late recurring slots coalesce to one run; future slots stay anchored to the
  original first-run cadence, without catch-up storms
- Restart records an active claim as `schedule_interrupted` and does not retry it
- A consumed/interrupted one-shot needs a new schedule; a recurring job proceeds
  at a future slot
- Disable only stops future claims; cancel also asks an active managed job to stop
- Re-enabling a recurring schedule skips disabled-time slots and uses its next
  future original-cadence slot; an unconsumed overdue one-shot runs when enabled
- Removal requires the schedule to be disabled and inactive
- A failed completion write retains the active claim and retries the compact
  result write without executing the programs again
- Full program evidence is memory-bounded; the durable last result is a compact
  status/timestamp/error summary

The daemon must be running. These definitions do not wake a sleeping computer,
start the daemon, or create an OS scheduled task. A hard process crash can leave
its disposable scratch directory behind; parent-owned child daemons exit when
they observe the dead parent. Restart does not resume these old executions.

## Shared clients and native REPL

Rust and TypeScript clients use the same `AutomationCommand`/`AutomationResult`
through their existing automation method. Native debug `waku.rpa` also exposes
recordingStart/recordingStatus/recordingStop/recordingCancel, replayStart,
jobStart/jobResult/jobAbort, and scheduleCreate/scheduleList/scheduleGet/
scheduleSetEnabled/scheduleCancel/scheduleRemove. `scratchCreate(seed,name,true)`
selects no-window execution. Use explicit supported snapshot sections for a
headless REPL attachment. CLI `inspect` and bare `state` select headless context
from the attachment identity automatically.

## Verification boundaries

The implementation's provider-free checks do not close the existing real-model
acceptance gates 2.7/3.11. Linux no-window tests do not establish macOS/Windows GUI
behavior, live Pi/Codex completion, cross-device support or credential handling.

See the [local acceptance record](local-automation-qa.md) for the tested build,
commands, test counts and remaining boundaries.
