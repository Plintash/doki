# Project and workspace dropdown layout QA

Verified on 2026-10-14 against main `2cc04ad` (including the task-entry and
searchable attachment-picker work).

## Fix

The issue-detail project and workspace menus already used the composer's shared
menu rows. Their virtualized list rendered each row as a layout root, losing
its margins and allowing it to shrink or grow with its label. A full-width
flex-column parent now gives each list item the same stretching layout as an
ordinary menu. No new menu style or component was introduced.

The existing 4 px row margins, 8 px inner padding, 26 px minimum height,
selection/checkmark styling, truncation, and interaction handlers are reused.
The praised Attach existing popover is unchanged. The opening snapshot,
virtualized list, and keyboard navigation remain unchanged.

## Automated checks

- The new GPUI geometry regression failed against the old list renderer, then
  passed with the wrapper. It verifies short and long row width, both horizontal
  insets, row height, header/separator spacing, and selection by clicking the
  trailing blank area of a short row.
- All 19 menu tests pass, including the 10,000-row bounded-rendering and distant
  keyboard-navigation regression.
- Full application suite: 655 passed, 2 existing ignored, 0 failed.
- Native debug app and daemon build, Rust formatting, and `git diff --check` pass.
- Independent specification and engineering-standards reviews have no actionable
  findings. The final screenshot review also checks the requested visual result.

Reproduction:

```sh
cargo test --locked --offline -p waku --lib ui::menu::tests
WAKU_RPA_DATA_DIR=/tmp/doki-dropdown-unit-profile \
  cargo test --locked --offline -p waku --lib -- --test-threads=1
cargo fmt --all -- --check
```

Use a fresh test-profile directory so saved native window preferences cannot
leak into GPUI's test platform.

## Native acceptance

The real GPUI app was rebuilt and launched by `scripts/dev.ts` as
`Doki Debug — Dropdown QA`, using a separate provider-disabled fixture on the
cloud Linux desktop. Verified build: `c12db54b5fbf492f-18de56fcfb8ce097`.

- Project and workspace entries measure 310 x 26 px inside a 320 px card, with
  matching left/right insets and full-width highlight/hit targets
- Long English project and selected workspace labels visibly ellipsize; the
  checkmark stays at the right edge, with readable separation from the label
- Separators retain their vertical spacing, and both light and dark menu
  backgrounds/selected rows render correctly
- A 30-project menu reaches its final action with End and returns with Home;
  only visible rows appear in the native semantic tree
- Home/End plus Enter changes the empty issue between Local and New worktree;
  Escape dismisses the workspace menu and Enter reopens it from its trigger
- The fixture retains 30 projects, 3 issues, 5 conversations, and the original
  8 messages, with SQLite integrity intact; no model prompt was submitted

Native captures: `project-light.jpg`, `workspace-light.jpg`, and
`workspace-dark.jpg`. They are direct app-window screenshots, not mockups.
macOS was not exercised in this cloud-only verification.
