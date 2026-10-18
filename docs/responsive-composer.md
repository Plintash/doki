# Responsive composer model controls

The composer reserves a fixed-size action area before distributing space to its
settings. Send, preparing and stop each retain a 26 px target; a working turn
with a follow-up draft reserves 58 px for stop, a 6 px gap and follow-up.
Model and effort are adjacent text targets in one content-sized group capped at
280 px. The model name truncates first. Unused space stays outside the actual
hit area; secondary controls and action targets keep their fixed positions.

Below 640 px of toolbar content width, safety mode, agent preset and goal labels
collapse to icons. The threshold uses the current chat-column width after
composer margins/border/row padding. Full labels remain available through hover
and keyboard focus; wide layouts keep the labels.

## Model and effort

The model name opens the existing searchable provider/favorites catalog. The
catalog has its full scrollable height below search, with no settings footer.
Selecting a model closes the picker and returns focus to the composer. Provider
locking, favorites, search, and model-picker keyboard navigation are unchanged.

The adjacent actual effort label opens a separate content-sized options popover
(280 px wide, clamped to narrow windows). Its reasoning control has a substantial
neutral rounded rail, visible stops, and a circular thumb. Supported service-tier
and context-window choices use short selected segments below it. Their full
meaning and provider-default status remain available to assistive technology
and tooltips, without a repeated heading, checkmark, or Default badge.

Provider labels and IDs are used as supplied. The synthetic `default` service
choice means the existing provider-default policy and is labelled Default unless
the provider catalog supplies a label for that exact ID. No rendering or opening
a menu writes a default value or clears an explicit selection.

There is no special highest-level alias, texture, flourish, or animation clock.
The slider locally previews the pointer and actual provider label continuously,
then commits one discrete setting on normal release, including outside the
track. Capture cancellation restores the committed value without writing. Session/model
identity changes cancel local preview, and stale handlers cannot write after
model changes or menu dismissal.
Keyboard arrows, Home/End and accessibility increment/decrement commit directly.
Disabled/fixed controls are inert. The app's existing settings/runtime path is
used only for a committed value and never submits a model turn.

This change addresses two source-visible costs in the previous design: each
detent invoked synchronous settings/state persistence (including fsync), and
pointer notifications targeted the owning application while the catalog was
open. Local preview avoids repeated persistence during capture, and the closed
catalog no longer rebuilds its model rows during effort adjustment. These are
source and automated-test findings, not a measured native frame-rate claim.

## Automated regression coverage

- Shared production geometry at 278, 318, 418, 558, 559, 560, 561, 639, 640, 641 and 698 px
  content widths, including long ASCII/CJK names, optional controls and every
  action state
- Adjacent model/options hit targets, independent popover bounds, full-height
  catalog geometry, and fixed trailing-edge submission targets
- Actual provider effort labels, distinct native/orchestration values, and
  explicit/default service-tier semantics
- Slider pointer preview and release/cancellation, keyboard/accessibility,
  local notification counts, and zero/one-option behavior
- Existing capability/default/remembered-state resolution and provider fallback

```sh
cargo test --locked -p waku --lib
cargo build --locked -p waku --bin waku
cargo fmt --all -- --check
```

Native/device visual acceptance remains user-owned. Automated geometry tests do
not claim visual inspection of a running native build.
