# Task entry and attachment picker QA

Verified on 2026-10-14 in an isolated, provider-disabled Linux debug fixture.
The app was rebuilt and launched by `scripts/dev.ts` with the `Dialog UX QA`
flavor. No user database, credentials, user Mac, or paid model calls were used.

## Behavior

- New task opens the full title, description, project, and workspace editor.
  No title-only modal or persisted empty task is created first.
- Blank and whitespace titles keep Create disabled. Cancel discards the draft.
  Navigating away and reopening New task retains the in-memory brief; Create
  commits the same identity once, preserving its description and workspace.
- Project and workspace menus belong to the current editor, preventing an old
  issue's cached menu callbacks from carrying into a new draft.
- Attach existing opens a search-focused picker containing only unfiled
  conversations eligible for the issue's project. Search covers titles,
  projects, and workspaces, including Unicode text.
- Rows have independently ellipsized title and project/workspace lines. The
  surface is bounded, the list is virtualized, and its scrollbar uses exact
  fixed-row geometry even before all rows have been measured.
- Arrow keys, Tab, Home/End, PageUp/PageDown, Enter, and Escape are supported.
  Activation resolves current selection and rechecks issue, conversation,
  unfiled status, and project compatibility before changing membership.

## Checks

- App unit tests: 654 passed, 2 intentionally ignored, 0 failed. Use an isolated
  `WAKU_RPA_DATA_DIR` when running the test binary; a native fixture's maximized
  window preference reaches an unimplemented GPUI test-platform `zoom` method.
- Native `scripts/rpa-workflow-live.test.ts`: 3 passed, 131 assertions. Covers
  draft cancellation/retention/create, blank-title disabled state, both old-menu
  transitions, and a 1,000-conversation picker with actual focus/key dispatch,
  Home/End reveal, scoped search, Down/Enter attachment, and Escape focus return.
- A 10,000-row GPUI harness verifies bounded rendering, distant keyboard reveal,
  exact scrollbar extent, and scrollbar-to-bottom without rendering the catalog.
- Semantic source guards: 8 passed, 63 assertions. Formatting and diff checks pass.
- Native screenshots inspected in light and dark themes with long English and
  Chinese titles and long workspace labels. Both text lines visibly ellipsize.
  A pointer scrollbar drag reached the final row of a 321-entry fixture while
  the semantic snapshot contained only six visible result rows.

Final verified native build: `2d9ec30da7331884-18de548caa18bf62`.
Independent spec and engineering-standards reviews were completed; identified
keyboard, scrollbar, and cross-editor-menu issues were repaired and re-reviewed.
Native macOS behavior remains unverified in this cloud Linux run.
