# Responsive composer model controls

The composer reserves a fixed-size action area before distributing space to its
settings. Send, preparing and stop each retain a 26 px target; a working turn
with a follow-up draft reserves 58 px for stop, a 6 px gap and follow-up.
The model entry sizes to its content, caps at 280 px, and truncates in a
shrinkable slot. Unused space stays outside the actual hit/selection area;
secondary controls and action targets keep their fixed positions.

Below 560 px of toolbar content width, safety mode, agent preset and goal labels
collapse to icons. The threshold is calculated from the current chat-column
width, including panel animation, after composer margins/border/row padding.
Full labels remain available through hover and keyboard focus, and goal status
uses a distinct glyph rather than color alone. Wide layouts keep the labels.

## Model and effort

One entry opens the existing searchable provider/favorites catalog directly.
Models with adjustable traits show a compact footer below the list, in the same
surface:

1. Current reasoning effort beside a small neutral label
2. Thin discrete effort rail with visible stops and a 12 px thumb, retaining a
   28 px pointer target and the catalog's option order
3. Supported service-tier and context-window options, with provider defaults

No intermediate page, repeated model-name header, lightning icon, reset icon,
or blue gauge styling remains. The footer uses Doki's neutral palette and the
picker uses its compact corner treatment.

Selecting a model with traits leaves the catalog open and moves keyboard focus
to the first adjustable footer control. Tab/Shift-Tab in search still cycle the
provider/favorites rail; arrows and Enter select a model, then native Tab
traversal moves through footer options and back to search. Clicking a provider or
favorites tab returns focus from the footer to search, so arrows navigate the
new catalog instead of changing effort. Escape closes the
picker directly. Models without traits have no footer and selection closes the
picker. A provider
locked by an established conversation stays locked; a draft on a disabled
provider opens the same usable fallback through either catalog-entry path.

The thumb follows the pointer continuously while the stored value snaps to the
nearest real provider option. Release settles onto that detent in 120 ms.
The highest of two or more choices displays `Galaxy · <provider label>` without
changing its ID or adding a capability. Claude’s appended Ultracode orchestration
mode remains separate from native Max and keeps its own name. Entering Galaxy
plays a bounded 420 ms
neutral star-gather/halo response, then leaves a quiet static rail texture.
Reopening at the highest setting does not replay the flourish. Reduced motion
settles immediately and disables the flourish; fixed/disabled sliders never
animate. Both transitions use the existing self-parking 30 Hz pulse clock,
with no repeating or display-rate animation.

The slider supports click, captured drag, arrows, Home/End and accessibility
increment/decrement actions. Models with no effort options get no slider;
a single fixed option is shown inert. Settings only update session options,
using the existing runtime path, and do not submit a model turn. Invalid stored
options are resolved against the selected catalog; Cursor suffixes and valid
per-model defaults are preserved.

## Automated regression coverage

- Shared production geometry tested at 278, 318, 418, 558, 559, 560, 561 and
  698 px content widths, including continuous resizing, long ASCII/CJK names,
  optional controls and all action states
- Real GPUI bounds, semantic labels, pointer targets and trailing-edge hits
- Content-sized short model names, capped long names, and one-surface catalog/
  footer geometry at normal and narrow sizes
- Footer focus chooses the first genuinely adjustable option and skips an inert
  single-option slider
- Slider endpoints, zero/one options, keyboard behavior inside the menu context,
  accessibility values/actions and captured drag across rerenders
- Capability/default/remembered-state resolution, Cursor suffixes, disabled
  provider fallback and unchanged session state apart from requested options

```sh
cargo test --locked -p waku --lib
cargo test --locked -p waku-protocol --lib
cargo build --locked -p waku --bin waku
cargo fmt --all -- --check
```

Native/device visual acceptance is intentionally owned by the user for this
change. Automated geometry tests do not claim visual inspection of a running
native build.
