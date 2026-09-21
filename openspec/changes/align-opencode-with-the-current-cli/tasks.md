# Tasks

## 1. API alignment (implemented on `fix/opencode-v2-stable`)

- [x] 1.1 Identify the service with `GET /api/info` and the pid check, and resolve the CLI as a 2.x `opencode`; verified by `identity_rejects_a_payload_that_is_not_a_service` passing
- [x] 1.2 Decode the current prompt answer and move rename onto a session PATCH, inbox delivery onto a pending-item PATCH, and export, instruction entries and MCP under `/api/experimental`; verified by `live_service_answers_the_startup_path_and_the_moved_routes` passing against a real 2.x service
- [x] 1.3 Match pending inbox items by the `inboxID` the stream names and the `id` the admitting route answers; verified by the enqueue/deliver/cancel round trip inside the same live test
- [x] 1.4 Drive the palette from `/api/command` and `/api/skill`; verified by `opencode_catalog_lists_commands_and_skills_together` and `opencode_builtin_catalog_against_a_real_service` passing
- [x] 1.5 Cover the catalogue-failure announcement with a test: a failed catalogue read must emit the provider-catalogue error on the session rather than degrading into an empty palette; verified by `a_failed_catalogue_reads_as_an_error_not_an_empty_palette` passing

## 2. Cold-location catalogue reads (implemented on `fix/opencode-v2-stable`)

- [x] 2.1 Route model, agent, skill and command reads through one settled reader that waits for the location's registries to report ready, bounded by the request timeout, and answers at once when the location is already ready; verified by `model_catalog_waits_for_cold_location_registries` and `a_warm_location_with_no_catalogue_answers_at_once` passing
- [x] 2.2 Add the harness helper for a location nothing has opened, canonicalized the same way the service compares locations; verified by `live_catalogues_settle_a_cold_location` passing
- [x] 2.3 Document the staged publication and the settle in `docs/providers.md`; verified by the OpenCode section's catalogue paragraph matching the implemented behaviour

## 3. Live verification harness (implemented on `fix/opencode-v2-stable`)

- [x] 3.1 Start a private service on an ephemeral port with its own data, state and config directories and publish its registration, then reach it through the production `read_registration → probe → identify` path; verified by the live tests passing with no service the developer started
- [x] 3.2 Remove the `#[ignore]` from the provider's live tests so a route rename fails the suite; verified by `cargo test --workspace` reporting no failures

## 4. Collapse to one provider (implemented on `fix/opencode-v2-stable`)

- [x] 4.1 Delete the OpenCode 1 transport — driver, resident-server pool, session import and fork, filesystem catalogue scanning, `models --verbose` discovery and the Computer Use environment path — and rename the v2 modules to the plain `opencode_*` names; verified by `cargo check --workspace --all-targets` reporting no unreachable match arms
- [x] 4.2 Leave one `ProviderKind::OpenCode` and one cursor variant carrying the session's `directory`, with the retired tag accepted on the wire and in stored state; verified by `the_openCode_spelling_still_decodes` passing
- [x] 4.3 Accept only a 2.x `opencode` binary, so a machine with OpenCode 1 alone shows the provider as not installed; verified by `provider_ids_are_stable` and the model-catalog discovery tests passing
- [x] 4.4 Regenerate the TypeScript bindings and carry one OpenCode entry through the desktop, web and mobile lists, icons and locale keyword lists; verified by `bun run protocol:check`, both app `typecheck` scripts and the app test suites passing
- [x] 4.5 Keep commit-message generation on the current CLI's flags (`run --standalone --agent plan`, effort as `#variant` on the model) and say so in `docs/commit-messages.md`; verified by `every_provider_uses_a_noninteractive_generation_mode` passing

## 5. Landing

- [x] 5.1 Rebase `fix/opencode-v2-stable` onto `main` and confirm the suite is green on the rebased tree; verified by `cargo test --workspace` (the rebase also repairs main's missing `AnnotationSpan` test import, which had left `main`'s test build broken)
- [x] 5.2 Record the provider in `CHANGELOG.md`; verified by the entry sitting under `[unreleased]`
- [ ] 5.3 Validate in the freshly rebuilt debug app against OpenCode 2: start a task in a workspace the service has never opened, and confirm the model picker and the composer palette are populated on first open rather than after a second probe
- [ ] 5.4 Archive the change with `openspec archive align-opencode-with-the-current-cli` after the rebase has landed, and confirm `openspec validate --all` passes with `opencode-provider` in `openspec/specs/`
