//! Snapshot scenes for [`super::render`]: every surface of the TUI at
//! real terminal sizes, drawn by the same call `app()` makes.
//!
//! One table ([`scenes`]) drives two tests:
//!
//! * `snapshots_dump` (`--ignored`) renders every scene at every size
//!   and writes, under `$KA_SNAP_DIR` (else `$TMPDIR/ka-tui-snapshots`),
//!   `<id>@<w>x<h>.txt` (one row per line, trailing spaces trimmed),
//!   `<id>@<w>x<h>.ansi` (the same rows with SGR escapes: truecolor
//!   foreground and background, bold, dim, italic, underline, reverse,
//!   one escape per run of equal style), and `index.tsv` (id, name,
//!   category, size, the `must_contain` strings found, the ones
//!   missing).
//! * `snapshots_stay_usable` (always on) renders every scene at the
//!   asserted sizes and fails naming the scene, the size, and each
//!   missing `must_contain` string.
//!
//! `must_contain` is a characterization: every string is on the frame
//! today at every asserted size, and the test fails when one leaves.
//!
//! An overlay scene (`ask`, `modal`) also fails on a bare selection
//! band ([`bare_selection_bands`]): a row whose selection ground shows
//! no glyph.
//!
//! A scene fixes what would otherwise move between runs:
//!
//! * the clock: `render`'s `now` is [`now`], frozen for the process,
//!   and scenes anchor `busy_since` and a running call's start to it;
//! * the `@`-mention walk: `app()` caches it from the checkout, scenes
//!   seed that cache with [`mention_entries`];
//! * session ages: `rel_age` reads the wall clock, so [`ago`] builds
//!   timestamps relative to it;
//! * the branch in the staging panel's header: `render` shells out to
//!   `git` under `PLAN_CWD`, which [`git_fixture`] points at a
//!   throwaway repository on `main`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use ka_protocol::{ContextPart, Effort, McpSummary, Mode, TodoItem, TodoState};
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthStr;

use super::*;

// ── sizes, clock, directory ──────────────────────────────────────

/// The sizes every scene must stay usable at.
const SIZES: [(u16, u16); 3] = [(80, 24), (120, 40), (200, 50)];

/// The stress size: dumped with the rest, never asserted.
const STRESS: (u16, u16) = (64, 18);

/// `render`'s `now`, frozen for the process. Scenes anchor `busy_since`
/// and a running call's start against it, so elapsed text and spinner
/// frames never move between renders or runs.
fn now() -> Instant {
    static NOW: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);
    *NOW
}

/// Where the dump writes: `$KA_SNAP_DIR`, else `$TMPDIR/ka-tui-snapshots`.
fn snap_dir() -> PathBuf {
    std::env::var_os("KA_SNAP_DIR")
        .filter(|dir| !dir.is_empty())
        .map_or_else(
            || std::env::temp_dir().join("ka-tui-snapshots"),
            PathBuf::from,
        )
}

// ── the render call ──────────────────────────────────────────────

/// The state `app()` holds when it calls `render`, under the loop's own
/// names. A scene builds one per size; [`draw`] hands it to `render`
/// argument for argument.
struct RenderInput {
    scroll: Option<usize>,
    transcript: Transcript,
    input: InputBuffer,
    busy: bool,
    busy_since: Option<Instant>,
    queue: Vec<String>,
    pending: Option<PendingAsk>,
    current_assistant: String,
    current_thought: String,
    current_tool: String,
    live_tool: Option<LiveTool>,
    slash_popup: Option<SlashPopup>,
    path_popup: Option<PathPopup>,
    modal: Option<Modal>,
    picker: Option<InputPicker>,
    meters: Meters,
    sidebar: SidebarState,
    fresh: bool,
    mouse_captured: bool,
    header_glyph: &'static str,
    toast: Option<(String, Instant)>,
    sel: Option<((usize, u16), (usize, u16))>,
    hover: Option<Hover>,
    status_order: Option<Vec<String>>,
    plain_mode: bool,
    mention_cache: Option<(Instant, Vec<(String, bool)>)>,
}

/// The hit-test cells `app()` allocates once and `render` refills every
/// frame.
#[derive(Default)]
struct Zones {
    strip: std::cell::Cell<Option<StripZones>>,
    arrows: std::cell::Cell<Option<TitleArrows>>,
    modal: std::cell::Cell<Option<Rect>>,
    popup: std::cell::Cell<Option<Rect>>,
    tx_content: std::cell::Cell<Option<(Rect, usize)>>,
    rail: std::cell::Cell<Option<RailZone>>,
    tx_width: std::cell::Cell<u16>,
}

/// One frame of `state` at `term_w × term_h`: the body of `app()`'s
/// `if draw_now { … terminal.draw(…) }` arm, argument for argument, on
/// a `TestBackend` the size of the terminal. A scene draws a single
/// frame per size, so `tx_width` starts at 0 and the live markdown
/// cache at `None`, exactly as on the loop's first frame.
fn draw(state: &mut RenderInput, (term_w, term_h): (u16, u16)) -> Buffer {
    let zones = Zones::default();
    let Zones {
        strip,
        arrows,
        modal,
        popup,
        tx_content,
        rail,
        tx_width,
    } = &zones;
    let input_snapshot = state.input.text.clone();
    let rsearch = state.input.search_title();
    let last_w = tx_width.get();
    let md_width = assistant_md_width(if last_w == 0 {
        transcript_width(term_w)
    } else {
        last_w
    });
    let live = if state.busy {
        let live_cache = render_live_md(None, &state.current_assistant, md_width);
        Some(LiveBlock {
            thought: state.current_thought.clone(),
            tool_header: state.current_tool.clone(),
            live_tool: state.live_tool.clone(),
            md: live_cache.rows,
        })
    } else {
        None
    };
    let cursor = state.input.cursor;
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(term_w, term_h))
        .expect("a TestBackend always has a size");
    terminal
        .draw(|frame| {
            render(
                frame,
                &mut state.transcript,
                state.scroll,
                &input_snapshot,
                cursor,
                state.busy,
                state.busy_since,
                now(),
                state.queue.len(),
                state.pending.as_ref(),
                live.as_ref(),
                state.slash_popup.as_ref(),
                state.path_popup.as_ref(),
                rsearch.as_deref(),
                state.modal.as_ref(),
                state.picker.as_ref(),
                &state.meters,
                &state.sidebar,
                state.fresh,
                state.mouse_captured,
                state.header_glyph,
                strip,
                state
                    .toast
                    .as_ref()
                    .filter(|(_, at)| at.elapsed() < TOAST_TTL)
                    .map(|(m, _)| m.as_str()),
                arrows,
                modal,
                popup,
                tx_content,
                rail,
                tx_width,
                state.sel,
                state.hover,
                state.status_order.as_deref(),
                state.plain_mode,
                state.input.vim.label(),
                ghost_suggestion(&input_snapshot, cursor, &mut state.mention_cache)
                    .map(|g| g.suffix(&input_snapshot)),
            );
        })
        .expect("a TestBackend draw never fails")
        .buffer
        .clone()
}

// ── frame files ──────────────────────────────────────────────────

/// Row `y` as the cells a terminal shows: each cell's symbol, less the
/// cells a wide glyph already covers.
fn row_cells(buf: &Buffer, y: u16) -> Vec<&Cell> {
    let mut cells = Vec::new();
    let mut cols = 0usize;
    for x in 0..buf.area.width {
        let cell = &buf[(x, y)];
        if cols <= usize::from(x) {
            cells.push(cell);
            cols += cell.symbol().width();
        }
    }
    cells
}

/// Row `y` as text, trailing spaces trimmed.
fn row_text(buf: &Buffer, y: u16) -> String {
    let row: String = row_cells(buf, y).iter().map(|cell| cell.symbol()).collect();
    row.trim_end_matches(' ').to_owned()
}

/// The rows of an overlay that wear the selection ground but show no
/// glyph: the remainder of a row laid out wider than its box, blank in
/// a text dump and a stray dark cell on screen. A selected row always
/// carries its pointer or its label, so a band of bare cells is never
/// meant.
fn bare_selection_bands(buf: &Buffer) -> Vec<String> {
    (0..buf.area.height)
        .filter(|&y| {
            let mut band = (0..buf.area.width)
                .map(|x| &buf[(x, y)])
                .filter(|cell| cell.bg == crate::palette::SEL_BG);
            band.next()
                .is_some_and(|_| band.all(|cell| cell.symbol() == " "))
        })
        .map(|y| format!("a selection band with no glyph on row {y}"))
        .collect()
}

/// The whole frame as text, one row per line.
fn frame_text(buf: &Buffer) -> String {
    (0..buf.area.height)
        .map(|y| row_text(buf, y))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Row `y` as text with SGR escapes. A run of same-styled cells carries
/// one `\e[0;…m`, so every run stands on its own. Trailing cells stop
/// at the last one that is not bare canvas: where the `.txt` row loses
/// its trailing spaces, bands and bars keep their true width here.
fn row_ansi(buf: &Buffer, y: u16) -> String {
    let cells = row_cells(buf, y);
    let end = cells
        .iter()
        .rposition(|cell| !is_air(cell))
        .map_or(0, |last| last + 1);
    let mut out = String::new();
    let mut run: Vec<&Cell> = Vec::new();
    for cell in &cells[..end] {
        if run.first().is_some_and(|head| head.style() != cell.style()) {
            paint(&mut out, &run);
            run.clear();
        }
        run.push(cell);
    }
    paint(&mut out, &run);
    out.push_str("\x1b[0m\n");
    out
}

/// The whole frame with SGR escapes, one row per line.
fn frame_ansi(buf: &Buffer) -> String {
    (0..buf.area.height).map(|y| row_ansi(buf, y)).collect()
}

fn paint(out: &mut String, run: &[&Cell]) {
    if let Some(head) = run.first() {
        out.push_str(&sgr(head.style()));
        out.extend(run.iter().map(|cell| cell.symbol()));
    }
}

/// A space on the bare canvas: nothing a `cat` would show.
fn is_air(cell: &Cell) -> bool {
    cell.symbol() == " "
        && Style {
            underline_color: None,
            ..cell.style()
        } == crate::palette::CANVAS
}

/// `\e[0;…m` for one style: the reset first, then its modifiers and colors.
fn sgr(style: Style) -> String {
    let mut params = vec!["0".to_owned()];
    for (flag, code) in [
        (Modifier::BOLD, 1),
        (Modifier::DIM, 2),
        (Modifier::ITALIC, 3),
        (Modifier::UNDERLINED, 4),
        (Modifier::SLOW_BLINK, 5),
        (Modifier::RAPID_BLINK, 6),
        (Modifier::REVERSED, 7),
        (Modifier::HIDDEN, 8),
        (Modifier::CROSSED_OUT, 9),
    ] {
        if style.add_modifier.contains(flag) {
            params.push(code.to_string());
        }
    }
    // `Reset` is the default the leading 0 already restored
    for (kind, color) in [(38, style.fg), (48, style.bg), (58, style.underline_color)] {
        if let Some(color) = color.filter(|color| *color != Color::Reset) {
            params.push(color_sgr(kind, color));
        }
    }
    format!("\x1b[{}m", params.join(";"))
}

/// The SGR parameters for `color` as foreground (`kind` 38),
/// background (48), or underline color (58).
fn color_sgr(kind: u8, color: Color) -> String {
    let named = match color {
        Color::Reset => return (kind + 1).to_string(),
        Color::Black => 30,
        Color::Red => 31,
        Color::Green => 32,
        Color::Yellow => 33,
        Color::Blue => 34,
        Color::Magenta => 35,
        Color::Cyan => 36,
        Color::Gray => 37,
        Color::DarkGray => 90,
        Color::LightRed => 91,
        Color::LightGreen => 92,
        Color::LightYellow => 93,
        Color::LightBlue => 94,
        Color::LightMagenta => 95,
        Color::LightCyan => 96,
        Color::White => 97,
        Color::Rgb(r, g, b) => return format!("{kind};2;{r};{g};{b}"),
        Color::Indexed(n) => return format!("{kind};5;{n}"),
    };
    (named + kind - 30).to_string()
}

/// A file body without a tab or a line break inside a field.
fn tsv(field: &str) -> String {
    field.replace(['\t', '\n', '\r'], " ")
}

/// Drops the files of an earlier dump so the directory mirrors the
/// table. Only `<id>@<w>x<h>.txt`, `<id>@<w>x<h>.ansi`, and `index.tsv`
/// are ever touched.
fn clear(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let stale = name == "index.tsv"
            || (name.contains('@') && (name.ends_with(".txt") || name.ends_with(".ansi")));
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn write(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

// ── fixtures: what a live session holds ──────────────────────────

/// A strand id in the store's format: `short_session` shows the eight
/// characters after the dash.
const SESSION: &str = "s1937f0a2c4e-7d1e3b5a9c2f40";

const CONFIG_PATH: &str = "/home/dev/.config/ka/ka.toml";

/// Meters after the engine's bootstrap events, before the first turn.
fn bootstrap_meters() -> Meters {
    Meters {
        model: "zai/glm-5.3".into(),
        mode: mode_label(Mode::Allow).into(),
        mode_kind: Mode::Allow,
        mode_icon: crate::icons::mode_icon(Mode::Allow).into(),
        effort: "high".into(),
        effort_supported: true,
        ..Meters::default()
    }
}

/// Meters three turns into a session.
fn live_meters() -> Meters {
    Meters {
        session: SESSION.into(),
        context: (61_440, 1_000_000),
        cost: 0.1842,
        cache_hit: Some(0.71),
        turns: 3,
        tokens_in: 148_300,
        tokens_out: 9_410,
        cache_read: 105_200,
        elapsed: 96.4,
        ..bootstrap_meters()
    }
}

fn ids(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

fn live_sidebar() -> SidebarState {
    SidebarState {
        inventory: Inventory {
            tools: ids(&[
                "bash", "read", "write", "edit", "grep", "glob", "web", "delegate", "todo",
                "tasks", "debug", "remember",
            ]),
            mcp: vec![
                McpSummary {
                    name: "github".into(),
                    ok: true,
                    tools: 12,
                },
                McpSummary {
                    name: "linear".into(),
                    ok: true,
                    tools: 8,
                },
                McpSummary {
                    name: "slack".into(),
                    ok: false,
                    tools: 0,
                },
            ],
            agents: ids(&["scout", "planner", "reviewer"]),
            skills: ids(&[
                "rust-docs",
                "tdd",
                "commit",
                "pr-review",
                "release",
                "debug-loop",
                "snapshot",
                "migration",
                "api-review",
                "perf-trace",
            ]),
            prompts: ids(&[
                "github/review-pr (owner, repo, number)",
                "github/issue (number)",
                "linear/issue (id)",
                "linear/create-issue (title, description)",
                "slack/post (channel, text)",
                "slack/threads (channel)",
                "notion/search (query)",
                "notion/create-page (title)",
            ]),
            disabled: ids(&["web"]),
        },
        known_tools: ids(&[
            "bash", "read", "write", "edit", "grep", "glob", "web", "delegate", "todo", "tasks",
            "debug", "remember",
        ]),
        sandbox: Some("fs".into()),
        todos: todos(),
        cwd: "…/projects/ka-harness".into(),
        branch: Some("main".into()),
        title: None,
    }
}

fn todos() -> Vec<TodoItem> {
    let item = |text: &str, done: bool| TodoItem {
        text: text.into(),
        state: if done {
            TodoState::Done
        } else {
            TodoState::Pending
        },
        confidence: Some(80),
    };
    [
        ("Map every Modal variant to a scene", true),
        ("Cover each hover target", true),
        ("Write the ANSI encoder", true),
        ("Fix the session picker clipping", false),
        ("Reserve the jobs pill slot", false),
        ("Add the update badge scene", false),
        ("Anchor busy_since at now − 37s", false),
        ("Seed the mention cache", false),
        ("Regenerate the baseline", false),
        ("Diff the three sizes", false),
        ("Report the missing strings", false),
        ("Land the harness commit", false),
    ]
    .into_iter()
    .map(|(text, done)| item(text, done))
    .collect()
}

/// The `@`-mention walk `app()` caches from the checkout, as a fixed
/// listing (directory, file) in the walk's sorted order.
fn mention_entries() -> Vec<(String, bool)> {
    [
        ("Cargo.lock", false),
        ("Cargo.toml", false),
        ("crates", true),
        ("crates/ka-term", true),
        ("crates/ka-term/Cargo.toml", false),
        ("crates/ka-term/src", true),
        ("crates/ka-term/src/git.rs", false),
        ("crates/ka-term/src/icons.rs", false),
        ("crates/ka-term/src/keys.rs", false),
        ("crates/ka-term/src/markdown.rs", false),
        ("crates/ka-term/src/palette.rs", false),
        ("crates/ka-term/src/panels.rs", false),
        ("crates/ka-term/src/plain.rs", false),
        ("crates/ka-term/src/screen.rs", false),
        ("crates/ka-term/src/theme.rs", false),
        ("crates/ka-term/src/tui.rs", false),
        ("crates/ka-term/src/tui_snapshots.rs", false),
        ("crates/ka-term/src/vim.rs", false),
        ("crates/ka-term/tests", true),
        ("crates/ka-term/tests/feature_contracts.rs", false),
        ("docs", true),
        ("docs/snapshots.md", false),
    ]
    .into_iter()
    .map(|(rel, is_dir)| (rel.to_owned(), is_dir))
    .collect()
}

fn call(tool: &str, detail: &str, ok: bool, note: &str) -> ToolCall {
    ToolCall {
        head: tool_header(tool, detail),
        ok,
        note: note.into(),
        excerpt: String::new(),
        spill: None,
        dur: Some(0.4),
        expanded: false,
    }
}

/// The `Stop::Done` verdict row.
fn done(dur: &str, input: &str, output: &str, cost: &str) -> Line {
    Line::Summary {
        glyph: '✓',
        tone: SummaryTone::Ok,
        text: format!("done · {dur} · {input} in · {output} out · 8.9k cache · {cost}"),
        detail: None,
        expanded: false,
    }
}

/// `lines` as a transcript, cached the way `apply_event` caches them.
fn transcript(lines: Vec<Line>) -> Transcript {
    let mut t = Transcript::default();
    for line in lines {
        match line {
            Line::ToolBlock(calls) => {
                for call in calls {
                    t.push_tool_call(call);
                }
            }
            line => t.push_separated(line),
        }
    }
    t
}

/// Two finished turns.
fn session() -> Transcript {
    transcript(vec![
        Line::User("Why does the status line jump when the jobs pill appears?".into()),
        Line::Assistant(
            "The pill joins the left cluster in `status_left`, so every segment after it shifts. \
             The right cluster keeps its width and the pad between the two shrinks first."
                .into(),
        ),
        Line::ToolBlock(vec![call(
            "read",
            "crates/ka-term/src/tui.rs",
            true,
            "26637 lines",
        )]),
        Line::Assistant(
            "Reserve the slot: the pill renders whenever the roster is non-empty, even with \
             nothing running."
                .into(),
        ),
        done("8.4s", "12.3k", "3.1k", "$0.0214"),
        Line::User("Patch it, then add a snapshot scene for the pill.".into()),
        Line::Assistant(
            "Done — `status-jobs-pill` renders the pill at every asserted size.".into(),
        ),
        done("31.7s", "24.8k", "1.9k", "$0.0521"),
    ])
}

/// The session's prompts, oldest first.
const PROMPTS: [&str; 12] = [
    "Why does the status line jump when the jobs pill appears?",
    "Move the jobs pill after the badges.",
    "Add a snapshot scene for the update badge.",
    "Why is the update badge clipped at 80 columns?",
    "Reserve the badge slots and add the scene.",
    "Run the snapshot assertions at all three sizes.",
    "Add a /focus digest row.",
    "Draft the release notes for 0.9.0",
    "Review the staged panel with a long diff",
    "Explain the transcript cache",
    "Add a scene for the ask dialog",
    "Ship the harness commit",
];

/// Six finished turns, each with its tool calls: enough rows to scroll
/// at every size.
fn long_session() -> Transcript {
    let mut lines = Vec::new();
    for (turn, (prompt, &tool)) in PROMPTS
        .iter()
        .zip(["read", "edit", "bash"].iter().cycle())
        .enumerate()
    {
        lines.push(Line::User((*prompt).into()));
        lines.push(Line::ToolBlock(vec![
            call(
                tool,
                "crates/ka-term/src/tui.rs",
                true,
                &format!("turn {}: applied 2 replacements", turn + 1),
            ),
            call(
                "bash",
                "cargo test -p ka-term --lib",
                true,
                "test result: ok. 748 passed; 0 failed",
            ),
        ]));
        lines.push(Line::Assistant(format!(
            "Turn {} settled: the {} call landed and the suite stays green.",
            turn + 1,
            tool
        )));
        lines.push(done("12.9s", "18.4k", "2.7k", "$0.0310"));
    }
    transcript(lines)
}

const THOUGHT: &str = "The user wants the status line to stop jumping. Three sources of movement: the \
jobs pill, the update badge, and the goal badge. All three append to the left cluster.\nLooking at \
status_left: segments push in a fixed order, so a new segment near the front shifts everything \
after it.\nThe right cluster ends with the mode pill, which is stable. Only the pad absorbs \
width.\nSimplest fix: give the pill a reserved slot whenever the roster is non-empty.\nCheck the \
status_segments tests before changing the order.";

const RICH_MD: &str = "## Findings\n\nThe pill is **appended** to the left cluster, so the *effort* \
segment and every badge after it shift right by its width:\n\n- `job_pill` returns `2 tasks · 1 \
job · 1 dap` once anything runs\n- the ~~pad~~ between the clusters absorbs the first twenty \
columns, then the right cluster clips\n- see [status_left](https://github.com/jaltez/ka-harness)\
/blob/main/crates/ka-term/src/tui.rs)\n\n| segment | width | moves |\n|---|---|---|\n| model | \
11 | no |\n| effort | 9 | no |\n| jobs | 19 | yes |\n\n```rust\nlet pad = (status.width as \
usize).saturating_sub(lw + rw + 2).max(1);\n```\n\n> The rightmost cell is the state the user \
acts from.";

const FAIL_MSG: &str = "provider: 529 overloaded — the model is over capacity right now; \
Retry-After: 30s; request id req_7d1e3b5a9c2f4011aa33";

/// Every [`Line`] kind in turn order. Entry indices matter: scenes
/// anchor the viewport on them.
fn all_kinds() -> Vec<Line> {
    let short: String = FAIL_MSG.chars().take(89).chain(['…']).collect();
    vec![
        Line::Info("⋯ digest ⋯".into()),
        Line::User("Add a snapshot harness for the TUI: one table of scenes, two tests.".into()),
        Line::Thought(THOUGHT.into()),
        Line::ToolBlock(vec![
            ToolCall {
                head: tool_header("read", "crates/ka-term/src/tui.rs"),
                ok: true,
                note: "26637 lines".into(),
                excerpt: "pub enum Line {\n    User(String),\n    Assistant(String),\n    \
                          Thought(String),\n    ToolBlock(Vec<ToolCall>),"
                    .into(),
                spill: None,
                dur: Some(0.2),
                expanded: false,
            },
            call(
                "edit",
                "crates/ka-term/src/tui.rs",
                true,
                "applied 1 replacement",
            ),
            ToolCall {
                head: tool_header("bash", "cargo test -p ka-term --lib"),
                ok: false,
                note: "(exit 101)\nerror: could not compile `ka-term`".into(),
                excerpt: "error[E0308]: mismatched types\n  --> crates/ka-term/src/tui.rs:5463:13"
                    .into(),
                spill: None,
                dur: Some(41.9),
                expanded: false,
            },
            ToolCall {
                head: tool_header("bash", "cargo clippy --workspace --all-targets"),
                ok: true,
                note: "warning: unused variable: `w`".into(),
                excerpt: "warning: unused variable: `w`\n  --> crates/ka-term/src/tui.rs:5463:13"
                    .into(),
                spill: Some("spill://7d1e3b5a9c2f40".into()),
                dur: Some(63.5),
                expanded: false,
            },
        ]),
        Line::Assistant(RICH_MD.into()),
        Line::Info("• agents: scout, planner, reviewer".into()),
        Line::Info("• mcp: github ✓ 12 · linear ✓ 8 · slack ✗".into()),
        Line::Warn(
            "config: [tui] status_line: unknown segment(s) \"costt\" (expected one of: model, \
             mode, session, effort, context, cost, tokens, cache, toggles, update, jobs)"
                .into(),
        ),
        Line::Err("provider: 429 rate limited — retry 1 of 3 in 20s".into()),
        Line::Shell("total 48".into()),
        Line::Shell("drwxr-xr-x 4 dev dev 4096 Nov 23 10:02 crates".into()),
        Line::Info("» ls — output joins your next prompt".into()),
        Line::Report("─ turn 1 of 3".into()),
        done("2m:04", "61.2k", "4.8k", "$0.1120"),
        Line::User("The pill still shifts the effort segment. Why?".into()),
        Line::Assistant(
            "Because `job_pill` sits before the effort badge in `status_left`. Move it after the \
             badges or reserve its slot."
                .into(),
        ),
        Line::Summary {
            glyph: '◐',
            tone: SummaryTone::Warn,
            text: "aborted · partial kept · 48.3s · 19.7k in · 2.2k out · $0.0388".into(),
            detail: None,
            expanded: false,
        },
        Line::User("Now add a scene for the update badge too.".into()),
        Line::Summary {
            glyph: '✗',
            tone: SummaryTone::Err,
            text: format!("failed · {short} · /retry · 9.1s · 2.0k in · $0.0044"),
            detail: Some(FAIL_MSG.into()),
            expanded: false,
        },
        Line::User(PROMPTS[11].into()),
        done("14.6s", "22.0k", "3.3k", "$0.0271"),
    ]
}

const UNICODE_USER: &str = "日志里的中文、日本語のテキスト、한국어 — emoji 👍🏽 🧑‍💻 🏳️‍🌈 \
👨‍👩‍👧‍👦 — accents: café cafe\u{301} naïve nai\u{308}ve, stacked Z\u{307}\u{34a}\u{302}o\u{316}\u{301}, \
Devanagari क़ हिन्दी, Thai กำา.";

const UNICODE_KA: &str = "**中文**과 *한국어* 텍스트, inline `コード と 日本語`, then emoji 🚀✨🧑‍💻, \
flags 🇯🇵🇰🇷, skin tone 👍🏻👎🏿, keycap 1️⃣, heart ❤️, family 🧑‍🧑‍🧒‍🧒, and accents café \
cafe\u{301} Z\u{307}\u{34a}o\u{301} — ﬁ ligature, NBSP a\u{a0}b, ZWSP x\u{200b}y.";

/// A very long unbroken token in a prompt and in a reply.
fn long_token() -> Transcript {
    let url = "https://github.com/jaltez/ka-harness/pull/1234/files#diff-\
               7d1e3b5a9c2f4011aa33bb22cc9d0e11ff2a3c4d5e6f708192a3b4c5d6e7f809";
    let sha = "3f7a1e9c2b5d4068a1c9e2f7b4d6083512ae9c7f5b3d10864297af0ce51d3b7a";
    transcript(vec![
        Line::User(format!(
            "Look at {url} and the sha256 {sha} before you merge."
        )),
        Line::Assistant(format!(
            "The digest is `{sha}` and the long word is {}.",
            "x".repeat(240)
        )),
        done("6.0s", "9.9k", "1.1k", "$0.0087"),
    ])
}

const STREAMING_MD: &str = "## Findings\n\n- The pill is appended after the loop badge\n- \
`status_right` never sees it\n\n```rust\nfn status_left(meters: &Meters) -> Vec<Span<'static>> {\n    \
let mut segs = Vec::new();\n    if let Some(pill) = job_pill(meters) {";

/// RFC 3339 timestamp `before` the wall clock. `rel_age` renders session
/// ages against `SystemTime::now()`, so building them relative to now
/// keeps "2h ago" the same on every run.
fn ago(before: Duration) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
        - before.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (h, m, s) = (rem / 3_600, rem % 3_600 / 60, rem % 60);
    // days → civil date (Howard Hinnant), the inverse of `rel_age_at`
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let (y, doy) = (yoe + era * 400, doe - (365 * yoe + yoe / 4 - yoe / 100));
    let mp = (5 * doy + 2) / 153;
    let (d, mth) = (
        doy - (153 * mp + 2) / 5 + 1,
        if mp < 10 { mp + 3 } else { mp - 9 },
    );
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

fn strand(
    id: &str,
    title: &str,
    messages: usize,
    cost: f64,
    tokens: u64,
    before: Duration,
    parent: Option<&str>,
) -> ka_strand::StrandSummary {
    ka_strand::StrandSummary {
        path: PathBuf::from(format!("strands/{id}.jsonl")),
        id: id.into(),
        ts: ago(before),
        title: title.into(),
        messages,
        cost,
        parent: parent.map(str::to_owned),
        name: None,
        pinned: false,
        recap: None,
        tokens,
    }
}

/// The newest sessions for the cwd.
fn strands() -> Vec<ka_strand::StrandSummary> {
    let h = |hours: u64| Duration::from_secs(hours * 3_600);
    vec![
        strand(
            "s1937c0d1e2f3-7d1e3b5a9c2f40",
            "Fix the status line jump",
            18,
            0.2144,
            262_400,
            h(2),
            None,
        ),
        strand(
            "s1936b9a8f7e6-1a2b3c4d5e6f701",
            "TUI snapshot harness",
            42,
            0.8931,
            1_204_500,
            h(5),
            None,
        ),
        strand(
            "s1935a4b5c6d7-9f8e7d6c5b4a33",
            "TUI snapshot harness",
            9,
            0.0412,
            51_800,
            h(26),
            Some("s1936b9a8f7e6-1a2b3c4d5e6f701"),
        ),
        strand(
            "s19349e8d7c6b-2c3d4e5f6a7b81",
            "Add a /focus digest row",
            27,
            0.3770,
            402_100,
            h(50),
            None,
        ),
        strand(
            "s1933d2c1b0a9-a1b2c3d4e5f677",
            "Release notes for 0.9.0",
            11,
            0.0988,
            96_300,
            h(96),
            None,
        ),
        strand(
            "s1932c1b0a9f8-b2c3d4e5f6a766",
            "Why is the update badge clipped at 80 columns?",
            6,
            0.0188,
            15_200,
            h(120),
            None,
        ),
        strand(
            "s1931b0a9f8e7-c3d4e5f6a7b855",
            "Staging panel review",
            33,
            0.6401,
            733_900,
            h(24 * 8),
            None,
        ),
        strand(
            "s1930a9f8e7d6-d4e5f6a7b8c944",
            "TUI snapshot harness",
            14,
            0.1275,
            141_700,
            h(24 * 9),
            Some("s1936b9a8f7e6-1a2b3c4d5e6f701"),
        ),
        strand(
            "s192f98e7d6c5-e5f6a7b8c9d033",
            "Transcript cache",
            4,
            0.0061,
            7_400,
            h(24 * 13),
            None,
        ),
        strand(
            "s192e87d6c5b4-f6a7b8c9d0e122",
            "Ask dialog sizing",
            21,
            0.2550,
            310_600,
            h(24 * 16),
            None,
        ),
        strand(
            "s192d76c5b4a3-a7b8c9d0e1f211",
            "Explain the spill files",
            8,
            0.0334,
            38_900,
            h(24 * 21),
            None,
        ),
        strand(
            "s192c65b4a3f2-b8c9d0e1f2a100",
            "Draft the release notes",
            2,
            0.0019,
            2_100,
            h(24 * 30),
            None,
        ),
    ]
}

fn providers() -> Vec<ProviderInfo> {
    let provider = |name: &str, env_var: &str, base_url: &str, key_set: bool| ProviderInfo {
        name: name.into(),
        env_var: env_var.into(),
        base_url: base_url.into(),
        key_set,
    };
    vec![
        provider(
            "anthropic",
            "ANTHROPIC_API_KEY",
            "https://api.anthropic.com",
            true,
        ),
        provider(
            "openai",
            "OPENAI_API_KEY",
            "https://api.openai.com/v1",
            true,
        ),
        provider(
            "google",
            "GEMINI_API_KEY",
            "https://generativelanguage.googleapis.com/v1beta/openai",
            false,
        ),
        provider("zai", "ZHIPU_API_KEY", "https://api.z.ai/api/paas/v4", true),
        provider(
            "deepseek",
            "DEEPSEEK_API_KEY",
            "https://api.deepseek.com",
            false,
        ),
        provider(
            "groq",
            "GROQ_API_KEY",
            "https://api.groq.com/openai/v1",
            false,
        ),
        provider(
            "mistral",
            "MISTRAL_API_KEY",
            "https://api.mistral.ai/v1",
            false,
        ),
        provider(
            "openrouter",
            "OPENROUTER_API_KEY",
            "https://openrouter.ai/api/v1",
            false,
        ),
        provider("ollama", "", "http://127.0.0.1:11434/v1", false),
        provider("lmstudio", "", "http://127.0.0.1:1234/v1", false),
    ]
}

/// The catalog rows the CLI builds from `dialects.toml`: priced, plan,
/// unpriced, keyless, and ids longer than the picker's id column.
fn catalog() -> Vec<ModelInfo> {
    let model = |id: &str,
                 wire: &str,
                 context: u32,
                 key_env: &str,
                 key_set: bool,
                 price: Option<(f64, f64)>,
                 plan: bool,
                 efforts: &[&str]| ModelInfo {
        id: id.into(),
        wire: wire.into(),
        context,
        key_env: key_env.into(),
        key_set,
        doc_url: String::new(),
        price_in: price.map_or(0.0, |(input, _)| input),
        price_out: price.map_or(0.0, |(_, output)| output),
        priced: price.is_some(),
        plan,
        efforts: ids(efforts),
    };
    vec![
        model(
            "anthropic/claude-sonnet-5",
            "anthropic_messages",
            200_000,
            "ANTHROPIC_API_KEY",
            true,
            Some((3.0, 15.0)),
            false,
            &["low", "medium", "high", "max"],
        ),
        model(
            "anthropic/claude-opus-4-1",
            "anthropic_messages",
            200_000,
            "ANTHROPIC_API_KEY",
            true,
            Some((15.0, 75.0)),
            false,
            &["low", "medium", "high"],
        ),
        model(
            "claude-plan/claude-sonnet-5",
            "anthropic_messages",
            200_000,
            "CLAUDE_CODE_OAUTH_TOKEN",
            true,
            None,
            true,
            &["low", "medium", "high"],
        ),
        model(
            "openai/gpt-5.1",
            "openai_chat",
            400_000,
            "OPENAI_API_KEY",
            true,
            Some((1.25, 10.0)),
            false,
            &["low", "medium", "high"],
        ),
        model(
            "openai/gpt-5-mini",
            "openai_chat",
            400_000,
            "OPENAI_API_KEY",
            true,
            Some((0.25, 2.0)),
            false,
            &["low", "medium", "high"],
        ),
        model(
            "deepseek/deepseek-flash",
            "openai_chat",
            1_000_000,
            "DEEPSEEK_API_KEY",
            true,
            Some((0.3, 1.2)),
            false,
            &[],
        ),
        model(
            "zai/glm-5.3",
            "openai_chat",
            1_000_000,
            "ZHIPU_API_KEY",
            true,
            Some((1.4, 4.4)),
            false,
            &["low", "high", "max"],
        ),
        model(
            "zai/glm-5.3-flash",
            "openai_chat",
            1_000_000,
            "ZHIPU_API_KEY",
            true,
            Some((0.075, 0.25)),
            false,
            &["low", "high", "max"],
        ),
        model(
            "zai-coding-plan/glm-5.3",
            "openai_chat",
            1_000_000,
            "ZHIPU_API_KEY",
            true,
            None,
            true,
            &["low", "high", "max"],
        ),
        model(
            "ollama/qwen3-32b",
            "openai_chat",
            131_072,
            "",
            true,
            None,
            false,
            &[],
        ),
        model(
            "openrouter/z-ai/glm-5.3-flash",
            "openai_chat",
            1_000_000,
            "OPENROUTER_API_KEY",
            true,
            Some((1.4, 4.4)),
            false,
            &["low", "high", "max"],
        ),
        model(
            "google/gemini-3-pro-preview",
            "openai_chat",
            1_048_576,
            "GEMINI_API_KEY",
            false,
            None,
            false,
            &["low", "high"],
        ),
    ]
}

/// A layered config, as `theme::surface_config` hands the panels one.
fn config(toml_text: &str) -> ka_engine::Config {
    let mut cfg = ka_engine::Config::default();
    let layer =
        ka_engine::Config::parse_layer(toml_text, "snapshots").expect("the fixture config parses");
    cfg.overlay(layer);
    cfg
}

/// `t-{id}` roster rows as the engine pre-renders them for `/tasks`.
fn task_rows() -> Vec<String> {
    [
        "t-17  running   12s   scout — summarize the diff$0.1234",
        "t-16  running   41s   scout — map every Modal variant to a scene",
        "t-15  done      1m    reviewer — check the index columns$0.0402",
        "t-14  done      2m    scout — list the hover targets$0.0211",
        "t-13  failed    3m    planner — draft the PRD (timeout)$0.0088",
        "t-12  done      9m    scout — read the status line code$0.0503",
        "job-1`  running  make test",
        "job-2`  exit 0   cargo build --release",
        "job-3`  running  npm run watch",
        "dap 7  running  breakpoints in 1 file(s)",
        "dap 9  live     breakpoints in 3 file(s)",
        "team alice  idle  turns 3  audit the modals",
        "team bob    running  turns 1  write the report",
    ]
    .iter()
    .map(|row| (*row).to_owned())
    .collect()
}

/// The paged result of task `t-17`.
fn task_result() -> String {
    (1..=40)
        .map(|i| {
            format!(
                "2026-11-23T10:{:02}:{:02}Z  scout  step {i}: read crates/ka-term/src/tui.rs \
                 lines {}-{} — status_left appends the pill before the badges",
                i % 60,
                (i * 7) % 60,
                i * 100,
                i * 100 + 99
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const GIT_DIFF: &str = "--- a/crates/ka-term/src/tui.rs
+++ b/crates/ka-term/src/tui.rs
@@ -13396,14 +13396,6 @@ fn status_left(meters: &Meters) -> Vec<ratatui::text::Span<'static>> {
         push(effort_segment(level), crate::palette::META);
     }
-    if let Some(pill) = job_pill(meters) {
-        // TUI-15: live background work (tasks · jobs · dap), fed by the
-        // /tasks roster snapshot
-        push(pill, crate::palette::TOOL);
-    }
     if meters.toggles > 0 {
@@ -13439,6 +13431,14 @@ fn status_left(meters: &Meters) -> Vec<ratatui::text::Span<'static>> {
     if meters.tan > 0 {
         push(format!(\"tan:{}\", meters.tan), crate::palette::TOOL);
     }
+    if let Some(pill) = job_pill(meters) {
+        // TUI-15: live background work (tasks · jobs · dap), fed by the
+        // /tasks roster snapshot. It sits after every badge so nothing
+        // before it moves when the roster changes.
+        push(pill, crate::palette::TOOL);
+    }
     segs
 }
@@ -13492,7 +13492,7 @@ fn status_right(meters: &Meters) -> Vec<ratatui::text::Span<'static>> {
-    let pad = (status.width as usize).saturating_sub(lw + rw + 2).max(1);
+    let pad = status.width.saturating_sub(lw + rw + 2).max(1);
     let mut bar: Vec<Span<'static>> = vec![Span::raw(\" \")];
     bar.extend(left);
     bar.push(Span::raw(\" \".repeat(pad)));
@@ -13716,3 +13716,9 @@ pub fn session_title(name: Option<&str>, session: &str, model: &str, busy: bool)
     if busy {
         title.push_str(\" · working\");
     }
+    // TUI-15: live background work (tasks · jobs · dap), fed by the /tasks roster snapshot; it sits after every badge so nothing before it moves when the roster changes
+        title.push_str(\" · styled\");
+    }
     title
 }";

const GIT_STATUS: &str = "M  crates/ka-term/Cargo.toml
A  crates/ka-term/src/tui_snapshots.rs
MM crates/ka-term/src/tui.rs
 M crates/ka-term/src/lib.rs
 M docs/snapshots.md
 D crates/ka-term/src/old_screen.rs
R  crates/ka-term/src/screen.rs -> crates/ka-term/src/screen_dump.rs
?? crates/ka-term/src/scratch.rs
 M README.md
 M CHANGELOG.md
?? docs/snapshots.md
 M crates/ka-harness/src/main.rs";

const EDIT_OLD: &str = "fn status_left(meters: &Meters) -> Vec<Span<'static>> {
    let mut segs: Vec<Span<'static>> = Vec::new();
    if !meters.model.is_empty() {
        push(meters.model.clone(), crate::palette::INK_DIM);
    }
    if meters.effort_supported {
        push(effort_segment(level), crate::palette::META);
    }
    if let Some(pill) = job_pill(meters) {
        push(pill, crate::palette::TOOL);
    }
    if meters.toggles > 0 {
        push(format!(\"⊘{}\", meters.toggles), crate::palette::WARN);
    }
    segs
}";

const EDIT_NEW: &str = "fn status_left(meters: &Meters) -> Vec<Span<'static>> {
    let mut segs: Vec<Span<'static>> = Vec::new();
    if !meters.model.is_empty() {
        push(meters.model.clone(), crate::palette::INK_DIM);
    }
    if meters.effort_supported {
        push(effort_segment(level), crate::palette::META);
    }
    if let Some(pill) = job_pill(meters) {
        push(pill, crate::palette::TOOL);
    }
    if meters.toggles > 0 {
        push(format!(\"⊘{}\", meters.toggles), crate::palette::WARN);
    }
    segs
}
// a trailing comment the engine's diff keeps as context so the hunk has more than the three rows the ask dialog budgets";

const EDIT_LONG_OLD: &str = "fn status_left(meters: &Meters) -> Vec<Span<'static>> {
    let mut segs: Vec<Span<'static>> = Vec::new();
    if !meters.model.is_empty() {
        push(meters.model.clone(), crate::palette::INK_DIM);
    }
    if meters.effort_supported {
        push(effort_segment(level), crate::palette::META);
    }
    if let Some(pill) = job_pill(meters) {
        push(pill, crate::palette::TOOL);
    }
    segs
}";

const EDIT_LONG_NEW: &str = "fn status_left(meters: &Meters) -> Vec<Span<'static>> {
    let mut segs: Vec<Span<'static>> = Vec::new();
    if !meters.model.is_empty() {
        push(meters.model.clone(), crate::palette::INK_DIM);
    }
    if meters.effort_supported {
        push(effort_segment(level), crate::palette::META);
    }
    if let Some(pill) = job_pill(meters) {
        // TUI-15: live background work (tasks · jobs · dap), fed by the /tasks roster snapshot; it sits after every badge so nothing before it moves when the roster changes between frames
        push(pill, crate::palette::TOOL);
    }
    segs
}";

// ── frame builders ───────────────────────────────────────────────

/// An idle session after a few turns: capture mode (the config
/// default), the default header glyph, an empty draft. The mention
/// cache carries the fixed walk, stamped on the real clock like
/// `app()`'s own fill (its TTL is what the call site reads).
fn base() -> RenderInput {
    RenderInput {
        scroll: None,
        transcript: session(),
        input: InputBuffer::default(),
        busy: false,
        busy_since: None,
        queue: Vec::new(),
        pending: None,
        current_assistant: String::new(),
        current_thought: String::new(),
        current_tool: String::new(),
        live_tool: None,
        slash_popup: None,
        path_popup: None,
        modal: None,
        picker: None,
        meters: live_meters(),
        sidebar: live_sidebar(),
        fresh: false,
        mouse_captured: true,
        header_glyph: "◆",
        toast: None,
        sel: None,
        hover: None,
        status_order: None,
        plain_mode: false,
        mention_cache: Some((Instant::now(), mention_entries())),
    }
}

/// `state` mid-turn, busy since `secs` before the harness clock, with
/// `queue` waiting.
fn busy(mut state: RenderInput, queue: Vec<String>, secs: u64) -> RenderInput {
    state.busy = true;
    state.busy_since = Some(now() - Duration::from_secs(secs));
    state.queue = queue;
    state
}

/// `state` with `msg` on screen: `pop_toast` stamps the real clock and
/// the call site drops a toast older than `TOAST_TTL`.
fn toast(mut state: RenderInput, msg: &str) -> RenderInput {
    state.toast = Some((msg.to_owned(), Instant::now()));
    state
}

fn asking(ask: PendingAsk) -> RenderInput {
    busy(
        RenderInput {
            pending: Some(ask),
            ..base()
        },
        Vec::new(),
        37,
    )
}

fn ask(question: &str, detail: Option<String>, selected: usize) -> PendingAsk {
    PendingAsk {
        id: AskId("ask-7".into()),
        question: question.into(),
        options: ids(&["allow", "always", "deny"]),
        selected,
        detail,
    }
}

/// A live tool call that started 5.2s before the harness clock.
fn running_call() -> LiveTool {
    LiveTool {
        id: "call-7d1e3b5a".into(),
        preview: ids(&[
            "running 750 tests",
            "test tui::tests::rail_thumb_sizes_and_positions_proportionally ... ok",
            "test result: ok. 748 passed; 0 failed; 0 ignored",
        ]),
        last: None,
        started: now() - Duration::from_millis(5_200),
    }
}

fn busy_text() -> RenderInput {
    busy(
        RenderInput {
            current_assistant: STREAMING_MD.into(),
            ..base()
        },
        Vec::new(),
        37,
    )
}

fn busy_thinking() -> RenderInput {
    busy(
        RenderInput {
            current_thought: THOUGHT.into(),
            ..base()
        },
        Vec::new(),
        37,
    )
}

fn busy_tool() -> RenderInput {
    busy(
        RenderInput {
            current_tool: tool_header("bash", "cargo test -p ka-term --lib"),
            live_tool: Some(running_call()),
            ..base()
        },
        Vec::new(),
        37,
    )
}

fn busy_everything() -> RenderInput {
    busy(
        RenderInput {
            current_thought: THOUGHT.into(),
            current_tool: tool_header("bash", "cargo test -p ka-term --lib"),
            live_tool: Some(running_call()),
            current_assistant: STREAMING_MD.into(),
            ..base()
        },
        Vec::new(),
        37,
    )
}

fn draft(text: &str) -> InputBuffer {
    let mut input = InputBuffer::default();
    input.insert_str(text);
    input
}

/// The draft mid reverse search for `query`.
fn rsearch(query: &str) -> InputBuffer {
    let mut input = InputBuffer::default();
    input.seed_history(ids(&[
        "fix the parser",
        "cargo test -p ka-term --lib",
        "cargo fmt --all",
    ]));
    input.search_start();
    for c in query.chars() {
        input.search_push(c);
    }
    input
}

fn vim(text: &str, mode: crate::vim::Mode) -> InputBuffer {
    let mut input = draft(text);
    input.vim.on = true;
    input.vim.mode = mode;
    input.vim.anchor = Some(0);
    input
}

/// `base()` over `lines`, with the first entry `pick` accepts opening
/// the viewport — the anchor a Ctrl+↑ jump leaves behind.
fn showing(w: u16, lines: Vec<Line>, pick: fn(&Line) -> bool) -> RenderInput {
    let mut t = transcript(lines);
    t.set_width(transcript_width(w));
    let top = entry(&t, pick);
    let scroll = t.entry_row_span(top).map_or(0, |(first, _)| first);
    RenderInput {
        scroll: Some(scroll),
        transcript: t,
        ..base()
    }
}

/// The index of the first entry `pick` accepts. `all_kinds()` gets blank
/// separators interleaved between families, so entries are looked up,
/// never counted.
fn entry(t: &Transcript, pick: fn(&Line) -> bool) -> usize {
    t.entries()
        .iter()
        .position(pick)
        .expect("the entry is in the transcript")
}

fn is_user(line: &Line) -> bool {
    matches!(line, Line::User(_))
}

fn is_thought(line: &Line) -> bool {
    matches!(line, Line::Thought(_))
}

fn is_tool_block(line: &Line) -> bool {
    matches!(line, Line::ToolBlock(_))
}

fn is_assistant(line: &Line) -> bool {
    matches!(line, Line::Assistant(_))
}

fn is_note(line: &Line) -> bool {
    matches!(line, Line::Info(text) if text.starts_with("• agents"))
}

fn is_verdict(line: &Line) -> bool {
    matches!(line, Line::Summary { .. })
}

fn is_failed(line: &Line) -> bool {
    matches!(
        line,
        Line::Summary {
            tone: SummaryTone::Err,
            ..
        }
    )
}

/// `showing` over [`all_kinds`], with the pointer on the entry `pick`
/// names.
fn hovered(w: u16, pick: fn(&Line) -> bool) -> RenderInput {
    let mut state = showing(w, all_kinds(), pick);
    let target = entry(&state.transcript, pick);
    state.hover = Some(Hover::Entry(target));
    state
}

/// The long session with its viewport anchored at transcript row `row`.
fn scrolled(w: u16, row: usize) -> RenderInput {
    let mut t = long_session();
    t.set_width(transcript_width(w));
    RenderInput {
        scroll: Some(row),
        transcript: t,
        ..base()
    }
}

fn selection(w: u16) -> RenderInput {
    let mut t = session();
    t.set_width(transcript_width(w));
    let (first, _) = t.entry_row_span(5).expect("the last user turn");
    RenderInput {
        sel: Some(((first.saturating_sub(4), 4), (first.saturating_sub(1), 40))),
        transcript: t,
        ..base()
    }
}

/// `base()` in plain view over `lines`, with the first entry `pick`
/// accepts opening the viewport (plain rows: one block plus a blank per
/// entry).
fn plain_showing(lines: Vec<Line>, pick: fn(&Line) -> bool) -> RenderInput {
    let t = transcript(lines);
    let top = entry(&t, pick);
    let mut offset = 0usize;
    for line in &t.entries()[..top] {
        let mut one = Transcript::default();
        one.push(line.clone());
        offset += crate::plain::transcript_rows(&one).len();
    }
    RenderInput {
        scroll: Some(offset),
        transcript: t,
        plain_mode: true,
        ..base()
    }
}

fn path_entries() -> Vec<(String, bool)> {
    ids(&[
        "git.rs",
        "icons.rs",
        "keys.rs",
        "markdown.rs",
        "palette.rs",
        "panels.rs",
        "plain.rs",
        "screen.rs",
        "theme.rs",
        "tui.rs",
        "vim.rs",
    ])
    .into_iter()
    .map(|name| (name, false))
    .collect()
}

/// Points `PLAN_CWD` at a throwaway repository on `main`: the staging
/// panel's header reads the branch through `git` at render time. Without
/// `git` on the machine the header falls back to `(no branch)`, which
/// no scene asserts on.
fn git_fixture() {
    let dir = std::env::temp_dir().join("ka-tui-snapshots-git");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .current_dir(&dir)
            .args(args)
            .output()
    };
    if std::fs::create_dir_all(&dir).is_ok() && !dir.join(".git").exists() {
        let _ = git(&["init", "-q"]);
        let _ = git(&["symbolic-ref", "HEAD", "refs/heads/main"]);
    }
    let _ = PLAN_CWD.set(dir);
}

// ── the scene table ──────────────────────────────────────────────

/// One row of the scene table.
#[derive(Clone)]
struct Scene {
    /// Short, stable, kebab-case: the snapshot file stem before `@`.
    id: String,
    name: String,
    category: &'static str,
    /// The frame `app()` would hand `render` at that terminal size.
    build: Rc<dyn Fn((u16, u16)) -> RenderInput>,
    /// The strings that make the scene usable: its title, its close
    /// chip, its decisive hint, its key content.
    must_contain: Vec<String>,
}

impl Scene {
    fn new(
        id: &str,
        name: &str,
        category: &'static str,
        must_contain: impl IntoIterator<Item = impl Into<String>>,
        build: impl Fn((u16, u16)) -> RenderInput + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            category,
            build: Rc::new(build),
            must_contain: must_contain.into_iter().map(Into::into).collect(),
        }
    }
}

/// A frame with `modal` open over the idle session.
fn opens(modal: Modal) -> RenderInput {
    RenderInput {
        modal: Some(modal),
        ..base()
    }
}

/// Every scene; ids are unique (asserted). Categories: `chrome`, `busy`,
/// `ask`, `composer`, `modal`, `transcript`, `status`, `header`,
/// `plain`.
fn scenes() -> Vec<Scene> {
    let mut all = chrome_scenes();
    all.extend(busy_scenes());
    all.extend(ask_scenes());
    all.extend(composer_scenes());
    let modals = modal_scenes();
    all.extend(modals.iter().cloned());
    all.extend(transcript_scenes());
    all.extend(status_scenes());
    all.extend(header_scenes());
    all.extend(plain_scenes());
    all.extend(plain_modals(&modals));
    let mut ids: Vec<&str> = all.iter().map(|scene| scene.id.as_str()).collect();
    ids.sort_unstable();
    assert!(
        ids.windows(2).all(|pair| pair[0] != pair[1]),
        "duplicate scene id"
    );
    all
}

/// The idle session with a stored title.
fn titled() -> RenderInput {
    RenderInput {
        sidebar: SidebarState {
            title: Some("Fix the status line jump".into()),
            ..live_sidebar()
        },
        ..base()
    }
}

/// A session before its first turn: the welcome is on screen.
fn fresh() -> RenderInput {
    RenderInput {
        fresh: true,
        transcript: Transcript::default(),
        meters: bootstrap_meters(),
        sidebar: SidebarState {
            title: None,
            ..live_sidebar()
        },
        ..base()
    }
}

fn chrome_scenes() -> Vec<Scene> {
    vec![
        Scene::new(
            "chrome-fresh",
            "Fresh empty session: the welcome",
            "chrome",
            [
                "new conversation",
                "Ask anything",
                "keys & commands",
                "past chats",
                "attach files",
                "run shell",
            ],
            |_| fresh(),
        ),
        Scene::new(
            "chrome-idle-session",
            "Idle session with history",
            "chrome",
            ["Fix the status line jump", "Ask anything", "done ·"],
            |_| titled(),
        ),
        Scene::new(
            "chrome-mouse-captured",
            "Mouse captured ([tui] mouse = \"capture\"): click zones and their hints",
            "chrome",
            ["▲ ▼", "todos", "skills", "info", "select+copy"],
            |_| titled(),
        ),
        Scene::new(
            "chrome-mouse-native",
            "Mouse native ([tui] mouse = \"native\")",
            "chrome",
            ["drag select"],
            |_| RenderInput {
                mouse_captured: false,
                ..titled()
            },
        ),
        Scene::new(
            "chrome-plain",
            "Plain view (/plain): the session as screen-reader text",
            "chrome",
            ["user: ", "done"],
            |_| RenderInput {
                plain_mode: true,
                ..titled()
            },
        ),
        Scene::new(
            "chrome-scrolled-top",
            "Scrolled to the top of a long session",
            "chrome",
            ["· 0%", "below · esc"],
            |(w, _)| scrolled(w, 0),
        ),
        Scene::new(
            "chrome-scrolled-mid",
            "Scrolled into the middle of a long session",
            "chrome",
            ["%", "below · esc"],
            |(w, _)| {
                let mut t = long_session();
                t.set_width(transcript_width(w));
                scrolled(w, t.total_rows() / 2)
            },
        ),
        Scene::new(
            "chrome-vim-normal",
            "Vim mode: NORMAL tag on the composer",
            "chrome",
            ["-- NORMAL --"],
            |_| RenderInput {
                input: vim("add a vim label scene", crate::vim::Mode::Normal),
                ..titled()
            },
        ),
        Scene::new(
            "chrome-vim-insert",
            "Vim mode: INSERT tag on the composer",
            "chrome",
            ["-- INSERT --"],
            |_| RenderInput {
                input: vim("add a vim label scene", crate::vim::Mode::Insert),
                ..titled()
            },
        ),
        Scene::new(
            "chrome-vim-visual",
            "Vim mode: VISUAL tag on the composer",
            "chrome",
            ["-- VISUAL --"],
            |_| RenderInput {
                input: vim("add a vim label scene", crate::vim::Mode::Visual),
                ..titled()
            },
        ),
        Scene::new(
            "chrome-ghost-slash",
            "Ghost text: the rest of a slash command",
            "chrome",
            ["❯ /model"],
            |_| RenderInput {
                input: draft("/mo"),
                slash_popup: update_suggestions("/mo"),
                ..titled()
            },
        ),
        Scene::new(
            "chrome-ghost-syntax",
            "Ghost text: the argument shape of /goal",
            "chrome",
            ["❯ /goal <condition>"],
            |_| RenderInput {
                input: draft("/goal"),
                slash_popup: update_suggestions("/goal"),
                ..titled()
            },
        ),
        Scene::new(
            "chrome-ghost-mention",
            "Ghost text: the rest of an @-mention",
            "chrome",
            ["@crates/ka-term/src/tui_snapshots.rs"],
            |_| RenderInput {
                input: draft("compare @crates/ka-term/src/tui_s"),
                ..titled()
            },
        ),
        Scene::new(
            "chrome-queued",
            "Busy turn with two prompts queued",
            "chrome",
            ["2 queued", "Steer the turn", "queue"],
            |_| {
                busy(
                    titled(),
                    ids(&["Then run the full suite.", "Update the changelog."]),
                    37,
                )
            },
        ),
        Scene::new(
            "chrome-toast",
            "Toast: short feedback",
            "chrome",
            ["✓ copied 3 rows to the clipboard"],
            |_| toast(titled(), "✓ copied 3 rows to the clipboard"),
        ),
        Scene::new(
            "chrome-toast-long",
            "Toast: a long message clipped to the pane",
            "chrome",
            ["mouse capture — everything clickable"],
            |_| toast(titled(), &mouse_mode_text(true)),
        ),
        Scene::new(
            "chrome-rsearch",
            "Reverse history search with a match",
            "chrome",
            ["search · `cargo`", "^r next", "cargo fmt --all"],
            |_| RenderInput {
                input: rsearch("cargo"),
                ..titled()
            },
        ),
        Scene::new(
            "chrome-rsearch-nomatch",
            "Reverse history search with no match",
            "chrome",
            ["(no match)", "^r next"],
            |_| RenderInput {
                input: rsearch("zq"),
                ..titled()
            },
        ),
        Scene::new(
            "chrome-selection",
            "Drag selection across transcript rows",
            "chrome",
            ["Reserve the slot", "done · 8.4s"],
            |(w, _)| selection(w),
        ),
        Scene::new(
            "hover-entry-user",
            "Hover: a user prompt (the rewind/fork popup target)",
            "chrome",
            ["▌ Add a snapshot harness"],
            |(w, _)| hovered(w, is_user),
        ),
        Scene::new(
            "hover-entry-thought",
            "Hover: a foldable thinking block",
            "chrome",
            ["Thinking"],
            |(w, _)| hovered(w, is_thought),
        ),
        Scene::new(
            "hover-entry-tool",
            "Hover: a tool card",
            "chrome",
            ["✓ ≡ read"],
            |(w, _)| hovered(w, is_tool_block),
        ),
        Scene::new(
            "hover-entry-summary",
            "Hover: a failed verdict with hidden detail",
            "chrome",
            ["✗ failed ·"],
            |(w, _)| hovered(w, is_failed),
        ),
        Scene::new(
            "hover-arrow-up",
            "Hover: the ▲ jump arrow",
            "chrome",
            ["▲ ▼"],
            |_| RenderInput {
                hover: Some(Hover::Arrow(true)),
                ..titled()
            },
        ),
        Scene::new(
            "hover-arrow-down",
            "Hover: the ▼ jump arrow",
            "chrome",
            ["▲ ▼"],
            |_| RenderInput {
                hover: Some(Hover::Arrow(false)),
                ..titled()
            },
        ),
        Scene::new(
            "hover-strip-todos",
            "Hover: the todos strip button",
            "chrome",
            ["todos"],
            |_| RenderInput {
                hover: Some(Hover::Strip(StripButton::Todos)),
                ..titled()
            },
        ),
        Scene::new(
            "hover-strip-skills",
            "Hover: the skills strip button",
            "chrome",
            ["skills"],
            |_| RenderInput {
                hover: Some(Hover::Strip(StripButton::Skills)),
                ..titled()
            },
        ),
        Scene::new(
            "hover-strip-info",
            "Hover: the info strip button",
            "chrome",
            ["info"],
            |_| RenderInput {
                hover: Some(Hover::Strip(StripButton::Info)),
                ..titled()
            },
        ),
        Scene::new(
            "hover-rail",
            "Hover: the scrollbar thumb",
            "chrome",
            ["┃"],
            |(w, _)| RenderInput {
                hover: Some(Hover::Rail),
                ..scrolled(w, 12)
            },
        ),
        Scene::new(
            "hover-chip-modal",
            "Hover: a modal's ✕ close chip",
            "chrome",
            ["✕", "help"],
            |_| RenderInput {
                hover: Some(Hover::Chip),
                modal: Some(Modal::Help { scroll: None }),
                ..titled()
            },
        ),
        Scene::new(
            "hover-chip-popup",
            "Hover: the slash popup's ✕ close chip",
            "chrome",
            ["✕", "commands"],
            |_| RenderInput {
                hover: Some(Hover::Chip),
                input: draft("/se"),
                slash_popup: update_suggestions("/se"),
                ..titled()
            },
        ),
    ]
}

fn busy_scenes() -> Vec<Scene> {
    vec![
        Scene::new(
            "busy-spinner",
            "Busy: the working row on an otherwise empty session",
            "busy",
            ["Working", "37.0s · esc to interrupt", "Steer the turn"],
            |_| {
                busy(
                    RenderInput {
                        transcript: Transcript::default(),
                        meters: bootstrap_meters(),
                        sidebar: SidebarState {
                            title: None,
                            ..live_sidebar()
                        },
                        ..base()
                    },
                    Vec::new(),
                    37,
                )
            },
        ),
        Scene::new(
            "busy-text-streaming",
            "Busy: assistant text streaming",
            "busy",
            ["Findings", "▌", "Working"],
            |_| busy_text(),
        ),
        Scene::new(
            "busy-thinking-streaming",
            "Busy: thinking streaming (the tail band)",
            "busy",
            ["Thinking", "Working", "esc to interrupt"],
            |_| busy_thinking(),
        ),
        Scene::new(
            "busy-tool-running",
            "Busy: a tool call running with its output preview",
            "busy",
            [
                "bash",
                "cargo test -p ka-term --lib",
                "5.2s",
                "test result: ok",
            ],
            |_| busy_tool(),
        ),
        Scene::new(
            "busy-turn-in-progress",
            "Busy: thinking, a running call, and streaming text over history",
            "busy",
            ["Working", "test result: ok", "Findings"],
            |_| busy_everything(),
        ),
    ]
}

fn ask_scenes() -> Vec<Scene> {
    fn diff(old: &str, new: &str) -> Option<String> {
        Some(ka_engine::hands::unified_diff(
            "crates/ka-term/src/tui.rs",
            old,
            new,
            24,
        ))
    }
    vec![
        Scene::new(
            "ask-edit",
            "Permission ask: an edit with its diff",
            "ask",
            [
                "permission",
                "allow edit to modify files?",
                "1 allow",
                "2 always",
                "3 deny",
                "esc deny",
            ],
            |_| {
                asking(ask(
                    "allow edit to modify files?",
                    diff(EDIT_OLD, EDIT_NEW),
                    0,
                ))
            },
        ),
        Scene::new(
            "ask-command",
            "Permission ask: a shell command",
            "ask",
            [
                "permission",
                "run `cargo test -p ka-term --lib`?",
                "1 allow",
                "2 always",
                "3 deny",
                "esc deny",
            ],
            |_| {
                asking(ask(
                    "run `cargo test -p ka-term --lib`? (rough est. ≈ $0.031 — pricing is per-Mtok \
                 catalog data)",
                    None,
                    0,
                ))
            },
        ),
        Scene::new(
            "ask-hardstop",
            "Permission ask: a hardstop",
            "ask",
            ["HARDSTOP", "Proceed anyway?", "1 allow", "2 always", "deny"],
            |_| {
                asking(ask(
                    "HARDSTOP — force push to a shared branch: `git push --force origin main`. \
                 Proceed anyway?",
                    None,
                    0,
                ))
            },
        ),
        Scene::new(
            "ask-selected-deny",
            "Permission ask: the deny option selected",
            "ask",
            ["▌ 3 deny", "1 allow", "2 always"],
            |_| asking(ask("run `rm -rf target`?", None, 2)),
        ),
        Scene::new(
            "ask-edit-long-lines",
            "Permission ask: an edit whose diff lines wrap",
            "ask",
            [
                "permission",
                "allow edit to modify files?",
                "1 allow",
                "2 always",
                "3 deny",
            ],
            |_| {
                asking(ask(
                    "allow edit to modify files?",
                    diff(EDIT_LONG_OLD, EDIT_LONG_NEW),
                    0,
                ))
            },
        ),
        Scene::new(
            "ask-sandbox-grant",
            "Permission ask: a sandbox grant folded into a wrapped question",
            "ask",
            ["sandbox grant", "allow", "2 always", "3 deny"],
            |_| {
                asking(ask(
                    "run `curl -sSf https://sh.rustup.rs | sh -s -- -y`?\nsandbox grant: network \
                 access to sh.rustup.rs and static.rust-lang.org",
                    None,
                    0,
                ))
            },
        ),
    ]
}

/// The completion popup for the draft `open ./crates/ka-term/src/`.
fn path_popup() -> PathPopup {
    PathPopup {
        entries: path_entries(),
        selected: 0,
        token_start: 5,
        prefix: "./crates/ka-term/src/".into(),
        mentions: false,
    }
}

fn composer_scenes() -> Vec<Scene> {
    let slash = update_suggestions("/").expect("the built-in commands match \"/\"");
    let deep = slash
        .items
        .get(40)
        .map(|(name, _)| name.clone())
        .expect("forty slash commands");
    vec![
        Scene::new(
            "composer-draft",
            "Composer: a multi-line draft with a wrapped line",
            "composer",
            ["❯ first line", "third line"],
            |_| RenderInput {
                input: draft(
                    "first line\na second line long enough to wrap at eighty columns because \
                     the draft only gets seventy-two\nthird line",
                ),
                ..base()
            },
        ),
        Scene::new(
            "composer-image-chips",
            "Composer: image chips in the draft",
            "composer",
            ["[Image #1]", "[Image #2]"],
            |_| RenderInput {
                input: draft("Compare [Image #1] with [Image #2] and describe the diff"),
                ..base()
            },
        ),
        Scene::new(
            "popup-slash-all",
            "Slash popup: every command",
            "composer",
            ["commands", "/model", "/tasks", "tab complete"],
            |_| RenderInput {
                input: draft("/"),
                slash_popup: update_suggestions("/"),
                ..base()
            },
        ),
        Scene::new(
            "popup-slash-filtered",
            "Slash popup: filtered to /se",
            "composer",
            ["/search", "/session", "/settings"],
            |_| RenderInput {
                input: draft("/se"),
                slash_popup: update_suggestions("/se").map(|popup| SlashPopup {
                    selected: 1,
                    ..popup
                }),
                ..base()
            },
        ),
        Scene::new(
            "popup-slash-windowed",
            "Slash popup: the selection deep in the list",
            "composer",
            [deep],
            move |_| RenderInput {
                input: draft("/"),
                slash_popup: Some(SlashPopup {
                    selected: 40,
                    ..slash.clone()
                }),
                ..base()
            },
        ),
        Scene::new(
            "popup-path",
            "Path popup: completing ./crates/ka-term/src/",
            "composer",
            ["path", "tab accept", "tui.rs"],
            |_| RenderInput {
                input: draft("open ./crates/ka-term/src/"),
                path_popup: Some(path_popup()),
                ..base()
            },
        ),
        Scene::new(
            "popup-path-windowed",
            "Path popup: the selection deep in the list",
            "composer",
            ["tui.rs"],
            |_| RenderInput {
                input: draft("open ./crates/ka-term/src/"),
                path_popup: Some(PathPopup {
                    selected: 9,
                    ..path_popup()
                }),
                ..base()
            },
        ),
        Scene::new(
            "popup-mentions",
            "Files popup: @-mention completion",
            "composer",
            ["files", "tab accept", "tui_snapshots.rs"],
            |_| RenderInput {
                input: draft("@crates/ka-term/src/t"),
                path_popup: Some(PathPopup {
                    entries: mention_matches(&mention_entries(), "crates/ka-term/src/t"),
                    selected: 1,
                    token_start: 0,
                    prefix: "@".into(),
                    mentions: true,
                }),
                ..base()
            },
        ),
        Scene::new(
            "popup-mentions-dir",
            "Files popup: @-mention completion after a directory",
            "composer",
            ["files"],
            |_| RenderInput {
                input: draft("@crates/ka-term/"),
                path_popup: Some(PathPopup {
                    entries: mention_matches(&mention_entries(), "crates/ka-term/"),
                    selected: 0,
                    token_start: 0,
                    prefix: "@".into(),
                    mentions: true,
                }),
                ..base()
            },
        ),
        Scene::new(
            "picker-mode",
            "Input picker: the /mode tiers",
            "composer",
            [
                "mode",
                "ask before every edit and command",
                "no prompts",
                "1-4 jump",
            ],
            |_| RenderInput {
                picker: Some(InputPicker::Mode(ModePicker::for_mode(Mode::Ask))),
                ..base()
            },
        ),
        Scene::new(
            "picker-thinking",
            "Input picker: the /thinking levels",
            "composer",
            ["thinking", "○ off", "◕ high"],
            |_| RenderInput {
                picker: Some(InputPicker::Thinking(ThinkingPicker::for_effort(
                    Some(Effort::High),
                    &ids(&["low", "high", "max"]),
                ))),
                ..base()
            },
        ),
    ]
}

/// Records of one strand: a header, then a user prompt and its answer
/// per turn. A fork keeps the parent's record ids, so two strands built
/// from the same prompts share them.
fn records(prompts: &[&str]) -> Vec<ka_strand::Record> {
    let message = |id: String, role: ka_strand::Role, content: &str| ka_strand::Record::Message {
        id: ka_protocol::RecordId(id),
        images: Vec::new(),
        role,
        content: content.into(),
        calls: Vec::new(),
        results: Vec::new(),
        thinking: None,
    };
    let mut out = vec![ka_strand::Record::Header {
        id: ka_protocol::StrandId("s".into()),
        ts: String::new(),
        cwd: String::new(),
        version: 1,
        repo: None,
        parent: None,
    }];
    for (turn, prompt) in prompts.iter().enumerate() {
        out.push(message(format!("u{turn}"), ka_strand::Role::User, prompt));
        out.push(message(
            format!("a{turn}"),
            ka_strand::Role::Assistant,
            "done",
        ));
    }
    out
}

/// `/tree` over a family: a root, a fork, a full copy, and a grandchild.
fn tree_modal(selected: usize) -> Modal {
    let all = vec![
        strand(
            "root",
            "Add a snapshot harness for the TUI",
            34,
            0.4210,
            412_300,
            Duration::from_secs(86_400 * 3),
            None,
        ),
        strand(
            "forked",
            "Reserve the badge slots",
            12,
            0.0931,
            88_400,
            Duration::from_secs(86_400 * 2),
            Some("root"),
        ),
        strand(
            "copied",
            "Add a snapshot harness for the TUI",
            41,
            0.5102,
            501_700,
            Duration::from_secs(86_400),
            Some("root"),
        ),
        strand(
            "grandkid",
            "Add a /focus digest row",
            6,
            0.0244,
            21_900,
            Duration::from_secs(3_600 * 5),
            Some("forked"),
        ),
        strand(
            "unrelated",
            "Draft the release notes for 0.9.0",
            9,
            0.0188,
            15_200,
            Duration::from_secs(3_600 * 20),
            None,
        ),
    ];
    let history: Vec<String> = PROMPTS[..8]
        .iter()
        .map(|prompt| (*prompt).to_owned())
        .collect();
    let (items, actions) = tree_modal_rows(&all, Some("root"), &history, 110 - 4, |id| match id {
        "root" | "copied" => records(&[PROMPTS[0], PROMPTS[1]]),
        "forked" => records(&[PROMPTS[0]]),
        "grandkid" => records(&[PROMPTS[0], PROMPTS[2]]),
        _ => Vec::new(),
    });
    Modal::Tree {
        items,
        actions,
        selected,
    }
}

fn context_modal() -> Modal {
    let parts: Vec<ContextPart> = [
        ("system", 9_120),
        ("user", 31_480),
        ("assistant", 14_250),
        ("tools", 6_590),
        ("hint:thinking 38k", 12_400),
        ("hint:outputs 9k", 4_100),
    ]
    .into_iter()
    .map(|(name, tokens)| ContextPart {
        name: name.into(),
        tokens,
    })
    .collect();
    let lines = long_session();
    let hints = context_hints(&parts, 200_000, lines.entries(), 2);
    Modal::Context {
        rows: context_rows(&parts, 200_000, &hints),
    }
}

fn tasks_modal() -> Modal {
    Modal::Tasks {
        entries: task_rows()
            .iter()
            .map(|row| (task_id_of_row(row), row.clone()))
            .collect(),
        selected: 1,
        previews: vec![
            (17, "42 tests passed · 0 failed · 1.9s".to_owned()),
            (16, "mapped 32 of 35 variants".to_owned()),
            (15, "index columns verified at 80x24".to_owned()),
        ],
    }
}

fn hooks_rows() -> Vec<String> {
    let cfg = config(
        "[[hooks]]\nevent = \"pre_tool_use\"\ntool = \"bash\"\ncommand = \
                      \"guard.sh\"\n[[hooks]]\nevent = \"stop\"\ncommand = \"notify-done.sh\"\n",
    );
    crate::panels::hooks_rows(&cfg, Path::new("/nonexistent-ka-snapshots"))
}

fn permissions_rows() -> Vec<String> {
    let cfg = config(
        "[[rules]]\ntool = \"bash\"\npattern = \"cargo *\"\nverdict = \"allow\"\n[[rules]]\n\
         tool = \"write\"\nverdict = \"ask\"\n[[rules]]\ntool = \"bash\"\npattern = \"git \
         push*\"\nverdict = \"deny\"\n[permissions]\nallow = [\"read\", \"grep\"]\n[sandbox]\n\
         mode = \"fs\"\nallow_write = [\"/opt/data\", \"/tmp/ka-snap\"]\n",
    );
    crate::panels::permissions_rows(&cfg, Some("off"), "allow")
}

fn doctor_rows() -> Vec<String> {
    let providers = providers();
    let mcp = live_sidebar().inventory.mcp;
    let facts = crate::panels::DoctorFacts {
        session: SESSION,
        model: "zai/glm-5.3",
        effort: "high",
        mode: "allow",
        providers: &providers,
        live_sandbox: Some("off"),
        turns: 3,
        cost: 0.1842,
        context: (61_440, 1_000_000),
        update: Some("v0.9.1"),
        mcp: &mcp,
    };
    crate::panels::doctor_rows(&config("[sandbox]\nmode = \"fs\"\n"), &facts)
}

fn memory_rows() -> Vec<String> {
    ids(&[
        "▸ /home/dev/projects/ka-harness/MEMORY.md",
        "# Project memory",
        "- Snapshot scenes live in `crates/ka-term/src/tui_snapshots.rs`.",
        "- Regenerate the baseline before every TUI audit.",
        "",
        "▸ /home/dev/.config/ka/MEMORY.md",
        "# User memory",
        "- Prefers a 120x40 terminal with truecolor.",
        "- Reads diffs side by side.",
    ])
}

/// Every [`Modal`] variant with realistic data. List variants carry at
/// least eight rows, so their windows scroll.
fn modal_scenes() -> Vec<Scene> {
    vec![
        Scene::new(
            "modal-session",
            "Session picker (/session)",
            "modal",
            ["(new session)", "type to filter", "start fresh", " msgs · "],
            move |_| {
                opens(Modal::Session(SessionPicker {
                    sessions: strands(),
                    selected: 3,
                    filter: String::new(),
                    current: Some(SESSION.into()),
                }))
            },
        ),
        Scene::new(
            "modal-session-filtered",
            "Session picker: filtered",
            "modal",
            ["❯ harness▌", "(new session)", "start fresh"],
            move |_| {
                opens(Modal::Session(SessionPicker {
                    sessions: strands(),
                    selected: 1,
                    filter: "harness".into(),
                    current: Some(SESSION.into()),
                }))
            },
        ),
        Scene::new(
            "modal-session-nomatch",
            "Session picker: the filter matches nothing",
            "modal",
            ["❯ zzz▌", "(new session)"],
            move |_| {
                opens(Modal::Session(SessionPicker {
                    sessions: strands(),
                    selected: 0,
                    filter: "zzz".into(),
                    current: None,
                }))
            },
        ),
        Scene::new(
            "modal-session-empty",
            "Session picker: no sessions yet",
            "modal",
            ["(new session)", "start fresh"],
            move |_| {
                opens(Modal::Session(SessionPicker {
                    sessions: Vec::new(),
                    selected: 0,
                    filter: String::new(),
                    current: None,
                }))
            },
        ),
        Scene::new(
            "modal-settings",
            "Settings panel (/settings)",
            "modal",
            [
                "model",
                "accept_edits",
                "providers:",
                "(keyless)",
                // a url that outgrows its row ends in an ellipsis
                "… │",
            ],
            move |_| {
                opens(Modal::Settings(SettingsPanel {
                    model: "zai/glm-5.3".into(),
                    mode: Mode::AcceptEdits,
                    effort: Some(Effort::High),
                    selected: 0,
                    edit: None,
                    providers: providers(),
                    config_path: CONFIG_PATH.into(),
                    status: None,
                }))
            },
        ),
        Scene::new(
            "modal-settings-edit",
            "Settings panel: editing the model selector",
            "modal",
            ["model ✎", "edit selector"],
            move |_| {
                opens(Modal::Settings(SettingsPanel {
                    model: "zai/glm-5.3".into(),
                    mode: Mode::Ask,
                    effort: None,
                    selected: 0,
                    edit: Some("anthropic/clau".into()),
                    providers: providers(),
                    config_path: CONFIG_PATH.into(),
                    status: None,
                }))
            },
        ),
        Scene::new(
            "modal-features",
            "Features panel (/features)",
            "modal",
            ["features", "subagents", "bash sandbox", "tools"],
            move |_| {
                opens(Modal::Features {
                    entries: feature_rows(&live_sidebar()),
                    selected: 5,
                    status: None,
                })
            },
        ),
        Scene::new(
            "modal-model",
            "Model picker (/model)",
            "modal",
            [
                "anthropic/claude-sonnet-5",
                "plan",
                "$3/$15",
                "type to filter",
            ],
            move |_| {
                opens(Modal::Model(ModelPicker {
                    models: catalog(),
                    vendor: None,
                    selected: 2,
                    filter: String::new(),
                }))
            },
        ),
        Scene::new(
            "modal-model-vendor",
            "Model picker: locked to one vendor (/model zai)",
            "modal",
            ["model · zai", "zai/glm-5.3"],
            move |_| {
                opens(Modal::Model(ModelPicker {
                    models: catalog(),
                    vendor: Some("zai".into()),
                    selected: 0,
                    filter: String::new(),
                }))
            },
        ),
        Scene::new(
            "modal-model-nomatch",
            "Model picker: the filter matches nothing",
            "modal",
            [
                "model",
                "kernel/forge-9▌",
                "enter sets 'kernel/forge-9' as a custom selector",
            ],
            move |_| {
                opens(Modal::Model(ModelPicker {
                    models: catalog(),
                    vendor: None,
                    selected: 0,
                    filter: "kernel/forge-9".into(),
                }))
            },
        ),
        Scene::new(
            "modal-model-noproviders",
            "Model picker: no configured provider",
            "modal",
            ["model", "no configured providers — /provider to connect"],
            move |_| {
                let unconfigured: Vec<ModelInfo> = catalog()
                    .iter()
                    .filter(|m| !m.key_env.is_empty())
                    .cloned()
                    .map(|mut m| {
                        m.key_set = false;
                        m
                    })
                    .collect();
                opens(Modal::Model(ModelPicker {
                    models: unconfigured,
                    vendor: None,
                    selected: 0,
                    filter: String::new(),
                }))
            },
        ),
        Scene::new(
            "modal-provider",
            "Provider picker (/provider)",
            "modal",
            ["providers", "ANTHROPIC_API_KEY ✓", "keyless · local", "✗"],
            move |_| {
                let mut picker = ProviderPicker::new(providers(), &catalog());
                picker.selected = 1;
                opens(Modal::Provider(picker))
            },
        ),
        Scene::new(
            "modal-provider-nomatch",
            "Provider picker: the filter matches nothing",
            "modal",
            ["providers", "(no providers match)"],
            move |_| {
                let mut picker = ProviderPicker::new(providers(), &catalog());
                picker.filter = "nope".into();
                opens(Modal::Provider(picker))
            },
        ),
        Scene::new(
            "modal-key",
            "API key prompt (/key)",
            "modal",
            [
                "api key",
                "ANTHROPIC_API_KEY",
                "get one",
                "value",
                "type value",
            ],
            |_| {
                opens(Modal::Key(KeyPrompt {
                    env_var: "ANTHROPIC_API_KEY".into(),
                    provider: "anthropic".into(),
                    doc_url: "https://docs.anthropic.com/en/api/getting-started".into(),
                    input: "sk-ant-api03-7d1e3b5a".into(),
                    drill: None,
                    pending_model: Some("anthropic/claude-sonnet-5".into()),
                }))
            },
        ),
        Scene::new(
            "modal-key-empty",
            "API key prompt: nothing typed yet",
            "modal",
            ["api key", "value"],
            |_| {
                opens(Modal::Key(KeyPrompt {
                    env_var: "ZHIPU_API_KEY".into(),
                    provider: "zai".into(),
                    doc_url: String::new(),
                    input: String::new(),
                    drill: Some("zai".into()),
                    pending_model: None,
                }))
            },
        ),
        Scene::new(
            "modal-spills",
            "Spill browser (/spills)",
            "modal",
            ["spills", "spill://7d1e3b5a-0001"],
            move |_| {
                opens(Modal::Spills {
                    items: (1..=12)
                        .map(|n| format!("spill://7d1e3b5a-{n:04}"))
                        .collect(),
                    selected: 4,
                })
            },
        ),
        Scene::new(
            "modal-spills-empty",
            "Spill browser: nothing spilled yet",
            "modal",
            ["(no spill files yet — full tool output lands here)"],
            move |_| {
                opens(Modal::Spills {
                    items: Vec::new(),
                    selected: 0,
                })
            },
        ),
        Scene::new(
            "modal-prompts",
            "MCP prompt picker (/prompt)",
            "modal",
            ["prompts", "github/review-pr", "run / fill args"],
            move |_| {
                opens(Modal::Prompts {
                    items: live_sidebar().inventory.prompts,
                    selected: 2,
                })
            },
        ),
        Scene::new(
            "modal-prompts-empty",
            "MCP prompt picker: none advertised",
            "modal",
            ["(no MCP prompts advertised)"],
            move |_| {
                opens(Modal::Prompts {
                    items: Vec::new(),
                    selected: 0,
                })
            },
        ),
        Scene::new(
            "modal-rewind",
            "Rewind menu (esc esc)",
            "modal",
            [
                "rewind",
                "pick a message",
                "[−1]",
                "Ship the harness commit",
            ],
            move |_| {
                opens(Modal::Rewind {
                    items: PROMPTS
                        .iter()
                        .rev()
                        .enumerate()
                        .map(|(i, prompt)| (i + 1, (*prompt).to_owned()))
                        .collect(),
                    selected: 2,
                })
            },
        ),
        Scene::new(
            "modal-prompt-actions",
            "Click-a-prompt popup: rewind, edit, or fork",
            "modal",
            ["rewind", "⏪ rewind here", "✎ rewind & edit", "⑂ fork here"],
            |_| {
                opens(Modal::PromptActions {
                    turns: 3,
                    prompt: PROMPTS[5].into(),
                    selected: 1,
                })
            },
        ),
        Scene::new(
            "modal-memory",
            "Memory viewer (/memory) with a staged inbox",
            "modal",
            ["memory", "MEMORY.md", "staged memories", "accept"],
            move |_| {
                opens(Modal::Memory {
                    rows: memory_rows(),
                    inbox: ids(&[
                        "The jobs pill sits in the left cluster",
                        "Snapshot scenes anchor busy_since at now − 37s",
                        "Plain mode drops the close chip",
                        "Regenerate snapshots before every audit",
                        "The mention walk is cached for five seconds",
                    ]),
                    selected: 1,
                })
            },
        ),
        Scene::new(
            "modal-memory-empty",
            "Memory viewer: no memory files",
            "modal",
            ["memory", "(no memory files)"],
            move |_| {
                opens(Modal::Memory {
                    rows: ids(&[
                        "(no memory files)",
                        "create MEMORY.md at the project root or ~/.config/ka/MEMORY.md",
                    ]),
                    inbox: Vec::new(),
                    selected: 0,
                })
            },
        ),
        Scene::new(
            "modal-usage",
            "Usage dashboard (/usage)",
            "modal",
            ["usage", "▸ session", "▸ recent sessions"],
            move |_| {
                opens(Modal::Usage {
                    rows: usage_rows(&live_meters(), &strands()),
                })
            },
        ),
        Scene::new(
            "modal-context",
            "Context breakdown (/context)",
            "modal",
            ["context", "▸ context", "▸ tips", "▸ used"],
            |_| opens(context_modal()),
        ),
        Scene::new(
            "modal-tree",
            "Fork tree (/tree)",
            "modal",
            ["tree", "branch from a past input", "⑂ at", "fork"],
            |_| opens(tree_modal(3)),
        ),
        Scene::new(
            "modal-tree-empty",
            "Fork tree: no family and no inputs",
            "modal",
            ["(no related sessions)"],
            move |_| {
                opens(Modal::Tree {
                    items: Vec::new(),
                    actions: Vec::new(),
                    selected: 0,
                })
            },
        ),
        Scene::new(
            "modal-tasks",
            "Agent hub (/tasks)",
            "modal",
            ["tasks", "t-17", "└", "page result"],
            |_| opens(tasks_modal()),
        ),
        Scene::new(
            "modal-tasks-empty",
            "Agent hub: nothing running",
            "modal",
            ["(no background tasks or jobs)"],
            move |_| {
                opens(Modal::Tasks {
                    entries: Vec::new(),
                    selected: 0,
                    previews: Vec::new(),
                })
            },
        ),
        Scene::new(
            "modal-task-detail",
            "Paged result of a background task (/tasks, ⏎ on a row): opens at the top",
            "modal",
            ["task t-17", "step 1:", "… ↓ "],
            |_| {
                opens(Modal::TaskDetail {
                    id: 17,
                    text: task_result(),
                    scroll: None,
                })
            },
        ),
        Scene::new(
            "modal-task-detail-scrolled",
            "Paged task result: scrolled into the middle",
            "modal",
            ["task t-17", "… ↑ ", "… ↓ "],
            |_| {
                opens(Modal::TaskDetail {
                    id: 17,
                    text: task_result(),
                    scroll: Some(12),
                })
            },
        ),
        Scene::new(
            "modal-debug",
            "Debug sessions (/debug)",
            "modal",
            ["debug sessions", "dap 7", "breakpoints in"],
            |_| {
                opens(Modal::Debug {
                    rows: debug_rows(),
                    scroll: None,
                })
            },
        ),
        Scene::new(
            "modal-todos",
            "Todos popup (alt+o)",
            "modal",
            [
                "todos",
                "Map every Modal variant",
                "Land the harness commit",
            ],
            move |_| {
                opens(Modal::Todos {
                    rows: todos_rows(&live_sidebar(), 64),
                })
            },
        ),
        Scene::new(
            "modal-todos-empty",
            "Todos popup: no live list",
            "modal",
            ["todos", "(no live todo list — the todo hand drives this)"],
            |_| {
                opens(Modal::Todos {
                    rows: todos_rows(&SidebarState::default(), 64),
                })
            },
        ),
        Scene::new(
            "modal-skills",
            "Skills, agents, and MCP popup (ctrl+t)",
            "modal",
            ["skills", "agents", "mcp", "rust-docs"],
            move |_| {
                opens(Modal::Skills {
                    rows: inventory_rows(&live_sidebar(), 64),
                })
            },
        ),
        Scene::new(
            "modal-skills-empty",
            "Skills popup: nothing discovered",
            "modal",
            ["(nothing discovered — skills, agents, MCP servers land here)"],
            |_| {
                opens(Modal::Skills {
                    rows: inventory_rows(&SidebarState::default(), 64),
                })
            },
        ),
        Scene::new(
            "modal-info",
            "Session info popup (alt+i)",
            "modal",
            ["info", "session #7d1e3b5a", "model zai/glm-5.3"],
            move |_| {
                opens(Modal::Info {
                    rows: info_rows(&live_sidebar(), &live_meters(), 64),
                })
            },
        ),
        Scene::new(
            "modal-info-empty",
            "Session info popup: before the first event",
            "modal",
            ["info", "ka "],
            |_| {
                opens(Modal::Info {
                    rows: info_rows(&SidebarState::default(), &Meters::default(), 64),
                })
            },
        ),
        Scene::new(
            "modal-help",
            "Help overlay (/help)",
            "modal",
            ["help", "keys", "send · steer mid-turn"],
            |_| opens(Modal::Help { scroll: None }),
        ),
        Scene::new(
            "modal-help-scrolled",
            "Help overlay: scrolled",
            "modal",
            ["help", "pgup/pgdn scroll"],
            |_| opens(Modal::Help { scroll: Some(60) }),
        ),
        Scene::new(
            "modal-theme",
            "Theme picker (/theme)",
            "modal",
            ["theme", "default (Nocturne)", "apply live"],
            |_| {
                opens(Modal::Theme {
                    names: ids(&[
                        "dawn",
                        "gruvbox",
                        "nord",
                        "solarized-dark",
                        "solarized-light",
                        "tokyo-night",
                        "catppuccin",
                        "everforest",
                        "kanagawa",
                        "rose-pine",
                        "dracula",
                    ]),
                    selected: 3,
                })
            },
        ),
        Scene::new(
            "modal-theme-empty",
            "Theme picker: no theme files",
            "modal",
            ["theme", "no themes found"],
            move |_| {
                opens(Modal::Theme {
                    names: Vec::new(),
                    selected: 0,
                })
            },
        ),
        Scene::new(
            "modal-style",
            "Output style picker (/style)",
            "modal",
            ["style", "built-in identity", "engineer"],
            |_| {
                opens(Modal::Style {
                    names: ids(&["engineer", "concise", "teacher", "reviewer", "playwright"]),
                    selected: 2,
                })
            },
        ),
        Scene::new(
            "modal-style-empty",
            "Output style picker: no style files",
            "modal",
            ["style", "no styles found"],
            move |_| {
                opens(Modal::Style {
                    names: Vec::new(),
                    selected: 0,
                })
            },
        ),
        Scene::new(
            "modal-copy-pick",
            "Copy picker (alt+y)",
            "modal",
            ["copy", "user", "ka", "save to file"],
            move |_| {
                opens(Modal::CopyPick {
                    blocks: copy_blocks(&long_session(), 40),
                    selected: 3,
                    save: None,
                })
            },
        ),
        Scene::new(
            "modal-copy-save",
            "Copy picker: the save-path prompt",
            "modal",
            ["copy · save", "save block to file", "path notes.md"],
            move |_| {
                opens(Modal::CopyPick {
                    blocks: copy_blocks(&long_session(), 40),
                    selected: 0,
                    save: Some("notes.md".into()),
                })
            },
        ),
        Scene::new(
            "modal-search",
            "Transcript search (/search)",
            "modal",
            ["search", "refine", "jumps to the hit"],
            move |_| {
                opens(Modal::Search {
                    query: "the".into(),
                    hits: search_hits(&long_session(), "the"),
                    selected: 2,
                })
            },
        ),
        Scene::new(
            "modal-search-empty",
            "Transcript search: nothing typed yet",
            "modal",
            ["search", "type to search the transcript…"],
            move |_| {
                opens(Modal::Search {
                    query: String::new(),
                    hits: Vec::new(),
                    selected: 0,
                })
            },
        ),
        Scene::new(
            "modal-search-nomatch",
            "Transcript search: no hits",
            "modal",
            ["search", "no matches"],
            move |_| {
                opens(Modal::Search {
                    query: "zzzqqq".into(),
                    hits: Vec::new(),
                    selected: 0,
                })
            },
        ),
        Scene::new(
            "modal-changelog",
            "Changelog (/changelog): opens at the top",
            "modal",
            ["changelog", "# Changelog", "… ↓ "],
            |_| {
                opens(Modal::Changelog {
                    rows: changelog_rows(),
                    scroll: None,
                })
            },
        ),
        Scene::new(
            "modal-changelog-scrolled",
            "Changelog: scrolled into the middle",
            "modal",
            ["changelog", "… ↑ ", "… ↓ "],
            |_| {
                opens(Modal::Changelog {
                    rows: changelog_rows(),
                    scroll: Some(40),
                })
            },
        ),
        Scene::new(
            "modal-queue",
            "Follow-up queue (/queue)",
            "modal",
            ["queue", "#1", "#9", "#10", "dequeues the picked prompt"],
            move |_| {
                opens(Modal::Queue {
                    items: ids(&PROMPTS[..10]),
                    selected: 3,
                })
            },
        ),
        Scene::new(
            "modal-hooks",
            "Hooks panel (/hooks)",
            "modal",
            ["hooks", "file hooks", "(absent)", "sources:"],
            |_| {
                opens(Modal::Hooks {
                    rows: hooks_rows(),
                    scroll: None,
                })
            },
        ),
        Scene::new(
            "modal-permissions",
            "Permissions panel (/permissions)",
            "modal",
            ["permissions", "mode: allow", "rules", "protected paths"],
            |_| {
                opens(Modal::Permissions {
                    rows: permissions_rows(),
                    scroll: None,
                })
            },
        ),
        Scene::new(
            "modal-doctor",
            "Doctor panel (/doctor)",
            "modal",
            ["doctor", "model:", "session:"],
            |_| {
                opens(Modal::Doctor {
                    rows: doctor_rows(),
                    scroll: None,
                })
            },
        ),
        Scene::new(
            "modal-git-list",
            "Staging panel (/git): the file list",
            "modal",
            [
                "git staging",
                " staged · ",
                " changed (working tree)",
                "crates/ka-term/src/tui.rs",
                "keys",
            ],
            |_| opens(GitPanel::new(crate::git::Pane::List).modal()),
        ),
        Scene::new(
            "modal-git-list-keys",
            "Staging panel: the key help overlay (?)",
            "modal",
            [
                "git staging",
                "choose a file",
                "stage everything",
                "close help",
            ],
            |_| {
                opens(
                    GitPanel {
                        keys_shown: true,
                        ..GitPanel::new(crate::git::Pane::List)
                    }
                    .modal(),
                )
            },
        ),
        Scene::new(
            "modal-git-list-clean",
            "Staging panel: a clean working tree",
            "modal",
            ["(working tree clean — nothing to stage)"],
            |_| {
                opens(
                    GitPanel {
                        files: Vec::new(),
                        ..GitPanel::new(crate::git::Pane::List)
                    }
                    .modal(),
                )
            },
        ),
        Scene::new(
            "modal-git-diff",
            "Staging panel: a file diff, pinned to the tail",
            "modal",
            ["git diff —", "scroll", "esc back"],
            |_| {
                opens(
                    GitPanel::new(crate::git::Pane::Diff {
                        path: "crates/ka-term/src/tui.rs".into(),
                        text: GIT_DIFF.into(),
                    })
                    .modal(),
                )
            },
        ),
        Scene::new(
            "modal-git-diff-top",
            "Staging panel: a file diff from the top",
            "modal",
            ["--- a/crates/ka-term/src/tui.rs", "@@"],
            |_| {
                opens(
                    GitPanel {
                        scroll: Some(0),
                        ..GitPanel::new(crate::git::Pane::Diff {
                            path: "crates/ka-term/src/tui.rs".into(),
                            text: GIT_DIFF.into(),
                        })
                    }
                    .modal(),
                )
            },
        ),
        Scene::new(
            "modal-git-diff-empty",
            "Staging panel: a file with no differences",
            "modal",
            ["(no differences)"],
            |_| {
                opens(
                    GitPanel::new(crate::git::Pane::Diff {
                        path: "docs/snapshots.md".into(),
                        text: String::new(),
                    })
                    .modal(),
                )
            },
        ),
        Scene::new(
            "modal-git-commit",
            "Staging panel: the commit pane",
            "modal",
            [
                "git commit",
                "staged for this commit:",
                "message:",
                "(⏎ commits the staged set · esc back)",
            ],
            |_| opens(GitPanel::new(crate::git::Pane::Commit).modal()),
        ),
        Scene::new(
            "modal-git-commit-draft",
            "Staging panel: typing the commit message",
            "modal",
            ["git commit", "message: fix(tui):"],
            |_| {
                opens(
                    GitPanel {
                        draft: "fix(tui): keep the jobs pill in a reserved slot".into(),
                        note: Some("committed 3 files as 7d1e3b5 — working tree clean".into()),
                        ..GitPanel::new(crate::git::Pane::Commit)
                    }
                    .modal(),
                )
            },
        ),
    ]
}

/// `git status --porcelain` as the panel lists it.
fn git_files() -> Vec<crate::git::GitFile> {
    crate::git::parse_porcelain(GIT_STATUS)
}

/// The staging panel's fields, as `Modal::Git` carries them.
struct GitPanel {
    files: Vec<crate::git::GitFile>,
    selected: usize,
    pane: crate::git::Pane,
    scroll: Option<usize>,
    draft: String,
    keys_shown: bool,
    note: Option<String>,
}

impl GitPanel {
    /// Over [`GIT_STATUS`] with `pane` open, three files in the index.
    fn new(pane: crate::git::Pane) -> Self {
        Self {
            files: git_files(),
            selected: 3,
            pane,
            scroll: None,
            draft: String::new(),
            keys_shown: false,
            note: None,
        }
    }

    fn modal(self) -> Modal {
        Modal::Git {
            files: self.files,
            selected: self.selected,
            pane: self.pane,
            scroll: self.scroll,
            draft: self.draft,
            keys_shown: self.keys_shown,
            note: self.note,
        }
    }
}

/// Debug session roster rows in the engine's format.
fn debug_rows() -> Vec<String> {
    (1..=12)
        .map(|n| {
            format!(
                "dap {n}  {:7}  breakpoints in {} file(s)",
                if n % 3 == 0 { "live" } else { "running" },
                n % 4
            )
        })
        .collect()
}

fn transcript_scenes() -> Vec<Scene> {
    vec![
        Scene::new(
            "transcript-turn",
            "Transcript: a user turn, a collapsed thinking block, tool cards",
            "transcript",
            [
                "▌ Add a snapshot harness",
                "Thinking",
                "⋯ +2 more",
                "✓ ≡ read",
                "crates/ka-term/src/tui.rs",
            ],
            |(w, _)| showing(w, all_kinds(), is_user),
        ),
        Scene::new(
            "transcript-thinking-collapsed",
            "Transcript: a foldable thinking block, collapsed",
            "transcript",
            ["✻ Thinking ▸", "5 lines", "⋯ +2 more"],
            |(w, _)| showing(w, all_kinds(), is_thought),
        ),
        Scene::new(
            "transcript-thinking-open",
            "Transcript: the same block expanded (alt+t or a click)",
            "transcript",
            ["✻ Thinking ▾", "Check the status_segments tests"],
            |(w, _)| {
                let mut state = showing(w, all_kinds(), is_thought);
                let thought = entry(&state.transcript, is_thought);
                state.transcript.toggle_thought(thought);
                state
            },
        ),
        Scene::new(
            "transcript-tools",
            "Transcript: four finished tool calls",
            "transcript",
            ["✓ ≡ read", "✎ edit", "✗ $ bash", "⏎ exit 101", "41.9s", "▸"],
            |(w, _)| showing(w, all_kinds(), is_tool_block),
        ),
        Scene::new(
            "transcript-tool-expanded",
            "Transcript: tool calls expanded in place (ctrl+o or a click)",
            "transcript",
            ["▾", "pub enum Line {", "full output parked in a spill file"],
            |(w, _)| {
                let mut state = showing(w, all_kinds(), is_tool_block);
                let tools = entry(&state.transcript, is_tool_block);
                state.transcript.toggle_tool_call(tools, 0);
                state.transcript.toggle_tool_call(tools, 3);
                state
            },
        ),
        Scene::new(
            "transcript-markdown",
            "Transcript: assistant markdown (heading, list, table, code, quote, link)",
            "transcript",
            ["Findings", "job_pill", "segment"],
            |(w, _)| showing(w, all_kinds(), is_assistant),
        ),
        Scene::new(
            "transcript-notes",
            "Transcript: notes, a warning, an error, shell output, a report, a verdict",
            "transcript",
            [
                "• agents:",
                "▲ config:",
                "✗ provider:",
                "» ls — output joins",
                "✓ done ·",
            ],
            |(w, _)| showing(w, all_kinds(), is_note),
        ),
        Scene::new(
            "transcript-verdicts",
            "Transcript: turn verdicts (done, aborted, failed)",
            "transcript",
            ["✓ done ·", "◐ aborted · partial kept", "✗ failed ·"],
            |(w, _)| showing(w, all_kinds(), is_verdict),
        ),
        Scene::new(
            "transcript-verdict-expanded",
            "Transcript: a failed turn's detail expanded (a click)",
            "transcript",
            ["✗ failed ·", "▾", "Retry-After"],
            |(w, _)| {
                let mut state = showing(w, all_kinds(), is_failed);
                let failed = entry(&state.transcript, is_failed);
                state.transcript.toggle_summary(failed);
                state
            },
        ),
        Scene::new(
            "transcript-unicode",
            "Transcript: CJK, emoji, and combining marks",
            "transcript",
            ["日志里的中文", "👍🏽", "café", "한국어"],
            |_| RenderInput {
                transcript: transcript(vec![
                    Line::User(UNICODE_USER.into()),
                    Line::Assistant(UNICODE_KA.into()),
                ]),
                ..base()
            },
        ),
        Scene::new(
            "transcript-long-token",
            "Transcript: a very long unbroken token",
            "transcript",
            [
                "https://github.com/jaltez/ka-harness/pull/1234/files#diff-7d1e",
                "sha256",
            ],
            |_| RenderInput {
                transcript: long_token(),
                ..base()
            },
        ),
        Scene::new(
            "transcript-focus",
            "Transcript: the /focus view (finished turns as one digest row)",
            "transcript",
            ["· "],
            |(w, _)| {
                let mut t = long_session();
                t.set_width(transcript_width(w));
                t.set_focus(true);
                RenderInput {
                    transcript: t,
                    ..base()
                }
            },
        ),
    ]
}

fn status_scenes() -> Vec<Scene> {
    vec![
        Scene::new(
            "status-default",
            "Status line: the default order",
            "status",
            ["zai/glm-5.3", "high", "$0.1842", "allow", "6%"],
            |_| base(),
        ),
        Scene::new(
            "status-custom-order",
            "Status line: a custom [tui] status_line order",
            "status",
            ["7d1e3b5a", "in ·", "cache 71%"],
            |_| RenderInput {
                status_order: Some(ids(&[
                    "mode", "model", "session", "effort", "context", "tokens", "cache", "cost",
                    "toggles", "update", "jobs",
                ])),
                ..base()
            },
        ),
        Scene::new(
            "status-jobs-pill",
            "Status line: the jobs pill and the tan count",
            "status",
            ["2 tasks · 1 job · 1 dap", "1 dap"],
            |_| RenderInput {
                meters: Meters {
                    jobs: JobsPill {
                        tasks: 2,
                        jobs: 1,
                        dap: 1,
                    },
                    tan: 2,
                    ..live_meters()
                },
                ..base()
            },
        ),
        Scene::new(
            "status-update-badge",
            "Status line: the update badge and the toggle count",
            "status",
            ["⤴ v0.9.1", "⊘2"],
            |_| RenderInput {
                meters: Meters {
                    update: Some("v0.9.1".into()),
                    toggles: 2,
                    ..live_meters()
                },
                ..base()
            },
        ),
        Scene::new(
            "status-badges",
            "Status line: goal, loop, and style badges",
            "status",
            ["◎ goal (paused)", "↻ loop", "◎ goal"],
            |_| RenderInput {
                meters: Meters {
                    goal: Some(GoalState {
                        condition: "every scene passes at 80x24".into(),
                        paused: true,
                        budget_usd: Some(5.0),
                    }),
                    loop_task: Some(LoopState {
                        task: "rerun the snapshot assertions".into(),
                        interval_secs: Some(30),
                    }),
                    style: Some("engineer".into()),
                    ..live_meters()
                },
                ..base()
            },
        ),
        Scene::new(
            "status-crowded",
            "Status line: a long model id with every badge and the jobs pill",
            "status",
            ["openrouter/z-ai/glm-5.3-flash"],
            |_| RenderInput {
                meters: Meters {
                    model: "openrouter/z-ai/glm-5.3-flash".into(),
                    effort: "max".into(),
                    context: (731_884, 1_000_000),
                    cost: 1.2047,
                    cache_hit: Some(0.12),
                    tokens_in: 702_110,
                    tokens_out: 29_774,
                    toggles: 3,
                    update: Some("v0.9.1".into()),
                    goal: Some(GoalState {
                        condition: "every scene passes at 80x24".into(),
                        paused: false,
                        budget_usd: None,
                    }),
                    loop_task: Some(LoopState {
                        task: "rerun the snapshot assertions".into(),
                        interval_secs: Some(30),
                    }),
                    style: Some("engineer".into()),
                    jobs: JobsPill {
                        tasks: 2,
                        jobs: 1,
                        dap: 1,
                    },
                    tan: 1,
                    ..live_meters()
                },
                ..base()
            },
        ),
        Scene::new(
            "status-bootstrap",
            "Status line: before the engine's first event",
            "status",
            ["$0.0000"],
            |_| RenderInput {
                meters: Meters::default(),
                ..base()
            },
        ),
    ]
}

fn header_scenes() -> Vec<Scene> {
    vec![
        Scene::new(
            "header-untitled",
            "Header: no title yet (\"new session\")",
            "header",
            ["new session"],
            |_| RenderInput {
                sidebar: SidebarState {
                    title: None,
                    branch: None,
                    ..live_sidebar()
                },
                ..base()
            },
        ),
        Scene::new(
            "header-titled",
            "Header: a titled session",
            "header",
            ["Fix the status line jump"],
            |_| RenderInput {
                sidebar: SidebarState {
                    title: Some("Fix the status line jump".into()),
                    branch: None,
                    ..live_sidebar()
                },
                ..base()
            },
        ),
        Scene::new(
            "header-titled-branch",
            "Header: a titled session with its cwd and branch",
            "header",
            [
                "Fix the status line jump",
                "…/projects/ka-harness",
                "⎇ main",
                "▲ ▼",
            ],
            |_| titled(),
        ),
        Scene::new(
            "header-long-title",
            "Header: a long title, a deep cwd, and a branch at every width",
            "header",
            ["Snapshot every surface of the TUI so the audit can size them"],
            |_| RenderInput {
                sidebar: SidebarState {
                    title: Some(
                        "Snapshot every surface of the TUI so the audit can size them".into(),
                    ),
                    cwd: "…/projects/ka-harness/crates/ka-term".into(),
                    ..live_sidebar()
                },
                ..base()
            },
        ),
        Scene::new(
            "header-custom-glyph",
            "Header: a configured header glyph ([tui] header_glyph)",
            "header",
            ["✦ ka"],
            |_| RenderInput {
                header_glyph: "✦",
                ..titled()
            },
        ),
    ]
}

fn plain_scenes() -> Vec<Scene> {
    vec![
        Scene::new(
            "plain-empty",
            "Plain view: a fresh session",
            "plain",
            ["ka ", "plain view — send a prompt to begin"],
            |_| RenderInput {
                plain_mode: true,
                ..fresh()
            },
        ),
        Scene::new(
            "plain-turn",
            "Plain view: a turn from its first row",
            "plain",
            [
                "user: ",
                "thinking: ",
                "tool: read crates/ka-term/src/tui.rs → ok",
            ],
            |_| plain_showing(all_kinds(), is_user),
        ),
        Scene::new(
            "plain-notes",
            "Plain view: notes, a warning, an error, shell output, a report, a verdict",
            "plain",
            ["note: ", "warning: ", "error: ", "shell: ", "done"],
            |_| plain_showing(all_kinds(), is_note),
        ),
        Scene::new(
            "plain-verdicts",
            "Plain view: turn verdicts",
            "plain",
            ["done", "aborted", "failed"],
            |_| plain_showing(all_kinds(), is_verdict),
        ),
        Scene::new(
            "plain-busy",
            "Plain view: a turn in progress",
            "plain",
            [
                "thinking: ",
                "tool: bash cargo test -p ka-term --lib → running",
                "working · ",
            ],
            |_| RenderInput {
                plain_mode: true,
                ..busy_everything()
            },
        ),
        Scene::new(
            "plain-ask",
            "Plain view: a permission ask",
            "plain",
            ["allow edit to modify files?", "esc denies"],
            |_| {
                let diff = ka_engine::hands::unified_diff(
                    "crates/ka-term/src/tui.rs",
                    EDIT_OLD,
                    EDIT_NEW,
                    24,
                );
                RenderInput {
                    plain_mode: true,
                    ..asking(ask("allow edit to modify files?", Some(diff), 0))
                }
            },
        ),
        Scene::new(
            "plain-picker-mode",
            "Plain view: the /mode tiers",
            "plain",
            [
                "ask — ask before every edit and command",
                "allow — no prompts",
            ],
            |_| RenderInput {
                plain_mode: true,
                picker: Some(InputPicker::Mode(ModePicker::for_mode(Mode::Ask))),
                ..base()
            },
        ),
        Scene::new(
            "plain-picker-thinking",
            "Plain view: the /thinking levels",
            "plain",
            ["> high", " max"],
            |_| RenderInput {
                plain_mode: true,
                picker: Some(InputPicker::Thinking(ThinkingPicker::for_effort(
                    Some(Effort::High),
                    &ids(&["low", "high", "max"]),
                ))),
                ..base()
            },
        ),
        Scene::new(
            "plain-rsearch",
            "Plain view: reverse history search",
            "plain",
            ["search: `cargo`", "cargo fmt --all"],
            |_| RenderInput {
                plain_mode: true,
                input: rsearch("cargo"),
                ..titled()
            },
        ),
        Scene::new(
            "plain-vim",
            "Plain view: the vim mode tag",
            "plain",
            ["[vim: NORMAL]"],
            |_| RenderInput {
                plain_mode: true,
                input: vim("add a vim label scene", crate::vim::Mode::Normal),
                ..titled()
            },
        ),
        Scene::new(
            "plain-draft",
            "Plain view: a multi-line draft",
            "plain",
            ["> first line", "  third line"],
            |_| RenderInput {
                plain_mode: true,
                input: draft("first line\nthird line"),
                ..titled()
            },
        ),
    ]
}

/// Every modal scene again through the screen-reader renderer: plain
/// mode has its own text per modal, and its first row names the modal.
fn plain_modals(modals: &[Scene]) -> Vec<Scene> {
    modals
        .iter()
        .map(|scene| {
            let decorated = scene.build.clone();
            let state = decorated((80, 24));
            let modal = state
                .modal
                .clone()
                .unwrap_or_else(|| panic!("{} opens no modal", scene.id));
            // the text file trims trailing spaces, so the header's
            // trailing padding is not part of what a reader can find
            let first = format!(
                "── {}",
                crate::plain::modal_lines(&modal, 64, &state.sidebar).0
            );
            Scene::new(
                &format!("plain-{}", scene.id),
                &format!("{} (plain view)", scene.name),
                "plain",
                [first],
                move |size| RenderInput {
                    plain_mode: true,
                    ..decorated(size)
                },
            )
        })
        .collect()
}

// ── the tests ────────────────────────────────────────────────────

/// One frame of `scene` at `size`: the buffer `render` painted, and
/// what makes it unusable — each `must_contain` string missing from
/// its text, and, for an overlay, each bare selection band.
fn shoot(scene: &Scene, size: (u16, u16)) -> (Buffer, Vec<String>) {
    let buf = draw(&mut (scene.build)(size), size);
    let text = frame_text(&buf);
    let mut unusable: Vec<String> = scene
        .must_contain
        .iter()
        .filter(|needle| !text.contains(needle.as_str()))
        .map(|needle| format!("missing {needle:?}"))
        .collect();
    if matches!(scene.category, "ask" | "modal") {
        unusable.extend(bare_selection_bands(&buf));
    }
    (buf, unusable)
}

/// Renders every scene at every size and writes the snapshot files.
#[test]
#[ignore = "writes the snapshot files under KA_SNAP_DIR: run with --ignored"]
fn snapshots_dump() {
    git_fixture();
    let dir = snap_dir();
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("create {}: {e}", dir.display()));
    clear(&dir);
    let mut index = String::from("id\tname\tcategory\tsize\tfound\tmissing\n");
    for scene in scenes() {
        for size in SIZES.into_iter().chain([STRESS]) {
            let (buf, unusable) = shoot(&scene, size);
            let stem = format!("{}@{}x{}", scene.id, size.0, size.1);
            let text = frame_text(&buf);
            write(&dir.join(format!("{stem}.txt")), &format!("{text}\n"));
            write(&dir.join(format!("{stem}.ansi")), &frame_ansi(&buf));
            let found: Vec<&str> = scene
                .must_contain
                .iter()
                .map(String::as_str)
                .filter(|needle| text.contains(*needle))
                .collect();
            let missing = unusable.join(" | ");
            let fields = [
                scene.id.as_str(),
                &scene.name,
                scene.category,
                &format!("{}x{}", size.0, size.1),
                &found.join(" | "),
                &missing,
            ];
            index.push_str(
                &fields
                    .iter()
                    .map(|field| tsv(field))
                    .collect::<Vec<_>>()
                    .join("\t"),
            );
            index.push('\n');
        }
    }
    write(&dir.join("index.tsv"), &index);
}

/// Every scene keeps, at every asserted size, what makes it usable.
#[test]
fn snapshots_stay_usable() {
    git_fixture();
    let mut unusable = Vec::new();
    for scene in scenes() {
        for size in SIZES {
            let (_, problems) = shoot(&scene, size);
            if !problems.is_empty() {
                unusable.push(format!(
                    "{} ({}) at {}x{}: {}",
                    scene.id,
                    scene.name,
                    size.0,
                    size.1,
                    problems.join("; ")
                ));
            }
        }
    }
    assert!(
        unusable.is_empty(),
        "{} frame(s) lost what makes them usable:\n{}",
        unusable.len(),
        unusable.join("\n")
    );
}
