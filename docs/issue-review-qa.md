# Issue review verification

Base: `045a120eadb142ee5d63ab549ab93c0cace65e70`.
Specification: [issue-review-spec.md](issue-review-spec.md).

## Independent two-axis review

Method: [Matt Pocock's Standards/Spec review](https://github.com/mattpocock/skills/blob/main/skills/engineering/code-review/SKILL.md).
The two reviewers are independent and read-only. Standards include the repository
rules and Fowler heuristics; heuristics never override documented conventions.

### Round 1: `202d5646e0f3090c683f1b31a13f10f62ab72a27`

Standards: two actionable findings.

- Generated handoff names were incorrectly assigned to user-owned `title`,
  preventing Pi's provider name from taking precedence. They now use `auto_title`.
- The virtualized issue editor row selected the latest review by scanning the
  full catalog. Its outer render now prepares the latest review and source
  readiness together; the row reads only that cached snapshot.

Spec: four actionable findings.

- A same-second stale catalog could erase the correction-session link. Review
  mutations now advance a monotonic revision, with stale-catalog/hydration tests.
- A fast Review click could race the Pi source's asynchronous hydration. The
  action now waits for loaded source context, with a visible loading hint and
  an action-side guard; existing draft text is never silently replaced.
- The shared TypeScript reducer treated a retryable provider error as a final
  review failure. Only terminal settlement or process exit now decides failure;
  a retry-then-success regression preserves the full review result.
- Board and issue detail used different review ordering. Both now select by the
  same activity-based key, including deterministic ties, with a two-review test.

### Round 2: `28e8a883cbe3b9e2d7d9884b9bd3609c04fc8fa4`

Standards/Fowler: zero actionable findings after full-delta re-review. Both prior
Standards findings are cleared.

The independent Spec re-review found a further same-second race: rollback or
truncation cleared the result without advancing its metadata revision, allowing
a queued catalog to restore the removed result. Destructive review invalidation
now advances that revision; protocol unwind/truncation and desktop stale-catalog
regressions cover the repair. Both repairs received the next independent committed-source review below.

The same pass found that Develop with Pi could reopen a correction draft from
the previously reviewed workspace after the issue moved. Ordinary development
reuse now requires matching project/workspace and excludes linked correction
drafts. A regression covers both same-workspace separation and workspace changes.

The next Spec pass found that current-result links alone lost this distinction
when the original review reran. Pi corrections now carry durable source-review
provenance in their own persisted catalog metadata; draft selection and automatic
workspace rebinding use that identity even after source rerun or removal.

## Verification boundary

All runtime tests use synthetic data and a provider-free JSON-RPC app-server
fixture. The fixture implements native review requests/events and never invokes
a provider executable, account or model. The native fixture lives under this
checkout's marked temporary data directory with an isolated HOME; all other
provider binaries are blocked and disabled.

Authenticated Codex review, macOS/Windows/Wayland, cross-device acceptance, and CI
are not established by these tests. CI and cross-device work remain deferred.
The final source and acceptance results are recorded below. The fixture is now
stopped; its isolated database and synthetic evidence remain available for replay.

## Protocol evidence

The installed Codex 0.159.2 app-server schema was generated locally without model
execution and checked against primary sources:

- [Official app-server review documentation](https://learn.chatgpt.com/docs/app-server#review)
- [Official target resolution](https://github.com/openai/codex/blob/7eed30ad1beaebc20b7d848ce9ee887525a379b0/codex-rs/prompts/src/review_request.rs)
- [Official review task](https://github.com/openai/codex/blob/7eed30ad1beaebc20b7d848ce9ee887525a379b0/codex-rs/core/src/tasks/review.rs)
- [Official fallback output sentinel](https://github.com/openai/codex/blob/7eed30ad1beaebc20b7d848ce9ee887525a379b0/codex-rs/protocol/src/review_format.rs)

`review/start` accepts a thread, target and delivery mode, not a second prompt
field. A supported custom target therefore contains the explicit Doki scope and
issue context. Inline delivery preserves the review's own fresh Codex thread;
Pi cursors are never converted or resumed as Codex. `exitedReviewMode` output is
held until successful turn settlement; empty output and Codex's explicit failure
sentinel cannot become a completed review.


## Final reviewed product checkpoint

- Base: `045a120eadb142ee5d63ab549ab93c0cace65e70`.
- Product commit: `f2d6a086f09693031edd24b14832fe3c00004779`.
- Product tree: `22779d350433df7b22a568dd2a029443092986b7`.
- Both independent axes re-reviewed the committed corrections: Spec and
  Standards/Fowler each reported **zero actionable findings** at this checkpoint.
- This acceptance record and the native test are a subsequent test/documentation
  delta. They do not change the tested product implementation.
- The parallel direct-issue-draft/attachment-picker PR 79 and later Board/menu
  fixes are not included in this checkpoint. Integration must preserve those
  separately reviewed changes when rebasing onto newer main.

## Executed gates

The source was clean and held at the product checkpoint throughout these gates.
The shared-target build scripts were refreshed to ensure generated migration
paths belonged to this checkout, including migrations 0008 and 0009.

- Debug workspace check: passed.
- Protocol: 131 passed; Rust client: 47 passed.
- Core: 754 passed, 25 existing ignored, 9 live OpenCode tests deliberately
  excluded to avoid account/provider calls.
- Desktop app: 657 passed, 2 existing ignored, serial isolated data profile.
- REPL: 26 passed. GPUI RPA list/spike: 2 passed.
- TypeScript client/web/mobile and RPA unit suites: 415 passed; client, web,
  mobile and selected RPA script typechecks passed. The new native test also
  passed strict TypeScript checking.
- Formatting, whitespace and generated-binding verification: passed.
- Release workspace check: passed.
- Native app, daemon, debug daemon and REPL build: passed. The actual development
  watcher then built and launched the app, with the cached native SDK packaged.

One initial full-core run stalled in the existing terminal-flood test and was
interrupted. That exact test passed alone in 0.18 seconds; the complete scoped
core rerun then passed in 45.58 seconds. This was not silently counted as a clean
first run or hidden by excluding the test.

## Native Linux acceptance

The actual X11 cloud desktop app, launched by `scripts/dev.ts "Issue Review QA"`,
used the isolated synthetic fixture and the existing native Codex transport.
Build identity: `904afa459016c644-18de5557dc100b53`.

The opt-in `scripts/rpa-issue-review-live.test.ts` passed **257 assertions** in
50.75 seconds. Its exact tested SHA-256 is
`b8274bc8c40c0ac295809d5f97d301c63ee46a3307c7483f40ec4971041bcb49`.
To preserve the clean product gate, it ran first from the checkout's ignored
`temp/` directory; the checked-in file is byte-identical and resolves the same
checkout/client/fixture paths.

Verified behaviors:

- Fresh Codex draft under the same issue and concrete Pi worktree, with the issue
  requirements and latest hydrated Pi response; no borrowed Pi cursor.
- All four selected scope/reference semantics appear in native inline
  `review/start` custom targets. No ordinary `turn/start` substitutes for review.
- Repeated Review and Fix clicks preserve actual user amendments and reopen the
  same draft rather than regenerating or duplicating it.
- Successful native completion stores the full result once, promotes its bounded
  summary/status to the issue catalog, and leaves the issue In progress.
- Open review and Board-card navigation reach the correct conversations/issue.
- Fix with Pi creates a separate unsent Pi draft containing the full findings,
  scope and brief in the reviewed workspace, with durable correction provenance.
  The pre-existing unsent implementation draft is unchanged.
- Provider hold followed by Stop produces native `turn/interrupt` and Stopped.
  Simulated sign-in refusal and empty provider output produce Incomplete with no
  completed result or usable Fix action.

The first native-test attempt incorrectly expected ordinary painted text in the
GPUI semantic tree. The test was corrected to assert actual controls/navigation
and promoted metadata. Visible text is verified by screenshots, not inferred
from that tree.

### Pixels and restart

Inspected actual app screenshots show the issue review controls, completed
review preview/status and Fix with Pi action, then the Pi correction composer
with full findings and the inherited worktree. Screenshot artifacts:

- `doki-issue-review-prototype.jpg`
- `doki-review-completed-detail.jpg`
- `doki-review-pi-correction.jpg`

A real Quit stopped the app, watcher and external daemon. A fresh launch used a
new executor (`b60ccb16-9841-4fc0-aba4-08882709663e`, replacing
`cedfc058-54bc-46b4-9fdb-70f0066544c0`) and rehydrated the saved data. It retained
the completed full result, result-to-correction link, correction provenance,
exact reviewed workspace, user-edited correction draft and original Pi draft.
SQLite integrity was `ok`; persisted review outcomes were Completed, Stopped and
Incomplete as expected. The fixture was cleanly quit again after verification.

## Running the provider-free native test

Launch `scripts/issue-review-fixture.py` through the normal native desktop build
environment. It seeds only its marker-owned synthetic data once and uses the real
watcher. Run the test with the fixture's existing daemon connection:

```sh
WAKU_RPA_ISSUE_REVIEW_TEST=1 WAKU_RPA_TEST_CHECKOUT="$PWD" \
  bun test scripts/rpa-issue-review-live.test.ts
```

The test independently verifies the checkout marker, running fixture/database,
exact synthetic executable wrapper, disabled other providers, daemon settings,
executor identity and model catalog before submitting anything. It never submits
Pi drafts, changes credentials, enables a real provider or launches an account
model. It preserves synthetic conversations as evidence and restores the fixture
mode afterward. Ordinary test runs skip it unless explicitly enabled.

## PR 79/82 integration checkpoint

The workflow was integrated onto actual main
`8d10896a633c3830f816561e833f82de7cdc9db0`, preserving the direct issue-draft
editor, searchable attachment picker and corrected shared dropdown geometry.
The frozen implementation was `c40b1741fb9961ee4f71dcbfb87faa08b51bad12`
(tree `42a4e7cbeceffa81a403877331233c6b48b8087a`).

Unsaved issue drafts omit review and conversation actions. Creating the issue
reveals those actions without recreating the editor. Independent integration
review identified a same-timestamp layout invalidation gap: the measurement key
now explicitly includes draft state. Spec and Standards both cleared the
resolved integration and this correction. The PR 82 rebase was conflict-free;
its only added paths exactly matched the already-reviewed upstream delta.

Executed on the frozen integrated implementation:

- Debug workspace check: passed.
- Desktop app: **664 passed**, 2 existing ignored, isolated serial profile.
- Native protocol-19 daemon/debug-daemon build: passed.
- The real development watcher built and launched the integrated Linux app.
  Executor `4c292b11-9bd2-4e5c-a19e-7d5445f0e8f7`, build
  `7db964aa048ec3bb-18de5a3da5625b4e`.
- The unchanged provider-free review acceptance passed, including all four
  scopes, edited draft preservation, complete result/correction handoff,
  interruption, simulated sign-in refusal and empty-output failure.
- Native scratch/draft/picker acceptance passed **3 tests / 136 assertions**.
  This includes immediate title edit + Create, hidden review controls before
  creation, visible review/development controls afterward, and the 1,000-row
  searchable keyboard picker.
- Strict TypeScript, formatting and whitespace checks passed.

The initial combined native run exposed two harness issues, not product
failures. Its first scratch comparison observed the window manager's delayed
geometry persistence; the unchanged comparison passed once that state settled.
The rapid-Create test typed a replacement title without selecting the old text,
so it appended instead. Two explicit focus/select-all calls corrected that
interaction without weakening an assertion. The corrected test ran from ignored
`temp/` while product source remained frozen, then was copied byte-for-byte into
`scripts/rpa-workflow-live.test.ts` in test-only commit
`756de787591c2a838d1dd593f5d4f3ac70a7ca18`. Its SHA-256 is
`ea9bfbe3fbc4526ad2dc075dac337b0eda7a9c60a684c49f5fa91cefeebf77ac`.
Both review axes cleared that exact two-line correction.

A separate native Create/scroll inspection confirmed the empty draft omits the
review section, then shows the scope selector, Review with Codex and Develop
with Pi together with a legible conversation footer after creation. Captures:

- `doki-review-integration-draft.jpg`
- `doki-review-integration-created.jpg`

Quit stopped the app and watcher, removed the fixture's running marker and
closed the native window. The RPA quit client observed daemon disconnection
while shutdown completed. No provider account or model was used. The original
full backend/restart evidence above remains valid for the unchanged backend;
those broader suites were not rerun solely for this UI integration. PRs 83/84
and the subsequent lightweight issue-detail styling are outside this checkpoint.
