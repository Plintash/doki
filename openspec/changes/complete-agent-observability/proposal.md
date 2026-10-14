# Complete agent observability

## Why

The agent can already operate the app, but ordinary inspection still asks for
unbounded history, and useful provider, approval and workspace facts remain
hidden in app memory. Diagnosis should not require screenshots, source-reading,
selection changes or a temporary logging rebuild.

## What changes

- Compact discovery and context, with usable argument contracts and explicit
  issue/conversation/automation identity distinctions
- Narrow tree and pointer-state reads; automatic action/failure evidence uses
  selected-conversation metadata rather than full history
- Selectable provider catalog/discovery, pending runtime choices, and cached
  workspace evidence with loading/error/not-requested states
- Explicit-conversation, bounded transcript index and text slices, including
  unselected unloaded histories without changing the UI

This change adds information only. It does not grant permission to use a model,
answer approvals, edit a workspace, or change provider/account configuration.
Recording/replay, unattended/headless suites and scheduling follow separately;
cross-device execution is deferred. Existing real-model RPA acceptance remains
open until actually run.
