# Design

## Context

See proposal.md - Why.

- `crates/waku-core/src/opencode_api.rs` sends the reply body for
  `POST /api/session/{id}/permission/{requestID}/reply` as `{"reply": …}`.
  The current service route (verified on OpenCode 2.0.10, both in the
  installed binary's route table and by live probe) declares
  `payload: {decision: Permission.Reply, message?: string}` where
  `Permission.Reply` is `once | always | reject`, and answers 204. A missing
  `decision` is a schema 400 (`InvalidRequestError: Missing key at
  ["decision"]`); a payload that parses but names an unknown request is a 404
  (`PermissionNotFoundError`). `PermissionReply` already serializes to the
  lowercase values, so only the field name is wrong.
- The driver's `Respond` command runs `support::permission_responses`, which
  removes the request from `permissions.pending`, marks it `responding`, and
  returns the one-shot replies. A failed HTTP call only emits a
  `DriverEvent::Error`; the id stays in `responding`, and `request_permission`
  early-returns for ids in either set. The app, meanwhile, clears its
  `pending_permission` as soon as the user clicks. A refused reply therefore
  loses the card on both sides with no re-ask path, including after a stream
  resync, because `reconcile` re-runs the same `request_permission` that the
  `responding` entry suppresses.
- `OpenCodePermissionRequest` carries `permission`, `patterns` and `always`,
  but not the provider's `message`, which the card's detail line prefers.
- OpenCode's rejection has two shapes. A rejection with no explanation fails
  the awaiting call with `Permission.DeclinedError`, which the engine turns
  into a fiber interrupt whose reason defaults to `shutdown`; the tool part
  records `{type: "aborted", message: "The user declined this tool call"}`.
  A rejection that carries an explanation fails it with
  `Permission.CorrectedError{feedback}` instead, which is not caught as an
  abort, so the agent reads the explanation and the turn continues. The
  service's own TUI offers exactly that second path ("Tell OpenCode what to do
  differently") and ignores `shutdown`-reason interrupts, including in its
  persisted state.
- `DriverEvent::TurnFinished` carries `{success, summary}` only, and the app
  maps `success: false` to a failed (red) session. A turn the user stopped has
  no representation on that wire, which is why the app's own Stop button
  settles locally as Idle plus `TurnStatus::Interrupted`.

## Goals / Non-Goals

**Goals:**

- The reply body matches the current route's payload exactly, so approvals
  work.
- A refused reply is recoverable: the user can answer again instead of the
  turn blocking with no card.
- A denial reads as a deliberate user stop: no provider interrupt reason in
  the transcript, an interrupted rather than failed turn, and the declined row
  naming the decline.
- A denial that carries an explanation keeps the agent working on the turn.
- Every property is pinned by tests that run in the normal suite: a
  captured-body unit test, a private-live-service route test, and driver/UI
  tests for the refusal, denial and note paths.

**Non-Goals:**

- No version branch or dual-key body. The provider drives the current API
  only, and a body that carries both `reply` and `decision` would depend on
  how the service treats excess properties.
- No rejection message. OpenCode's rejection note is carrier of an
  explanation, not a rejection reason; only OpenCode gets the note field.
- No note UI in the web or mobile clients, which keep their current approval
  controls.
- No change to which decisions are allowed: `always` still never goes on the
  wire, and durable choices stay driver-local.

## Decisions

**Reply envelope.** Replace the body with `{"decision": reply}` and keep
`PermissionReply`'s existing values. The alternative — probing the service's
version and picking a key — is rejected by the provider's own contract
("either the routes answer or the build is refused"), and a compatibility
branch for a retired shape would be stale within days, which is exactly what
the previous alignment change removed.

**Recovery policy on a refused reply.** When `reply_permission` fails:

- Always remove the id from `permissions.responding`, so the request is no
  longer suppressed.
- If the error is a 404, treat the request as settled elsewhere: drop it,
  since another client or the ended turn already answered it.
- Otherwise restore the request to `permissions.pending` and re-emit the
  same `DriverEvent::Permission` card. The card is rebuilt from
  `OpenCodePermissionRequest`, so that record grows an optional `message`
  field; both construction sites (the `permission.asked` decode and the
  reconcile snapshot) fill it, and the emission is extracted from
  `request_permission` so the retry card and the first card cannot drift.
- For the extra ids an `always` answer also resolves (`permission_responses`
  returns them), do NOT restore them. Their rule was already remembered, so
  the next `permission.asked` or reconcile auto-answers them again through
  `is_approved`; restoring them would ask the user a question the policy
  already answered.

**Test seam for the failure path.** Extract the per-reply send into a small
helper that takes the sender as a closure, wire the production call to
`opencode_api::reply_permission`, and let the driver test inject an
`ApiError`. The alternative — testing only against the live service — cannot
reach a real pending permission without a model turn, so the recovery would
go untested.

**A denial is tracked where the provider semantics are known.** The OpenCode
driver records that it sent a rejection the service accepted *without* an
explanation. When `session.execution.interrupted` follows, that record turns
the interrupt into a user stop; a real service shutdown has no such record and
keeps its current reconnect-repair behaviour. The driver is the only layer
that knows which rejections abort, so the app never has to guess. If a future
service keeps the turn alive after a plain rejection, no interrupt arrives,
the record is cleared when the turn settles, and nothing changes.

**A provider-side stop needs its own turn outcome.** `TurnFinished` gains an
`interrupted` flag (defaulting false on the wire, so older payloads and the
web/mobile clients are unaffected) and a denial emits it instead of
`success: false`. The app then takes the same ending as its Stop button: Idle,
`TurnStatus::Interrupted`, the localized "Stopped" fallback, and no queued
follow-up drain. Reusing `success: false` was rejected because it paints the
session red for a deliberate user action; reusing a localized string in
`summary` was rejected because it asks the app to pattern-match prose.

**The explanation rides the existing respond command.** `Command::Respond`
gains `message: Option<String>` and `DriverControl` gains
`respond_with_message`, whose default drops the note and delegates to
`respond`. Only the OpenCode transport overrides it; every other provider
keeps its current signature and semantics, and a note sent to a provider
without the notion is dropped rather than misdelivered. `reply_permission`
adds `message` to the body only when one is present.

**The note field is offered only where it works.** The approval card shows its
inline note input for an OpenCode session's deny option, gated on the
session's provider. A per-option capability flag was rejected: it would ride
eleven option constructors and every generated client for a capability exactly
one transport has, while the provider is already the discriminator the app
uses for other provider-specific surfaces. The note is optional everywhere it
appears: an empty note is the plain deny.

**The declined row speaks, not the envelope.** OpenCode's tool error is a
`{type, message}` envelope; whichever path produced it, the row's content is
the message. A denial Waku knows about substitutes its own localized line, so
the transcript does not depend on the provider's English copy.

**Live route assertion without a pending request.** In the private-service
test, reply to a request id that does not exist and assert the failure is a
404 `PermissionNotFoundError`, not a 400. The payload is validated before the
request id is looked up, so this pins the envelope against the real service at
no model cost. A real permission request would need a tool-asking turn: slow,
flaky, and dependent on a model.

**Offline body assertion.** `opencode_api`'s canned-server harness already
captures the outgoing request; assert the permissions reply body equals
`{"decision": "once"}` (and `"reject"` for a rejection). This runs on every
`cargo test` even when no service or CLI is installed.

## Risks / Trade-offs

- [The reply envelope changes again upstream] → the offline body test and the
  live 404/400 test both fail loudly, and the driver now re-asks instead of
  wedging if a build is already running.
- [A restored card refers to a request the provider has since dropped without
  a 404] → answering it returns 404, which settles the request; the cost is one
  visible extra answer attempt, not a stuck turn.
- [Re-emitting a card while the app already shows one] → the app replaces
  `pending_permission` wholesale on `DriverEvent::Permission`, so a duplicate
  emission is idempotent; the driver's pending/responding dedupe keeps the
  live stream from producing one.
- [The `message` field widening the shared request record] → it is optional
  and decoded with a default, so the reconcile path and existing tests that
  build the record keep compiling once they fill the field.
- [OpenCode stops aborting on a plain denial] → no interrupt arrives, the
  denial record is cleared when the turn settles, and the note path is
  unaffected; the driver-level tracking self-heals where an app-side guess
  would not.
- [Web and mobile keep the old ending for a denied turn] → they ignore the new
  optional fields and present the turn as failed, which is today's behaviour
  and not a regression; the desktop app is the surface this change targets.

## Migration Plan

None. No stored state, protocol, or wire enum changes; the only externally
visible change is the reply body, which the service already expects.

## Open Questions

None.
