# Local recording and unattended automation

## Why

The existing daemon can run an authored scenario after its CLI disconnects, but
cannot capture a portable driven workflow, execute without a GUI, or retain a
local schedule across daemon restarts. This change closes those specific gaps.

## What Changes

- Record explicitly driven, supported semantic calls and waits. Export a
  versioned replay with required parameters and earlier-result identity bindings.
- Add an explicit debug daemon-only executor with a discoverable backend-only
  capability boundary, plus isolated no-window scratch execution.
- Run ordered, daemon-owned suites with owned scratch cleanup and inspectable
  terminal results, including failure, cancellation and timeout.
- Persist bounded local schedules with one-shot/interval timing, non-overlap,
  interruption evidence and a defined missed-run policy.

## Impact

One shared versioned protocol and authenticated daemon remain the transport.
No OS cron jobs, user schedules, provider credentials, permission answers or
real model calls are created by implementation/acceptance. Cross-device driving,
Board changes and the independent Codex review workflow remain separate. Existing
live-provider acceptance gates are not closed by these provider-free checks.
