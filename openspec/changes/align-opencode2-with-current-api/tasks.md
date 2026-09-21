# Tasks

## 1. API alignment (implemented on `fix/opencode-v2-stable`, c05a52d)

- [x] 1.1 Identify the service with `GET /api/info` and the pid check, and resolve the CLI as `opencode2` or as a 2.x `opencode`; verified by `identity_rejects_a_payload_that_is_not_a_service` and `identity_decodes_the_current_route_and_reports_its_version` passing
- [x] 1.2 Decode the current prompt answer and move rename onto a session PATCH, inbox delivery onto a pending-item PATCH, and export, instruction entries and MCP under `/api/experimental`; verified by `live_service_answers_the_startup_path_and_the_moved_routes` passing against a real 2.x service
- [x] 1.3 Match pending inbox items by the `inboxID` the stream names and the `id` the admitting route answers; verified by the enqueue/deliver/cancel round trip inside the same live test
- [x] 1.4 Drive the palette from `/api/command` and `/api/skill`; verified by `opencode2_catalog_lists_commands_and_skills_together` and `opencode2_builtin_catalog_against_a_real_service` passing
- [ ] 1.5 Cover the catalogue-failure announcement with a test: a failed catalogue read must emit the provider-catalogue error on the session rather than degrading into an empty palette; verified by that test failing when the error path is removed

## 2. Cold-location catalogue reads (implemented on `fix/opencode-v2-stable`, b3e4695)

- [x] 2.1 Route model, agent, skill and command reads through one settled reader that waits for the location's registries to report ready, bounded by the request timeout, and answers at once when the location is already ready; verified by `model_catalog_waits_for_cold_location_registries` and `a_warm_location_with_no_catalogue_answers_at_once` passing
- [x] 2.2 Add the harness helper for a location nothing has opened, canonicalized the same way the service compares locations; verified by `live_catalogues_settle_a_cold_location` passing
- [x] 2.3 Document the staged publication and the settle in `docs/providers.md`; verified by the OpenCode 2 section's catalogue paragraph matching the implemented behaviour

## 3. Live verification harness (implemented on `fix/opencode-v2-stable`, c05a52d + b3e4695)

- [x] 3.1 Start a private service on an ephemeral port with its own data, state and config directories and publish its registration, then reach it through the production `read_registration → probe → identify` path; verified by the live tests passing with no service the developer started
- [x] 3.2 Remove the `#[ignore]` from the provider's live tests so a route rename fails the suite; verified by `cargo test -p waku-core --lib` reporting 486 passed, 0 failed

## 4. Landing (remaining)

- [ ] 4.1 Rebase `fix/opencode-v2-stable` onto `main` and confirm `cargo test -p waku-core --lib` is still fully green on the rebased tree
- [ ] 4.2 Add the CHANGELOG entry for the provider fix and confirm it sits in the section the release will read
- [ ] 4.3 Validate in the freshly rebuilt debug app against OpenCode 2: start a task in a workspace the service has never opened, and confirm the model picker and the composer palette are populated on first open rather than after a second probe
- [ ] 4.4 Archive the change with `openspec archive align-opencode2-with-current-api` after the rebase has landed, and confirm `openspec validate --all` passes with `opencode2-provider` in `openspec/specs/`
