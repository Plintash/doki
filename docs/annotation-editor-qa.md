# Multiline annotation editor verification

The floating annotation editor now uses the existing native multiline text input:
**Enter saves; Shift+Enter inserts a newline**. Pasted paragraphs retain spaces,
indentation and blank lines. The shared input continues to normalize CRLF/lone CR
to LF, and a whitespace-only comment still represents no comment.

The input grows to 240px, or the space available in the transcript if smaller,
then scrolls with the existing caret-following overlay scrollbar. Delete, Cancel,
and Save remain outside that scroll viewport. The editor follows its measured
size to stay above the composer. The hint and buttons share fixed footer geometry.

## Exact source and automated checks

Product source: `7ea7ca33c60fb77618994fb8ca689eb1532e8086`, tree
`e3681b75c8e6a234244cd46df4ec6ca26025520d`, based on `0e82ccfd`.

Against that clean source:

- `cargo test --locked --offline -p waku --lib -- --test-threads=1`:
  **639 passed, 2 pre-existing ignored**, with an isolated test profile
- Debug and release `cargo check --locked --offline --workspace` passed
- `cargo fmt --all -- --check` and `git diff --check` passed

New regressions exercise multiline submit/Shift+Enter, marked IME composition,
changing the auto-height cap, exact nonblank comment whitespace, and the editor's
multiline subscription/footer contract. Existing preview-exclusivity regressions
continue to pass. Independent Standards/Fowler and Specification reviews of the
cumulative diff found no remaining actionable source issues.

## Native Linux acceptance

The actual `scripts/dev.ts` watcher rebuilt an isolated provider-disabled native
Linux app. The final connected build was
`92df7d5a60051478-18de483847d59cde`, instance
`7d8f7308-52e2-4dfa-b858-2189d803b5be`. No model was called.

The old native build first reproduced single-line paragraph collapse using the
system clipboard. On final source, native semantic actions and screenshots verified:

- Shift+Enter adds line breaks without saving; Enter saves and closes the editor
- Reopening retains exact Chinese text, surrounding spaces, blank lines and the
  same annotation identity
- Save-button changes persist; Cancel and Escape discard only the unsaved edits
- A real system-clipboard paste of 30 lines retains every line and the final
  newline, including after saving and reopening
- Manual wheel scrolling works while Save/Cancel stay visible; Home then typing
  returns the caret to the start without losing the remaining text
- The long editor remains contained in a shorter native window
- The chat draft and transcript history counts remain unchanged

Screenshots contain disposable synthetic fixture data only. The before image,
multiline editor image, long-scroll image, and short-window image were captured
from the native app; they are not HTML mockups. The machine-readable acceptance
record and per-step observations are retained with the cloud QA artifacts.

## IME and platform boundary

The composition regression proves that Enter cannot emit Submit while native
marked text is active, and a separate ordinary Enter works after text is committed.
Pinned GPUI `4d2f3b3f3a8bb1f892c9c5c73e07047a9b3c61db` also consumes handled
macOS/XIM confirmation keys before normal keybinding dispatch; Wayland text-input
commits insert text without synthesizing Enter.

A real Chinese OS IME candidate-confirmation session was **not run** because that
service is not configured in the cloud desktop. No macOS runtime acceptance is
claimed. This source and regression evidence must not be described as a tested
native Chinese IME session.
