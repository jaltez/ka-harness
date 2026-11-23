# TUI UX audit and pass — 2026-11-23

Goal: every surface of the TUI properly sized, legible, and doing what a user expects when they click, type, or press a key.

## How the audit was done

Three lenses, so a claim could be checked against two independent observations.

1. **A frame harness, committed.** `crates/ka-term/src/tui_snapshots.rs` renders 214 scenes through the real `render` at 80x24, 120x40 and 200x50 (plus a 64x18 stress size), with exact colors. Each scene asserts the strings that make it usable. Regenerate with
   `KA_SNAP_DIR=/tmp/snaps cargo test -p ka-term --all-features --lib tui::snapshots -- --ignored`; assert with the same command without `--ignored`.
2. **The real binary on a pty.** `/tmp/ka-ux-rig/shoot.py` drives ka in a private tmux server, types, presses, clicks, and captures text and styled frames. Every capture run gets a throwaway HOME, `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and data dir, because in-app saves write the user's config regardless of `--ephemeral` (see the open findings).
3. **The real terminal.** Windows Terminal windows attached to the same sessions and captured with `wincap.ps1`, so the look is judged in the user's own font and color scheme.

Two traps in the method were found and fixed during the run, both worth remembering:
- This shell exports `NO_COLOR=1`; crossterm obeys it and strips every color, so early captures were monochrome. The rig launches ka with `NO_COLOR` removed and flags terminals RGB-capable.
- The first mouse encoder sent the release as `<64;x;ym`, which crossterm reads as a scroll, so release-acted clicks were dead. Fixed to a real press and release with 1-based cells.

## What is fixed in this pass

Each landed as its own commit with its test or its harness scene.

**Traps (a key did something other than the screen promised)**
- Unknown slash commands, and Enter on a partial one, no longer reach the model: the draft is refused with the nearest command, and Enter with the popup open runs the highlighted one (`/hel` used to send `/hel` while the box read `/help`).
- Escape clears a draft; previously it closed the popup, kept the text, and did nothing after that.
- Reverse search takes typed characters instead of typing them into the draft.
- Modified keys never act as their bare letter inside a modal (Ctrl+R no longer rewinds from the rewind menu; Ctrl+A no longer runs `git add -A`).
- A click outside a panel no longer throws away an api key, a settings edit, or a save path.
- The session picker refuses Enter on a filter that matches nothing instead of starting a fresh session.
- Ctrl+C inside a modal backs out of the modal; a second Ctrl+C during a turn quits; history keeps the half-typed draft; sending returns the view to the tail.
- `--ephemeral` no longer prints a resume hint for a session that was not saved.
- Prompt history holds prompts only.

**Sizing and layout**
- Modals size against the area they are drawn in. Before, rows were built for the whole frame and then clamped into a smaller box, which wrapped every 80-column dialog two columns too wide.
- Every list keeps its selection on screen (model, provider, session, tasks, rewind, memory), windows over rendered rows, and keeps a selected task's preview with it.
- Doc pagers (changelog, hooks, permissions, doctor, debug, task detail) open at the top, page from there to the last page, scroll by line and page, close only on Esc, and keep their `… ↑ / … ↓` markers inside the box.
- The permission ask shows the changed lines of the diff (never the file headers), and always its options: at 80x24, `1 allow`, `2 always` and `3 deny` are on screen even with a tall question and a long diff.
- The status line never cuts a segment; whole spans drop from the end of the left cluster, so the cost and the mode chip always survive.
- Plain mode clears its pane before painting a modal (a screen reader used to hear fragments of both), wraps rows, and says a verdict once.
- Commands echo as one dim line instead of a full prompt band.
- The snapshot harness itself is the durable part: 214 scenes, asserted on every run.

## Open findings

Every row has frames and a `file:line` at `/tmp/ka-ux-vis-*/findings.tsv`, `/tmp/ka-ux-beh-*/findings.tsv` and `.audit/tui-ux-findings.tsv` (all on the run's worktree). Grouped by what they need.

**Needs a decision (engine or persistence, deliberately untouched)**
- Settings and api-key saves rewrite the real `~/.config/ka/ka.toml`, `ka.schema.json` and `.env` under `--ignore-user-config --ephemeral` (M01). Ready patch: a `set_user_config_override`, mirroring `ka_strand::set_data_dir_override`, set by `--ephemeral` and `--ignore-user-config` in `crates/ka-harness/src/main.rs`.
- Tool-card detail is capped by the engine at 48 chars (bash) and 32 (paths), with no ellipsis (`crates/ka-engine/src/voice.rs:5442`), so a 120-column card shows the same text as an 80-column one. Ready patch: raise the cap, mark the cut, update `call_detail_summarizes_head_path_and_pattern`.
- A local command's permission ask carries a dollar estimate from `voice.rs:4388`.
- Provider junk reaches the transcript: `No reasoning available.` (a non-reasoning model's reasoning text) and `<|start|>`-style control tokens. Ready patch: two data fields per dialect in `dialects.toml`, applied at the wire.

**Remaining polish (no decision needed)**
- Header: shorten the title before dropping cwd and branch; show `~` for home (VC13, VC14).
- Welcome: drop the four rows that repeat the header, status line and placeholder; center the block (VC15, VC16).
- Hover lifts every card in a transcript entry instead of the one under the pointer (VC27); the close chip's hover zone includes the border corner (VC28).
- Streaming caret rides the panel's right edge, far from the text (VC25).
- Composer while an ask is pending: the placeholder says `esc to abort` while Esc denies, and the composer keeps a second amber frame (VC02, VC06).
- Hint rows: mixed chord notation (`alt+o` next to `^t`), verb drift (interrupt/abort), scroll hints on content that cannot scroll, and most hints lost at 80 columns (VC17, VC18, UX-012, UX-003).
- `/workspace`-class panels that still cannot scroll: usage, the settings provider list, and the skills popup (M11, M18, M09).
- Empty states: several open a full-size box or a selectable row for one line (UX-008, UX-017, panels item 9).
- Plain mode: the model picker ignores the vendor lock (VT-02, P1); selection marked only with `>` (VT-05); empty-state sentences dropped (VT-07); todo state on glyphs only (VT-08).
- Transcript: collapsed thinking can take nine of seventeen rows at 80x24 (VT-10); `/focus` digests cut prompts without an ellipsis (VT-11); failed verdict rows carry two ellipses (harness report).
- Wording: provider list uses a red cross for "not configured" (UX-014), raw Markdown in the changelog (UX-015), developer ids in the features panel (UX-016), "1 turns" (UX-021), raw git stderr (UX-020).
- A stray backtick in the tasks roster row (`job-1\``), from `ka-engine/src/engine.rs:2735`.

## Proposals (the larger ideas)

1. **One palette for everything.** Today ~30 panels are reached four ways: slash commands, chords (alt+o, ^t, alt+i), strip buttons, and title arrows, and `/todos`, `/skills`, `/info` (the names printed on screen) are not commands, so typing them sends a prompt to the model. One fuzzy palette (a superset of the slash popup) that lists commands, panels, sessions, models, modes and themes with their chords would make every feature discoverable and kill the whole unknown-command class.
2. **Approvals as the core loop, not a dialog.** The author's own survey calls approvals ka's weakest lane. This pass makes the ask show the change and keep its options; the next step is remembering a decision (tool + argument pattern, per project, with an expiry), batching several asks, and auto-allowing read-only hands in `ask` mode. The ask should also say what it is for: "read 3 files" not "allow read".
3. **Quiet by default for finished work.** A real session is Thinking / tool card / verdict, repeated. Empty reasoning should print nothing (a catalog data flag), consecutive calls of the same tool should fold into one card with a count, and a finished turn should collapse to its summary line unless expanded (`/focus` already defines the shape; make it the default for older turns).
4. **The status bar tells you what things mean.** The toggle count (`⊘1`) and the jobs pill (`2 tasks · 1 job · 1 dap`) are cryptic; capture mode already tracks hover, so a hovered badge can explain itself on the status line. One color per meaning: the mode chip is the safety indicator and should be the only amber thing there.
5. **Resume restores its totals.** A resumed session shows `$0.0000` although the strand carries usage records; replay them into the meters at load.
6. **Accessibility as a checked surface.** Plain mode is the screen-reader view and had a P1 (modals painted over the transcript). The harness now has 74 plain scenes; make "plain mode is linear and self-describing" a contract with the same weight as the decorated one.

## Reproducing the checks

```sh
# frame harness (committed)
KA_SNAP_DIR=/tmp/snaps cargo test -p ka-term --all-features --lib tui::snapshots -- --ignored
cargo test -p ka-term --all-features --lib tui::snapshots
# a live session on a private tmux server, with isolated HOME and data
python3 /tmp/ka-ux-rig/shoot.py --name demo --cols 80 --rows 24 --out /tmp/shots \
  --cwd /tmp/cwd -- /path/to/ka --ignore-user-config --ephemeral --offline -- \
  wait:ka sleep:1 "type:/help" key:Enter sleep:1 shot:help
# a true-pixel frame through Windows Terminal (needs WSL interop and one calibration)
python3 /tmp/ka-ux-rig/winshot.py --calibrate
python3 /tmp/ka-ux-rig/winshot.py --socket /tmp/ka-ux-sock-demo --session demo --out /tmp/shots/t.png --cols 80 --rows 24
```
