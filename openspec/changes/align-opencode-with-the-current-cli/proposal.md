# Proposal

## Why

The OpenCode provider was written against the first spelling of a preview API
that then moved under it, and every move fails quietly. The identity route alone
went `/api/health` → `/api/server` → `/api/status` → `/api/info` inside a
fortnight, and the probe hard-coded the first one: on the current 2.x CLI it
answers 404 and **no OpenCode task can start**, while the provider still looks
like an ordinary choice in the picker. The same drift took the prompt answer's
shape, reshaped the session routes at 2.0.4, and removed the member `/api/skill`
used to mark a skill user-invocable, so even a provider that launched would lose
its palette and its model list without saying so.

At the same time Waku carried two OpenCode entries while the CLI only ever
installs one name: the preview shipped `opencode2`, while the npm, zip and
Homebrew channels install `opencode`, which is also OpenCode 1's name. A machine
could therefore list two provider rows for one binary, one of them broken. The
two lines cannot coexist either — v1's `serve` takes no `--service` and speaks a
different API — and the current line is where the CLI is going.

The repair exists (branch `fix/opencode-v2-stable`), but nothing in the repo
states what the provider must do. That absence is how the drift went unnoticed:
the API assumptions were the only record of them, and every test that would have
caught a rename was `#[ignore]`d and pointed at whatever service the developer
happened to have running.

## What Changes

- **One entry, the current line** — `OpenCode` is the only OpenCode in the
  provider list. The OpenCode 1 transport is deleted (its resident-server pool,
  session import and fork, driver, filesystem catalogue scanning and
  `models --verbose` discovery); the wire enums, the generated TypeScript and
  the apps carry a single `openCode`; and state written under the retired tag
  still decodes. The `opencode` binary is accepted only when it reports a 2.x
  version, so a machine with OpenCode 1 alone sees the provider as absent rather
  than broken.
- **Identity and launch** — identify with `GET /api/info` and its pid check
  against the descriptor the service itself publishes. Nothing branches on the
  reported version: the route either answers or it does not.
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
- **Landing** — rebase onto `main` and record the change in `CHANGELOG.md`.

**BREAKING**: OpenCode 1 is no longer supported, and neither is a preview-era
2.x build. The CLI's name is shared between the lines; the API is not, and Waku
will not half-drive a build whose routes have moved.

## Capabilities

### New Capabilities

- `opencode-provider`: what Waku's OpenCode transport must do — adopt the user's
  own background service without signalling it, identify what it finds and
  refuse what it cannot drive, decode the current routes and event stream into
  the driver contract, and read the service's location-scoped catalogues.

### Modified Capabilities

(none)

## Impact

- **Code**: `crates/waku-core/src/opencode_api.rs`, `opencode_service.rs`,
  `opencode_session.rs`, `live_service.rs`, `model.rs`, `model_catalog.rs`,
  `slash_command_catalog.rs`, `composer_complete.rs`, `git_commit.rs`,
  `daemon.rs`, `driver/opencode.rs`, `driver/opencode_computer_use.rs`,
  `driver/support.rs`; docs `providers.md`, `computer-use.md`,
  `commit-messages.md`; the provider icon set and the locale keyword lists.
- **Wire and state**: `ProviderKind` and `ProviderResumeCursor` lose their
  `openCode2` members, `packages/waku-client`'s generated types are regenerated,
  and the apps carry one OpenCode entry. Sessions, cursors and settings written
  while both lines existed still decode through a serde alias.
- **Removed**: `driver/opencode.rs` (v1), `opencode_pool.rs`,
  `opencode_session.rs` (v1), the OpenCode 1 Computer Use path, the
  `opencode2` binary alias, and the second provider icon.
- **Status**: implemented on `fix/opencode-v2-stable`; not yet on `main`.
