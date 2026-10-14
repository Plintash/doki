# Lightweight issue detail prototype

## Scope

- Match the native shell’s ghost icon controls for Back, Complete/Reopen, Attach, and development/review actions
- Replace raised conversation cards with compact hover rows and directly available unlink controls
- Hide repeated default workspace metadata and idle labels; preserve different-workspace details and review/error states
- Retain the editable issue title and specification, menu scope/reference validation, review findings, and unsent drafts
- Provide semantic action names, conventional keyboard activation, focus-visible treatment, hover tooltips, and viewport-snapped keyboard hints

The existing icon collection is mixed: `compose.svg` credits Microsoft Fluent and `info.svg` credits Lucide. This change reuses existing glyphs; it does not introduce a new icon family. `paperclip.svg` is now included in the native embedded asset registry.

## Source identity

Standalone issue UI: `39f6a93fe621967f4da35a577ae9ba7481925d9f` (main `8d10896` plus UI and asset correction).

Native acceptance used the combined tree at `6a13b5738fd4d919b75f1338847c55628c50fdac`, including verified main `8d10896`, pending Board-exit and project-dedup fixes, issue review, titlebar continuity, and the narrow review visual integration. Standalone branches were not merged or published. Do not describe this as a standalone-only runtime result.

Native instance: `043a8d5a-75fd-495d-89c2-2f4223917133`
Build: `34f907748efa997c-18de5baad4e5c8ed`

## Verification

- Source formatting and `git diff --check`: passed
- Source/semantic identity tests: 47 passed, 338 assertions
- Combined `cargo test --locked --offline -p waku --lib -- --test-threads=1`: 678 passed, 2 ignored
- Combined `cargo check --locked --offline --workspace`: passed
- Native issue UI acceptance: 26 assertions passed
  - Mouse, Enter, and Space unlink without opening the conversation
  - Removed associations preserve conversation history
  - Keyboard focus moves to a surviving row, or Compose when the list becomes empty
  - Refused unsent-draft unlink preserves focus and issue context
  - Reattachment works
  - Issue title/specification remain unchanged
- Existing native synthetic review acceptance: 257 assertions passed
  - Review scope, findings, failure/cancel states, correction drafts, and prior Pi drafts preserved
- All 9 issue action glyphs verified present in the embedded SVG registry
- Independent Standards/Fowler and Specification review cleared the UI changes, integration conflicts, and review visual delta

The app was built and launched by its native dev watcher in an isolated Linux cloud fixture. All review responses came from the checked-in local protocol stub. No authenticated provider, account, or model call was used. macOS-specific traffic lights/fullscreen animation are not claimed as validated here.

## Visual evidence

- `final-prototype-light.jpg`: complete issue page, original title/specification, visible Attach glyph, unfilled rows and review section
- `final-prototype-dark-keyboard.jpg`: dark theme, actionable review state, visible keyboard-focused Attach hint

Initial screenshots exposed an unregistered Attach glyph. The registry was corrected and the final screenshot was taken from the fresh rebuilt instance above. The native screenshot bytes are JPEG, and the delivered files were verified with an image decoder before Library upload.

Titlebar-specific verification on this exact running build passed 20 semantic snapshots and actual Linux transition/fullscreen checks. At the 980×680 minimum window size, Back and the last visible unlink control’s keyboard hints remained within the viewport. Physical titlebar drag also passed, and idle render frames stayed at 1790 across 2.3 seconds. The native app, watcher, daemon, and extra request bridge were stopped; the running marker was removed. Cargo and the cloud display were released.

## Alignment feedback correction

User screenshot feedback identified inconsistent visible content insets: title/specification text used 11px, headings 0px, chips 7px, and conversation rows 9px. Commit `881617d3e3f3c1f2f419d9aa47d510365ef92fed` aligns the visible content baseline to 11px while retaining field and control hitboxes. Validation warnings share the same baseline. Review, new-session, and unlink actions share the trailing content edge. This is a two-file layout-only correction; standalone branches remain unchanged.

- Native measured bounds passed at 1178×814: content left 330px, trailing edge 1100px
- Native measured bounds passed at minimum 980×680: content left 288px, trailing edge 945px
- Light and dark native screenshots inspected, plus minimum-width keyboard-focused project chip
- Focused native issue interaction regression: 26 assertions passed again
- Source/identity suite: 47 tests and 338 assertions passed again
- Independent source review cleared the final alignment delta; formatting and diff checks passed
- Prior full app/workspace/review/titlebar results above remain the pre-alignment checkpoint; backend tests were not unnecessarily repeated for this layout-only change

Corrected screenshots: `aligned-prototype-light.jpg` and `aligned-prototype.jpg`. The fixture was stopped after verification; no real provider/model call was made.

## Description border follow-up

The user subsequently requested that the description box itself must not protrude beyond surrounding content. Product commit `1e6b539` adds persistent 11px horizontal margins to that field; its visible outer border now aligns with the section content edges, while its text retains natural inner padding. Hover and focused editing use the same wrapper geometry.

Native verification passed at 1178×814 (outer border edges 330/1100px, text inset 341px) and 980×680 (outer edges 288/945px). The geometry check derives the wrapper edges from the measured editor bounds and the unchanged 10px padding plus 1px border. Focus/edit round-trips preserved the original description and unchanged field bounds. Light/dark screenshots, unfocused hover, and narrow focused editing were inspected. Source suite again passed 47 tests/338 assertions, native issue interaction regression passed 26 assertions, and independent focused source review cleared the one-line change. No provider/model calls were made. Prior full backend/runtime-suite checkpoints above are unchanged; this bounded layout delta did not rerun them.

Evidence: `description-edge-light.jpg`, `description-edge-dark.jpg`, `description-edge-hover.jpg`, and `description-edge-narrow.jpg`. The app, watcher, and daemon were stopped after verification.
