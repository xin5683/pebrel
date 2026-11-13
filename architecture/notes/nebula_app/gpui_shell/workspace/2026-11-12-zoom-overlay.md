# Pane zoom uses a floating overlay, not animated terminal bounds

## Status

Accepted.

## Context

`Ctrl+Shift+Enter` (`ToggleZoom`) used to flip `WorkspaceTab::Terminal.zoomed`
and re-render the focused pane as a full-card terminal. The split tree, pane
entities, PTYs and focus handles never moved; only the final layout changed.
That was correct but gave no spatial feedback: the pane appeared to jump from
its split slot to the card, with no cue that the split was still there.

The product request was a temporary floating state: the focused pane lifts
into a card with margins, the split stays visible and dimmed behind it, and
opening/closing has a spring feel. The existing shell is pinned to GPUI
`Kuddev/zed` rev `4f2c269…` (`Cargo.toml`), whose `gpui` crate has
`Animation`/`with_animation`/`with_easing` but no `SpringAnimation` and no
public element paint transform. Upstream `SpringAnimation` was introduced
after this pin.

## Evidence

- `workspace.rs` `toggle_zoom`/`render_terminal_tab`: the old zoom path only
  switched which pane was rendered at full card size.
- `terminal/view/layout.rs` `set_layout` and `mark_structural_resize`: every
  observed bounds change can feed a grid/PTY resize. Structural changes are
  committed once; a per-frame bounds animation would create a stream of
  intermediate grids and conhost rewraps, drifting the cursor.
- `theme/transition.rs` shows the established Windows capture path: one
  `PrintWindow(PW_CLIENTONLY | PW_RENDERFULLCONTENT)` client frame, cropped or
  painted as a `RenderImage`.
- `widgets/segmented.rs` shows the established interruptible motion pattern
  (shared sampled position + `window.request_animation_frame`) when GPUI's
  animation element is too coarse for a reversible spring.

## Decision

Keep `WorkspaceTab.zoomed` as the single logical authority. The visual
transition is a workspace-owned `Option<ZoomMotion>` (tab, pane, spring
position/velocity, target, last sample, slot, Ghost, viewport/scale).

Opening and closing flip the logical state in `set_zoom` before the next
frame. The layout then changes once per direction:

- opening: the split renders normally but the focused leaf is a placeholder;
  the only real `TerminalView` instance moves into the floating card;
- closing: the real instance returns to its split leaf immediately; only a
  Ghost image remains until the spring settles.

The floating card is laid out inside the terminal card content area
(`absolute inset_0` + `ZOOM_MARGIN` padding), so it inherits the card's
clipping and rounded corners. A backdrop dims the split and, while logically
zoomed, occludes pointer input. The terminal grid and PTY therefore resize
exactly once per open/close; animation frames only paint an image and a
backdrop.

The spring is a local semi-implicit Euler sampler with fixed 1/240 s substeps
(stiffness 170, damping 18). It supports mid-flight reversal because position
and velocity are kept; `cx.reduce_motion()` snaps to the target and creates no
Ghost.

The flying cue is a "veil": a rounded rect interpolated from the recorded split
slot to the card rect. When a Windows `PrintWindow` snapshot exists it is
painted inside that rect (scaled); when capture is unavailable (non-Windows,
failure, size mismatch) the veil is a plain card-colored rounded rect with a
1 px border, so "where it grew from / shrinks back to" never depends on the
screenshot. The real card only fades in over the last 18% of the spring
(`CARD_REVEAL = 0.82..1.0`); before that it stays invisible so the flight does
not read as a full-size card popping in beside a small image. A captured frame
is always used — the capture is synchronous, so discarding a slow frame would
only add latency *and* lose the transition. A 32 MiB crop cap still applies.

`RuntimeCommand::ZoomPane` and the header/context-menu buttons all converge on
`set_zoom`. The runtime snapshot still derives `zoomed_pane_id` from the same
boolean. Focus always stays on the same `Entity<TerminalView>`; opening defers
focus to it, closing leaves it focused in the split.

## Rejected alternatives

- **Upgrade GPUI to use upstream `SpringAnimation`.** The pin carries a long
  fork patch/regression checklist and a paired gpui-component fork. The new
  API still has no public paint transform, so the terminal-grid problem would
  remain, and upgrading would not remove the need for a custom Ghost.
- **Animate the real terminal bounds every frame.** It violates the grid/PTY
  commit contract, causes repeated ConPTY resizes and visible reflow, and
  makes input/focus behavior depend on animation state. The Ghost is a plain
  image and does not drive the grid.
- **Introduce `SplitLayout`/`PaneManager`/`SplitNode` as a parallel model.**
  `nebula_split::SplitTree<u64>` plus `WorkspaceTab` is the single split
  authority. The overlay only adds transient rendering state.
- **Double-click pane title to toggle zoom.** Double-click is rename
  (`pane_header.rs`), and changing it would break a stable interaction.
- **Bare `Esc` to restore.** It would conflict with terminal applications
  such as Vim and tmux.
- **Click a background pane to move the zoom target (v1).** The backdrop
  blocks pointer hits; restore first, then select another pane. Runtime
  `ZoomPane` can still move an already-open zoom to another pane, which snaps
  the card without a cross-pane Ghost flight.

## Consequences

- The logical `zoomed` field remains memory-only and is not serialized; a
  restored session never comes back zoomed.
- Legacy/direct `*zoomed = true/false` writes outside `set_zoom` are accepted:
  a direct close drops the transient motion, and a direct open renders a
  settled card with no Ghost.
- Tab removal clears the transient motion because its stored tab index would
  otherwise shift. Normal tab switching keeps the per-tab boolean, and the
  motion resolves its tab by pane id on each frame.
- The pane drag path is disabled while zoomed: the pointer geometry is the
  floating card, not the split slot.
- While zoomed, `pane_bounds` records the floating card rect for the focused
  pane because the card hosts the only visible instance. Direction navigation,
  tab-dock hit areas and pane-menu placement therefore keep using the on-screen
  geometry; the Ghost's return slot is stored separately in `ZoomMotion.slot`,
  captured before the logical flip.
- On non-Windows platforms, or when capture fails, the veil falls back to a
  plain card-colored rect: the growth/shrink still shows the split slot as the
  origin, only the live pixels are missing.
- The card fades in with `.opacity`, but a cached entity view may not be
  reused under a changing ancestor opacity: `ViewElement`'s cache key is
  bounds/content-mask/text-style only, while paint bakes the current
  `element_opacity` into the recorded primitives. The overlay therefore
  renders the pane through `terminal_surface_live` (uncached, repainted each
  frame) while `card_opacity < 1`, and only switches back to the cached
  `terminal_surface` at full opacity. Without this split, zooming in on an idle
  terminal left the card showing only its background until the terminal entity
  was notified (typing) and repainted.
- The floating card owns a second wallpaper layer because `card_content_bg`
  is intentionally translucent under wallpapers; without it the dimmed split
  would show through the card.

## Validation

- `zoom_overlay.rs` unit tests cover spring endpoints/settling, reversal,
  card inset, fractional-DPI interpolation and smoothstep bounds.
- GPUI tests cover single-pane no-op, one live terminal instance, focus
  preservation, grid growth/restore and output visibility of background panes.
- Manual: four-pane open/close, mid-flight reverse, Windows 100/125/150/200%
  DPI, window resize during flight (snap to final), non-Windows fallback.

## Supersedes

None. The old full-card zoom behavior had no decision note.

## Revisit when

- The pinned GPUI exposes a paint-time transform (so the real terminal could
  be visually scaled without changing layout), or a supported terminal path
  can resize without reflow/ConPTY churn.
- `PrintWindow` capture turns unreliable on large/high-DPI windows (the veil
  then flies as a plain card), or a non-Windows capture path with the same
  ownership/lifetime guarantees lands.