# Add-to-chat annotation composer

Issue: [#30](https://github.com/Plintash/doki/issues/30)

## Scope and cause

The creation popup and badge editor are separate entry points. PR #75 upgraded
only the badge editor. The creation path still used the default single-line
`TextInput` in a fixed 360×36 pill without a mouse confirmation control.

The creation popup now follows the user's compact, rounded Codex reference:

- One persistent native multiline input, initially one row
- A circular upward-arrow button that **stages the annotation**; it never sends
  the conversation or starts an agent turn
- Enter confirms; Shift+Enter inserts a newline; Escape/outside click discards
- Automatic wrapping and explicit newlines grow the pill into a textarea;
  deleting back to one line reverses the same transition
- Size, placement, corner radius and button feedback transition over 180 ms
- New targets start at the currently displayed value; ordinary keystrokes with
  unchanged measured height do not restart animation
- Reduced motion settles directly; the input grows to 240 px (or the available
  viewport height), then scrolls internally with caret following
- The button stays outside the input's scroll viewport and is keyboard-operable
- Exact comment whitespace/newlines and the existing badge editor are preserved
- No microphone or voice-input implementation

The popup uses the roomier side above/below the selection throughout its growth,
and caps the input against that actual free space. Only when neither side fits
one usable input row plus its controls does it fall back inside the transcript
viewport rather than covering window chrome.

## Verification evidence (2026-10-18)

Product source: local `201e8c5769000226b0de3766c9bde628e9b38382`,
published as `bd6185061753caaa8b6e31cfdb2cc23faf7b466f`; their complete trees
were checked equal (`866003bf8d851234043e29f542604ae9bb6c4d2c`). The later
QA-document/artifact commit does not change product source.

### Automated checks and independent review

- `cargo test --locked --offline -p waku --lib -- --test-threads=1`:
  **753 passed, 0 failed, 3 ignored** (existing manual benchmarks)
- `cargo check --locked --offline --workspace`: passed
- `cargo fmt --all -- --check` and `git diff --check`: passed
- Eleven focused popup regressions cover exact whitespace/Shift+Enter/Enter,
  wrapping and shrinkage without replacing the input, click and keyboard
  confirmation, Escape, marked-text composition, retarget/reversal continuity,
  unchanged-target timing, reduced motion, safe placement at intermediate
  frames, and cached font metrics when reopening
- Separate Spec and Standards reviews followed the Matt Pocock two-pass review
  process. Initial findings about keyboard tab stops and selection overlap were
  fixed with regression tests; both reviewers cleared the final product source

### Fresh native acceptance

A freshly built Linux GPUI application was launched through the normal native
watcher, build `3f623ff2068deefb-18df928af9816b6c`. A separate HOME contained
only synthetic transcript data, with providers disabled. Validation used the
actual transcript selection → **Add to chat** creation route, native
keyboard/mouse/clipboard actions, native screenshots and semantic state checks.
The scoped native checklist finished with **23 passes, 0 failures, 3 not run**.

- The empty popup is 360×48 px with a visible circular up-arrow
- Native Shift+Enter adds a newline without staging; four lines including a
  blank line grow to 144 px while preserving the same input and focus
- Automatic wrapping grows the popup without inserting newlines; deleting the
  text contracts it back to the compact state
- Recorded native frames show intermediate growth heights
  `48 → 82 → 96 → 100 → 118 → 122 → 142 → 144` px and contraction
  `144 → 85 → 63 → 52 → 48` px; pure transition tests separately cover immediate
  retargeting/reversal from the currently displayed height
- Clipboard-pasted leading/trailing spaces and blank lines survive arrow-click
  staging and reopening the annotation badge editor exactly
- Confirmation increases the annotation count by one, leaves the existing
  composer draft unchanged, keeps the same two messages/one completed turn,
  and restores composer focus
- Tab visibly focuses the arrow; Shift+Tab returns to the same input; Enter and
  Space activate the arrow; Enter in the input also stages the annotation
- A real 30-line, 1,259-character clipboard paste caps the input at 240 px,
  keeps the arrow visible, shows the caret at lines 20–30, scrolls back to
  lines 1–11 with the wheel, and returns to the bottom with Ctrl+End
- Hover visibly enlarges the arrow circle from 28 to 30 px; holding a native
  pointer drag inside the hitbox shows intermediate contraction before release
  stages exactly one annotation and restores composer focus
- Escape and outside click cancel without staging an annotation; outside click
  also clears the selection and leaves the composer draft unchanged

Artifacts use only synthetic content, not the user's reference screenshots:

- [Compact popup](qa/annotation-popup/compact.jpg)
- [Multiline popup](qa/annotation-popup/multiline.jpg)
- [Long note with visible caret and confirmation button](qa/annotation-popup/long-note.jpg)
- [Short native growth/contraction recording](qa/annotation-popup/motion.mp4)
- [Native held-pointer arrow feedback and staging](qa/annotation-popup/arrow-feedback.mp4)
- [Measured frame-height changes and measurement metadata](qa/annotation-popup/frame-heights.json)

The short video is a cropped excerpt with idle portions removed. The JSON
indexes 3,400 samples resampled at 60 fps from the full 56.67-second native
recording, not frames in the 182-frame excerpt. Its metadata records the border
pixel measurement method and coordinate space; these are visual measurements,
not application instrumentation or a claim of sustained 60 fps rendering.

### CI and platform limits

The [source-head Tests run](https://github.com/Plintash/doki/actions/runs/39111761875)
completed with successful Ubuntu and macOS jobs, including live OpenCode API
acceptance and Cua SDK/REPL lifecycle checks; Ubuntu also passed generated
protocol/browser client verification. Windows passed its app suite (747 tests,
including all eleven popup regressions) but failed two unchanged core tests:
known [Instant fixture overflow #115](https://github.com/Plintash/doki/issues/115)
and [intermittent notification disconnect #121](https://github.com/Plintash/doki/issues/121).
The overall workflow is therefore **not green**. The matching
[Apple Silicon preview build/archive](https://github.com/Plintash/doki/actions/runs/39111761876)
passed. The PR reports subsequent
head-specific CI results separately; no unrelated transport/expiry repair is
included here.

Native containment was verified at 1180×812. Smaller-window native acceptance
was not run because the desktop window manager did not apply corner-drag or
Alt+F8 resizing; geometry regression tests cover constrained placement. The OS
reduced-motion setting was not toggled; its transition behavior is unit-tested.

No native macOS GUI or real OS IME session was available. Marked-text composition
is covered through GPUI's actual input-handler API, not claimed as a real OS IME
acceptance run. The macOS CI test/build result is not a substitute for native
macOS visual acceptance.
