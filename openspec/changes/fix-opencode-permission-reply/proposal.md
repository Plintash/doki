# Proposal

## Why

Every OpenCode approval Waku answers is refused by the service with HTTP 400:
the reply body carries `{"reply": …}`, while the current
`POST /api/session/{id}/permission/{requestID}/reply` route requires
`{"decision": …}`. Reproduced live against OpenCode 2.0.10 (the service's own
log records `schema rejection … Missing key at ["decision"]` for each reply).
The failure also consumes the request in driver state, so the approval card
never comes back and the task wedges until it is stopped.

## What Changes

- Answer a permission request with the current `Permission.Reply` payload:
  `{"decision": "once" | "reject"}`. The accepted values and the one-shot
  policy are unchanged — `always` still never goes on the wire, and no version
  branch is added, because the provider drives the current API only.
- A failed reply no longer strands the request: when the service rejects the
  answer for any reason other than "the request no longer exists" (404), the
  request is restored and its approval card is re-emitted so the user can
  retry. A 404 clears the request as settled, since another client already
  answered it or the turn ended.
- A plain denial ends the turn the way the Stop button does. OpenCode answers a
  rejection with no explanation by aborting the execution and emitting an
  interrupt whose default reason is `shutdown`, which is also the literal a
  session restart uses; that reason is therefore never shown as the turn's
  outcome. The denied tool row states the decline in Waku's own words instead
  of a JSON envelope, and the session settles idle rather than failed.
- A denial can carry an optional explanation. OpenCode hands a
  rejection-with-explanation to the agent as feedback and keeps the turn
  alive — its own TUI offers exactly this as "Tell OpenCode what to do
  differently" — so the approval card grows a note field wherever the provider
  supports one, and an empty note stays the plain deny.
- A provider-side user stop is distinguishable from a failure: the
  turn-finished event carries an `interrupted` flag, so the app can present it
  as an interruption rather than a red session.
- Regression coverage that runs without a model turn: a wire assertion that
  the reply body is `decision`-keyed, a private-live-service assertion that the
  route accepts the payload (a missing request answers 404, never a schema
  400), and driver tests for the refused reply, the plain denial, the
  note-carrying denial and a real service shutdown.

## Capabilities

### New Capabilities

(none)

### Modified Capabilities

- `opencode-provider`: the "Approvals are one-shot" requirement changes — a
  reply MUST use the current reply envelope the service accepts, and a reply
  the service does not accept MUST leave the request answerable rather than
  silently consuming it. A new "A denial is a first-class answer" requirement
  covers how denial, denial-with-explanation and provider-side interruption
  reach the transcript.

## Impact

- `crates/waku-core/src/opencode_api.rs` — `reply_permission` body and its
  route test.
- `crates/waku-core/src/driver/opencode.rs` and
  `crates/waku-core/src/driver/support.rs` — the `Respond` failure path, the
  pending-request record it restores, and the denial/interrupt handling.
- `crates/waku-protocol` — `Command::Respond` carries the optional note,
  `DriverEvent::TurnFinished` carries the `interrupted` flag, and the
  generated TypeScript bindings are refreshed.
- `crates/waku-core/src/driver/{mod,acp,claude,codex,deepseek,amp,pi}.rs` —
  the note is dropped by transports without the notion and every turn-finished
  constructor reports `interrupted: false`.
- `src/app/{composer,sessions,streaming,background_work}.rs` — the note field,
  the interrupted ending, and the new respond plumbing.
- `crates/waku-core/src/live_service.rs` — the live tests exercise the reply
  route through the private service the harness already starts.
- `docs/providers.md` — the Approvals paragraph names the `decision` payload
  and the two denial paths.
- `locales/app.yml` — the denial copy.
- No storage changes. Web and mobile clients ignore the new optional wire
  fields and keep their current approval behaviour.
