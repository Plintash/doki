# Responsive composer model controls

The composer reserves a fixed-size action area before distributing space to its
settings. Send, preparing and stop each retain a 26 px target; a working turn
with a follow-up draft reserves 58 px for stop, a 6 px gap and follow-up.
The model entry is the flexible child and long labels truncate inside it.

Below 560 px of toolbar content width, safety mode, agent preset and goal labels
collapse to icons. The threshold is calculated from the current chat-column
width, including panel animation, after composer margins/border/row padding.
Full labels remain available through hover and keyboard focus, and goal status
uses a distinct glyph rather than color alone. Wide layouts keep the labels.

## Model and effort

One entry opens a configuration panel for models with adjustable traits:

1. Current reasoning effort and a reset-to-provider-default action
2. Current model, opening the existing searchable provider/favorites catalog
3. Discrete effort slider using the catalog's option order
4. Supported service-tier and context-window options, with provider defaults

Selecting a model with traits returns to configuration. Back or Escape from the
catalog returns without changing the model; Escape from configuration closes
it. Models without traits continue to open the catalog directly. A provider
locked by an established conversation stays locked; a draft on a disabled
provider opens the same usable fallback through either catalog-entry path.

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
