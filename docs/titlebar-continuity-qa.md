# Titlebar layout continuity

## Change

The native traffic-light clearance is 86 px on windowed macOS and the ordinary
8 px edge inset in fullscreen. Other platforms keep their existing 8 px inset
and their configured client-side window controls. AppKit still owns the native
buttons and their position.

The sidebar toggle and history buttons now have one window-root owner. Closing
or reopening the sidebar moves its clip and the content edge around the same
controls instead of removing one copy and inserting another. Conversation and
Board headers use the same inset, derived from the actual animated sidebar
width. The Board no longer cancels the sidebar's existing slide.

Fullscreen clearance uses the existing 200 ms ease-out width tween. A reversal
starts from the last rendered clearance; reduce motion settles immediately.
The root's existing pane-observer gate also covers this short transition, with
one reconciliation notify when it finishes. No recurring animation is added.

On fullscreen exit, native buttons may return before the clearance tween
finishes. Navigation is clipped out of their 86 px region, including its hit
regions. The controls emerge from that boundary rather than covering AppKit's
buttons. Toggle/history keyboard focus stays stable across panel changes.

## Platform evidence and limits

The GPUI revision pinned by Cargo.lock is
`4d2f3b3f3a8bb1f892c9c5c73e07047a9b3c61db`. Its `gpui_macos/src/window.rs`
restores native traffic-light frames in fullscreen and repositions them after
exit; `platform_title_bar/src/platform_title_bar.rs` likewise checks live
fullscreen state when reserving native-button space. This fix uses
`Window::is_fullscreen`, not a separate persisted approximation.

Source review and Linux GPUI tests do not validate AppKit fullscreen animation
or the native traffic-light reveal. macOS runtime acceptance remains required
for those platform-specific effects. No user computer is used by this task.

## Acceptance coverage

Automated geometry/state tests cover native clearance in all platform/mode
combinations, every sidebar width from 0 to 300 px, interrupted clearance
changes, first-frame layout, and reduce-motion settlement. Existing width-tween
tests cover easing and interrupted reversal.

Native acceptance should inspect conversation and Board surfaces with the
sidebar open and closed, toggle rapidly during the 200 ms slide, and verify a
single operable navigation group with no title overlap. Check drag regions,
Enter/Space activation, focus retention, and steady-state redraw quiescence.
For macOS, additionally enter/exit fullscreen in both sidebar states, reverse
while possible, and check the native-button reveal and all three hit targets.

## Verified 2026-10-14

The isolated combined UI tree `6a13b5738fd4d919b75f1338847c55628c50fdac`
contains the titlebar changes (`5ac4776`, `7b88f53`), issue-detail simplification,
and reviewed issue-review integration. Its native build identity was
`34f907748efa997c-18de5baad4e5c8ed`.

- Full app library suite: 678 passed, including the four titlebar tests and
  existing width-tween tests; workspace check passed on that combined tree
- Standalone semantic identity source contract: 8 passed, 63 assertions
- Native provider-free titlebar acceptance: 20 snapshots across conversation,
  Board and issue detail; exactly one navigation group with unchanged node
  identities, positions and 26 px hit targets throughout
- Keyboard Space/Enter sidebar toggles, Back/Forward, six rapid sidebar
  reversals, Board reversal, and Board entry during a right-panel slide passed
- Real pointer clicks and blank-sidebar-titlebar dragging worked in Linux;
  screenshots captured an actual intermediate panel position and both endpoints
- Linux fullscreen at 1364 × 1024 and restored 980 × 680, with sidebar open or
  closed: navigation x positions stayed 8, 40, 68 px with no duplication
- At the minimum supported 980 × 680 size, Back and the last visible detach
  control's keyboard hints remained fully inside the viewport
- After settlement, frame count remained 1790 in three samples over 2.3 seconds:
  the transition did not leave an animation loop running
- Independent Spec and Standards source reviews passed; the follow-up right-panel
  retirement guard also passed focused source review

The Linux software-rendered screenshots establish real layout changes, not
high-refresh frame-rate quality. AppKit traffic lights, their temporary reveal
on fullscreen exit, and macOS fullscreen animation still require macOS runtime
acceptance. No macOS runtime validation or user-computer access is claimed.
