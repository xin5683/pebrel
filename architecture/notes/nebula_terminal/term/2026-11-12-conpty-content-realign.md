# ConPTY realign follows viewport text, not the cursor row alone

## Status

Accepted. Fixes the 2026-11-12 zoom / Claude Code input misalignment.

## Context

Direct pass-through ConPTY emits zero repaint bytes on `ResizePseudoConsole`;
conhost rewraps its own buffer internally with path-dependent wrapping rules.
After a resize the local grid (reflowed by `grid/resize.rs`) and conhost's
buffer can disagree by a few rows.

The original `Term::conpty_realign(target)` reconciles by asking conhost for its
cursor row and scrolling the whole local grid by the row difference. That
assumption holds for shells where nothing is painted below the caret. Claude
Code's TUI paints below the caret: the input-box border and footer rows sit
under the insertion point, and the renderer is incremental. When the local grid
is a full-screen scroll off, the cursor row can coincidentally equal conhost's
while the content is N rows away; a cursor-only realign then scrolls the whole
frame away from conhost's picture. The app's next absolute CUP writes land at
conhost's rows, so typed echo appears below the box (the 2026-11-12 incident,
surfaced by the new floating pane zoom, which resizes the pane once per
direction).

## Evidence

- Probe: `event_loop.rs` attaches to the ConPTY host console
  (`AttachConsole` + `GetConsoleScreenBufferInfo`) and reads the bottom
  viewport rows as text via `ReadConsoleOutputCharacterW` on `CONOUT$`.
- Paired dump of the incident pane (debug build 2026-11-12 14:42, both sides
  101x39): local row 31 separator, row 32 input, row 33 separator, row 34
  warning, row 35 auto mode, cursor `(100, 35)`; conhost row 34 separator, row
  35 input, row 36 separator, row 37 warning, row 38 auto mode, cursor row 35.
  Local text equaled conhost text shifted up 3; cursor rows were equal; three
  blank rows sat at the bottom. That is the signature of a cursor-row-only
  scroll.
- `ScreenShot_2026-11-12_144816_203.png`: typed characters echo below the input
  box border with the caret block, matching the displaced-grid explanation.

## Decision

Add `term/conpty_align.rs`:

- `ConhostViewportText` is the probe snapshot: viewport height, cursor row,
  first sampled row, and bottom-anchored line text. `new` rejects samples whose
  rows do not add up, so row numbers are always usable across coordinate
  systems.
- `Term::conpty_realign_to` normalizes both sides (whitespace runs collapsed,
  blank rows become `None`) and scores every whole-row shift in
  `-(screen-1)..=(screen-1)`, comparing conhost rows against local rows.
- A shift is accepted only when it is the unique best match with at least
  `MIN_SHIFT_MATCHES = 3` matching rows and `MIN_SHIFT_CHARS = 12` matched
  characters. The grid then scrolls by that shift (down when conhost content
  is lower, up when higher, top rows into history) and the caret adopts
  conhost's cursor row; a moved caret clears pending-wrap.
- Shift zero leaves the content untouched and only adopts the cursor row —
  the incident case, where the content was already right and only the caret
  bookkeeping needed conhost's value.
- No confident match returns `false` and changes nothing. When the text probe
  is unavailable (read failure, viewport height not landed, non-Windows), the
  event loop falls back to the cursor-row `conpty_realign`, which moves into the
  same module as the documented fallback so both strategies stay together.

## Rejected alternatives

- Keep cursor-row-only reconciliation: it cannot distinguish "content aligned"
  from "content off by N with a coincidentally matching caret", which is
  exactly the incremental-TUI failure above.
- Apply the best-scoring shift even when several shifts tie: one wrong
  whole-screen scroll is permanent, while doing nothing only preserves the
  status quo until the next probe.
- Align only the rows around the caret: an incremental renderer rewrites rows
  by absolute coordinates, so the whole frame must agree, not just the band
  near the input box.
- Read the entire screen buffer instead of the viewport tail: history rows are
  not part of what the app is currently addressing, and reading them inflates
  the attach window.

## Consequences

- Alignment quality now depends on visible text. Repeated identical rows can
  make the shift ambiguous; the grid is then left unchanged rather than rolled
  on a guess. Shells whose frames are distinguishable (prompt lines, flags,
  separators) realign as before, now anchored on content.
- Wide-char continuation cells are normalized to a single space on both sides,
  so double-width text compares equal without a width model.
- The probe performs up to `ALIGN_TAIL_ROWS = 12` extra reads inside the
  already-serialized attach window, only on resize and the deferred align
  deadline, never on the PTY read path.
- This is a compatibility heuristic against conhost's rendering, not a VT
  contract; it must stay opt-out safe for alternate-screen apps.

## Validation

- `term/tests.rs`: content shift under a cursor that already matches (incident
  shape), cursor adoption with aligned content, unrecognized content leaves the
  grid untouched, ambiguous periodic sample leaves the grid untouched, snapshot
  validation.
- `cargo test -p nebula_terminal --lib`: 398 passed, 2 ignored. Integration
  suites `inline_image`, `osc7501`, `pi_redraw_anchor`, `connection_transport`
  pass. `windows_pty_lifecycle` cannot run in this checkout because
  `assets/windows/conhost` fixtures are absent (pre-existing).
- Manual acceptance remains the user-facing check: zoom a pane running Claude
  Code and type; the echo must stay inside the input box.

## Supersedes

None. Extends the cursor-row realign rationale in `Term::conpty_realign`
(2026-08 split / PSReadLine drift); that path remains the fallback when text is
unavailable.

## Revisit when

- A TUI is observed whose visible rows are too ambiguous for the text path.
- Conhost changes `ReadConsoleOutputCharacterW` wide-character behavior.
- A non-Windows PTY backend needs the same reconciliation and the probe snapshot
  has to be generalized.