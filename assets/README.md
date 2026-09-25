# DD-GUI icon

**Concept "Platters":** a lowercase `dd` whose bowls are disk platters (hub, clamp
ring) and whose stems are the read/write arms lying over them, split off by a thin
cut. The first d is the source (white), the second is the copy (orange), which is
what `dd` does. It sits on a graphite rounded-square tile with macOS Big Sur corners.
Solid colors only, no gradients or glows.

`contact-sheet.png` shows it next to the two runner-up concepts (B "Sectors",
C "Cards") at 1024, 256, 64, 32 and 16 px on dark and light backgrounds.

## Palette

| Role | Color |
|---|---|
| Tile | `#1C1C1F` graphite, with a `#2E2E33` rim at large sizes |
| Source d | `#F2F2F3` (the app's text color) |
| Copy d | `#FF7A1A` signal orange (the app's accent) |

The app itself: dark bg `#0F0F10`, surface `#17171A`, raised `#202024`, border
`#26262B`, text `#F2F2F3`, muted `#A5A5AD`, accent `#FF7A1A` (pressed `#E86A0C`,
text on orange `#140A02`); light bg `#F5F5F3`, surface `#FFFFFF`, text `#111113`,
accent `#F46A0E`.

## Files

| File | What |
|---|---|
| `icon.svg` | Master, 1024 × 1024. Used for 128 px and up, as the scalable Linux icon, and as the window icon (`ui/app.slint`). Slint renders it with resvg, so it stays plain paths: no filters, no external references. |
| `icon-small.svg` | 32 px version drawn on the pixel grid (no platter detail). |
| `png/dd-gui-<n>.png` | 16, 24, 32, 48, 64, 128, 256, 512, 1024 px. |
| `dd-gui.ico` | Windows: 16–256 px (BMP up to 64, PNG for 128 and 256). Embedded in `dd-gui.exe` by `build.rs`. |
| `dd-gui.icns` | macOS: 16–1024 px, on Apple's icon grid (824 px tile in 1024). Goes into `DD-GUI.app`. |
| `../ui/logo.svg` | The in-app mark, drawn on a 30 px grid. |
| `contact-sheet.png` | The three concepts. |

File names are lowercase `dd-gui`, like the binary; "DD-GUI" is only the name people see.

The Linux binary carries `icon.svg` and the PNGs from 16 to 512 px itself
(`src/desktop.rs`): when it starts, it puts them in `~/.local/share/icons/hicolor` as
`dd-gui.png` / `dd-gui.svg` (if they differ), next to a `dd-gui.desktop` entry. So a
changed icon reaches users with the next build of the binary.

At 64 px and below each size is drawn separately, with every edge on a whole pixel.
The detail drops away as space runs out: 64 px keeps the clamp ring and the arm cut,
48 px the cut, and 32/24/16 px are bold pixel-snapped `dd`s whose hub holes double
as the letters' counters.

## Regenerating

Everything above except the contact sheet comes from one script. Edit the parameters
at the top (colors, `R`, `S`, …, and the `HINTED` table for the small sizes), then run:

```sh
python3 assets/make-icons.py   # needs rsvg-convert and ImageMagick 7 (`magick`)
```
