# Native streaming playground

The debug app's **Playground** button opens a persistent development surface
for replaying local Markdown through the real native renderer and transcript
scroll reserve. The tool stays in the repository so streaming changes can be
checked without repeatedly constructing a provider session.

The replay itself makes no model calls, needs no provider credentials, and
does not create a conversation. It runs inside the normal debug app, which
still uses its ordinary local daemon and development data directory.

## Open it

Use the checkout's existing development watcher when it is already running.
Otherwise, from the repository root:

```sh
bun ./scripts/dev.ts "Streaming Playground"
```

After the rebuilt debug app opens, click **Playground** at the right of the
main window header. **Close** or Escape returns to the application. There is
no separate playground server, web page, or command-line mode.

To inspect the resolved application and database paths without launching:

```sh
bun ./scripts/dev.ts "Streaming Playground" --print
```

`WAKU_DEV_FLAVOR="Streaming Playground"` is equivalent to the flavor argument.
The watcher derives these debug-only runtime identity overrides:

- `WAKU_APP_NAME="Doki Debug — Streaming Playground"`
- `WAKU_APP_ID="sh.doki.dev.streaming-playground"`
- `WAKU_DATA_DIR="Doki Debug — Streaming Playground"`

The ordinary debug database is still `<checkout>/temp/app.db`; the window's
debug badge identifies it. The watcher may inherit the primary checkout's
development database when a new worktree has no sessions. This is not needed
for the replay itself, and it never requires adding provider credentials.

For a separately managed disposable debug instance, `WAKU_RPA_DATA_DIR` selects
an absolute directory containing `app.db`, `app.json`, and `state.json`.
`WAKU_RPA_SCRATCH_NAME` supplies its debug badge label. `WAKU_DAEMON_PATH` selects
the debug daemon binary when launching an already-built desktop binary
directly. Keep the app and its daemon on the same scratch instance; follow
the supported scratch workflow in [RPA validation](rpa.md).

The watcher handles the native launch differences, including explicit
environment forwarding through macOS LaunchServices. Do not start a second
watcher or manually rebuild its bundle while it owns this checkout.

## Payload and controls

The stress sample is [streaming-stress.md](fixtures/streaming-stress.md), the
existing mixed Markdown fixture with headings, paragraphs, lists, tables,
fenced code, Unicode text, and oversized blocks. It deliberately avoids images
and formulas, which take the renderer's asynchronous/full-body path. A prose
sample supplies a simpler comparison. The stress fixture is embedded in the
debug binary, so launching from another directory does not change the payload.

Use the rate, chunk, and jitter controls to compare slow, fast, and bursty
arrival patterns. The playback and restart controls let the same sample be
repeated without a model call. Use the follow control to compare following
against manual scrollback, and inspect the displayed delivery, geometry, and
render measurements while interacting with the native surface.

The initial settings inherited from the supplied playground branch are a
target rate of **500 chars/s**, chunk size **6 chars**, and **50% jitter**.
Here, `chars` counts Unicode scalar values (`str::chars()`), not UTF-8 bytes,
model tokens, or grapheme clusters. The renderer's grapheme reveal is a
separate presentation mechanism.

Native delivery commits coalesce at **120 ms**, matching the production
stream-commit cadence. A commit can combine multiple simulated chunks; the
chosen chunk size is not a promise of one redraw per chunk. Reveal and scroll
animation run separately on the shared motion clock.

- **Play / Pause** starts or suspends delivery of the remaining source
- **Finish** reveals the entire remaining source immediately and settles the
  full Markdown body
- **Restart** clears the arrived prefix and begins the selected sample again
- **Follow: on / off** controls automatic tail following; wheel and scrollbar
  input also exercise the production manual-scroll behavior

These controls change the local replay. They do not change the application's
provider configuration or a real conversation's content.

### Reading the measurements

- Delivery and progress counts use Unicode scalar values, as described above
- **fps** is the first preview's label for estimated playground render
  invocations per second; it does not measure display refresh rate or
  presented frames per second
- **build** measures Markdown body element construction time; it excludes
  native layout and paint and is not whole-frame CPU time
- Scroll offset, range, and reserve report the native transcript geometry

Use these measurements to compare replay conditions, then inspect the actual
native output. They are not a substitute for display-level performance
profiling.

## Production paths under test

The playground uses the same `MarkdownView`, `MarkdownCtx`, and
`markdown`/`markdown_windowed` renderer as conversation replies. Streaming
prefixes pass through the real incremental parser, incomplete-Markdown
mending, grapheme reveal, measured block ledger, and windowed body rendering.
Settled content takes the normal full-body rendering path.

For [PR #100](https://github.com/Plintash/doki/pull/100), the preview uses the
production `TranscriptBuffer`, `BufferedTranscriptScroll`,
`BufferedTranscriptList`, wheel observer, and overlay scrollbar. It does not
implement a separate browser-style scroll simulation. The old Markdown-only
leading-height spring is disabled on this anchored path, matching real
anchored turns.

Reserve is based on measured native list geometry. Upward input consumes the
actual distance moved, reverse movement cannot recreate consumed reserve,
completion retains the current extent, and resizing adopts new geometry.
See [Streaming scroll reserve](streaming-scroll-reserve.md) for the controller
contract and [Streaming render performance](performance.md) for commit and
animation cadence constraints.

## Behavioral reference

The control-and-preview workflow takes behavioral inspiration from
LobeHub's Streamdown playground. The reviewed reference is pinned to commit
`d948e7964b7053577633d4efe6522642c02dccdc`:

- [Playground.tsx](https://github.com/lobehub/streamdown/blob/d948e7964b7053577633d4efe6522642c02dccdc/site/src/sections/Playground.tsx):
  sample selection, stream controls, replay, and a live output/measurement
  layout
- [useLocalStream.ts](https://github.com/lobehub/streamdown/blob/d948e7964b7053577633d4efe6522642c02dccdc/site/src/lib/useLocalStream.ts):
  provider-free incremental input driven by rate, chunk size, and jitter
- [MIT license](https://github.com/lobehub/streamdown/blob/d948e7964b7053577633d4efe6522642c02dccdc/LICENSE)
  for the referenced project

No Streamdown source code or sample payloads were copied into this integration.
The implementation is native GPUI, the renderer and reserve controller are
Doki's production components, and the stress payload is Doki's existing
checked-in fixture. The native 120 ms commit cadence and character-counting
semantics above describe this implementation, not the web reference.

## Repeatable acceptance pass

1. Open the stress sample and let a slow replay grow past one viewport.
   Following should advance smoothly without clipping arrived text.
2. Repeat with fast and bursty delivery. Check that a large arrival does not
   become a multi-screen jump in one frame.
3. Scroll upward while running, then reverse direction before the next
   repaint. The consumed blank reserve must not return, and reading position
   must remain under manual control.
4. Drag and release the native scrollbar away from the tail. Re-enable
   following deliberately and confirm that it rejoins the tail.
5. Pause and resume, restart repeatedly, use Finish during a partially arrived
   code fence or table, and let a separate replay complete naturally. Watch
   the transition from live to settled Markdown and verify that an older
   replay cannot add text after reset or close.
6. Resize narrower/wider and shorter/taller, both while following and while
   reading earlier content. Look for blank frames, stale spacers, clipped
   blocks, or a restored reserve that had already been consumed.
7. Repeat with system reduced motion enabled. Functional following still
   works without decorative animation.
8. Close and reopen the playground. Confirm that normal application content
   returns and the next replay starts cleanly.

For provider-free controller and native test-platform checks:

```sh
cargo test --locked -p waku --lib transcript_buffer -- --nocapture
```

A test-platform pass does not establish native display timing, smoothness, or
macOS trackpad momentum behavior. Inspect the freshly rebuilt app and retain
screenshots or recordings separately from test results.

## Release exclusion and source locations

The module declaration, import, `Waku` field, initializer, render branch, and
header entry are all guarded by `#[cfg(debug_assertions)]`. Ordinary release
builds exclude the playground module and state rather than merely hiding the
button. The source remains checked in for future development work.

- Playground implementation: [src/app/stream_test.rs](../src/app/stream_test.rs)
- Debug state and module gates: [src/app.rs](../src/app.rs)
- Debug entry and surface routing:
  [sidebar.rs](../src/app/sidebar.rs), [render.rs](../src/app/render.rs)
- Production reserve controller:
  [transcript_buffer.rs](../src/app/transcript_buffer.rs)
- Production list and input adapters:
  [transcript_view.rs](../src/app/transcript_view.rs)
- Production Markdown renderer: [md/render.rs](../src/md/render.rs)
- Debug launch and identity:
  [scripts/dev.ts](../scripts/dev.ts),
  [identity.rs](../crates/waku-protocol/src/identity.rs)

When changing the integration, validate both a debug build and release
compilation so a debug-only type cannot leak into the ordinary application.
Source-level inspection of the gates does not establish that release
compilation has passed.

## Cloud preview result (2026-10-15)

The native app and debug daemon compiled successfully, and the window opened
on the supported Linux cloud desktop with the isolated playground identity.
Its content did not paint: the default Mesa backend reported
`MESA: error: Failed to attach to x11 shm`. Selecting the already-installed
Chromium SwiftShader Vulkan driver, including its explicit GPUI device ID,
also left a black window. This is a failed native presentation check, not a
visual acceptance pass. The underlying presentation failure remains unresolved;
no desktop security settings or permissions were changed. The owned black
preview window was closed, with its logs and binaries retained.

For an already configured native development environment, the ordinary watcher
command above remains the preferred entry point. A separately managed Linux
build can also be launched directly (default Cargo target directory shown):

```sh
cargo build --locked -p waku --bin waku -p waku-daemon \
  --features waku-daemon/dev-binary --bin waku-debug-daemon
WAKU_APP_NAME="Doki Debug — Streaming Playground" \
WAKU_APP_ID="sh.doki.dev.streaming-playground" \
WAKU_RPA_DATA_DIR="$PWD/temp/playground" \
WAKU_DAEMON_PATH="$PWD/target/debug/waku-debug-daemon" \
  ./target/debug/waku
```

Adjust both binary paths if `CARGO_TARGET_DIR` is set. Open **Playground** in
the debug header; the replay itself does not require a project, account,
provider session, API key, or paid model call. This command requires a working
native desktop renderer and does not fix the cloud presentation failure above.

## Source validation (2026-10-15)

- Exact-source native debug app and debug-daemon build passed
- Four playground replay tests passed: low-rate/large-chunk progress, Unicode
  delivery plus final-frame reconciliation, pause/restart isolation, and
  paused-to-Finish settlement
- Full application library: 686 passed, 2 existing benchmarks ignored. This
  includes PR100's 18 controller/input/layout regressions; these counts are
  not separate additional tests
- `cargo rustc --locked -p waku --lib -- -C debug-assertions=no` passed, and the
  resulting library had no `app::stream_test` symbols. This checks release
  configuration exclusion in the application crate; it is not a full release
  package, signing, or platform deployment validation
- Changed-file formatting and `git diff --check` passed
- Independent Spec/Standards review found no source-level blocker for Draft
  publication, including the completion and resize reconciliation addenda

Native presentation, manual input, keyboard focus, resize, reduced-motion feel,
and platform-specific momentum still need a visible native acceptance pass
before marking the playground Ready. The source/test checks do not replace it.
