# Global assistant

The sparkle control (or Command/Ctrl–Shift–A) opens a conversation on the right of the workspace. Its transcript and draft survive switching issues/projects, closing the panel and restarting the app. Existing project transcripts, annotations and composer controls are independent.

The assistant has one reserved session identity per daemon database, registered and persisted atomically before its first prompt. It uses the existing provider transport, persistence, streaming pump and Ask permissions in a private projectless workspace. The default provider/model comes from the existing app selection. Opening the panel does not start a model call; only sending ordinary text does. Provider approval requests open the existing full-session approval surface through **Review request**.

Every user prompt carries a bounded metadata snapshot: selected project and issue, global issue counts and active/waiting/failed work sessions. It never copies other conversations, reads repositories on a render frame, or treats issue descriptions as authority. This is a snapshot of Doki's loaded daemon catalog, not a claim to have inspected GitHub or every file.

`/status` returns the local snapshot without calling a model. `/open <issue ID or prefix>` navigates to an existing issue; ambiguous prefixes are rejected. Commands are dispatched only from new user input. Provider output is never parsed into actions.

For a selected issue, the compact workflow section opens a Plan → Execute → Review → Report setup. Each stage has its own existing harness/model picker. Matching configured board stages can supply defaults, but workflow order is independent of board columns. The default preset does not mark the issue Done. An existing, unambiguous Report stage mapped to Done is preserved and shown in setup; the daemon applies it only after the report provider finishes. `/start [issue ID or prefix]` opens that setup; the explicit Start button launches it after pending issue edits have saved.

Pause stops advancement after the current stage. Resume, Retry and Cancel route to the same daemon API; cancelling remains pending until provider shutdown is confirmed. `/pause`, `/resume`, `/retry` and `/cancel` act only on the selected issue's latest run. The panel retains an uncertain start's request identity so retries cannot duplicate a run. Polling and reconnect only refresh status, never start work. Board cards also show the current workflow stage or blocking reason. A completed workflow shows Report ready, an existing session link, and a separate Done action when needed; it never claims a merge.

Workflow execution remains in the daemon issue-run API. The panel does not contain a scheduler, infer approval from prose, or make hidden background submissions. Cached workflow metadata is included in the assistant snapshot; stale or missing snapshots are marked explicitly.

## Validation

Provider-free tests cover singleton identity, draft isolation, history filtering, bounded context, Unicode truncation, strict command parsing and ambiguity. The right-side column uses existing fitted panel geometry; its editor is multiline, height bounded, and its action buttons do not shrink. Conversation rows are virtualized and Markdown views are cached.

Real-device visual acceptance and provider-backed interaction are separate from automated checks and are performed by the user for this change.
