# cim — mouse & modifier commands

## On an image pane

- **Left-drag** — pan the image. Panning follows the pane's own view, or the
  shared one when the pane is spatially synced.
- **Left-click** — focus that pane (it becomes the target of the keyboard
  shortcuts, of Single view, and of the Transformations panel).
- **Ctrl + left-drag** — reorder panes in Grid view: drag one pane onto
  another's cell to swap it into that slot.
- **Alt + left-drag** — rotate the pane about the image centre, following the
  cursor's angle, snapped to whole degrees.
- **Wheel** — zoom about the cursor.
- **Shift + wheel** — zoom about the cursor, roughly twice as fast.
- **Ctrl + wheel** — scrub the sequence one frame at a time (up = next frame,
  down = previous). Same stepping as the next/previous-frame keys, so it obeys
  the loop window and holds at an undiscovered frontier.
- **Right-drag** — draw the **statistics region**. It is stored in image space,
  so the same rectangle and each pane's own stats appear on every pane; the
  panel shows a mini histogram plus mean / std / count. _Compute LUT from
  region_ then pins every pane's tone to it.
- **Right-click** (or a near-zero right-drag) — clear the statistics region.
- **Shift + right-drag** — draw the **intensity-profile line**, which opens the
  Line profile window. Press near an endpoint to drag that end, near the middle
  to move the whole line, anywhere else to start a new one. Clearing the line
  closes the window.

## Timeline scrubber (bottom bar)

- **Click / drag the track** — seek to that frame.
- **Drag a bracket** (`[` or `]`) — set the loop range; playback and _Use loop
  range_ in the Export panel follow it.

## Media manager

- **Drag the ⠿ handle** — reorder the media rows.

---

## Other things worth knowing

- **Ctrl+Shift+C** copies the _View command_ — a `cim …` line that reopens the
  current files at the current view — from anywhere in the app.
- Hovering a button shows its keyboard shortcut, read live from your bindings.
- Settings are **saved automatically** a moment after you stop editing;
  _Reset to defaults_ puts everything, shortcuts included, back.
