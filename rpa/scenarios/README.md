# Daemon-owned RPA scenarios

These JSON files use `AutomationScenario` from the shared daemon protocol.
The CLI starts an ordered run and prints its `sessionId` and `runId`. The run
continues after the CLI disconnects; `scenario status` resumes the session and
returns the daemon's results. A failed step stops subsequent steps and retains
its full failure bundle. Neither the CLI nor these scenarios polls snapshots
to resolve a condition.

## Provider-free smoke

`core-smoke.json` only observes pinned controls. Run it against an idle debug
executor after the checkout's dev watcher has finished a successful build.
Select a session and leave the sidebar visible first; this read-only scenario
reports a missing precondition rather than opening sessions or panels itself:

```sh
bun scripts/rpa.ts --checkout "$PWD" scenario start rpa/scenarios/core-smoke.json --json
bun scripts/rpa.ts scenario status RUN_ID --session SESSION_ID --json
bun scripts/rpa.ts detach --session SESSION_ID --json
```

Use the actual IDs printed by `scenario start`. A terminal scratch scenario
automatically removes its disposable app; its bounded final result remains
available through the owning daemon. Normal terminal/removal/backend-shutdown
cleanup reaps owned processes; a hard-killed owning daemon can leave processes
or data behind. See `docs/rpa.md` for lifecycle limits and the stable-daemon
prerequisite when waiting across a watcher relaunch.

## Real-model fan-out: authorization and configured target required

`fan-out-real-model.json` is an acceptance recipe in this change, **not an
executed acceptance result**. It opens a standalone session with `newSession`, selects the existing Pi
provider and submits a prompt that launches two real children. The prompt
prohibits source edits, shell commands and network access; the scenario still
changes its disposable app and can incur model charges. It is never started
by the default test suite. Use existing user authorization for this exact
real-model trial and disposable target; do not ask for redundant approval when
that authorization and an allowed configuration already exist. Do not upgrade
Pi, copy or probe credentials, or modify a working task to make it pass.

The target must already have a configured supported Pi provider, a model and
the foreground subagent tool. A missing provider/model is an actionable
failure, not permission to change the user's configuration. The scenario
asserts the serialized runtime state of both children and the containing run,
then the parent turn settling. Its final snapshot retains model and UI
observations. It does not claim to prove concurrency from UI state alone;
inspect the correlated provider timeline for the actual fan-out chronology.

**Fresh scratch is not configured for this recipe.** `scratch create` disables
providers and isolates HOME/configuration; it does not inherit credentials or
provider profiles. A seed file supplied with `scratch create --seed seed.json`
contains typed `projects`, `sessions` and optional Board `tasks` fixtures. It neither copies the
working database implicitly nor enables/authenticates Pi. Provider setup or
credential copying is not a step in this recipe.

When an already authorized, disposable, provider-configured target is actually
available, use its registered instance ID. The existing daemon connection must
already be supplied through `WAKU_DAEMON_ADDRESS` and `WAKU_DAEMON_TOKEN`; the
CLI never prints its transport token:

```sh
bun scripts/rpa.ts --instance APPROVED_DISPOSABLE_INSTANCE_ID scenario start rpa/scenarios/fan-out-real-model.json --json
bun scripts/rpa.ts scenario status RUN_ID --session SESSION_ID --json
```

No such configured-target live result is established by this change. Missing
configuration leaves the trial pending; **task 2.7 and phase 3.11 remain
UNVERIFIED**. To stop a pending run use
`scenario abort RUN_ID --session SESSION_ID`, then `detach --session SESSION_ID`
after inspecting its result. For a scratch target, terminal success/failure/abort
tears down its app/daemon while retaining a bounded final result. Remove any
remaining unused scratch instance with `scratch remove SCRATCH_INSTANCE_ID`.

To reproduce an explainable failed observation on a still-running executor:

```sh
bun scripts/rpa.ts --instance INSTANCE_ID wait --id deliberately.missing --timeout 100 --json
```

That command is expected to exit 1 and print the complete failure bundle on
stderr. Use a still-live disposable executor before scratch scenario teardown,
or a separate approved disposable instance. Provider-free smoke and this
deliberate failure do not establish the real-model acceptance gate.

Direct `logs`, `timeline`, `counters` and `verbosity` commands query the daemon
named by the connection. They reject `--instance`, `--checkout` and `--session`
rather than returning evidence from a different target. Targeted action, wait
and scenario failures keep their instance-specific evidence bundles.

For watcher rebuilds use `wait-build --checkout PATH`, which follows that
checkout across per-process instance UUID changes. Without a selector the
first unambiguous selection pins its checkout. `--instance ID` deliberately
pins that exact process instead. Checkout selection prefers a unique live
executor over retained offline history; multiple live/scratch matches remain
ambiguous. `--timeout` is a shared budget for connection, lookup, attach,
verification and best-effort cleanup rather than a new budget per RPC.


## Board issue and conversation identity

`openTask`/`newTask` operate on real Board issues; `openSession`/`newSession`
operate on conversations. Use `newSession {"taskId":"ISSUE_UUID"}` to start or
reuse that issue's draft with inherited project and isolated-worktree intent.
`tasks` and `sessions` snapshots expose these separate catalogs. See
`docs/rpa.md` for the complete contract and asynchronous issue-save state.
The opt-in `scripts/rpa-workflow-live.test.ts` includes provider-free scratch
coverage of empty issue creation, one issue with two conversations, UUID
rejection, issue-draft reuse, and standalone draft separation.
