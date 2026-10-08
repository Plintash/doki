# Tasks

## 1. Prompt delivery (a message is never lost)

- [x] 1.1 Send every prompt with `streamingBehavior: "followUp"`, replacing the parked `fix/pi-streaming-behavior` commit's version, and state why one constant covers both cases (Pi reads the option only while streaming); verified by `every_prompt_asks_the_provider_to_queue_it_while_streaming` passing — two successive prompt writes, the second sent while the first is still unanswered, both carrying the field — and by the option staying absent from no prompt path in the writer
- [x] 1.2 Read the prompt answer's disposition — `started`, `queued`, `handled` — and keep Oh My Pi's `agentInvoked: false` as an additional handled signal, deleting the test that treats a missing `agentInvoked` as "keep waiting"; verified by `a_prompt_the_provider_queued_waits_for_the_run` (the answer settles nothing on its own), `a_prompt_an_extension_handled_settles_without_a_run` (settles with no turn start) and `local_slash_command_output_and_completion_do_not_need_an_agent_turn` (the Oh My Pi two-step shape still settles) passing
- [x] 1.3 Stop settling a refused prompt as if it started: no `TurnStarted` for a prompt the provider refused, and no dependence on receiving a prompt response at all (a prompt submitted inside the provider's settle window is answered with nothing); verified by `asynchronous_prompt_errors_settle_only_the_current_prompt` and `a_run_settles_the_turn_the_provider_never_answered` passing, with the whole `driver::pi` suite green (23 passed, 0 failed) and `pi_context_usage_against_the_real_rpc` passing against the installed Pi 1.0
- [x] 1.4 Record a refusal against the message that caused it and present it as undelivered rather than storing the provider's error text as an assistant message; verified by an app test where the submitted user message is marked undelivered, no assistant message contains the provider's error, and the turn is not shown as an answer

## 2. Queues, steering and stop

- [x] 2.1 Decode the provider's queue reports and use them as the pending list, so a queued message is shown pending until the provider says it left the queue; verified by driver and app tests feeding two successive reports and asserting the pending list follows them
- [x] 2.2 Offer steering only to a live run: the driver sends `steer` while its run is live and the prompt path otherwise, and both flavours keep their existing acknowledgements; verified by a driver test where a steer arriving after the settle is written as a prompt and never parked in the provider's steering queue
- [x] 2.3 Make stop clear the provider's queue before aborting, and write the abort without waiting for its answer so the control timeout no longer applies; verified by a driver test asserting `clear_queue` precedes `abort` and that a stop with no abort answer reports no error and lets the next prompt through
- [x] 2.4 Return the text the queue clears to the user instead of discarding it; verified by an app test where stopping a turn puts the stopped message back in the composer
- [x] 2.5 Keep Oh My Pi's behaviour unchanged through all of the above (its prompt answer, its settle event, its fork commands); verified by the existing `ohmypi_*` tests passing untouched, which cover its settle event, its titles and its fork commands — the prompt write is shared with Pi, so the queueing field goes out for both flavors, and Oh My Pi's own prompt path is covered only by the ignored live test because its CLI is not installed where the suite runs

## 3. Agent-initiated runs and extension messages

- [x] 3.1 Allow Pi and Oh My Pi to open a turn with no prompt, as Codex, Claude and OpenCode already may, and keep the guard that only a run-start signal opens one; verified by an app test where a `TurnStarted` with no active turn opens one, its deltas land, and a custom message with no run does not
- [x] 3.2 Decode `role: "custom"` messages — kind, text, display flag — in the transport's inbound stream; verified by driver tests for a custom message and for one marked not for display
- [x] 3.3 Classify subagent notifications onto the detached-work surface (`BackgroundWork`, kind subagent) and everything else to a transcript system line; verified by driver tests for `subagent-incremental-child-notify`, `subagent-notify` and an unknown type, plus an app test showing the child where detached work is shown
- [x] 3.4 Render a detached-work message on the work surface whatever its display flag says and add no conversation row for it, and add no row or stored copy for any other message the provider marked not for display; verified by driver tests asserting one `BackgroundWork` upsert and no `ExtensionMessage` for a hidden child record, and by an app test asserting a hidden notice adds no row

## 4. Extension UI surfaces

- [x] 4.1 Route the provider's fire-and-forget requests to real surfaces: notifications with their severity, status updates, widgets, window title, and editor text into the composer; verified by per-method driver tests and an app test per surface
- [x] 4.2 Answer the provider's dialogs instead of cancelling them: `select`, `confirm`, `input` and `editor` reach the user and return the answer in the provider's response shape, a dismissal returns a cancellation, and nothing is cancelled on the user's behalf — the provider resolves the dialogs it sent a `timeout` for itself; verified by driver tests for the answer shape, the dismissal and the settlement that drops an unanswered dialog, plus app tests for answering and dismissing
- [x] 4.3 Delete the blanket auto-cancel so no known request is answered on the user's behalf; verified by a test asserting a known dialog is not cancelled on arrival and only an unrecognised method is answered immediately

## 5. Verification

- [x] 5.1 Extend the live RPC suite so it drives a real busy session: a prompt sent mid-stream is queued and delivered, the queue report matches, and a stop clears it; verified by the tests passing against the installed Pi 1.0 with no service the developer started
- [x] 5.2 Add a fixture extension under the driver's fixtures that sends a custom message with a triggered run and asks one question, and a live test that covers the self-started turn, the message's classification and the dialog's answer; verified by the live test passing and by the fixture being loaded through the launch path the product uses
- [x] 5.3 Run the acceptance path with `npm:pi-subagents`: a background workflow whose child completion wakes the parent, with the completion visible and the reply in the transcript; verified by `pi_subagents_bring_a_background_child_wakes_the_parent_against_the_real_rpc` passing against the installed Pi 1.0 with the extension installed — the ignored live test that drives the product path, run with `env -u PI_SUBAGENT_CHILD cargo test -p waku-core --lib -- --ignored pi_subagents`
- [x] 5.4 Update `docs/providers.md`: the Pi section's per-turn paragraph, the inbound-stream table (custom messages, queue reports), cancel (`clear_queue` then abort), steering's gate, and a new paragraph on extension surfaces; verified by each paragraph matching the implemented behaviour

## 6. Landing

- [x] 6.1 Run `cargo fmt --package waku --package waku-protocol --package waku-client --package waku-core --package waku-daemon -- --check`, `cargo check`, `bun run protocol:check` and `cargo test --locked`; verified by all reporting no changes or errors
- [ ] 6.2 Validate in the freshly rebuilt debug app against Pi 1.0 with `npm:pi-subagents` installed: send a message while a turn is running and confirm it is queued and delivered, stop a turn and confirm the queued text comes back, run a background workflow and confirm the wake and its completion are visible — **owned by the user**, who runs the app-level checks by hand; the driver and app tests cover the same paths headlessly
- [x] 6.3 Record the change in `CHANGELOG.md` under the unreleased section; verified by the entry describing the user-visible outcome rather than the transport
- [ ] 6.4 Sync the `pi-provider` delta into `openspec/specs/` and archive the change; verified by the spec existing at the main path and the change moving into `openspec/changes/archive/`
