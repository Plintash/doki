# Tasks

## 1. Reply with the envelope the service accepts

- [x] 1.1 Change `opencode_api::reply_permission` to send `{"decision": <once|reject>}`, leaving `PermissionReply`'s values and the local `always` refusal untouched; verified by an added canned-server test asserting the captured request body is `{"decision":"once"}` for an approval and `{"decision":"reject"}` for a rejection
- [x] 1.2 Add a live-service test that replies to a request id the session does not have and asserts the failure is `404 PermissionNotFoundError` rather than a 400 schema rejection, which is only true when the payload parses; verified by running the test against the private service the harness starts
- [x] 1.3 Name the accepted reply payload in the Approvals paragraph of `docs/providers.md`; verified by the paragraph matching the implemented body and values

## 2. A refused reply never strands the turn

- [x] 2.1 Carry the provider's optional `message` on `OpenCodePermissionRequest` and fill it from both construction sites — the `permission.asked` decode and the reconcile snapshot; verified by `cargo check` and the existing permission decode tests passing with the field populated
- [x] 2.2 Extract the approval-card emission from `request_permission` into one helper that builds the `DriverEvent::Permission` from the request record, so a first card and a retry card cannot differ; verified by the existing permission tests observing the same title, detail and options
- [x] 2.3 Route every resolved reply through one function that takes the reply sender as an injected closure; on failure it clears the id from `responding`, and for any error other than 404 restores the request and re-emits its card, while a 404 clears the request as settled elsewhere; verified by new driver tests injecting a failing sender for both branches
- [x] 2.4 Keep the additional ids an `always` answer resolves out of the restore: their rule is already remembered, so a later `permission.asked` or reconcile auto-answers them; verified by a driver test where a failed reply for an extra approved id emits no card and a later ask is auto-answered
- [x] 2.5 Verify the retry path end to end at the driver level: a failed reply emits exactly one `DriverEvent::Error` and one replacement `DriverEvent::Permission`, and answering the retried request reaches the sender once

## 3. Validation

- [x] 3.1 Run `cargo fmt --package waku --package waku-protocol --package waku-client --package waku-core --package waku-daemon -- --check` and `cargo check`; verified by both reporting no changes or errors
- [x] 3.2 Run `cargo test -p waku-core` and then the full `cargo test --locked`; verified by the permission, driver and live-service tests passing with no failures

## 4. A denial ends the turn like a stop

- [x] 4.1 Driver: record a rejection the service accepted without an explanation and consume it when `session.execution.interrupted` arrives, turning that interrupt into a user stop; verified by a driver test where a denied turn reports an interrupted turn while a real service shutdown keeps its current behaviour
- [x] 4.2 Stop passing any provider interrupt reason as turn text: `shutdown` and `user` no longer reach `TurnFinished.summary`; verified by the same tests asserting no `shutdown`/`user` string appears in any emitted event payload
- [x] 4.3 Carry the stop distinctly: `DriverEvent::TurnFinished` gains an `interrupted` flag, the daemon wire round-trips it with a `false` default, and every other driver constructor reports `false`; verified by a wire round-trip test and `cargo check` across the workspace
- [x] 4.4 App: an interrupted turn-finished takes the Stop button's ending — Idle session, `TurnStatus::Interrupted`, the localized "Stopped" fallback, no queued follow-up drain, no failed status; verified by an app-level test asserting the session and turn states
- [x] 4.5 OpenCode tool rows: a failed call with a `{type, message}` error envelope shows the message, and a decline Waku answered shows its own localized line instead of a JSON blob; verified by activity tests for both shapes
- [x] 4.6 Suppress the abort the provider raises for the declined step (`session.step.failed` with an `aborted` error while a denial is outstanding), which was a "Step interrupted" toast on top of the stop; a genuine step failure still reports, verified by a driver test for both

## 5. Denying with an explanation keeps the turn alive

- [x] 5.1 Protocol: `Command::Respond` carries an optional `message`, daemon and `DriverControl` pass it through via a `respond_with_message` default that drops it for transports without the notion, and `bun run protocol:generate` refreshes the bindings; verified by `bun run protocol:check` and `cargo check`
- [x] 5.2 OpenCode driver: `reply_permission` adds `message` to the reply body only when present, and a rejection carrying a note does not arm the stop path because the service hands the note to the agent and the turn continues; verified by a reply-body test and a driver test asserting a noted denial emits no interruption
- [x] 5.3 App: a deny option on an OpenCode card opens an inline note field (empty note = the plain deny), confirm sends the note, Escape cancels, and the control is keyboard operable; verified by a state test for open/confirm/cancel plus manual focus behaviour in the rebuilt app
- [x] 5.4 `docs/providers.md` and `locales/app.yml` describe the two denial paths and carry the new copy; verified by reading the rendered app strings and the paragraph matching the implementation

## 6. Full validation

- [x] 6.1 Run `cargo fmt --package waku --package waku-protocol --package waku-client --package waku-core --package waku-daemon -- --check`, `cargo check`, `bun run protocol:check` and `bun run protocol:generate`; verified by all reporting no changes or errors
- [x] 6.2 Run `cargo test --locked`; verified by the full suite passing with no failures
- [ ] 6.3 Validate in the freshly rebuilt debug app against OpenCode 2: approve once and confirm the tool runs; deny plainly and confirm the turn ends as stopped, not red, with no provider reason in the transcript; deny with a note and confirm the agent reads it and continues — **owned by the user**, who runs the app-level checks by hand; the driver and app tests cover the same paths headlessly
