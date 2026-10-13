# RPA bridge

State and operating notes for the automation bridge, the OpenSpec change
`openspec/changes/add-rpa-bridge/`.

This document records what the branch has landed, how to use and verify it,
and which decisions are already settled. It is the handoff for whoever
continues the change; task 3.10 will extend it with the full validation loop.

## Where this branch stands

- Branch `feat/rpa-automation`, based on `main` at `3c3309c`, ten commits
  ahead. The private GPUI fork is `Plintash/zed`, branch `waku-rpa` (pinned in
  `Cargo.toml`; the taps are forced tree building, the frame snapshot, and
  action dispatch).
- OpenSpec progress is 6/20 tasks:
  - **1.1, 1.2** — the fork and the spike (loopback endpoint, semantic
    snapshot/actions, `scripts/rpa.ts`).
  - **2.1–2.4** — the diagnostic stream: protocol, app records, daemon
    aggregation, both client libraries.
  - **2.5–2.7** — query surface (`logs`, `timeline`, `counters`, `verbosity`),
    health and failure bundles, the live fan-out trial.
  - **3.1–3.11** — P1: the daemon-hosted runtime, executor registration,
    sessions/cursors/waits, model-derived state, instance targeting, scratch
    instances, scenario runs, CLI/REPL bindings, instrumentation conventions,
    docs, phase acceptance.
- The change is not ready to archive or merge: P0 is unfinished and the P1
  transport (daemon-mediated execution) has not started.

## What runs where

- **Protocol** (`crates/waku-protocol/src/diagnostics.rs`) owns the record and
  event shapes; the wire version is `PROTOCOL_VERSION = 13`.
- **App** (`src/diagnostics.rs`) emits records over its existing daemon
  connection (`ClientMessage::Diagnostic`). It keeps no files and no queue:
  the daemon is the instance's stream and file owner.
- **Daemon** (`crates/waku-core/src/diagnostics.rs`, `server.rs`) sequences the
  instance's one stream: a 2048-record ring with an epoch and a monotonic
  sequence, subscribers with cursor replay, and rotating NDJSON files.
- **Clients** (`crates/waku-client`, `packages/waku-client`) consume the
  stream, resume from the last cursor after a reconnect, and surface expiry.
- Everything is debug-only. A release app builds no records (lazy builders and
  a single gate in `diagnostics::record`); a release daemon neither classifies
  nor sequences (`EventSink::record`, `Hub::record_diagnostic`,
  `ProviderDiagnostics::stderr` all gate). Release builds also pass no
  diagnostics directory in `waku-daemon`.

## The diagnostic stream

A record is `timestampMs`, `level` (`trace`/`debug`/`info`/`warn`/`error`),
`origin`, `source`, `message`, bounded `fields`, and the correlation it owns
(`taskId`, `turnId`, `messageId`, `requestId`). Origins: `app` (pid,
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
  'select(.level != "debug") | {seq: .sequence, source, message, taskId, turnId}'
```

Per-record shape (correlation is flattened; structured detail sits under
`fields`):

```json
{"epoch":"…","sequence":42,"timestampMs":…,"level":"info",
 "origin":{"process":"app","pid":123,"version":"…"},
 "source":"turn","message":"settled","turnCount":1,
 "taskId":"…","turnId":"…","messageId":"…"}
```

### Driving the app (spike)

The spike's loopback endpoint still exists until P1 retires it. It publishes
`temp/rpa.json`; `scripts/rpa.ts` reads it (or `--config <other
checkout>/temp/rpa.json`, or `WAKU_RPA_CONFIG`):

```sh
bun scripts/rpa.ts describe          # instance, build, protocol
bun scripts/rpa.ts tree              # controls on screen: id, role, label
bun scripts/rpa.ts state /composer   # one JSON pointer of app state
bun scripts/rpa.ts snap              # the full snapshot
bun scripts/rpa.ts click composer.send
bun scripts/rpa.ts type "run the tests" --into composer.input
bun scripts/rpa.ts key secondary-l
bun scripts/rpa.ts invoke openTask '{"id":"<uuid>"}'
bun scripts/rpa.ts wait --id composer.send --timeout 5000
bun scripts/rpa.ts raw '{"type":"describe"}'
```

A call from this branch is also recorded as an `action` record (read-only
`describe`/`snapshot` calls are not), which is how the stream shows what an
agent did around a failure.

## Verifying a change

```sh
cargo fmt --all -- --check
cargo test -p waku-protocol
cargo test -p waku-core --lib diagnostics
cargo test -p waku-core --lib server::
cargo test -p waku-client
cargo test -p waku --lib
cargo test -p waku --test rpa_spike --test rpa_list
bun run protocol:check              # generated TS bindings are current
(cd packages/waku-client && bun test)
openspec validate add-rpa-bridge --strict
```

Protocol edits require regenerating the TypeScript bindings with
`bun run protocol:generate`; ts-rs copies doc comments too, so a comment-only
change can make the check stale.

Anything that touches a streaming frame path also needs the reasoning in
`docs/performance.md`; diagnostics are deliberately off that path (channel
send, background writer, bounded rings).

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
  task, turn and timestamp.
- **No provider OS pid.** `runtime_id` is the process generation and is what
  distinguishes a restarted provider for the same task.
- **Tool completions are milestones; provider progress ticks are not.** ACP
  emits `RichActivity { complete: false }` per tick; recording those put
  provider-rate work on the UI thread and mislabelled one activity as many
  starts. A hanging tool shows as an unfinished turn, not as a start record.
- **Failed RPCs are the daemon's own records.** They are rare (real errors),
  correlate to the task, and give the `daemon` origin its purpose.
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
  count"; dropped records are not individually marked until the counters land
  in 2.5/2.6.

## Next

- **2.5** — daemon queries (`logs`, `timeline`, `counters`, `verbosity`) and
  the `scripts/rpa.ts` surface over them. A stuck-turn timeline is the first
  consumer of the `turn`/`driver` records above.
- **2.6** — health in every snapshot, failure bundles on failed waits,
  actions and calls.
- **2.7** — reproduce the subagent fan-out, explain a deliberately failed wait
  from one command, and write the validation loop into this document.
- **P1** — see `openspec/changes/add-rpa-bridge/design.md` (D1–D12); the
  spike endpoint is retired behind an environment flag once the executor
  registers on the daemon connection.
