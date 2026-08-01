# cim — mouse & modifier commands

Everything on this page is driven by the **mouse**. The keyboard shortcuts are
listed (and rebindable) in *Settings → Keyboard shortcuts*, so they are not
repeated here.

This document is read from `help.md` next to the cim executable (or from the
working directory). Edit it and press **Reload** — no restart needed.

---

## On an image pane

- **Left-drag** — pan the image. Panning follows the pane's own view, or the
  shared one when the pane is spatially synced.
- **Left-click** — focus that pane (it becomes the target of the keyboard
  shortcuts, of Single view, and of the Transformations panel).
- **Ctrl + left-drag** — reorder panes in Grid view: drag one pane onto
  another's cell to swap it into that slot.
- **Alt + left-drag** — rotate the pane about the image centre, following the
  cursor's angle, snapped to whole degrees. Rotation follows the *Geometry*
  sync group, so synced panes turn together.
- **Wheel** — zoom about the cursor.
- **Shift + wheel** — zoom about the cursor, roughly twice as fast.
- **Ctrl + wheel** — scrub the sequence one frame at a time (up = next frame,
  down = previous). Same stepping as the next/previous-frame keys, so it obeys
  the loop window and holds at an undiscovered frontier.
- **Right-drag** — draw the **statistics region**. It is stored in image space,
  so the same rectangle and each pane's own stats appear on every pane; the
  panel shows a mini histogram plus mean / std / count. *Compute LUT from
  region* then pins every pane's tone to it.
- **Right-click** (or a near-zero right-drag) — clear the statistics region.
- **Shift + right-drag** — draw the **intensity-profile line**, which opens the
  Line profile window. Press near an endpoint to drag that end, near the middle
  to move the whole line, anywhere else to start a new one. Clearing the line
  closes the window.
- Hovering any pane marks the same source pixel on all the others with a red dot
  and shows each pane's native value at it in its footer. The dot can be turned
  off in *Settings → Cursor dot on other panes*.

## While selecting an export crop

With the Export panel's **Select…** active the buttons swap roles:

- **Right-drag** — draw the crop rectangle.
- **Left-drag** — pan, and the **wheel** zooms, so you can move to the region
  first. Reorder, click-to-focus and the statistics region are suspended until
  the selection ends.

## A/B view

- **Left-drag on the divider** — move the wipe split.
- Pan and zoom act on whichever side the cursor is over.

## Timeline scrubber (bottom bar)

- **Click / drag the track** — seek to that frame.
- **Drag a bracket** (`[` or `]`) — set the loop range; playback and *Use loop
  range* in the Export panel follow it.
- **Click the frame readout** — type a frame index and press Enter to jump
  straight there, even past the discovered end.

## Media manager

- **Drag the ⠿ handle** — reorder the media rows.

---

## Other things worth knowing

- **Ctrl+Shift+C** copies the *View command* — a `cim …` line that reopens the
  current files at the current view — from anywhere in the app.
- Hovering a button shows its keyboard shortcut, read live from your bindings.
- Settings are **saved automatically** a moment after you stop editing;
  *Reset to defaults* puts everything, shortcuts included, back.
