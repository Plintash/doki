# Design

## Context

See [proposal.md](proposal.md) for motivation. The constraints that shape the
approach:

- **Waku does not own the process.** OpenCode 2 is one background service the
  user's own terminal usually started first, and its CLI's own `serve --service`
  is idempotent against a healthy incumbent. It is discovered through a
  descriptor in the user's state directory that the service itself watches.
- **The API moves between builds.** The identity route relocated four times
  inside a fortnight, the prompt answer changed shape, and the session routes
  were reshaped at 2.0.4. Anything written against one build decays.
- **A long-lived session, not a process per prompt**, is the driver contract
  ([driver/mod.rs](../../../crates/waku-core/src/driver/mod.rs)): the transport
  holds one session across turns and normalizes provider events into
  provider-agnostic activity rows.
- **No socket work on the UI thread.** Every route call blocks, so the wire
  module documents that every caller is already off the render path; catalogue
  reads run in the daemon's background workspace operations and the driver's own
  thread.
- **The developer's daemon is not a test fixture.** The tests that existed were
  `#[ignore]`d and pointed at whatever service was running, which is why the
  drift went in unnoticed.

The branch's layering, which this change keeps: `opencode2_service` owns
discovery, adoption and lifetime; `opencode2_api` is typed route bindings that
know nothing about ownership; `opencode2_session` reads session history off the
service; `driver/opencode2.rs` holds the session, the subscription and the
stream; `live_service` is the test harness.

## Goals / Non-Goals

**Goals:**

- One module decides what a route looks like, one decides who owns the process,
  so a rename is a one-file change plus a failing test.
- Every API assumption is executable against a real service, through the same
  discovery path production uses.
- Failures are visible: a moved route becomes a reported error, never an empty
  picker or a silently missing catalogue.

**Non-Goals:**

- Driving preview-era 2.x builds. No channel branches, no version matrix.
- Owning the service: no restart, no health supervision, no repair of the
  descriptor, no signalling.
- Subagent child transcripts; only the parent's tool row and its synthetic note
  are rendered.
- Changing the OpenCode 1 provider, which keeps its own resident-server pool.

## Decisions

**Adopt the service; start one only for a task.** A per-session process would
contend with the user's own terminal for the same local resources and would lose
state the service already holds. Alternative rejected: spawning a server per
workspace, because the service is already there and `serve --service` converges
on it.

**Readiness comes from the service, not from a sleep.** A location publishes its
catalogues in stages — a cold one answers empty, then answers its contents a
moment later, and the model list even churns (61 → 9 → 7 across the first
seconds). The service publishes no `ready` flag, but it does publish its plugin
registry, whose entries are what assemble those catalogues. A catalogue read
therefore waits for that location's registries to report ready and reads again,
bounded by the ordinary request budget with the newest answer winning.
Alternatives rejected: a fixed sleep (an assumption about timing), and polling
only for a non-empty answer (a legitimately empty catalogue would pay the whole
budget, and an intermediate answer that is already non-empty would still be
taken early).

**Errors are reported where an empty answer would lie.** A catalogue failure is
announced on the session rather than folded into an empty list, because the two
are indistinguishable in the UI and the whole API-drift failure mode was exactly
that. The model picker still falls back to its last good catalogue when
discovery fails, so a provider that cannot be read does not shrink the picker.

**Durable approvals stay in Waku.** The service accepts an "always" answer, but
it writes into a store shared with the user's own terminal, so the driver keeps
durable decisions in its own state and every wire reply is one-shot.

**Client-minted session ids, subscription before creation.** The create/first
event race is removed by construction instead of by buffering frames for a
session that does not exist yet.

**The execution outcome settles a turn.** The service emits no idle event, so
waiting for one would pin finished turns to a running state; settling is
idempotent because a steered message shares the turn's outcome.

**A live harness with its own world.** A private service on an ephemeral port
with its own data, state and config directories, reached through the production
`read_registration → probe → identify` path, is the only way to notice a rename.
Canned fixtures cannot; `#[ignore]`d tests against a running service did not,
because nobody ran them. The harness claims a test state root before anything
can spawn, and the production spawn path asserts one was claimed, so a test
cannot register into the user's tree.

**Locations are canonicalized strings.** The service compares location
directories by exact equality and resolves neither alternative spelling, so
Waku canonicalizes once and reuses that string for creation and every read.

## Risks / Trade-offs

- **The API keeps moving** → all wire knowledge is in one module and the live
  suite fails on a rename; the version matrix that would go stale is deliberately
  absent.
- **A cold location's first read is slower** → the wait only happens while the
  location reports not-ready, it is bounded, it runs off the UI thread, and a
  ready location pays one extra loopback request.
- **A wedged daemon stays wedged** → the descriptor belongs to the user, so Waku
  re-probes on the next acquire instead of restarting anything.
- **Adopting the user's service couples Waku to their configuration** → the two
  places where that would leak into the user's world (the data directory and the
  saved-permission store) are both explicitly not written to.
- **An unchecked provider is not noticed** → the live tests are not ignored and
  cover the startup path plus every route the provider calls.

## Migration Plan

- The implementation is the branch `fix/opencode-v2-stable` (`c05a52d` aligns
  the API, `b3e4695` settles a cold location). `main` has not touched the
  OpenCode 2 files since the branch's base, and the only file both sides change
  is `locales/app.yml`, where the two insert at different points, so the rebase
  is expected to apply cleanly.
- Rollback is reverting the merge: the provider is chosen per session, and
  OpenCode 1 keeps its own transport, so nothing else depends on it.
- A CHANGELOG entry records the provider fix for users of the released app.

## Open Questions

- The picker reads the model and agent catalogues unscoped, which the service
  resolves against its own working directory rather than the session's
  workspace. That is what keeps one catalogue from being pinned to whichever
  workspace asked first, but it means the picker's contents are not
  workspace-specific. Worth revisiting only if the service starts publishing
  genuinely per-workspace model availability; the requirement above is written
  so either choice satisfies it.
