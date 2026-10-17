# Streaming scroll reserve

The active turn owns its vertical runway at the transcript level. This is
separate from Markdown grapheme reveal and from the body's row/window cache.

## Behavior

- Sending a prompt reserves enough trailing room to keep that prompt at the
  viewport top, as before
- Measured layout growth feeds a time-based rate estimate. While following a
  live turn, a 32px baseline plus 350ms of expected growth determines the
  desired runway, capped at 180px or 30% of the viewport
- A continuous extent advances toward that target. An arriving line spends
  existing runway before more space is added; a burst can briefly outrun the
  follower, but text is never clipped to the runway. Following is additionally
  limited to two viewport heights per second, so an entire-response burst
  cannot turn an exponential catch-up into a multi-screen frame jump
- Following applies incremental explicit list offsets. Streaming commits never
  reassert GPUI's past-the-end sentinel
- Completion and interruption retain the current extent. Working-row folds
  above the reader preserve the visible logical anchor because controller
  coordinates are relative to the viewport, not absolute document offsets
- Scrolling upward immediately releases following and consumes only the actual
  distance the native list moved. Padding reduction never adds a second scroll
- Reverse scrolling cannot recreate consumed blank. New visible generation may
  replenish runway only after following has resumed; a reverse gesture alone
  cannot do so
- Scrollbar release and zero-motion wheel events cannot silently override an
  upward gesture's manual ownership. A deliberate return to the tail or the
  scroll-to-bottom action resumes following
- A fresh turn resets the controller. Session switches clear it
- Resize/reflow adopts fresh measurements without treating rewrapping as
  generation. Reduced motion uses direct following and does not lease a
  scrolling animation clock

The existing Markdown-only leading-height spring is disabled for anchored
turns, avoiding two competing runway controllers. Unanchored historical or
reattached sessions keep their existing rendering path.

## GPUI integration constraints

The pinned fork's scrollbar maximum excludes list padding. The transcript
adapter calculates the actual range from measured tail geometry, including
padding and short-content cases. Wheel observation runs in capture and bubble
phases around the list's own handler, so input coalescing and edge clamping are
accounted for before reserve is consumed. Reverse input in the same painted
frame is clamped against the already-reduced range.

The fork also skips all row prepaint when list padding fills the viewport.
Reserve is therefore always strictly shorter than the current viewport,
including a shorter resize and the provisional first frame.

Only the existing shared dissolve clock is leased while the continuous extent
is moving. No frame-path I/O, whole-transcript geometry scan, new provider
calls, or per-commit animation-frame loop is introduced.

## Validation

Provider-free checks:

```sh
cargo test --locked -p waku --lib transcript_buffer -- --nocapture
```

Controller tests cover slow/fast/bursty generation, 30/60/120Hz cadence,
completion/interruption, detached growth, upward consumption, reverse motion,
new-generation rearming, unknown measurements, provisional consumption,
reflow/adoption, offscreen completion folds, shorter resize, reduced motion,
and repeat turns. GPUI test-platform cases exercise actual list wheel and
scrollbar paths rather than synthetic wheel-delta arithmetic.

Native visual acceptance is a separate gate. Inspect a freshly rebuilt debug
app with slow, bursty, and fast provider-free streams; scroll away and return,
finish/cancel, repeat turns, resize both directions, and test reduced motion.
Check content continuity, blank-space ratchet, scrollbar geometry, and the
scroll-to-bottom action. A deterministic or test-platform pass does not prove
native visual feel, display timing, or macOS trackpad momentum behavior.
