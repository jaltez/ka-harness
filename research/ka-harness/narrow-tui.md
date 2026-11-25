# Narrow terminals — 2026-11-25

The owner asked for `ka` to stay usable on a phone-sized terminal reached through a remote client (mosh and the like): "it gets stretched as expected but adds some cluttering and weird stacking; also colors and some consistency are lost". Two separate problems, each fixed on its own terms. Decision log: [`.audit/narrow-tui.tsv`](../../.audit/narrow-tui.tsv).

## What was wrong, measured

Captured through a real pty at phone sizes (`bench/pty_shot.py`).

| Size | What it showed |
|---|---|
| 46×14, portrait with the keyboard up | 7 of 14 rows were chrome. The `/` and `@` popups were boxes sitting directly on the composer box, so two fused boxes filled 11 rows. The model picker showed 2 rows, cut off. Hints were cut mid-word. The scrollbar column touched the text. |
| 40×12, smaller phone | The same, and worse: after one short turn the transcript showed fragments with the user's own message scrolled under the header. |
| 50×28, portrait without the keyboard | Reasonable. |
| 66×16, 88×20, landscape | Clean. |
| Every size, four terminal identities | `ka` wrote 24-bit colour codes unconditionally: 350 in its first frame, whether or not the client could show them. No code read what the terminal announced. |

## The bar

Sizes: 46×14 and 40×12 (phone with the keyboard up), 50×28 (portrait without it), 88×20 (landscape), plus the sizes the frame harness already asserts (80×24, 120×40, 200×50, and the stress size 64×18).

Usable on a phone means: with nothing open the transcript keeps three quarters of the rows; with a popup or modal open the input line and five list rows at least stay visible; no two bordered boxes touch; `ka`'s own hints drop whole rather than being cut; prose gets nine tenths of the columns after its indent. At 56 columns and 18 rows or more, every existing frame stays byte-identical.

Colours: at 256 colours no 24-bit code leaves the process, and text contrast stays within a few percent of 24-bit; at sixteen colours no accent becomes black, white or gray, and no tinted surface lands on a primary.

## The layout: three designs, one chosen

Mocked at 46×14 as text grids, because a terminal layout can be judged from its cells.

| Design | Transcript rows | What it costs |
|---|---|---|
| Today | 5 to 7 of 14 | |
| **A, slim chrome** | **11 of 14** | Each piece of chrome thinned; the skeleton and the mental model stay |
| B, one bar: header merged into the status line | 12 of 14 | Two intricate widgets made one; title and folder lost |
| C, reading first: no header unless typing | 13 of 14 | New state, and no scroll indicator while reading |

A was chosen: it clears the bar with the smallest structural change, and keeps the touch-scroll arrows and the session line where a desktop user knows them. B is the next step if one more row is ever wanted.

What A is, in code: `FrameMode` (two independent switches, `short` under 18 rows and `narrow` under 56 columns) decided once per frame; `frame_chunks` and `pane` shared by the decorated and the plain renderers, so their viewport arithmetic cannot drift; a bare composer line on a short frame; no transcript margins, inset or scrollbar column on a narrow one; overlays spanning the frame; a shorter placeholder; a smaller pinned todo panel.

## The colours

`color_depth` reads what the terminal announces once (`COLORTERM`, then `TERM`, with two gaps in crossterm 0.28's own rule closed: it ignores a `256color` TERM whenever COLORTERM is set to anything, and does not know `xterm-direct`), and the one place every colour passes through, `ThemeBackend::draw`, fits each colour to the nearest one that depth shows exactly, judged in OKLab against xterm's own 256-colour and sixteen-colour palettes. At sixteen colours a colour darker than OKLab lightness 0.40 becomes black: every surface and plain border in the palette sits under 0.35, the darkest colour that must stay one (the focus border) at 0.57.

Measured on a pty: a 256-colour terminal name gets zero 24-bit codes and only palette indices; plain `xterm` gets only the sixteen ANSI indices; a truecolour terminal is unchanged. At 256 colours 22 of the 30 palette colours stay distinct, every accent keeps its own entry, and text contrast is unchanged within a few percent (body text on the canvas 14.5 to 14.9; the widest loss is the error text on the error panel, 5.38 to 4.75, since the palette has no entry near that dark red tint).

## What is pinned by tests

- `compact_frames_keep_the_transcript_and_its_input`: renders the real frames at 46×14 and 40×12 and checks the bar directly (bare input line, nothing boxed while quiet, one popup box on the input line with five list rows at least, a paged overlay that says it continues), and that 80×24 keeps its box and air row.
- `changelog_end_reaches_the_oldest_entry` now runs at 46×14 and 40×12 too, with each size's own layout mode.
- The 215-scene frame harness still asserts 80×24, 120×40 and 200×50, and the full-size frames are byte-identical.

## Left for later

- The model picker still fits its columns to the width at phone sizes (`ModelCols::fit`), which squeezes the model id to a few characters. Dropping columns whole on a narrow frame is the same treatment the mode picker got and would read better.
- On a narrow frame the scrollbar column is gone and the header's scroll percent plus the jump pill carry the position. A touch user cannot scroll by dragging a rail.
- Design B (header merged into the status line) buys one more row.

## Reproduce

```sh
cargo build -p ka-harness
python3 bench/pty_shot.py --bin target/debug/ka --cols 46 --rows 14 --out shots --name phone \
    --cwd /tmp --ka-args "--ignore-user-config --ephemeral --offline --no-discovery" -- \
    wait:ka sleep:1 shot:start "type:hello" key:Enter sleep:3 shot:done "type:/help" key:Enter sleep:1 shot:help
# and the depths: --term xterm-256color --colorterm none, or --term xterm
```
