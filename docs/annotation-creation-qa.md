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

## Verification status

Behavioral regression tests, independent Standards/Spec reviews, and fresh native
GUI validation are required before marking the PR ready. This section will be
replaced with exact results; implementation alone is not acceptance evidence.
