# Issue workflows

An issue workflow runs an ordered set of harness stages in one issue workspace.
It is separate from Board lifecycle: adding or moving columns still does not
start a provider. Completing a workflow never merges or deploys. By default it
leaves a report-ready run for the user to inspect; a final Report stage explicitly
mapped to the project's terminal Board stage can finish the issue after all owned
providers have stopped.

The global assistant is a client of the daemon's workflow API. A native or
browser client can disconnect without becoming the owner of stage progression.
The manual **Prepare** flow remains available for explicit, one-off drafts.

## Workflow configuration

`IssueWorkflow` contains an ordered `stages` array and `max_review_retries`.
Each `IssueWorkflowStage` has a stable ID, a name, a role, a provider, an optional
model, and instructions. `board_stage_id` retains optional Board provenance.
Ordinary stages do not move cards; the explicit final terminal-stage mapping is
the completion exception. Existing custom Board columns are retained.

The supplied preset is Plan → Execute → Review → Report. Every stage's harness,
model and instructions can differ. Plan inspects and proposes; execution changes
and verifies; review independently assesses; report records what was actually
delivered and tested. A rejection returns to execution with the review findings
and is reviewed again. The preset permits two correction rounds; the configured
bound is at most ten. Exhausting the bound requires manual intervention.

Custom sequences support up to 32 stages. A review needs a preceding execution
stage; the last stage must be Report. Configuration is snapshotted on the run,
so editing a project later does not rewrite a running attempt's provenance.

## Durable control API

The transport commands are:

- `ListIssueRuns { task_id }`: global list when `task_id` is absent, or one issue
- `StartIssueRun { task_id, request_id, workflow }`: start one explicit intent
- `ControlIssueRun { run_id, action }`: Pause, Resume, Cancel or Retry

Responses are `IssueRuns { runs }` and `IssueRun { run }`. Rust exposes
`IssueRunClient`; the browser client exposes `listIssueRuns`, `startIssueRun`
and `controlIssueRun`. These clients do not implement a second state machine.
Blocking RPC calls run away from the native UI thread. The native controls show
run status separately from the issue's Board lifecycle, so a ready report is not
misrepresented as an agent still working.

The caller retains the start's `request_id` across a timeout or reconnect. The
transport request has its own identity; a repeated start intent must return the
same durable run, not spawn another provider. A separate start cannot overlap
an unfinished run for the issue. Concrete checkouts also exclude competing
daemon-owned agent starts, rewind and built-in file/Git mutations, including
another issue targeting the same Git worktree. Distinct worktrees remain
independent, and read-only inspection stays available. This ownership guard is
not a filesystem sandbox: existing terminals and external editors remain under
the user's control.

Pause stops advancement after the current attempt settles; it is not a promise
that an in-flight provider immediately stops editing. Cancel requests provider
shutdown and remains pending until shutdown is known. Resume and Retry are
separate actions: resuming a paused boundary does not duplicate its finished
stage, while retrying an interrupted/failed stage creates a recorded attempt.

The run and attempt are persisted before provider startup. After a daemon
restart, interrupted work is parked for explicit recovery rather than
resubmitted on the assumption that a lost result means no work happened.
An unfinished configured Report-to-Done transition is reconciled separately,
without rerunning a provider or regenerating its durable report.

## Explicit completion, handoff and reports

A successful transport turn or arbitrary assistant prose is insufficient to
advance. Stages return a terminal `doki-stage-result` fenced JSON object with
these fields:

- `run_id` and `attempt_id`: exact identities from the submitted instructions
- `decision`: completed, approved, rejected or blocked, appropriate to the role
- `summary`: public result or actionable review findings
- `evidence`: references to the work and the verification actually performed

The daemon validates the envelope after the provider settles. Private reasoning
and unrelated provider history are not stage handoff inputs. Source attempt and
session identities remain available in the durable run, alongside the outcome.
The report is retained on the run; it is not inferred from a green-looking card
or a provider's generic success event.

The initial automatic-stage adapters support Codex, Claude, Pi, Oh My Pi and
OpenCode. Other harnesses remain usable manually and return an explicit workflow
blocker instead of silently substituting a provider.

Permission and user-input requests require attention. Unsupported harnesses,
provider failures, invalid/missing outcomes, exhausted review retries and
interrupted recovery are visible states, not silent approval or successful
completion.

## Validation boundary

Automated acceptance uses fake harness events and disposable persistence. No
model account, credentials or real provider turn is required. Regression cases
cover progression, correction/re-review, controls, duplicate requests,
reconnect/restart, failure and malformed results. Browser protocol tests verify
that reconnect does not issue a start and that an explicit retry preserves its
original intent ID. Native/device acceptance belongs to the owner.

See [issue #124](https://github.com/Plintash/doki/issues/124) for scope and the
implementing PR for the exact validated tree, test results and remaining limits.
