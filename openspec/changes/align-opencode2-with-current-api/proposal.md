# Proposal

## Why

The OpenCode 2 provider was written against the first spelling of a preview API
that then moved under it, and every move fails quietly. The identity route alone
went `/api/health` → `/api/server` → `/api/status` → `/api/info` inside a
fortnight, and the probe hard-coded the first one: on a current 2.0.10 build it
answers 404 and **no OpenCode 2 task can start at all**, while the provider still
looks like an ordinary choice in the picker. The same drift took the prompt
answer's shape, reshaped the session routes at 2.0.4, and removed the member
`/api/skill` used to mark a skill user-invocable, so even a provider that
launched would lose its palette and its model list without saying so.

The repair exists (branch `fix/opencode-v2-stable`), but nothing in the repo
states what the provider must do. That absence is how the drift went unnoticed:
the API assumptions were the only record of them, and every test that would have
caught a rename was `#[ignore]`d and pointed at whatever service the developer
happened to have running.

## What Changes

- **Identity and launch** — identify with `GET /api/info` and its pid check, and
  accept the CLI as `opencode2` or as `opencode` only when that binary reports a
  2.x version, because Homebrew and the standalone zip install the second name
  and OpenCode 1 uses it too. Nothing branches on the reported version: the
  route either answers or it does not.
- **Prompt and session shape** — decode the prompt answer
  `{id, sessionID, time: {created}, type, payload, delivery}`; rename by
  patching the session; change a pending message's delivery by patching that
  inbox item; move export, instruction entries and the MCP runtime to
  `/api/experimental`.
- **Inbox identity** — the event stream names a pending item `inboxID` while the
  route that admitted it answers `id`, so enqueued/delivered/cancelled matching
  reads both spellings instead of silently never firing.
- **Catalogues** — commands and skills feed the palette, since the current
  `/api/skill` no longer marks which entries are user-invocable; and every
  location-scoped catalogue waits out a cold location's staged publication
  instead of taking its first empty answer as the truth.
- **Live verification** — the provider's tests drive a private service the
  harness starts on an ephemeral port with its own XDG tree, through the
  production discovery path, so a route that moves fails the suite rather than
  the user.
- **Landing** — rebase the branch onto `main` and record the change in
  `CHANGELOG.md`.

**BREAKING**: a preview-era 2.x build is no longer driven. Its routes are gone
from the current line, and rather than carrying a branch per channel — which
would be stale within days — Waku drives what the service answers today and
refuses the rest by failing to identify it. OpenCode 1 keeps its own transport
and is unaffected.

## Capabilities

### New Capabilities

- `opencode2-provider`: what Waku's OpenCode 2 transport must do — adopt the
  user's own background service without signalling it, identify what it finds
  and refuse what it cannot drive, decode the current routes and event stream
  into the driver contract, and read the service's location-scoped catalogues.

### Modified Capabilities

(none)

## Impact

- **Code**: `crates/waku-core/src/opencode2_api.rs`,
  `opencode2_service.rs`, `opencode2_session.rs`, `live_service.rs`,
  `model.rs`, `slash_command_catalog.rs`,
  `driver/opencode2.rs`, `driver/opencode2_computer_use.rs`; docs
  `docs/providers.md` and `docs/computer-use.md`; one new error string in
  `locales/app.yml`.
- **Compatibility**: the provider refuses builds whose routes have moved, as
  above; the OpenCode 1 provider keeps its own server pool and is untouched. A
  machine that has only OpenCode 1 never advertises it as OpenCode 2.
- **Status**: implemented on `fix/opencode-v2-stable` (`c05a52d` aligns the API,
  `b3e4695` settles a cold location); not yet on `main`.
