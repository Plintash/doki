# RPA bridge

State and operating notes for the automation bridge, the OpenSpec change
`openspec/changes/add-rpa-bridge/`.

This document records the implemented surface, how to use and verify it, and
which acceptance gates are still open. The P0 checkpoint is historical verified
evidence; P1 infrastructure now has native provider-free verification and
independent review. Full phase acceptance still has the live-model gate below.
See [the verification record](rpa-p1-qa.md) for the historical P1 checkpoint,
and [agent-information QA](agent-observability-qa.md) for the later compact
context, bounded history reads and native annotation evidence.

## Where this branch stands

- This continuation started at `feat/rpa-automation` commit `ada8352`; the
  verified P0 checkpoint is `484480e`. The private GPUI fork is `Plintash/zed`,
  branch `waku-rpa`, pinned at `4d2f3b3f` by `Cargo.lock`.
- Landed foundation and P0: app-local semantic snapshot/actions, correlated
  app/daemon/provider diagnostics, bounded retention, reconnect cursors, typed
  diagnostic queries in both clients, health, and failure bundles.
- P1 infrastructure is committed and verified: daemon-routed execution,
  sessions/cursors/diffs/subscriptions, event-driven waits, model-derived
  selectable state, isolated scratch instances, daemon-owned scenarios, and
  CLI/REPL bindings on the shared protocol. Provider-free native scratch and
  explicit-build fixture replacement checks passed. Tasks 3.1–3.10 are complete.
  The actual `scripts/dev.ts` watcher relaunch passed on 2026-10-14 using the
  same external daemon, with native REPL snapshots before and after; see the
  [combined QA evidence](board-rpa-integration.md#combined-tree-results).
  The real-model phase acceptance gate also remains open.
- Agent-information completion is verified: discoverable compact context,
  cached provider/runtime/workspace state, bounded live and persisted transcript
  reads, and observable annotation controls. Final native diagnosis passed
  with a 5.31 MB selected history and unchanged unselected conversation/draft;
  see [the exact checkpoint and boundaries](agent-observability-qa.md).
- The default transport is the existing authenticated daemon connection. The
  app-local TCP endpoint is an explicit legacy fallback only.
- **Task 2.7 and phase acceptance 3.11 remain UNVERIFIED.** A real-model fan-out
  recipe exists, but provider-free smoke, Pi RPC initialization, and the
  presence of a scenario file do not establish that trial. Do not archive or
  merge the whole change on the strength of those checks.

## What runs where

- **Protocol** (`crates/waku-protocol/src/automation.rs`, `diagnostics.rs`)
  owns the shared automation and diagnostic shapes. The daemon wire version
  is `PROTOCOL_VERSION = 17`.
- **App** (`src/app/rpa.rs`, `src/app/rpa/daemon.rs`) registers its instance
  and build identity over its existing daemon connection. It owns UI truth,
  on-demand full-refresh observations, semantic input and action effects;
  input dispatch holds no view lease. `src/diagnostics.rs` sends records on
  that same connection. The app keeps no diagnostic files or offline queue.
- **Daemon** (`crates/waku-core/src/automation.rs`, `automation_scratch.rs`,
  `diagnostics.rs`, `server.rs`) owns sessions, bounded replay, subscriptions,
  waits, scenario runs, scratch lifecycle and diagnostic aggregation. The
  registry does not maintain a mirror of the full UI tree.
- **Clients** (`crates/waku-client`, `packages/waku-client`) provide executor
  routing and automation APIs alongside diagnostics. Session resume carries
  cursors; replay expiry is explicit. `scripts/rpa.ts` and debug `waku.rpa`
  use this contract rather than a second control plane.
- The surface is debug-only: release apps register no executor or `waku.rpa`
  binding; release daemons reject automation/diagnostic queries and write no
  diagnostic files. Recording also remains gated in the app and providers.
  Release-profile core/REPL checks and isolated native release-app startup
  verified these gates; see the QA record for build settings and boundaries.

## The diagnostic stream

A record is `timestampMs`, `level` (`trace`/`debug`/`info`/`warn`/`error`),
`origin`, `source`, `message`, bounded `fields`, and the correlation it owns
(`sessionId`, `turnId`, `messageId`, `requestId`). Origins: `app` (pid,
version), `daemon` (pid), `provider` (provider kind and runtime id — the
driver process generation). Sources in use: `turn`, `driver`, `tool`,
`permission`, `action`, `stderr`, `request`, `diagnostics`.

The daemon stamps each record as a `DiagnosticEvent` with its `epoch` and a
per-instance `sequence`. Clients read after a `DiagnosticCursor`; a cursor
older than retention is answered with `DiagnosticCursorExpired` **before** the
retained tail, so a gap is named rather than implied.

Retention and size bounds:

- Ring: `MAX_DIAGNOSTIC_EVENTS = 2048` records.
- One record: message and every string in `fields` are truncated at
  `MAX_DIAGNOSTIC_MESSAGE_BYTES = 8 KiB` (with a `…` marker).
- Files: one file per daemon lifetime under `<instance data dir>/diagnostics/`,
  rotated at 4 MiB, at most 3 kept. A new lifetime starts a new file because
  the sequence restarts with its epoch.
- Writer queue: 4096 records; overflow drops the oldest.

Files are the ground truth when the app is hung or not answering: everything
the app had sent is already there. Records produced while the daemon link is
down are dropped; the daemon is the stream's owner, and a restarted daemon is
a new stream (new epoch).

## Working on this branch

The dev watcher is expected to be running. Each checkout has its own debug app
and database; this branch uses flavor `RPA` (`Doki Debug — RPA`), with data and
diagnostics under `<checkout>/temp/`. Do not run `scripts/bundle.sh debug` for
a checkout whose watcher is running.

See the raw stream while the app runs:

```sh
tail -f temp/diagnostics/diagnostics-*.ndjson | jq -c \
  'select(.level != "debug") | {seq: .sequence, source, message, sessionId, turnId}'
```

Per-record shape (correlation is flattened; structured detail sits under
`fields`):

```json
{"epoch":"…","sequence":42,"timestampMs":…,"level":"info",
 "origin":{"process":"app","pid":123,"version":"…"},
 "source":"turn","message":"settled","turnCount":1,
 "sessionId":"…","turnId":"…","messageId":"…"}
```

### Connect and select the intended executor

Supply `WAKU_DAEMON_ADDRESS` and `WAKU_DAEMON_TOKEN` through the existing daemon
connection environment. Do not print, paste into commands, save in an example,
or expose the token in a snapshot. The CLI redacts transport secrets. It reads
no app-local discovery file by default. Clear a previously selected legacy
fallback before using the daemon workflow:

```sh
unset WAKU_RPA_CONFIG
bun scripts/rpa.ts instances --json
bun scripts/rpa.ts --checkout "$PWD" describe --json
# Or use the instanceId returned by instances:
bun scripts/rpa.ts --instance INSTANCE_ID describe --json
```

Replace uppercase IDs in examples with returned values. The instance list names
checkout, database, pid, build and executor connection state. For checkout or
default selection, a unique connected executor takes precedence over retained
disconnected identities. Multiple connected matches remain ambiguous, including
a working app and a scratch app sharing the checkout. `--instance` always means
the exact supplied process UUID; it does not silently choose a replacement.
No selector means the current checkout if it matches, otherwise an unambiguous
registered instance. Selecting a checkout does not discover or connect to a
different daemon.

The default app-owned daemon binds a random loopback port and exits with its
parent app. A normal watcher relaunch can therefore replace both the daemon
address and its authentication token. The CLI and REPL reconnect only to the
connection they were supplied; they do not rediscover a new address. Refresh
the caller's supplied connection through the existing authorized setup after
such a relaunch before making more calls. Sessions/cursors in a daemon that
has exited cannot be resumed on its replacement.

For a wait that spans app rebuilds, both the app and the client must already
use the same stable external daemon connection, and that daemon must remain
running throughout the wait. This is the persistent fixture used by native
watcher/reconnect QA, not proof of cross-address recovery for the default
app-owned daemon. Supplying credentials only to the CLI does not configure the
app. On macOS, the watcher explicitly forwards the already-supplied
`WAKU_DAEMON_ADDRESS`/`WAKU_DAEMON_TOKEN` through LaunchServices, because the
parent environment alone is insufficient. That forwarding has a source guard;
a native macOS launch remains unverified. This workflow does not create a new
daemon, credentials, settings or discovery mechanism.

### Observe, then act

```sh
bun scripts/rpa.ts --instance INSTANCE_ID tree
bun scripts/rpa.ts --instance INSTANCE_ID state /composer
bun scripts/rpa.ts --instance INSTANCE_ID snap --sections tree,session,composer,panels
bun scripts/rpa.ts --instance INSTANCE_ID wait --id composer-field --timeout 5000
# Mutating examples: use an approved disposable target, not a working draft
bun scripts/rpa.ts --instance SCRATCH_INSTANCE_ID focus composer-field
bun scripts/rpa.ts --instance SCRATCH_INSTANCE_ID type "draft only" --into composer-field
bun scripts/rpa.ts --instance SCRATCH_INSTANCE_ID key secondary-a
```

The actual core IDs are `composer-field`, `send-or-stop`, `toggle-sidebar` and
`open-settings`. `send-or-stop` runs the real send/stop handler; do not click it
as a harmless observation. Other IDs come from the current tree, not guessed
names. Actions fail for unavailable targets and return actionable target
information. With `--json`, actions expose their cursor, observed frame,
`treeDiff` and `stateDiff`, so a second snapshot is not needed to see the effect.

Selectable snapshot sections are `tree`, `state`, `model`, `task`, `tasks`,
`session`, `sessions`, `projects`, `route`, `board`, `composer`, `toast`, `panels`,
`ui`, `selection`, `providers`, `runtime`, and `workspace`. An empty selection
returns all sections; `state` includes every state section. Health, instance
and window identity remain present, and unselected `tree` is null. Unknown
section names fail explicitly. `/session` is the selected serialized session,
including runtime subagent runs; `/model` includes the persisted model with
observable sessions. `/tasks` contains actual Board issues, including issues
with no conversations. `/task` is the issue open in Board detail, or null.
`/sessions` contains lightweight conversation list projections with their own
IDs, `detailLoaded`, history counts and optional `task_id` bindings; one issue can have many conversations. History arrays are omitted from this list rather than falsely reported empty.
Use `/session` or `/model/sessions` when transcript detail is needed.
`/route/view` distinguishes `board`, `issue`, `session` and `settings`;
`/route/selectedTask` is separate from `/route/selectedSession`. The retained
selected conversation may be hidden while the Board is open. `/board` reports
project/search/activity filters, dialogs, and `savePending`/`saveError`. Issue
creation is optimistic; wait for `savePending: false` and check `saveError`
before claiming its asynchronous database save succeeded. Board, route,
composer, panels, toast and UI layout fields are explicit UI-only state. JSON pointers for `state` and waits are relative to
this state object, for example `/composer/text` or `/selection/status`.

Waits accept one condition: `--id` (optionally `--gone`), `--text ID=SUBSTR`,
`--state PTR=JSON`, `--state-contains PTR=SUBSTR`, `--frame N`, or
`--diagnostic JSON`. The daemon resolves the registered wait from observations;
the CLI only checks its result, never polls snapshots to decide the condition.
A pending wait does not block unrelated calls.

Control, text, frame and diagnostic waits retain lightweight tree/UI evidence
and the selected session's current health summary; they do not serialize stored
transcripts. State waits capture the section named by their JSON pointer. An
explicit `/session/status` wait therefore selects the complete `session`
section (use `/selection/status` for compact status-only observation), and an oversized requested section fails with `observation_too_large`
at the 4 MiB observation bound rather than silently truncating its model fields.
Concurrent selected sections are captured independently, including when another
client subscribes to full model state. A section's size failure only fails waits
requesting that section; it does not prevent other sections from updating.
Model observation errors identify their selection with `observationSection`.
A valid observation may still produce a delta above the 256 KiB replay-event
bound. That delta clears retained events and advances the cursor: live consumers
of its stream reconnect and receive explicit replay expiry, while clients not
subscribed to that stream keep their connection and pending waits. Observations
above the separate 4 MiB executor-wire bound remain invalid.

Executor maintenance: root notifications and paints capture only lightweight
UI sections, including cached `ui` layout values such as `sidebarViewport`, so
resize-only paints can resolve UI state waits without a model notification.
Model mutations must call `app::rpa::daemon::model_changed` at the
commit edge (save, event-pump commit, hydration or an unsaved optimistic edit).
Do not infer model changes from generic root notifications: animation, pulse
and overlay frames also generate them.

High-level `invoke` verbs use the app's existing handlers:

- `openTask {"id":"ISSUE_UUID"}` opens a real Board issue's detail. Conversation
  UUIDs are rejected, including conversations belonging to that issue.
- `newTask {}` opens the New issue dialog. `newTask {"title":"Issue title"}`
  fills and commits that same dialog, then returns the issue UUID as `opened`.
  It creates no conversation and inherits the Board's project scope/default
  isolated-worktree intent. Empty or non-string titles are rejected.
- `openSession {"id":"SESSION_UUID"}` opens a conversation.
- `newSession {}` follows the standalone New session action, including its
  ordinary reuse of an existing standalone draft. It never reuses an issue
  draft. `newSession {"taskId":"ISSUE_UUID"}` follows the issue's New session
  action: require a project, inherit issue context/workspace, reuse that issue's
  unsent draft if present, and explicitly reopen a completed issue for new work.
  Worktree creation remains deferred until submission; opening a draft makes
  no provider call. The returned `opened` is the requested conversation UUID;
  existing conversation hydration can complete asynchronously.
- `selectProject`, `submitPrompt`, `cancelPrompt`, `answerPermission`,
  `openSettings`, `togglePanel`, `chooseProvider` and `chooseModel` retain their
  existing behavior. `selectProject` selects the conversation project through
  its existing handler; Board filtering is a separate UI control.

This corrects the old RPA naming: migrate conversation-oriented `openTask` and
`newTask` calls to `openSession` and `newSession`. Provider/model selection
requires an already enabled provider and catalogued model. These verbs do not
authorize model charges, provider setup or changes to working data. Inspect the
scenario before starting it.

Diagnostic `sessionId` and `timeline --conversation` identify a conversation
session. Protocol 16 replaces the old misleading diagnostic `taskId`; migrate
log filters, timeline queries, health consumers and stored diagnostic tooling.
The old `taskId` query field and CLI `--task` option are rejected rather than
silently querying all conversations. Board issue IDs stay in model `task_id`
bindings and issue verbs.
Automation `sessionId` is a third identity, the client's attachment to an
executor, and is never a conversation or Board issue ID.

### Agent validation loop

1. Record the target build before editing. Keep the checkout's existing dev
   watcher in charge of rebuilding, signing and relaunching; start
   `bun scripts/dev.ts "RPA"` only if that watcher is confirmed unavailable.
2. After the edit, wait for the watcher's successful build and verify the fresh
   executor. With the stable external daemon prerequisite above already met,
   `wait-build --checkout` follows that checkout across new process UUIDs and
   verifies the new executor with `describe`:

   ```sh
   # App and CLI already use the same stable external daemon connection
   # Run before the edit/rebuild; jq reads only public build identity
   old_build=$(bun scripts/rpa.ts --checkout "$PWD" describe --json | jq -er '.instance.build')
   # After the edit, wait for a different build of this checkout
   bun scripts/rpa.ts --checkout "$PWD" wait-build --after-build "$old_build" --timeout 120000
   ```

   For the default app-owned daemon, save `old_build` before editing, wait for
   the rebuild, refresh the supplied connection, then run `describe` on the
   rebuilt checkout and compare its build. A wait using the old address cannot
   bridge that replacement. Explicit `--instance` pins one UUID and is not the
   checkout-following mode. Without `--after-build`, the first observed build
   becomes the baseline; it waits for the next build, not merely the current app.
3. Choose the specific checkout or instance. Use a disposable scratch target
   for mutations; run the read-only core smoke against an idle selected session
   with the sidebar visible, or prepare those preconditions in scratch.
4. Start the relevant scenario, save its session/run IDs, and read its terminal
   result. A `pending` acknowledgement is not a pass. On failure, read the
   attached evidence before adding a temporary probe or rebuilding.
5. For ordinary behavior, semantic results are the primary evidence. For
   visible changes or visual questions, show the requested prototype and
   inspect/share screenshots of the freshly rebuilt signed app. Check the
   exact provider interaction where relevant; an unrelated smoke pass does
   not validate its appearance.
6. Abort unfinished runs when stopping, then release persistent sessions and
   remove remaining scratch instances. Record the actual results and any
   unverified acceptance gates; do not infer a real-model pass from smoke.

### Agent information without disturbing the app

Protocol 17 adds bounded history reads and replaces the implicit full-session
observation in action/Describe replies with `selection`, a lightweight selected
conversation projection. It contains IDs/status/workspace, the provider-native
ID and cached context usage, `detailLoaded`, and history counts. Counts are null for unloaded history, never a false empty list.
Full `session` and `model` snapshots remain explicit, unbounded selections and
retain the existing 4 MiB observation limit.

Start with discovery and compact context:

```sh
bun scripts/rpa.ts --instance INSTANCE_ID describe
bun scripts/rpa.ts --instance INSTANCE_ID inspect
bun scripts/rpa.ts --instance INSTANCE_ID state /runtime/permission
bun scripts/rpa.ts --instance INSTANCE_ID state /providers
bun scripts/rpa.ts --instance INSTANCE_ID state /workspace
bun scripts/rpa.ts --instance INSTANCE_ID state /ui/annotations
```

`describe` returns the capability manifest in ordinary and JSON output: verb
argument contracts/effects, sections, identity domains, examples and limits.
`inspect` and bare `state` use compact sections; `tree` requests only the tree,
and `state /composer/text` requests only `composer`. `snap` without sections is
the explicit full snapshot. `--sections` remains an explicit override.

The new sections are:

- `selection`: selected conversation metadata without transcript bodies
- `providers`: cached installation, enablement, version/catalog and discovery
  state, model choices, and whether this conversation can select them.
  `authentication: notChecked` is intentional; no probe/login/model call occurs
- `runtime`: selected process state, errors/backlog, permission details and
  actual option IDs, user questions/choices, computer-approval target and
  conditional decisions, queue count and reported subagent state
- `workspace`: selected conversation's concrete path and workspace intent,
  cached branch and review results, with `notRequested`, `loading`, `ready`,
  `notRepository` or `error` as applicable. These are app caches, not a fresh
  filesystem scan; retained review data can coexist with loading/error
- `ui.annotations`: current selection, toolbar/comment/editor and preview
  visibility. `composer` also names staged attachment/annotation counts.
  Annotation editor inputs and buttons now have semantic IDs

Provider and workspace observations use only in-memory app state. Background
branch/review completions notify the same existing model observer; observation
never starts Git, reads a filesystem, changes a provider, or grants approval.
Computer approval options name their semantic `controlId`; `answerPermission`
continues to mean provider permissions only. A listed decision is information,
not permission for an agent to take it.

Read a conversation's history without opening it:

```sh
bun scripts/rpa.ts --instance INSTANCE_ID transcript --conversation CONVERSATION_UUID \
  '{"type":"index","collection":"messages","offset":0,"limit":20}'
bun scripts/rpa.ts --instance INSTANCE_ID transcript --conversation CONVERSATION_UUID \
  '{"type":"index","collection":"activities","offset":0,"limit":20}'
# Copy a returned field.query, then follow nextOffsetBytes exactly
bun scripts/rpa.ts --instance INSTANCE_ID transcript --conversation CONVERSATION_UUID \
  '{"type":"text","collection":"messages","itemId":"MESSAGE_UUID","field":"content","offsetBytes":0,"maxBytes":32768}'
```

Collections are `messages`, `activities`, and `turns`. Index pages preserve order
and contain metadata, limited previews, full scalar text lengths, continuation,
and usable text-query locators. Structured fields expose counts and fixed leaf
path templates, such as `reasoning/content` or `file_changes/0/diff`; they never
inline an image or serialize a complete large object merely to make a preview.
Text reads copy at most 64 KiB of one field and return UTF-8-safe byte
continuations. Unknown items, fields, malformed paths, or invalid offsets fail
explicitly. Read the returned locator rather than guessing a persisted field.

Loaded conversations are read from current app memory (`source: live`). Unloaded
conversations use an independent read-only daemon database query
(`source: persisted`, `detailLoaded: false`); they do not hydrate, navigate, save,
or start a provider. Stored activity/turn JSON is scanned inside the daemon,
not transferred whole to the app. Persisted data may lag unsaved live changes.
Each call observes current state, not an immutable recording: re-index if
another actor changes or removes items while you read. Native stored IDs are
preserved. Old reasoning blocks have no native activity ID; their explicitly
marked derived IDs last only until normal hydration/migration, so re-index when
that provenance changes.

The REPL has the same `inspect`, `tree`, and `transcript` calls. They return the
shared result envelope; `state` is a convenience returning the pointer value:

```js
const { result } = await waku.rpa.inspect(attachmentId);
const pending = await waku.rpa.state(attachmentId, "/runtime/permission");
const history = await waku.rpa.transcript(attachmentId, conversationId,
  { type: "index", collection: "activities", offset: 0, limit: 20 });
```

Transcript reads require the daemon transport. The optional legacy snapshot
request now honors `sections`, but that fallback has no transcript RPC.

### Scratch setup and scenario lifecycle

`scratch create` starts a disposable app and daemon with a private data
folder/database. It does not copy the working database, credentials, provider
profiles or original HOME. Providers are disabled by default, and model
startup/discovery is not enabled. The launcher requires this checkout's native
debug app and daemon binaries and the owning daemon's `WAKU_APP_EXECUTABLE`
configuration. Data lives under its `rpa-scratch/<uuid>/` folder; HOME/XDG/TMP
are isolated and the child's private token is never returned. This is not a
headless test runner. Creation waits for executor registration, which is not
a guarantee that the first workspace frame has painted.

```sh
bun scripts/rpa.ts scratch create --name "RPA smoke" --json
# Alternative: a prepared AutomationScratchSeed fixture
bun scripts/rpa.ts scratch create --name "Seeded smoke" --seed seed.json --json
```

Use one creation command per desired scratch instance. A seed has typed
`projects` and `sessions` arrays and an optional `tasks` array (empty by
default); use the shared `Project`, `AgentSession` and Board `Task` wire shapes.
Project paths are remapped into the scratch folder; concrete worktrees become
local scratch workspaces, while issues' `newWorktree` intent is retained.
Provider/runtime cursors, active status and queued messages are reset. Session
`task_id` bindings are retained only for valid seeded issues in the same
project. Empty issues and issue-owned drafts are durable; standalone empty
seeded sessions retain their identity initially but later ordinary saves may
remove unused standalone drafts. Seed data does not provision a provider or
copy a repository.

An empty scratch app begins on onboarding. Prepare the smoke scenario without
submitting a prompt:

```sh
bun scripts/rpa.ts --instance SCRATCH_INSTANCE_ID wait --id onboarding-projectless --timeout 5000
bun scripts/rpa.ts --instance SCRATCH_INSTANCE_ID click onboarding-projectless
bun scripts/rpa.ts --instance SCRATCH_INSTANCE_ID wait --id composer-field --timeout 5000
bun scripts/rpa.ts --instance SCRATCH_INSTANCE_ID state /panels/sidebar
# Only if the previous value was false:
bun scripts/rpa.ts --instance SCRATCH_INSTANCE_ID invoke togglePanel '{"panel":"sidebar"}'
```

The included `rpa/scenarios/core-smoke.json` only observes its pinned controls;
it does not create a session or open the sidebar itself. Start it on the prepared
scratch target, or replace `--instance` with `--checkout "$PWD"` for a working
app that already meets those read-only preconditions:

```sh
bun scripts/rpa.ts --instance SCRATCH_INSTANCE_ID scenario start rpa/scenarios/core-smoke.json --json
# Save the returned sessionId and runId, then request results until terminal
bun scripts/rpa.ts scenario status RUN_ID --session SESSION_ID --json
# If stopping a pending run:
bun scripts/rpa.ts scenario abort RUN_ID --session SESSION_ID --json
# After inspecting its final result:
bun scripts/rpa.ts detach --session SESSION_ID --json
bun scripts/rpa.ts scratch remove SCRATCH_INSTANCE_ID --json
```

Scenarios continue in the daemon after the CLI exits. `status` returns ordered
step outcomes and the final cursor; failure stops later steps and exits the CLI
with status 1, including the run and complete failure bundle on stderr. A
scratch instance owns one scenario. Its terminal run automatically tears down
that app/daemon and directory; a bounded final result remains queryable through
the owning daemon until retention evicts it or that daemon exits. Resume then
reports `executorConnected: false`; it does not revive the app. Explicit
`scratch remove` is idempotent after teardown. Abort a scratch run before
detaching its active session. Remove scratch instances that were created but
never used in a run.

Normal removal, terminal-run teardown and owning-backend shutdown request a
graceful child-daemon shutdown, wait up to 500 ms for backend cleanup, then
kill/reap the owned processes and remove their data. Disconnecting a routed
CLI/REPL session only closes its transport; it does not stop the scratch daemon.
A hard-killed owning daemon bypasses that cleanup and can leave a scratch app
retrying a dead child daemon and scratch data on disk. Crash-recovery monitoring
is not part of this P1 slice; do not treat normal lifecycle cleanup as a
crash-proof guarantee.

### Persistent sessions, diffs and subscriptions

Ordinary one-shot CLI calls attach and detach automatically. `attach` and
`scenario start` retain sessions; `--session` resumes one instead of selecting
a new instance and cannot be combined with `--checkout` or `--instance`:

```sh
session_json=$(bun scripts/rpa.ts --instance INSTANCE_ID attach --json)
session_id=$(printf '%s' "$session_json" | jq -er '.sessionId')
cursor=$(printf '%s' "$session_json" | jq -c '.cursor')
bun scripts/rpa.ts resume --session "$session_id" --after "$cursor" --json
bun scripts/rpa.ts diff --session "$session_id" --after "$cursor" --streams ui,model,diagnostic --json
bun scripts/rpa.ts detach --session "$session_id" --json
```

Use the actual returned cursor, not a fabricated epoch or assumed sequence.
Diffs carry retained events, the next cursor and explicit expiry when retention
has been exceeded. The shared clients also expose pushed UI/model/diagnostic
subscriptions and wait/run notifications. In TypeScript,
`subscribeAutomation(sessionId, listener)` installs a local listener; also send
`client.automation({ type: "subscribe", sessionId, streams: ["ui", "model"] })`
to select daemon streams. Resume with the saved cursor after reconnecting to
the same live daemon; a replacement daemon does not retain those sessions.
No UI frame events are pushed when no subscription or pending wait/run needs
them. Unsubscribe by sending `streams: []`, then detach when finished.

The debug coding-agent REPL exposes `waku.rpa` independently of Computer Use.
Authentication stays in native code, not a JavaScript-visible token. These
bindings return shared protocol result envelopes:

```js
const { instances } = await waku.rpa.instances();
const target = instances.find(instance => instance.instanceId === "INSTANCE_ID");
if (!target) throw new Error("Expected executor is not registered");
const { session } = await waku.rpa.attach(target.instanceId);
await waku.rpa.subscribe(session.sessionId, ["ui", "model"]);
const { result } = await waku.rpa.snapshot(session.sessionId, ["tree", "session", "composer"]);
const changes = await waku.rpa.diff(session.sessionId, result.cursor, ["ui", "model"]);
const pushed = await waku.rpa.events(session.sessionId);
await waku.rpa.subscribe(session.sessionId, []);
await waku.rpa.detach(session.sessionId);
```

`waku.rpa` also provides `describe`, `act`/`click`, `type`, `key`, `invoke`,
`wait`/`waitResult`, `resume`, `scenarioStart`/`scenarioResult`/`scenarioAbort`,
`scratchCreate`/`scratchRemove`, `waitForBuild`, and the diagnostic helpers.
`wait` returns a registered wait; consume `events` or `waitResult` for its
resolution. `waitForBuild(checkout, previousBuild, { timeoutMs, pollMs })` requires
an explicit checkout and baseline build plus the same stable external-daemon
prerequisite as CLI `wait-build`. It follows the checkout across process UUIDs,
ignores stale disconnected history when one connected candidate exists, and
rejects multiple connected candidates. It retries transient same-connection
races and verifies the fresh executor with `describe`; it neither retries a
mutation nor rediscovers another daemon address. The timeout defaults to 30000 ms
(allowed: 1–300000) and poll interval to 100 ms (allowed: 10–1000). Diagnostic
helpers query the connected daemon, not an attached automation session.

### Semantic identity rules and limits

Use a stable descriptive ID for a static control, preserving established IDs
such as `composer-field` and `send-or-stop`. For a repeated domain control,
include its object UUID, never its current list position. Shared primitives
supply roles and disabled-state annotation; the caller supplies the stable ID
and meaningful label/value. Add the identity before claiming that a new
interactive control is semantically testable.

Debug warnings cover instrumented primitives missing IDs and interactive nodes
that GPUI actually emits with missing IDs/roles. A hand-written `Div` with
neither ID nor role is invisible to the pinned GPUI tree API, so this is not a
universal catch-all audit. Keep explicit identity on those callsites, the
source guard in `scripts/rpa-identity.test.ts`, and the core smoke's pinned IDs.
The tree describes rendered controls, not off-screen virtualized rows.

### Temporary legacy fallback

Only launching a debug app with `WAKU_RPA_LEGACY_TCP=1` enables the transitional
loopback listener and `temp/rpa.json` publication. Release builds ignore it.
Selecting `--config <checkout>/temp/rpa.json` or setting `WAKU_RPA_CONFIG`
explicitly opts the CLI into that listener; neither happens by default:

```sh
bun scripts/rpa.ts --config temp/rpa.json describe
```

This fallback retains P0 regression coverage, not daemon sessions, scratch,
scenarios or subscriptions. `raw` means a shared `AutomationCommand` on the
default transport; it means the old P0 request only with the fallback selected.
The fallback uses TCP protocol 3 for the session/issue identity split. Both
ends reject older versions; the CLI never adopts an arbitrary discovery
version. Do not read or print the discovery file's token.

## Explain a failure without rebuilding

The diagnostic query implementation lives in the daemon. Both daemon client
libraries expose typed helpers, and the CLI calls them directly on its existing
authenticated connection. The same connection carries runtime verbosity changes
and app counter reports. `logs`, `timeline`, `counters` and `verbosity` reject
`--instance`, `--checkout` and `--session`: they query only the connected daemon.
Targeted scratch action/wait/scenario failures still carry evidence from that
scratch instance; a direct query on the owning daemon is not a scratch query.

```sh
bun scripts/rpa.ts logs --filter '{"minLevel":"warn"}'
bun scripts/rpa.ts logs --filter '{"source":"driver","sessionId":"<uuid>"}'
bun scripts/rpa.ts timeline --conversation SESSION_UUID --turn TURN_UUID
bun scripts/rpa.ts counters
bun scripts/rpa.ts verbosity
bun scripts/rpa.ts verbosity '{"defaultLevel":"info","overrides":[{"process":"app","source":"pump","level":"debug"}]}'
bun scripts/rpa.ts verbosity '{"defaultLevel":"info","overrides":[]}'
bun scripts/rpa.ts --instance INSTANCE_ID --json wait --id deliberately.missing --timeout 100
```

The last command deliberately exits 1 and prints the complete error as JSON
on stderr, including `error.bundle`. The ordinary human-readable error also
prints the bundle. There is no separate dump command to remember after a
failure. Authentication failures do not reveal the app state or diagnostics.
Snapshots and bundles expose public instance identity only, never a transport
token.

A bundle contains:

- `observed`: the failure-time state, semantic tree, and health summary
- `treeDiff`: added, removed and changed nodes and focus since the call's
  baseline, with explicit flags when either frame was unavailable
- `recentEvents`: up to 128 relevant records from the previous minute,
  preserving each origin and correlation, plus cursor/truncation information
- `counters`: app failure-time counters and the daemon's reported samples
- `diagnosticErrors`: which evidence could not be fetched; a dropped or hung
  daemon never replaces the original error or hides the app's own evidence

Health names the actual daemon link, last error, session/provider states and
pending permissions, rather than presenting raw logs. Health correlation fields
use `sessionId` for the conversation, including selected/active sessions,
providers, pending requests and session errors. Counter reports retain
sampling timestamps, so an old sample is distinguishable from a current one.
App counters include `framesDrawn`, `renderCommits`, `pumpCommits`,
`eventBacklog` and transport `requestRoundTripMs`; daemon counters include
retention/writer drops, queue depths, runtime/client counts and request service
time. Render instrumentation only increments memory counters; a zero-size paint
probe counts on-demand draws too. It performs no I/O or extra frame requests. The
existing one-second RPA maintenance timer publishes app samples.

Logs accept `sinceMs`, `untilMs`, `minLevel`, `process` (`app`, `daemon`, or
`provider`), `source`, `sessionId`, `turnId`, `messageId`, `requestId`, `runtimeId`,
`after` (a diagnostic cursor), and `limit`. Results retain the most recent
matching records in stream order and name any truncation or cursor expiry.
Timelines merge lifecycle milestones by emitter timestamp, with sequence as
the tie-breaker, keeping duplicate app/provider observations attributable.
An unfinished turn names its last phase and the time it last changed.

Verbosity replaces the recording policy. The default is `info`; overrides
match process and/or source, and the last matching override wins. Raising
`app`/`pump` to `debug` emits coalesced pump commits, never one record per
provider chunk. Restoring the default stops these details without a restart.

Read the targeted failure bundle first, then use `timeline` or filtered `logs`
on the correct daemon if more history is needed. If the app cannot answer, that
instance's daemon NDJSON files remain available directly. Missing or unavailable
evidence is reported explicitly rather than replaced with another instance's
records. Authentication failures and calls without an observable executor may
not have a UI bundle.

## Verifying a change

These are validation commands; [the verification record](rpa-p1-qa.md) lists
which checks ran and their limitations. Coordinate native builds/tests with the checkout's watcher and review process; do not start
a competing debug bundler.

```sh
cargo fmt --all -- --check
cargo test -p waku-protocol
cargo test -p waku-core --lib diagnostics
cargo test -p waku-core --lib automation
cargo test -p waku-core --lib server:: -- --test-threads=1
cargo test -p waku-client
cargo test -p waku --lib -- --test-threads=1
cargo test -p waku --test rpa_spike --test rpa_list
bun run protocol:check
(cd packages/waku-client && bun test && bun run check)
bun test scripts/rpa.test.ts scripts/rpa-identity.test.ts
openspec validate add-rpa-bridge --strict
```

With this checkout's rebuilt native debug app idle and the existing
`WAKU_DAEMON_ADDRESS`/`WAKU_DAEMON_TOKEN` environment already supplied:

```sh
# Read-only against the working executor; includes deliberate failed observations
WAKU_RPA_DAEMON_TEST=1 bun test scripts/rpa-daemon-live.test.ts
# Mutates only a newly created scratch app; submits no model prompt
WAKU_RPA_WORKFLOW_TEST=1 bun test scripts/rpa-workflow-live.test.ts
```

The daemon live test supports `WAKU_RPA_TEST_INSTANCE` or
`WAKU_RPA_TEST_CHECKOUT`; the scratch workflow test supports
`WAKU_RPA_TEST_INSTANCE` for choosing the working baseline. Both are opt-in.
Legacy P0 native coverage additionally requires the app to have been launched
with `WAKU_RPA_LEGACY_TCP=1`:

```sh
WAKU_RPA_TEST_CONFIG="$PWD/temp/rpa.json" bun test scripts/rpa-live.test.ts
```

Protocol edits require regenerating TypeScript bindings with
`bun run protocol:generate`; ts-rs copies doc comments too, so a comment-only
change can make the check stale. Release verification must separately establish
that no executor/binding/query surface is available; the debug native suites
cannot prove it.

Anything touching a streaming frame path also needs the reasoning and checks
in `docs/performance.md`. Diagnostics use bounded queues/rings and a background
writer. UI subscription capture must not serialize all stored transcripts on
every paint.

### Real-model acceptance remains open

`rpa/scenarios/fan-out-real-model.json` is the repository recipe for a real Pi
foreground two-child run. It opens a standalone session with `newSession`, selects the already configured
Pi provider, submits a real prompt, and asserts child and parent completion.
It may incur model charges. Run it only with explicit approval for that trial
and a disposable target whose supported provider, model and subagent tool are
already configured. Existing authorization covering that trial and target is
sufficient; do not ask again just because the recipe is now daemon-owned. It
does not establish actual fan-out chronology from UI state alone; retain the
correlated provider timeline too.

**A newly created scratch instance cannot run this recipe as-is:** providers
are disabled and credentials/configuration are not inherited. `--seed` only
provides model fixtures; it does not configure or authenticate Pi. Stop and
report the missing disposable provider setup instead of copying credentials,
upgrading Pi, enabling providers or testing against a working task without
separate authorization.

After that prerequisite is explicitly satisfied, the acceptance operations are:

```sh
bun scripts/rpa.ts --instance APPROVED_DISPOSABLE_INSTANCE_ID scenario start rpa/scenarios/fan-out-real-model.json --json
bun scripts/rpa.ts scenario status RUN_ID --session SESSION_ID --json
# While a disposable executor is still running, deliberately fail one observation
bun scripts/rpa.ts --instance APPROVED_DISPOSABLE_INSTANCE_ID wait --id deliberately.missing --timeout 100 --json
```

A scratch scenario tears its executor down when terminal; perform the failed
observation before teardown or on a separate approved still-live disposable
instance. Save the failed wait's complete stderr bundle, the run's final result
and correlated timeline from the correct daemon. Record the actual command
outcomes without temporary probes. No live real-model result is recorded here:
**task 2.7 and phase 3.11 remain UNVERIFIED**.

### P0 implementation checkpoint (2026-10-13)

Tasks 2.5 and 2.6 are verified. The final implementation was reviewed against
`ada83525d161c563b8d6ad1a48e06e29650d3926` in five independent Standards/Spec
rounds; both axes ended with no actionable findings.

- Protocol: 94 tests; Rust client: 33 tests; diagnostic queries/retention:
  18 tests; server: 45 tests, run serially
- App library: 552 passed, 2 pre-existing ignored tests
- TypeScript client and CLI: 35 tests, plus TypeScript checking and generated
  binding freshness
- Native debug app: 9 live tests, including all failure paths, authentication
  isolation and advancing frame/render counters while the window is covered
- Native CLI: snapshot, filtered logs, timeline, counters, changing/restoring
  verbosity and a deliberately failed wait with complete cross-process evidence
- Workspace debug build and release check, formatting and strict OpenSpec
  validation passed

The lifecycle-delivery regression holds the supervisor's actual mutex while
four correlated milestones are sent over a real test WebSocket. Optional
sampling skips a contended transition; required records use the current
lock-free transport handle. Release builds allocate no diagnostic handle cache.

**Task 2.7 remains open.** These native checks used an isolated, idle app and
no model prompt. They do not establish the real-model subagent fan-out acceptance
gate. P1 infrastructure is a separate increment and must retain that pending
live acceptance gate rather than treating this checkpoint as full P0 acceptance.

## Decisions already settled

The change went through ten two-axis review rounds. These points were raised
and deliberately settled; do not re-litigate them without new evidence:

- **The daemon owns retention and files.** The spec delta's "Always-on bounded
  retention" names the daemon; app records reach the files by being sent.
  App-side persistence or an offline queue is out of scope (design D7:
  "no second per-process copy").
- **App and provider milestones both exist.** One driver event can produce an
  `app`-origin record (the app's view) and a `provider`-origin record (the
  daemon's view of the same event). The timeline is expected to merge them by
  session, turn and timestamp.
- **No provider OS pid.** `runtime_id` is the process generation and is what
  distinguishes a restarted provider for the same session.
- **Tool completions are milestones; provider progress ticks are not.** ACP
  emits `RichActivity { complete: false }` per tick; recording those put
  provider-rate work on the UI thread and mislabelled one activity as many
  starts. A hanging tool shows as an unfinished turn, not as a start record.
- **Failed RPCs are the daemon's own records.** They are rare (real errors),
  correlate to the session, and give the `daemon` origin its purpose.
- **Expiry names the gap, then replays the retained tail.** The scenario the
  spec states is "says the cursor expired instead of a silent gap"; the
  retained tail is still useful, so it follows the notice.
- **The writer stops on the first failed write.** A broken disk cannot be
  fixed from the writer thread; the in-memory ring keeps serving, and the
  failure is printed once. An unusable diagnostics directory is reported in
  the stream (`source: "diagnostics"`) and the daemon still starts.
- **`TurnStatus::Running` in `turn_settled` and `StreamPhase::Activity` in
  the stream label** are total-function coverage over shared enums, not live
  paths.
- **Records are size-bounded by truncation** (message and field strings at
  8 KiB). That is the enforcement of "retention MUST be bounded by size or
  count"; writer and retention counters in 2.5/2.6 expose dropped totals.

## Next

- Preserve the provider-free native evidence, independent Standards/Spec
  clearance and original P0 review baseline when integrating this branch.
- **2.7 / 3.11** — establish an explicitly approved, provider-configured
  disposable target; execute the real-model fan-out recipe and the one-command
  failed-wait explanation with no temporary probes. Keep these acceptance
  gates open until the actual run and its evidence have been reviewed.
- Archive only after the project's review and acceptance requirements are
  satisfied. P2 scheduling/scripted-provider/headless work remains separate.
