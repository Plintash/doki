# Clipped transcript selection regression (#74)

## Cause and behavior

A long user message is displayed in a height-limited scroll viewport. Its text
still has full layout bounds, including rows hidden below that viewport. Those
bounds can overlap the following assistant message. Selection previously used
the full bounds for both mouse-down and drag, letting invisible prompt text
steal an assistant paragraph's selection. Later paragraphs beyond that overlap
can behave normally in the very same message.

The renderer now records the paint-time content mask for prose/code and math.
Pointer targeting uses the intersection with that mask; full layout geometry
remains available for copying ranges and positioning annotations. Clipped edge
coordinates are sampled just inside the visible row, accounting for floating
point precision when the layout has a negative scroll origin. Unclipped text
retains its usual beginning/end gutter behavior.

## Automated coverage

Seven GPUI regressions cover ordinary cross-block dragging; overlapping clipped
prose, math and code prompts at multiple scroll offsets; mouse-up persistence;
unchanged reply layout; fully hidden entries; ordinary gutters; and exact
clipped soft-wrap/hard-line boundaries. The overlap regression fails on the
unmodified implementation because the anchor belongs to `prompt`, not `reply`.

- `cargo test --locked -p waku --lib -- --test-threads=1`: 642 passed, 2 ignored
- `cargo check --locked --workspace`: passed
- `cargo check --locked --release --workspace`: passed
- `cargo fmt --all -- --check`: passed
- `git diff --check`: passed

Independent specification and standards reviews found no remaining actionable
issues. Geometry behavior was checked against pinned GPUI revision `4d2f3b3`.
The change adds constant-time geometry work to existing registry scans, with no
render-time I/O, new allocations, or layout mutations.

## Native reproduction

Use an isolated provider-disabled database with a 45-line user message followed
by an assistant opening paragraph, a status heading/list, and several later
paragraphs. Keep the long user message collapsed to its normal scroll viewport.

1. Drag from the assistant opening sentence into its first status bullet.
2. Release, copy, and paste into the draft composer without submitting.
3. Compare with selection in a later paragraph of the same assistant message.

On the baseline native Linux build, step 1 showed no visible selection wash or
selection toolbar despite active selection state. Copy/paste produced hidden
prompt lines 25–44. The later assistant paragraph selected normally, with both
highlight and toolbar. This proves content clipping can cause paragraph-local
selection failure without session corruption.

On the rebuilt app, the identical opening-paragraph drag shows the correct
highlight and selection toolbar after release. Copy/paste contains the intended
assistant text and no hidden prompt lines. Repeating the drag after scrolling
the prompt's own viewport also works. The later paragraph remains selectable,
and the reply's visible positions do not move during these selections.

The verified native build changed from `2317e5b952fe30ef-18de488250bf9390` to
`d53bb1d3740dbb60-18de4a56ccc36aa1`, with a fresh connected executor launched by
the worktree's watcher. The native fixture was disposable and provider-disabled;
its layout was patterned after the report. The user's private recording,
annotated image, and reconstructed fixture contents are not included in the
repository. No provider or paid model request was made. Cloud Linux native
acceptance does not replace a macOS platform check.
