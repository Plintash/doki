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
surface.

**Overlay scrollbars are the classic violator of both cadences.** A streaming
surface moves its content every commit, so the bar sits in its reveal hold for
the whole turn — and the hold is constant-opacity, needing zero repaints.
[src/ui/scrollbar.rs](../src/ui/scrollbar.rs) therefore schedules a single
one-shot wake for hold expiry and rides the pulse clock only through the
350 ms fade. Driving frames through the hold pinned the pane at pulse rate the
moment any scrollbar became visible.

## Terminal output

A terminal is a second streaming source, and it obeys the same rule as the
transcript: the daemon coalesces output and the client renders from state,
never per byte.

- The daemon reader appends each read to the terminal's bounded ring and emits
  one `terminalOutput` event per batch. A batch flushes when it reaches
  `FLUSH_BYTES` (32 KiB) or `FLUSH_INTERVAL` (16 ms) after the previous flush,
  whichever comes first, so an interactive echo waits at most one frame and a
  flooding command cannot drive one message per write
  ([crates/waku-core/src/terminal.rs](../crates/waku-core/src/terminal.rs)).
  The ring keeps the most recent 1 MiB; the batch and ring caps are what stop a
  flooding producer from growing daemon memory at the stream rate.
- Attach replay and the live stream meet at an exact sequence boundary. The
  terminal stamps every batch with its cumulative end offset, and
  `AttachTerminal` returns the retained bytes with the offset at their last
  byte, taken under the same lock the reader holds while appending and
  delivering a batch. The client drops any batch at or below that offset, so a
  byte cannot reach the emulator twice or slip through the gap.
- The desktop driver is `TerminalSession` ([src/terminal.rs](../src/terminal.rs)):
  a background reader feeds bytes into the Alacritty grid off the UI thread,
  the grid renderer is unchanged from the pre-daemon terminal, and the view
  polls it on the existing 24 ms pump. Input and resize are fire-and-forget
  daemon notifications, so a keystroke never blocks the frame that produced it.

Verify the cadence with
`cargo test -p waku-core --locked websocket_terminal_flood_delivers_bounded_batches`
and the boundary with
`cargo test -p waku-core --locked websocket_terminal_attach_boundary_has_no_duplicates_or_gaps`.

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
row (gpui `list()` semantics). If that ever needs to shrink: fork-level cached
list rows need a measure-once extension to `ViewElement` caching (cached views
lay out from style, not content, which breaks the list's measurement as-is);
alternatively fold activities into the virtualized list as block-granularity
rows. Smaller levers, in memory and unproven: stable
`StyledText` element ids for gpui's per-element layout memo, and the per-row
`Message` clones in the row builder.

## Startup and reopen latency

The window paints before the daemon answers, and closing it destroys it, so
both the first launch and the rebuild that follows a Dock activation are
measured properties with recorded budgets. Instrumentation is opt-in:

- `WAKU_STARTUP_TRACE=1` (or `stderr`) writes one milestone line per completed
  launch or rebuild to stderr.
- `WAKU_STARTUP_TRACE=<path>` appends the same lines to a file, which is what
  the harness uses because `open` sends an app's stderr to the unified log.

A line is written when a run reaches `interactive`, from a writer thread rather
than the frame that got there. `src/latency.rs` owns the format, the parser,
and the budgets; `src/startup_trace.rs` owns the collection. Both are unit
tested; only the launch timing itself needs the harness.

```
startup-trace pid=97892 run=0 kind=cold start_ms=0.000 process_start_ms=0.000 \
  window_open_ms=55.000 first_frame_ms=108.000 daemon_ready_ms=190.000 \
  tasks_hydrated_ms=303.000 interactive_ms=306.000
```

Every timestamp is milliseconds since the process started, and `start_ms` is
the activation that produced the run: zero for the cold launch, the moment the
window opener ran for a rebuild. A run's latency is `interactive - start_ms`,
which is what the budgets bound. `run` and `kind` separate the two runs in one
process's trace file.

| Milestone | Recorded at |
| --- | --- |
| `process_start` | The first line of `run`, before GPUI is built; always 0 |
| `window_open` | Just before `open_window`, and only when opening rather than focusing |
| `first_frame` | The window's first render — skeleton content on a cold launch |
| `daemon_ready` | The window observing `DaemonState::Ready`; for a rebuild the daemon was already connected |
| `tasks_hydrated` | `Waku::new` returned, task state loaded from the daemon |
| `interactive` | The first frame showing the hydrated workspace |

`first_frame` and `interactive` are render passes, not confirmed presents:
GPUI exposes no presented-frame callback, and the two are the same frame on a
rebuild because the workspace is built while the window is constructed.

### Baselines and budgets

Measured on the reference machine (Apple silicon, debug build, 2026-10-08)
over five warm runs plus the first launch after a bundle:

| Run | `window_open` | `first_frame` | `daemon_ready` | `tasks_hydrated` | `interactive` | Latency |
| --- | --- | --- | --- | --- | --- | --- |
| Cold launch | 50–59 | 99–112 | 160–185 | 269–293 | 271–295 | **271–295 ms** |
| Cold launch, first after a build | 51 | 95 | 809 | 929 | 933 | **933 ms** |
| Reopen | 592–616 | 677–719 | 608–632 | 677–719 | 677–719 | **74–111 ms** |

The first launch after a build is the slow one: the daemon binary is cold in
the page cache, so `daemon_ready` alone is ~800 ms against ~170 ms warm. The
rebuild reuses the running daemon and its process is warm, which is why it is
an order of magnitude faster than the launch.

Budgets are set from those baselines in `BUDGETS` (`src/latency.rs`) and
repeated here; re-derive them on a materially slower machine rather than
raising them on a hunch:

| Budget | Baseline | Budget |
| --- | --- | --- |
| Cold launch | 271–933 ms | **3000 ms** |
| Reopen | 74–111 ms | **500 ms** |

The headroom is roughly three to five times the worst observed run. That is
loose enough for a cold page cache and tight enough that putting a blocking
daemon spawn or state load back on the reopen path fails: a rebuild that waits
on the daemon pays the ~800 ms `daemon_ready` cost the launch pays.

### Running the harness

The harness launches the debug app with `open -g`, so it never takes focus,
and terminates the app and its daemon when it is done. A traced cold launch
closes its own window and rebuilds it through the same opener Dock activation
uses, which is what produces the reopen run; driving AppKit's own reopen from
outside needs accessibility control a repeatable harness cannot rely on.

```sh
cargo build --package waku-daemon --bin waku-daemon
scripts/bundle.sh debug
cargo run --bin waku-latency-harness
```

It prints each run's milestones and both budget verdicts, and exits non-zero
when either budget is missed. Two ways to check the gate itself:

- `cargo run --bin waku-latency-harness -- --reopen-budget-ms 1` fails without
  rebuilding, which shows the comparison is live.
- A deliberately slowed build fails for real. Adding a 1200 ms sleep to the
  reopen path (`MainWindow::attach_workspace`, before `Waku::new`) pushes the
  rebuild to ~1300 ms against the 500 ms budget while the cold launch stays
  inside its own budget, and the harness reports the overrun.

The harness assumes a debug bundle in `target/debug`; `--app` points it at
another one. Debug builds overweight layout and scene generics, so treat the
absolute numbers as structure, not as what users feel, and confirm user-facing
claims on a release build.

### Where reopen time goes

Issue #41 profiled the rebuild below the milestone resolution to decide whether
GPUI window or renderer initialization warranted a patch on the pinned GPUI
fork (`egoist/zed`, branch `waku-webview`, `Cargo.toml`). It does not: a rebuild
spends most of its time re-loading state from the daemon, not in GPUI.

The split is available from the shipped milestones without new
instrumentation. On a rebuild `daemon_ready` is recorded when the build
closure asks the application for its daemon and finds it already connected
(so `daemon::request` returns immediately), which makes
`daemon_ready - window_open` the native window plus renderer initialization and
`tasks_hydrated - daemon_ready` everything the workspace does to load and
construct itself. To attribute that second span, temporary millisecond spans
were placed around `Waku::new`'s daemon reads (`ComposerDraftStore::load`,
`StateStore::load_or_fresh`) and a `sample <pid> 3 -file` capture was taken
across the launch and rebuild. Four rebuilds (debug build, reference machine,
2026-10-08):

| Rebuild | Window + renderer init | State load + construction | Latency |
| --- | --- | --- | --- |
| 1 | 16.2 ms | 84.5 ms | 100.7 ms |
| 2 | 17.7 ms | 64.8 ms | 82.6 ms |
| 3 | 16.1 ms | 68.2 ms | 84.4 ms |
| 4 | 30.4 ms | 57.7 ms | 88.2 ms |

Inside the second column the temporary spans and the capture agree: the
synchronous `LoadTaskState` round-trip is the one stable cost, ~52–54 ms in
every run, and workspace construction after hydration is ~1–3 ms:

| Rebuild | `LoadTaskState` | Composer drafts | View construction |
| --- | --- | --- | --- |
| 1 | 54.1 ms | 29.1 ms | 1.1 ms |
| 2 | 52.2 ms | 9.9 ms | 2.3 ms |
| 3 | 54.1 ms | 10.5 ms | 3.1 ms |
| 4 | 52.3 ms | 1.1 ms | 2.3 ms |

The `sample` capture of the rebuild's `open_main_window` path shows the same
shape: `MacPlatform::open_window` holds ~13 of 1 ms samples against ~41 for
`Waku::new` → `StateStore::load_or_fresh` → `StateStore::load`, i.e. the daemon
request. The first-frame render that follows `interactive` was 2.5–18 ms.
These runs had no selected session (`temp/state.json` in the debug profile has
`selected_session: null`), so `StateStore::load_or_fresh` skipped the
selected-session `HydrateSession`; on a profile with a selected task that
request would extend the same hydration span.

**Decision: no GPUI fork patch.** Window and renderer initialization is
16–30 ms, roughly a fifth to a third of a rebuild, while the daemon round-trips
that reload task state into the freshly built workspace are 55–83 ms. A fork
patch could only address the smaller slice, and the rebuild already comes in
~5–6× under the 500 ms budget, so the patch would add a third carried change to
the fork for no user-visible win. The dominant cost is also the one the design
already knows how to move: `Waku::new` fetches the whole task state (and the
selected session's detail) synchronously inside the window's build closure, so
a rebuild re-pays the hydration the cold launch pays even though it reuses the
running daemon. If the budget ever tightens, the lever is moving or caching
that hydration in `startup-latency` Phase 3 — app-side work that needs no GPUI
change — which is why this profile is recorded instead of a patch.
