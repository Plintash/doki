# Streaming render performance

How Waku keeps CPU flat while a provider streams, what each piece of the
pipeline is allowed to cost, and how to measure before changing any of it.
This encodes the results of the 2026-08-16 streaming investigation, which took
sustained streaming CPU from 40–60% to under ~10% average (debug build) across
text and fast-reasoning streams.

The one-line model: **CPU ≈ redraw rate × visible element count.** GPUI
rebuilds and lays out every visible element on every frame a view renders —
[`list()`](https://github.com/egoist/zed) re-renders and `layout_as_root`s
every visible row per frame, cached heights only spare the overdraw — so every
rule below either bounds how often frames happen or how much is visible inside
one.

## Who is allowed to cause a frame

A frame happens when an entity is notified, when `window.refresh()` is called,
or when something re-arms `request_animation_frame`. Each has a different
price:

| Trigger | Cost | Allowed users |
| --- | --- | --- |
| `cx.notify(view)` | Re-renders that view and its ancestors; **cached sibling panes replay** | The stream pump (per commit), the pulse and dissolve clocks, user-event handlers |
| `window.refresh()` | Re-renders everything and **bypasses every cached pane** | Genuine whole-window invalidation only: hover transitions, drags, theme |
| `request_animation_frame` | Display-rate (120 Hz) re-render of the current view for as long as it re-arms | Nothing during streaming. One mounted repeating `with_animation` pinned the window at 120 Hz for a whole turn (~36% CPU by itself). The one sanctioned transient: the 200 ms panel show/hide slide ([src/app/render.rs](../src/app/render.rs)), which re-arms only while an edge is moving and gates the pane fan-out (below) |

The root `Waku` view re-renders on every frame regardless of what is dirty, so
it must stay thin: the sidebar, transcript, and right panel are `WakuPane`
islands ([src/app.rs](../src/app.rs)) embedded with the fork's
`Entity::cached`. Each pane observes the root — any root notify still
re-renders every island, so caching can never show stale state — while a
notify targeted at one pane (the pulse clock leases `window.current_view()`)
rebuilds only that island and replays the rest.

The one exception to that fan-out is the 200 ms panel slide: its display-rate
root notifies would price every tick at a three-island rebuild, so while
`panels_sliding()` the observer skips the fan-out and the cached-view keys
decide instead — the sliding panel (its clip moves) and the transcript (its
bounds move) miss their caches and rebuild with fresh state anyway, while the
island nothing is moving replays. Updates born inside an island still land
mid-slide because a child notify dirties its ancestor pane without the
observer, and the slide's retirement schedules one ungated notify so any
root-state drift in a reused pane converges the frame after the slide ends. Two traps to know: gpui's
`mark_view_dirty` walks **ancestors only**, which is why panes must observe
the root rather than expect root notifies to reach them; and a cached pane
lays its content out **as a root**, so a `flex_1`-sized subtree collapses to
its zero flex basis without a `size_full` flex wrapper.

## The two cadences

Everything during a stream happens at one of two rates, and every change to
the pipeline must keep it that way:

**Commits, ≤ ~8.3 Hz.** Provider chunks queue for a full
`STREAM_FRAME_INTERVAL` (120 ms) and fold into one drain → one notify → one
tail remeasure ([src/app.rs](../src/app.rs),
[src/app/runtime.rs](../src/app/runtime.rs)).
Two hard-won rules:

- The pump timer must **not** race the wake channel. It used to, which made
  the notify rate equal the provider's chunk rate.
- Every *streaming* delta kind must set the flags that route the pump onto the
  `StreamFrame` schedule. `ReasoningDelta` originally did not set
  `markdown_changed`, so a reasoning-only drain reported `Idle`, the pump went
  back to sleeping on the wake channel, and every fast thinking chunk woke it
  for an immediate drain-and-notify — **40+ commits and full re-renders per
  second**, sailing straight past the floor. Fast thinking hitting 40% CPU
  while text streamed at 10% was this one flag.

**Pulse ticks, ≤ 60 Hz.** All repeating animation rides the shared
self-parking clock in [src/ui/motion.rs](../src/ui/motion.rs): loaders read
a phase from a shared epoch, leases expire 300 ms after the loader last
painted, and the clock parks when no leases remain. Never use
`with_animation(...).repeat()` — it re-arms `request_animation_frame` every
display frame. A view's whole subtree rebuilds per tick, so cadence is priced
per *view*, not per animation: spinners use the full 60 Hz rate, while
`spin_slow` uses every second tick (≈ 30 Hz). Non-spinning pulses and
`pulse_lease` retain ≈ 30 Hz; `pulse_lease_slow` and `Pulse::every(2)` use
≈ 15 Hz for loaders mounted on expensive surfaces — the working dots set the
transcript pane's tick floor for the entire turn. Strides re-establish on
every tick (a lease's stride resets after it fires); an earlier version kept
the minimum stride forever, so one full-rate lease permanently dragged its
pane back to 30 Hz.

The veil dissolve rides a second clock. Loader ticks quantize a travelling
gradient into visible steps, so `display_lease` (same file) ticks at display
rate — 120 Hz on a ProMotion panel, with the extra wake-ups collapsing into one
vsync on a 60 Hz display — while a streamed dissolve is in flight. The cadence
is a Settings choice (`settings.stream_dissolve`: 120 fps by default, 60 and 30
offered for battery); the two slower options stride the loader clock instead of
starting the second one. It is still one `cx.notify` on
`window.current_view()`, so a dissolve only rebuilds the island that hosts it,
for the same per-frame work a display-rate redraw costs; the fee is that the
whole visible transcript (not just the fading tail) rebuilds per frame while
text streams. The reasoning peek keeps its ≈ 15 Hz pulse lease: minutes of
thinking would pay that rebuild at display rate for a dim, compact, secondary
surface. A windowed body builds only the blocks the viewport can reach, so the
veil carries two rules of its own: a fade in a block the window dropped expires
on the wall clock, because nothing on screen would ever drain it and a unit
that never expires would leave `is_fading` true with nothing fading, holding
the display-rate lease open for the rest of the turn; and a block the window
builds again is adopted at full opacity rather than re-dissolved, because the
reader has already read past it.

**Overlay scrollbars are the classic violator of both cadences.** A streaming
surface moves its content every commit, so the bar sits in its reveal hold for
the whole turn — and the hold is constant-opacity, needing zero repaints.
[src/ui/scrollbar.rs](../src/ui/scrollbar.rs) therefore schedules a single
one-shot wake for hold expiry and rides the pulse clock only through the
350 ms fade. Driving frames through the hold pinned the pane at pulse rate the
moment any scrollbar became visible.

## Bounding what is visible

- The transcript is virtualized with `list()`; per-commit invalidation is the
  last `STREAM_REMEASURE_TAIL_ROWS` rows only, and row folds, navigation
  turns, response footers, and sidebar rows are all fingerprint-cached
  ([src/app/transcript.rs](../src/app/transcript.rs),
  [src/app/sidebar.rs](../src/app/sidebar.rs)). A fingerprint must hash at
  display granularity: the sidebar row cache keys session recency, and hashing
  raw seconds would bust it on every commit.
- The live reasoning peek renders a **byte window** of the tail
  (`live_reasoning_window_start`,
  [src/app/transcript_view.rs](../src/app/transcript_view.rs)): markdown cost
  is O(rendered source) per tick regardless of block shape — a wall-of-text
  think is one giant paragraph and a bulleted think one giant list, so a
  block-count cap bounds neither. The slide hysteresis is wide
  (`LIVE_REASONING_WINDOW_MAX`) because fast reasoning appends several KB per
  commit and each slide rebuilds the window from a fresh view. The full trace
  renders once the turn settles.
- `markdown_tail` and block-index element ordinals
  (`block_ix << 16 | position`, [src/md/render.rs](../src/md/render.rs)) let
  a capped walk hand settled blocks the same flatten-cache and veil keys as a
  full walk.
- The live response body is **windowed by measured block offset**
  (`markdown_windowed`, [src/md/render.rs](../src/md/render.rs)). A message
  row is one `list()` item, so the whole body rebuilds whenever any of it is
  visible; the dissolve lease then repeats that at up to 120 fps. The renderer
  records each block's height as it lays it out and builds only the blocks the
  transcript viewport can reach (plus `MARKDOWN_WINDOW_MARGIN` above and
  below, and the volatile tail) with spacers sized from the same ledger, so
  the row still measures exactly as tall as the full body. Appends keep the
  ledger, with one exception: an append that starts a new top-level block
  leaves the block that just left the mended tail with its source range but no
  height, so that commit's frame builds the whole body once and measures it
  (a pure text append stays windowed, and the next frame windows again). A
  wrap-width, metric, or rewrite change drops the ledger, and the next frame's
  full pass re-measures. Only a pass a window can read keeps and fills it — the
  streaming response body, on every frame including the ones its window falls
  back to a plain walk; a settled body, the live reasoning tail, which streams
  through `markdown_tail`, and a body holding an image or a formula, which no
  window is built for, skip the per-block measuring wrapper
  entirely. Planning itself still
  walks the whole ledger — two vectors and two scans a frame — so it stays
  proportional to the document, and the build is proportional to the viewport
  **per top-level block**: the planner selects whole blocks, so a body that is
  a single block
  — one list, one table, one fenced code block, one wall-of-text paragraph —
  is one group covering the whole body, and no spacer can stand in for any of
  it: that frame rebuilds the whole body, at the plain walk's cost. Windowing
  pays off only once a body spans many top-level blocks, because the part the
  spacers can drop is exactly the part the viewport cannot reach — the bench
  below times both shapes, and the same payload spread over 400 top-level
  blocks drops the windowed frame well under a millisecond.
  Bodies containing an image or formula are never windowed, because those
  blocks can change height after their first frame and a spacer would freeze
  the old value. The gate is scanned from the parsed block tree, which marks a
  formula's runs whether or not math rendering is on, so a body holding one
  keeps the full walk even when the formula paints as static source text. And
  a body with a search or annotation mark keeps the full walk because a reveal
  reads its geometry back from the frame's registry — as does the streaming
  body whenever a selection exists anywhere in the
  transcript: its spans and drag anchor live in the same registry a shift-click
  resolves against, so a hidden block could not be extended into.
  On a 400-block reply this takes the streaming frame from ~2.7 ms to ~0.36
  ms in the debug build
  (`cargo test --locked -p waku --lib bench_markdown_frame -- --ignored
  --nocapture`).
- The live response body **reports a leading container height**
  (`MarkdownView::advance_clip`, [src/md/render.rs](../src/md/render.rs)). Text
  layout grows the row in whole-line steps, and the row's height is what the
  tail pin follows, so pinning the measured height makes every wrap a vertical
  jolt however smooth the grapheme fade is. The row instead reports a height
  that leads the measured body through a critically damped spring fed by the
  body's smoothed growth rate, so a layout step mostly goes into acceleration
  rather than position — it steps only by growth beyond the lead the spring has
  already banked (at most `CLIP_RUNWAY_MAX`), which steady streaming stays
  inside. The clip stays at or above the height measured on the
  previous frame, so a line laid out this frame lands in space that already
  exists and only text appended since that measurement ever sits below the
  clip edge — and the veil has not painted that yet, because a newly appended
  grapheme is born at zero opacity. The gap under the text is the only
  artifact, bounded by the rate lead (`CLIP_RUNWAY_MAX`). That trade holds
  only while the reader rests on the tail with the dissolve running: reduce
  motion, or a viewport scrolled away from the tail — where a row that keeps
  growing under a preserved scrollback anchor reads as a tremor — turns the
  same call into a release, and the row reports the body's real height.
  Settling, a rewrite, a reflow, a metric change, and a seeded re-attach all
  release the clip too, because a height kept across any of them would cut a
  body that has since grown past it. So does a body holding an image or a
  formula: such a block can land on a later frame and grow the body past a
  height measured before it, and unlike appended text it paints opaque, so
  the clip would cut it in the open. The seeded re-attach is the subtle one:
  it adopts the body it finds, text that arrived while the row was off screen
  included, at full opacity, so nothing holds that text back from paint and
  the clip goes with it. Only a streaming body is measured for
  that controller at all; a settled one skips the wrapper.
- `MarkdownView::set_text` derives the mended display tail only when content
  or the streaming flag changed — the derivation re-parses the final block and
  runs for every visible row every frame.

## Markdown math

Inline `$…$` and display `$$…$$` formulas use the native RaTeX engine
([src/md/math.rs](../src/md/math.rs)). Parsing TeX, loading embedded font
outlines, producing SVGs and rasterizing them all run on the background
executor. A frame only queues cache misses and reads completed results.
The General setting **Render math expressions** is enabled by default.
When disabled, every Markdown surface uses selectable LaTeX text without
queuing math jobs. Right-clicking a rendered formula adds **Copy Expression**
to its context menu; the copied source is captured at the click so later
streaming or layout changes cannot change the expression being copied.

Two workers process batches of up to eight formulas. Pending work is
deduplicated across Markdown views and capped at 128 requests. The image
cache retains at most 256 entries / 32 MiB, including failed results so
invalid formulas never retry on every frame. Eviction explicitly releases
GPUI sprite-atlas entries as well as CPU images. A separate bounded cache
keeps color-independent typesetting for theme and display-scale changes.
Individual formulas are limited to 8 KiB of source and two million raster
pixels; pending, invalid or oversized formulas keep selectable source text.

Ordinary prose retains the existing `StyledText` path. A paragraph containing
math uses one measured element with cached glyph runs and line positions,
including baseline alignment and Unicode wrapping. It does not create an
element per word or start an animation clock. Selection and search use the
original LaTeX byte ranges. The manual measurement is
`cargo test --locked -p waku --lib md::math::tests::benchmark_native_math_cache -- --ignored --nocapture`.

## Measuring

`docs/fixtures/streaming-stress.md` is the long mixed-Markdown payload used to
exercise the streaming path by eye: roughly 200 top-level blocks, tall code
blocks, tables and nested lists, no images or formulas so the windowed body
stays on the windowed path.

Sampling alone misled this investigation for hours; counters cracked it in one
run. In order of usefulness:

1. **Per-second counters, logged from render.** Static `AtomicU32`s counting
   window frames, per-pane renders, and pump commits, flushed once per second
   to a file from the root render (write on the background executor). One
   streaming run gives the full decomposition — it is how the 40-commits/sec
   reasoning bug and the stuck-stride bug were found after profiles showed
   nothing but generic layout work. Wire it temporarily; do not ship it.
2. **`sample <pid> 5` during a captured stream**, `awk '/Sort by top of
   stack/,0'` for leaves, ancestor-walk for the hot chain. Good for *what*
   is expensive (taffy vs shaping vs app code), useless for *how often*.
   The production binary is stripped — profile the debug build.
3. **CPU traces across a whole turn**: poll `ps -o %cpu= -p <pid>` every
   500 ms from trigger until settle. Averages over a turn hide phase
   plateaus; report both.
4. The built-in FPS counter pins the window at display rate by design and
   cannot measure streaming cadence.

Debug-build numbers overweight taffy/style/scene generics by several fold;
treat them as structure, not as what users feel, and confirm user-facing
claims on a release build.

## Known floor and next levers

With both cadences enforced, a streaming frame still rebuilds every visible
row (gpui `list()` semantics), and every visible row except the windowed
streaming body lays out its whole content. If that ever needs to shrink:
fork-level cached list rows need a measure-once extension to `ViewElement`
caching (cached views lay out from style, not content, which breaks the
list's measurement as-is); alternatively fold activities into the
virtualized list as block-granularity rows. Smaller levers, in memory and
unproven: stable `StyledText` element ids for gpui's per-element layout memo,
and the per-row `Message` clones in the row builder.
