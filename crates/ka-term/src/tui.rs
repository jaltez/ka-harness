//! The ka TUI: streaming transcript, input editor, footer meters, and ask
//! dialogs. Built on ratatui; talks to the engine exclusively through the
//! Command/Event queues.

use std::collections::VecDeque;
use std::io::Write;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use ka_protocol::{AskId, Command, Event, ReplayedMessage};
use tokio::sync::mpsc;

/// How the app decided to exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// User quit.
    Quit,
    /// Engine ended.
    EngineEnded,
    /// `/update`: hand the session to the updater, which re-execs ka
    /// attached to the same strand when done.
    Update {
        /// Strand id to re-attach after the update.
        session: String,
    },
}

/// Editable single-line-ish input buffer with history (pure logic, tested).
#[derive(Debug, Default, Clone)]
pub struct InputBuffer {
    /// Current text.
    pub text: String,
    /// Cursor position (char index).
    pub cursor: usize,
    history: VecDeque<String>,
    /// Index while browsing history; None = live edit.
    browsing: Option<usize>,
    /// Undo snapshots (text + cursor), most recent last; capped at 100.
    undo: Vec<(String, usize)>,
    /// Readline-style kill ring (single entry: kills REPLACE, not append).
    kill: Option<String>,
    /// Active reverse incremental search (Ctrl+R); None = not searching.
    pub search: Option<SearchState>,
}

/// State of a Ctrl+R reverse incremental history search. The search scans
/// `history` newest-first; the pre-search draft is snapshotted so Esc can
/// restore it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SearchState {
    /// Current query text.
    pub query: String,
    /// History index of the current match (None = no match yet).
    pub match_idx: Option<usize>,
    /// `(text, cursor)` captured when the search started.
    draft: (String, usize),
}

impl InputBuffer {
    /// Insert a character at the cursor.
    pub fn insert(&mut self, c: char) {
        self.push_undo();
        let byte = self.char_to_byte(self.cursor);
        self.text.insert(byte, c);
        self.cursor += 1;
        self.browsing = None;
    }

    /// Delete the character before the cursor.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.push_undo();
        let start = self.char_to_byte(self.cursor - 1);
        let end = self.char_to_byte(self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
        self.browsing = None;
    }

    /// Move the cursor left one char.
    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Move the cursor right one char.
    pub fn right(&mut self) {
        let len = self.text.chars().count();
        if self.cursor < len {
            self.cursor += 1;
        }
    }

    /// Move to the start.
    pub fn home(&mut self) {
        self.cursor = 0;
    }

    /// Move to the end.
    pub fn end(&mut self) {
        self.cursor = self.text.chars().count();
    }

    /// Insert a line break at the cursor.
    pub fn newline(&mut self) {
        self.push_undo();
        let byte = self.char_to_byte(self.cursor);
        self.text.insert(byte, '\n');
        self.cursor += 1;
        self.browsing = None;
    }

    /// Insert a string at the cursor; CRLF/CR normalized to `\n`.
    pub fn insert_str(&mut self, s: &str) {
        let normalized = s.replace("\r\n", "\n").replace('\r', "\n");
        if normalized.is_empty() {
            return;
        }
        self.push_undo();
        let byte = self.char_to_byte(self.cursor);
        self.text.insert_str(byte, &normalized);
        self.cursor += normalized.chars().count();
        self.browsing = None;
    }

    /// The input split into rows (one per source line).
    pub fn rows(&self) -> Vec<&str> {
        self.text.split('\n').collect()
    }

    /// Cursor position as `(row, column)` over `rows()`.
    pub fn cursor_row_col(&self) -> (usize, usize) {
        cursor_row_col(&self.text, self.cursor)
    }

    /// Move the cursor up one row (column clamped to that row's end).
    /// Returns `false` for single-line text — history-browse territory.
    pub fn move_up(&mut self) -> bool {
        if !self.text.contains('\n') {
            return false;
        }
        let (row, col) = self.cursor_row_col();
        self.place_cursor(row.saturating_sub(1), col);
        true
    }

    /// Move the cursor down one row (column clamped to that row's end).
    /// Returns `false` for single-line text — history-browse territory.
    pub fn move_down(&mut self) -> bool {
        if !self.text.contains('\n') {
            return false;
        }
        let (row, col) = self.cursor_row_col();
        self.place_cursor(row + 1, col);
        true
    }

    fn place_cursor(&mut self, row: usize, col: usize) {
        let mut char_idx = 0;
        for (i, r) in self.rows().iter().enumerate() {
            if i == row {
                char_idx += col.min(r.chars().count());
                self.cursor = char_idx;
                return;
            }
            char_idx += r.chars().count() + 1; // +1 for the newline
        }
        self.cursor = char_idx; // row past the end: clamp to text end
    }

    /// Take the current text (clearing the buffer).
    pub fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        if !text.trim().is_empty() {
            self.history.push_back(text.clone());
            if self.history.len() > 100 {
                self.history.pop_front();
            }
        }
        self.browsing = None;
        text
    }

    /// Seed the draft history from a resumed session's prompts (oldest
    /// first). Consecutive duplicates collapse, blank prompts are
    /// skipped, and the 100-entry cap holds by dropping the oldest.
    pub fn seed_history(&mut self, items: impl IntoIterator<Item = String>) {
        for item in items {
            if item.trim().is_empty() || self.history.back() == Some(&item) {
                continue;
            }
            self.history.push_back(item);
        }
        while self.history.len() > 100 {
            self.history.pop_front();
        }
    }

    /// Browse history upward (older).
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.browsing {
            None => self.history.len() - 1,
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.browsing = Some(idx);
        self.load_history(idx);
    }

    /// Browse history downward (newer).
    pub fn history_next(&mut self) {
        let Some(idx) = self.browsing else {
            return;
        };
        if idx + 1 >= self.history.len() {
            self.browsing = None;
            self.text.clear();
            self.cursor = 0;
        } else {
            self.browsing = Some(idx + 1);
            self.load_history(idx + 1);
        }
    }

    fn load_history(&mut self, idx: usize) {
        if let Some(entry) = self.history.get(idx) {
            self.text = entry.clone();
            self.cursor = self.text.chars().count();
        }
    }

    /// Snapshot (text, cursor) BEFORE a mutation so `undo` restores the
    /// state preceding the op. Cap at 100 entries (drop the oldest).
    /// Clear the whole draft (Ctrl+C on a non-empty input). Routed
    /// through the undo stack and kill ring like every destructive
    /// edit, so Ctrl+Z restores and Ctrl+Y yanks it back.
    pub fn clear_draft(&mut self) {
        if self.text.is_empty() {
            return;
        }
        self.push_undo();
        let killed = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.kill_push(Some(killed));
    }

    fn push_undo(&mut self) {
        if self.undo.len() >= 100 {
            self.undo.remove(0);
        }
        self.undo.push((self.text.clone(), self.cursor));
    }

    /// Delete the char AT the cursor (forward delete); no-op at the end.
    pub fn delete_forward(&mut self) {
        let len = self.text.chars().count();
        if self.cursor >= len {
            return;
        }
        self.push_undo();
        let start = self.char_to_byte(self.cursor);
        let end = self.char_to_byte(self.cursor + 1);
        self.text.replace_range(start..end, "");
        self.browsing = None;
    }

    /// Move the cursor to the start of the current line.
    pub fn line_home(&mut self) {
        let mut start = 0;
        for (i, c) in self.text.chars().enumerate() {
            if i >= self.cursor {
                break;
            }
            if c == '\n' {
                start = i + 1;
            }
        }
        self.cursor = start;
    }

    /// Move the cursor to the end of the current line.
    pub fn line_end(&mut self) {
        let mut end = self.text.chars().count();
        for (i, c) in self.text.chars().enumerate().skip(self.cursor) {
            if c == '\n' {
                end = i;
                break;
            }
        }
        self.cursor = end;
    }

    /// Move the cursor back one word (whitespace-delimited).
    pub fn word_left(&mut self) {
        let mut i = self.cursor;
        while i > 0 && word_ws(self.text.chars().nth(i - 1)) {
            i -= 1;
        }
        while i > 0 && !word_ws(self.text.chars().nth(i - 1)) {
            i -= 1;
        }
        self.cursor = i;
    }

    /// Move the cursor forward one word (whitespace-delimited).
    pub fn word_right(&mut self) {
        let len = self.text.chars().count();
        let mut i = self.cursor;
        while i < len && word_ws(self.text.chars().nth(i)) {
            i += 1;
        }
        while i < len && !word_ws(self.text.chars().nth(i)) {
            i += 1;
        }
        self.cursor = i;
    }

    /// Delete the word before the cursor; returns the deleted text.
    pub fn delete_word_backward(&mut self) -> Option<String> {
        let mut target = self.cursor;
        while target > 0 && word_ws(self.text.chars().nth(target - 1)) {
            target -= 1;
        }
        while target > 0 && !word_ws(self.text.chars().nth(target - 1)) {
            target -= 1;
        }
        if target == self.cursor {
            return None;
        }
        self.push_undo();
        let deleted: String = self
            .text
            .chars()
            .skip(target)
            .take(self.cursor - target)
            .collect();
        self.delete_char_range(target, self.cursor);
        self.cursor = target;
        Some(deleted)
    }

    /// Delete the word after the cursor; returns the deleted text.
    pub fn delete_word_forward(&mut self) -> Option<String> {
        let len = self.text.chars().count();
        let mut target = self.cursor;
        while target < len && word_ws(self.text.chars().nth(target)) {
            target += 1;
        }
        while target < len && !word_ws(self.text.chars().nth(target)) {
            target += 1;
        }
        if target == self.cursor {
            return None;
        }
        self.push_undo();
        let deleted: String = self
            .text
            .chars()
            .skip(self.cursor)
            .take(target - self.cursor)
            .collect();
        self.delete_char_range(self.cursor, target);
        Some(deleted)
    }

    /// Delete from the start of the current line to the cursor; returns
    /// the deleted text. No-op (None) when already at the line start.
    pub fn delete_to_line_start(&mut self) -> Option<String> {
        let mut start = 0;
        for (i, c) in self.text.chars().enumerate() {
            if i >= self.cursor {
                break;
            }
            if c == '\n' {
                start = i + 1;
            }
        }
        if start == self.cursor {
            return None;
        }
        self.push_undo();
        let deleted: String = self
            .text
            .chars()
            .skip(start)
            .take(self.cursor - start)
            .collect();
        self.delete_char_range(start, self.cursor);
        self.cursor = start;
        Some(deleted)
    }

    /// Delete from the cursor to the end of the current line (not
    /// including the `\n`); returns the deleted text. No-op (None) when
    /// already at the line end.
    pub fn delete_to_line_end(&mut self) -> Option<String> {
        let mut end = self.text.chars().count();
        for (i, c) in self.text.chars().enumerate().skip(self.cursor) {
            if c == '\n' {
                end = i;
                break;
            }
        }
        if end == self.cursor {
            return None;
        }
        self.push_undo();
        let deleted: String = self
            .text
            .chars()
            .skip(self.cursor)
            .take(end - self.cursor)
            .collect();
        self.delete_char_range(self.cursor, end);
        Some(deleted)
    }

    /// Undo the last text mutation, restoring the pre-op snapshot.
    pub fn undo(&mut self) {
        if let Some((text, cursor)) = self.undo.pop() {
            self.text = text;
            self.cursor = cursor;
            self.browsing = None;
        }
    }

    /// Store deleted text into the kill slot. Deliberate simplification
    /// of readline's consecutive-kill append: every kill REPLACES.
    pub fn kill_push(&mut self, deleted: Option<String>) {
        if let Some(d) = deleted {
            self.kill = Some(d);
        }
    }

    /// Insert the kill-slot text at the cursor (insert_str semantics).
    pub fn yank(&mut self) {
        if let Some(k) = self.kill.clone() {
            self.insert_str(&k);
        }
    }

    /// Delete the char range `[start..end)` in CHAR indices, clearing
    /// `browsing` (text changed).
    fn delete_char_range(&mut self, start: usize, end: usize) {
        let byte_s = self.char_to_byte(start);
        let byte_e = self.char_to_byte(end);
        self.text.replace_range(byte_s..byte_e, "");
        self.browsing = None;
    }

    fn char_to_byte(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.text.len())
    }

    /// Start a reverse incremental search over `history` (newest first),
    /// snapshotting the current draft so [`InputBuffer::search_cancel`]
    /// can restore it. No-op while already searching.
    pub fn search_start(&mut self) {
        if self.search.is_some() {
            return;
        }
        self.search = Some(SearchState {
            query: String::new(),
            match_idx: None,
            draft: (self.text.clone(), self.cursor),
        });
    }

    /// Whether a reverse search is active.
    pub fn searching(&self) -> bool {
        self.search.is_some()
    }

    /// Type a character into the search query and jump to the newest
    /// matching history entry. No-op when not searching; an empty match
    /// set leaves the buffer untouched.
    pub fn search_push(&mut self, c: char) {
        let query = match self.search.as_mut() {
            Some(s) => {
                s.query.push(c);
                s.query.clone()
            }
            None => return,
        };
        let idx = self.find_match(&query, self.history.len());
        if let Some(s) = self.search.as_mut() {
            s.match_idx = idx;
        }
        self.show_match();
    }

    /// Step to the next OLDER match for the current query (Ctrl+R again).
    /// Stays on the current match when the history is exhausted; no-op
    /// when not searching or the query is still empty.
    pub fn search_next(&mut self) {
        let Some(s) = self.search.clone() else {
            return;
        };
        if s.query.is_empty() {
            return;
        }
        let from = s.match_idx.unwrap_or(self.history.len());
        if let Some(idx) = self.find_match(&s.query, from) {
            if let Some(live) = self.search.as_mut() {
                live.match_idx = Some(idx);
            }
            self.show_match();
        }
    }

    /// Remove the last query character and re-match from the newest
    /// entry. No-op when not searching or the query is empty.
    pub fn search_backspace(&mut self) {
        let query = match self.search.as_mut() {
            Some(s) => {
                if s.query.pop().is_none() {
                    return;
                }
                s.query.clone()
            }
            None => return,
        };
        let idx = self.find_match(&query, self.history.len());
        if let Some(s) = self.search.as_mut() {
            s.match_idx = idx;
        }
        self.show_match();
    }

    /// Accept the current match: exits search mode, leaving the matched
    /// history entry loaded in the buffer. No-op when not searching.
    pub fn search_accept(&mut self) {
        self.search = None;
    }

    /// Cancel the search and restore the pre-search draft. No-op when
    /// not searching.
    pub fn search_cancel(&mut self) {
        if let Some(s) = self.search.take() {
            self.text = s.draft.0;
            self.cursor = s.draft.1;
        }
    }

    /// The live query line for the input box title, e.g.
    /// ``rsearch: `do` `` or ``rsearch: `do` (no match)``.
    pub fn search_title(&self) -> Option<String> {
        self.search.as_ref().map(|s| {
            let miss = if s.match_idx.is_none() && !s.query.is_empty() {
                " (no match)"
            } else {
                ""
            };
            format!("rsearch: `{}`{miss}", s.query)
        })
    }

    /// Newest-first history index strictly above `from` whose entry
    /// contains `query`. Empty queries never match.
    fn find_match(&self, query: &str, from: usize) -> Option<usize> {
        if query.is_empty() {
            return None;
        }
        (0..from.min(self.history.len()))
            .rev()
            .find(|&i| self.history[i].contains(query))
    }

    /// Load the current match into the buffer (cursor at end); a missed
    /// query leaves the buffer as-is.
    fn show_match(&mut self) {
        let idx = self.search.as_ref().and_then(|s| s.match_idx);
        if let Some(i) = idx {
            self.load_history(i);
        }
    }
}

/// The resumed session's user prompts, oldest first — the seed for
/// ↑/↓ draft history. Assistant rows are not prompts and compaction
/// digests are not user input; blank prompts are dropped later by
/// [`InputBuffer::seed_history`].
fn replay_user_prompts(messages: &[ReplayedMessage]) -> impl Iterator<Item = String> + '_ {
    messages
        .iter()
        .filter(|m| m.role == "user" && !m.digest)
        .map(|m| m.content.clone())
}

/// Whitespace for word navigation: space, newline, tab.
fn word_ws(c: Option<char>) -> bool {
    matches!(c, Some(' ' | '\n' | '\t'))
}

/// Transcript line kinds for rendering.
#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    /// User input.
    User(String),
    /// Assistant text.
    Assistant(String),
    /// Reasoning (rendered dim).
    Thought(String),
    /// One or more consecutive tool calls, each a timeline row
    /// (status, tool, detail) with its threaded body.
    ToolBlock(Vec<ToolCall>),
    /// System/status note (`• ` gutter).
    Info(String),
    /// Caution row (`▲ ` gutter): refused actions, usage errors.
    Warn(String),
    /// Failure row (`✗ ` gutter, bold): the loudest system tier.
    Err(String),
    /// Turn report: empty = a blank separator row, else `─ {msg}`.
    Report(String),
    /// `!cmd` passthrough output: plain foreground, no gutter.
    Shell(String),
    /// One-row turn verdict (`✓ done · 2.1s · 1.2k in · $0.0042`).
    /// A failure longer than the inline budget keeps its full text as
    /// `detail`; the row then folds (▸/▾) — click to expand.
    Summary {
        glyph: char,
        tone: SummaryTone,
        text: String,
        /// Untruncated error text, shown wrapped when expanded; None =
        /// nothing hidden (no fold affordance, no chevron).
        detail: Option<String>,
        /// Fold state of the detail body (click toggles).
        expanded: bool,
    },
}

/// Tone of a [`Line::Summary`] verdict: colors glyph+text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryTone {
    Ok,
    Warn,
    Err,
}

/// Click target of one rendered transcript row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowRef {
    /// The whole entry (thinking blocks toggle on click).
    Entry(usize),
    /// One call within a [`Line::ToolBlock`]: `(entry, call)`.
    ToolCall(usize, usize),
    /// A verdict summary carrying hidden detail (folds on click).
    Summary(usize),
}

/// One finished tool call inside a [`Line::ToolBlock`] row.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    /// `→ {tool} · {detail}` header (the live band's header verbatim).
    pub head: String,
    /// Whether the call finished successfully.
    pub ok: bool,
    /// First non-blank output line, capped at 40 columns.
    pub note: String,
    /// Full output excerpt from CallOutput (already capped upstream).
    pub excerpt: String,
    /// Spill-file path from CallOutput, when the engine spilled.
    pub spill: Option<String>,
    /// Local elapsed CallStarted→CallFinished; `None` when unknown
    /// (replayed turns carry no durations).
    pub dur: Option<f64>,
    /// Whether the full excerpt is expanded inline (fold state).
    pub expanded: bool,
}

/// A provider row for the settings panel (built by the CLI from the
/// ka-dialect registry; ka-term stays dialect-free).
#[derive(Debug, Clone)]
pub struct ProviderInfo {
    /// Vendor prefix.
    pub name: String,
    /// Env var holding the API key (empty = keyless).
    pub env_var: String,
    /// Endpoint base URL.
    pub base_url: String,
    /// Whether the env var is set in this process.
    pub key_set: bool,
}

/// Transcript with a render cache. Entries are immutable once pushed, so
/// each is rendered (role blocks + markdown) exactly once per terminal
/// width; resizing rebuilds. Without the cache every frame re-parsed the
/// whole transcript — O(transcript) work per streaming delta.
#[derive(Debug, Default)]
pub struct Transcript {
    lines: Vec<Line>,
    rendered: Vec<Vec<ratatui::text::Line<'static>>>,
    width: u16,
    /// Thinking blocks expanded? Collapsed entries render their first
    /// line plus a count marker; Alt+T toggles.
    thoughts_open: bool,
    /// Entry indices toggled against `thoughts_open` — per-block
    /// click-to-collapse for individual thinking blocks.
    thought_overrides: std::collections::HashSet<usize>,
    /// Entry indices still rendering expanded because they streamed
    /// live — they collapse once fully out of view or at the next
    /// turn's boundary (see [`Transcript::collapse_streamed_above`]).
    streamed_open: Vec<usize>,
    /// Render passes issued (cache-audit in tests).
    renders: usize,
}

impl Transcript {
    /// Append an entry; renders it at the current width.
    pub fn push(&mut self, line: Line) {
        let w = self.width;
        let idx = self.rendered.len();
        self.rendered
            .push(render_line(&line, w, self.thought_open(idx)));
        self.lines.push(line);
        self.renders += 1;
    }

    /// Adopt a new terminal width, rebuilding the cache if it changed.
    pub fn set_width(&mut self, width: u16) {
        if width != self.width {
            self.width = width;
            self.rebuild();
        }
    }

    /// Expand/collapse thinking blocks, rebuilding the cache on change.
    /// Effective open state for one entry (global XOR per-block override).
    fn thought_open(&self, entry: usize) -> bool {
        self.thoughts_open ^ self.thought_overrides.contains(&entry)
    }

    /// Toggle one thinking block (click target). Only blocks taller
    /// than the collapsed band participate — a shorter one has nothing
    /// to hide, so toggling it would look broken.
    pub fn toggle_thought(&mut self, entry: usize) {
        let foldable =
            matches!(self.lines.get(entry), Some(Line::Thought(t)) if thought_foldable(t));
        if !foldable {
            return;
        }
        if !self.thought_overrides.remove(&entry) {
            self.thought_overrides.insert(entry);
        }
        if let Some(line) = self.lines.get(entry).cloned() {
            let w = self.width;
            let open = self.thought_open(entry);
            if let Some(slot) = self.rendered.get_mut(entry) {
                *slot = render_line(&line, w, open);
            }
        }
    }

    /// Flush one streamed thinking block into the cache, rendering it
    /// as the live tail band — spacer, head, the last three lines,
    /// spacer — row-for-row the live rows it replaces (minus the live
    /// head's shimmer), so the swap never jumps the viewport. The
    /// entry joins `streamed_open` and settles to the ordinary
    /// collapsed band later: once fully scrolled out of view
    /// ([`Transcript::collapse_streamed_above`]) or at the next turn
    /// boundary ([`Transcript::collapse_streamed_all`]).
    pub fn push_thought_streamed(&mut self, text: String) {
        // no separator can land here: separate_before skips Thought on
        // either side (bands ship their own spacer rows), so the entry
        // index is unconditionally lines.len()
        let idx = self.lines.len();
        // effective-open regardless of the global flag: the override
        // bit flips the default (closed) state, so it is only needed
        // when the global flag is closed
        if !self.thoughts_open {
            self.thought_overrides.insert(idx);
        }
        self.streamed_open.push(idx);
        let w = self.width;
        let tail = thought_rows_tail(&text, w as usize, None);
        self.lines.push(Line::Thought(text));
        self.rendered.push(tail);
        self.renders += 1;
    }

    /// Force one entry to its effective-closed render (override bit
    /// equal to the global flag), rebuilding just its cached rows.
    fn close_streamed(&mut self, idx: usize) {
        if self.thoughts_open {
            self.thought_overrides.insert(idx);
        } else {
            self.thought_overrides.remove(&idx);
        }
        if let Some(line) = self.lines.get(idx).cloned() {
            let w = self.width;
            let open = self.thought_open(idx);
            if let Some(slot) = self.rendered.get_mut(idx) {
                *slot = render_line(&line, w, open);
                self.renders += 1;
            }
        }
    }

    /// Collapse streamed-open thinking blocks whose rendered row span
    /// ends strictly before `first_visible_row` — the height change is
    /// invisible, so the viewport never jumps. Entries still (partly)
    /// visible stay expanded; indices past a rewind are dropped.
    pub fn collapse_streamed_above(&mut self, first_visible_row: usize) {
        if self.streamed_open.is_empty() {
            return;
        }
        let mut starts = Vec::with_capacity(self.lines.len());
        let mut at = 0usize;
        for rows in &self.rendered {
            starts.push(at);
            at += rows.len();
        }
        let mut keep = Vec::new();
        for idx in std::mem::take(&mut self.streamed_open) {
            if let Some(&s) = starts.get(idx) {
                let end = s + self.rendered[idx].len();
                if end > first_visible_row {
                    keep.push(idx);
                } else {
                    self.close_streamed(idx);
                }
            } // an index past a rewind is dropped
        }
        self.streamed_open = keep;
    }

    /// Collapse every streamed-open thinking block (turn boundary: the
    /// next turn's user prompt masks the height change).
    pub fn collapse_streamed_all(&mut self) {
        for idx in std::mem::take(&mut self.streamed_open) {
            if self.lines.get(idx).is_some() {
                self.close_streamed(idx);
            }
        }
    }

    /// What a rendered row belongs to (click mapping): a plain entry
    /// (thought-toggle target) or one specific call inside a tool
    /// block (expand target, `(entry, call)`). Every row of a call —
    /// header and threaded body alike — maps to that call; calls stack
    /// with no rows between them.
    pub fn row_ref_at(&self, row: usize) -> Option<RowRef> {
        let mut acc = 0usize;
        for (i, (line, rows)) in self.lines.iter().zip(&self.rendered).enumerate() {
            let next = acc + rows.len();
            if row < next {
                return Some(match line {
                    Line::Summary {
                        detail: Some(_), ..
                    } => RowRef::Summary(i),
                    Line::ToolBlock(calls) => {
                        let mut off = row - acc;
                        for (j, call) in calls.iter().enumerate() {
                            let h = tool_card_height(call, self.width);
                            if off < h {
                                return Some(RowRef::ToolCall(i, j));
                            }
                            off -= h;
                        }
                        RowRef::Entry(i)
                    }
                    _ => RowRef::Entry(i),
                });
            }
            acc = next;
        }
        None
    }

    /// Plain text of one rendered row (the drag-selection copy source):
    /// the row's spans concatenated verbatim. `None` past the end.
    pub fn row_text(&self, row: usize) -> Option<String> {
        let mut acc = 0usize;
        for rows in &self.rendered {
            let next = acc + rows.len();
            if row < next {
                let mut s = String::new();
                for span in &rows[row - acc].spans {
                    s.push_str(span.content.as_ref());
                }
                return Some(s);
            }
            acc = next;
        }
        None
    }

    /// One tool call by `(entry, call)` index (click-to-expand source).
    pub fn tool_call(&self, entry: usize, call: usize) -> Option<&ToolCall> {
        match self.lines.get(entry) {
            Some(Line::ToolBlock(calls)) => calls.get(call),
            _ => None,
        }
    }

    /// Toggle one call's inline expansion (click / Ctrl+O target) and
    /// re-render just that block. Indices that no longer point at a
    /// tool call (rewound transcript) no-op, and so does a call with
    /// nothing to reveal (no excerpt, no spill).
    pub fn toggle_tool_call(&mut self, entry: usize, call: usize) {
        let Some(Line::ToolBlock(calls)) = self.lines.get_mut(entry) else {
            return;
        };
        let Some(c) = calls.get_mut(call) else {
            return;
        };
        if c.excerpt.is_empty() && c.spill.is_none() {
            return;
        }
        c.expanded = !c.expanded;
        let line = self.lines[entry].clone();
        if let Some(slot) = self.rendered.get_mut(entry) {
            *slot = render_line(&line, self.width, false);
            self.renders += 1;
        }
    }

    /// Toggle a verdict summary's detail fold (click target) and
    /// re-render just that entry. Summaries without hidden detail
    /// no-op, like a card with nothing to reveal.
    pub fn toggle_summary(&mut self, entry: usize) {
        let Some(Line::Summary {
            detail: Some(_),
            expanded,
            ..
        }) = self.lines.get_mut(entry)
        else {
            return;
        };
        *expanded = !*expanded;
        let line = self.lines[entry].clone();
        if let Some(slot) = self.rendered.get_mut(entry) {
            *slot = render_line(&line, self.width, false);
            self.renders += 1;
        }
    }

    /// `(entry, call)` of the most recent call in the last block — the
    /// Ctrl+O target.
    pub fn last_tool_ref(&self) -> Option<(usize, usize)> {
        self.lines
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, l)| match l {
                Line::ToolBlock(v) if !v.is_empty() => Some((i, v.len() - 1)),
                _ => None,
            })
    }

    /// Append a finished tool call, merging into the trailing block
    /// when consecutive: a run of calls reads as one timeline; any
    /// other push between calls ends the group.
    pub fn push_tool_call(&mut self, call: ToolCall) {
        if let Some(Line::ToolBlock(v)) = self.lines.last_mut() {
            v.push(call);
            let idx = self.lines.len() - 1;
            if let Some(slot) = self.rendered.get_mut(idx) {
                let line = self.lines[idx].clone();
                *slot = render_line(&line, self.width, false);
                self.renders += 1;
            }
        } else {
            self.push_separated(Line::ToolBlock(vec![call]));
        }
    }

    pub fn set_thoughts_open(&mut self, open: bool) {
        if open != self.thoughts_open {
            self.thoughts_open = open;
            self.thought_overrides.clear();
            self.rebuild();
        }
    }

    fn rebuild(&mut self) {
        let w = self.width;
        self.rendered = self
            .lines
            .iter()
            .enumerate()
            .map(|(i, l)| render_line(l, w, self.thought_open(i)))
            .collect();
        self.renders += self.lines.len();
    }

    /// Drop everything (session switch).
    pub fn clear(&mut self) {
        self.lines.clear();
        self.rendered.clear();
        // per-entry thought bookkeeping is index-based: stale entries
        // would otherwise land on recycled indices after the rebuild
        self.thought_overrides.clear();
        self.streamed_open.clear();
    }

    /// Drop every entry from the `nth`-from-last user message onward
    /// (the visual half of `rewind n`). Returns the dropped prompt when
    /// the cut happened, `None` when there are fewer user messages.
    pub fn rewind_user(&mut self, nth_from_end: usize) -> Option<String> {
        let mut seen = 0usize;
        let mut cut = self.lines.len();
        for (i, line) in self.lines.iter().enumerate().rev() {
            if matches!(line, Line::User(_)) {
                seen += 1;
                if seen == nth_from_end {
                    cut = i;
                    break;
                }
            }
        }
        if seen < nth_from_end {
            return None;
        }
        let dropped = match self.lines.get(cut) {
            Some(Line::User(t)) => Some(t.clone()),
            _ => None,
        };
        self.lines.truncate(cut);
        self.rendered.truncate(cut);
        // stale index-based overrides would attach to whatever entry
        // lands on the recycled indices after the rewind
        self.thought_overrides.retain(|&i| i < cut);
        self.streamed_open.retain(|&i| i < cut);
        dropped
    }

    /// The source entries, in order.
    pub fn entries(&self) -> &[Line] {
        &self.lines
    }

    /// Cached rendered row count.
    /// Rendered row count of one entry (tests).
    pub fn rendered_rows_of(&self, entry: usize) -> usize {
        self.rendered.get(entry).map(Vec::len).unwrap_or(0)
    }

    pub fn total_rows(&self) -> usize {
        self.rendered.iter().map(Vec::len).sum()
    }

    /// Whether one entry is a thinking block that collapsing would
    /// shorten — the only thought rows a click (or hover) can fold.
    pub fn is_foldable(&self, entry: usize) -> bool {
        matches!(self.lines.get(entry), Some(Line::Thought(t)) if thought_foldable(t))
    }

    /// Whether one entry is a verdict summary with hidden detail (the
    /// fold a click expands).
    pub fn summary_foldable(&self, entry: usize) -> bool {
        matches!(
            self.lines.get(entry),
            Some(Line::Summary {
                detail: Some(_),
                ..
            })
        )
    }

    /// Rendered row span of one entry: `(first_row, row_count)`.
    pub fn entry_row_span(&self, entry: usize) -> Option<(usize, usize)> {
        let mut at = 0usize;
        for (i, rows) in self.rendered.iter().enumerate() {
            if i == entry {
                return Some((at, rows.len()));
            }
            at += rows.len();
        }
        None
    }

    /// Rendered row index where each user entry starts, in transcript
    /// order (▲▼ title-arrow jump targets).
    pub fn user_entry_rows(&self) -> Vec<usize> {
        let mut rows = Vec::new();
        let mut at = 0usize;
        for (line, rendered) in self.lines.iter().zip(&self.rendered) {
            if matches!(line, Line::User(_)) {
                rows.push(at);
            }
            at += rendered.len();
        }
        rows
    }

    /// One rendered row by absolute index.
    pub fn row(&self, i: usize) -> Option<&ratatui::text::Line<'static>> {
        let mut i = i;
        for entry in &self.rendered {
            if i < entry.len() {
                return entry.get(i);
            }
            i -= entry.len();
        }
        None
    }

    /// Append an entry, preceded by [`separate_before`] air when the
    /// cached tail is a different-family block. Every cached block push
    /// goes through this, so the vertical rhythm has exactly one source.
    pub fn push_separated(&mut self, line: Line) {
        separate_before(self, family(&line));
        self.push(line);
    }
    /// Case-insensitive search over the source entries starting at
    /// absolute rendered row `from_row`: the first entry whose FIRST row
    /// sits at or below `from_row` and whose text contains `needle`.
    /// Returns `(entry_index, first_row_offset)`; None on a miss.
    pub fn find_from(&self, from_row: usize, needle: &str) -> Option<(usize, usize)> {
        let lower = needle.to_lowercase();
        if lower.is_empty() {
            return None;
        }
        let mut offset = 0usize;
        for (i, line) in self.lines.iter().enumerate() {
            let len = self.rendered.get(i).map(Vec::len).unwrap_or(0);
            if offset >= from_row && line_text(line).to_lowercase().contains(&lower) {
                return Some((i, offset));
            }
            offset += len;
        }
        None
    }
    /// Render passes issued so far (tests).
    pub fn render_passes(&self) -> usize {
        self.renders
    }
}

/// The searchable text of a transcript entry. Tool blocks expose their
/// heads and notes (excerpts stay out of transcript search).
fn line_text(line: &Line) -> std::borrow::Cow<'_, str> {
    match line {
        Line::User(t)
        | Line::Assistant(t)
        | Line::Thought(t)
        | Line::Info(t)
        | Line::Warn(t)
        | Line::Err(t)
        | Line::Report(t)
        | Line::Shell(t) => t.into(),
        Line::Summary { text, .. } => text.into(),
        Line::ToolBlock(calls) => {
            let mut s = String::new();
            for c in calls {
                s.push_str(&c.head);
                s.push('\n');
                s.push_str(&c.note);
                s.push('\n');
            }
            s.into()
        }
    }
}

/// Coarse visual family of an entry: vertical air (one blank canvas row)
/// separates consecutive blocks of different families, while a
/// same-family run stays tight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    User,
    Assistant,
    Thought,
    Tool,
    /// Notes, turn reports, error rows — and the blank separator itself.
    Meta,
}

/// The [`Family`] of a transcript entry (a separator blank reads as
/// Meta, but [`separate_before`] no-ops on it before consulting family).
fn family(line: &Line) -> Family {
    match line {
        Line::User(_) => Family::User,
        Line::Assistant(_) => Family::Assistant,
        Line::Thought(_) => Family::Thought,
        Line::ToolBlock(_) => Family::Tool,
        Line::Info(_)
        | Line::Warn(_)
        | Line::Err(_)
        | Line::Shell(_)
        | Line::Summary { .. }
        | Line::Report(_) => Family::Meta,
    }
}

/// Air between blocks: push one blank canvas row (`Report("")`) unless
/// the transcript is fresh, the tail already is a blank, or the tail
/// belongs to the incoming family — same-family runs stay tight, and air
/// is never doubled. Separators only ever sit BETWEEN blocks: the final
/// entry of a session leaves no trailing blank.
fn separate_before(transcript: &mut Transcript, incoming: Family) {
    let Some(last) = transcript.entries().last() else {
        return;
    };
    if line_text(last).is_empty() || family(last) == incoming {
        return;
    }
    // thinking bands ship their own leading/trailing spacer rows; an
    // extra separator around one would double the air
    if family(last) == Family::Thought || incoming == Family::Thought {
        return;
    }
    transcript.push(Line::Report(String::new()));
}
/// Render one transcript entry into styled rows at content width
/// `width` (W). Blocks sit on the canvas and take their structure from
/// rails, rules, and type weight; only user turns (`BG_USER`) and
/// expanded tool output (`BG_TOOL` / `BG_ERR_TINT`) lift off it. No
/// block pads itself with blank rows — air between blocks is
/// [`separate_before`]'s job (thinking blocks excepted: they carry
/// their own canvas spacers, see below). Thinking entries collapse to
/// their first lines plus a "more" row unless `thoughts_open`.
fn render_line(line: &Line, width: u16, thoughts_open: bool) -> Vec<ratatui::text::Line<'static>> {
    use crate::palette;
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line as TuiLine, Span};
    use unicode_width::UnicodeWidthStr;
    let w = width as usize;
    let mut out: Vec<TuiLine> = Vec::new();
    match line {
        Line::User(text) => push_block(&mut out, text, width),
        // assistant prose sits on the canvas: markdown at W-2, every
        // row indented two columns. The live stream renders at the
        // same width and indent (`live_region_rows`), so the hand-off
        // never reflows.
        Line::Assistant(text) => {
            let mut rows = crate::markdown::render(text, assistant_md_width(width));
            indent_assistant(&mut rows);
            out.extend(rows);
        }
        // thinking blocks carry one canvas spacer row above and below:
        // the live tail band ships the same air, so its flush lands in
        // place whatever the neighbors are, and the trailing spacer
        // keeps the composer clear when a block is last
        Line::Thought(text) => {
            out.push(TuiLine::default());
            out.extend(thought_rows(text, w, thoughts_open));
            out.push(TuiLine::default());
        }
        // finished tool calls cache as a timeline: one header row per
        // call plus its threaded body, stacked with no rule between
        // calls (the status glyph leading each header marks the beat)
        Line::ToolBlock(calls) => {
            for call in calls {
                out.extend(tool_call_rows(call, width));
            }
        }
        Line::Info(text) => {
            push_gutter(&mut out, text, width, "• ", Style::new().fg(palette::META))
        }
        Line::Warn(text) => {
            push_gutter(&mut out, text, width, "▲ ", Style::new().fg(palette::WARN))
        }
        Line::Err(text) => push_gutter(
            &mut out,
            text,
            width,
            "✗ ",
            Style::new().fg(palette::ERR).add_modifier(Modifier::BOLD),
        ),
        // `!cmd` passthrough output: plain prose color, no gutter
        Line::Shell(text) => out.push(TuiLine::from(vec![Span::styled(
            text.clone(),
            Style::new().fg(palette::FG),
        )])),
        // the turn verdict: tone-colored glyph, faint text, then a rule
        // closing the turn like a chapter break
        Line::Summary {
            glyph,
            tone,
            text,
            detail,
            expanded,
        } => {
            let tone_style = match tone {
                SummaryTone::Ok => Style::new().fg(palette::OK),
                SummaryTone::Warn => Style::new().fg(palette::WARN),
                SummaryTone::Err => Style::new().fg(palette::ERR).add_modifier(Modifier::BOLD),
            };
            let faint = Style::new().fg(palette::FAINT);
            // the fold chevron trails the verdict text, which truncates
            // to leave room for it — a long verdict can never clip the
            // marker off the pane edge
            let chevron = match detail {
                Some(_) if *expanded => " ▾",
                Some(_) => " ▸",
                None => "",
            };
            let lead = format!("{glyph} ");
            let lead_w = lead.width();
            let room = w.saturating_sub(lead_w + chevron.width() + 1).max(1);
            let head = trunc_cols(text, room);
            let used = lead_w + head.width() + chevron.width();
            let mut spans = vec![Span::styled(lead, tone_style), Span::styled(head, faint)];
            if !chevron.is_empty() {
                spans.push(Span::styled(chevron, faint));
            }
            if w > used + 1 {
                spans.push(Span::raw(" "));
                spans.push(Span::styled("─".repeat(w - used - 1), palette::RULE_STYLE));
            }
            out.push(TuiLine::from(spans));
            if *expanded && let Some(d) = detail {
                // the full error, wrapped like prose in the faint tier
                // so the verdict row stays the visual anchor
                for row in wrap_rows(d, w.saturating_sub(2)) {
                    out.push(TuiLine::styled(format!("  {}", row.text), faint));
                }
            }
        }
        // the vertical separator is one true canvas-blank row (gutter
        // text is empty, so the prefix loop would emit nothing)
        Line::Report(text) if text.is_empty() => out.push(TuiLine::default()),
        Line::Report(text) => push_gutter(&mut out, text, width, "─ ", palette::META.into()),
    }
    out
}

/// Columns of canvas indent in front of assistant prose.
const ASSISTANT_INDENT: usize = 2;

/// Markdown fold width of an assistant block at content width `width`
/// — the one authority shared by the cache ([`render_line`]) and the
/// run loop's live markdown cache, so streamed and settled rows fold
/// identically.
fn assistant_md_width(width: u16) -> u16 {
    width.saturating_sub(ASSISTANT_INDENT as u16)
}

/// Indent markdown rows by [`ASSISTANT_INDENT`] canvas columns. Blank
/// rows stay span-less (true canvas blanks).
fn indent_assistant(rows: &mut [ratatui::text::Line<'static>]) {
    for row in rows.iter_mut().filter(|r| !r.spans.is_empty()) {
        row.spans
            .insert(0, ratatui::text::Span::raw(" ".repeat(ASSISTANT_INDENT)));
    }
}

/// The collapsed band shows at most this many body lines, so a block
/// no taller than it has nothing to fold.
const THOUGHT_COLLAPSED_LINES: usize = 3;

/// Whether collapsing a thinking block hides anything: the chevron,
/// the click/hover target, and the toggle all key on this, so a short
/// block never shows a dead indicator.
fn thought_foldable(text: &str) -> bool {
    text.trim().lines().count() > THOUGHT_COLLAPSED_LINES
}

/// Thinking-block rows (between the canvas spacers [`render_line`]
/// adds): head `✻ Thinking {▸|▾}`, then body rows `│ {text}` — the
/// rail in `RULE`, the prose in [`crate::palette::THOUGHT`], wrapped at
/// W-2. Collapsed keeps the first [`THOUGHT_COLLAPSED_LINES`] source
/// lines plus a `│ ⋯ +N more` row while anything is hidden; open
/// carries every line. The chevron (and the head's right-aligned
/// `N lines` count while collapsed) rides only blocks collapsing would
/// actually shorten.
fn thought_rows(text: &str, w: usize, open: bool) -> Vec<ratatui::text::Line<'static>> {
    // blank lines at the edges of a reasoning stream are noise, never
    // content: every thought view (fold test, band, tail) trims them
    let text = text.trim();
    let foldable = thought_foldable(text);
    let n = text.lines().count();
    let mut rows = vec![thought_head(foldable, open, n, w, None)];
    let keep = if open {
        usize::MAX
    } else {
        THOUGHT_COLLAPSED_LINES
    };
    for line in text.lines().take(keep) {
        push_thought_body(&mut rows, line, w);
    }
    // collapsed with more to show: the "more" row, gone again the
    // moment the block opens
    if foldable && !open {
        rows.push(thought_more_row(n.saturating_sub(THOUGHT_COLLAPSED_LINES)));
    }
    rows
}

/// The live tail band: canvas spacer, the same head (open chevron),
/// the LAST [`THOUGHT_COLLAPSED_LINES`] lines, canvas spacer — the
/// streaming view keeps at most three thinking lines on screen. The
/// live region passes `shimmer_ms` (the head label shimmers); the flush
/// ([`Transcript::push_thought_streamed`]) caches the same rows with
/// `None`, so the hand-off never jumps the viewport — only the label's
/// light settles. The tail is the collapsed band minus its "more" row
/// while body lines fit one row; the settle to the collapsed band
/// happens out of view.
fn thought_rows_tail(
    text: &str,
    w: usize,
    shimmer_ms: Option<u128>,
) -> Vec<ratatui::text::Line<'static>> {
    let text = text.trim();
    let n = text.lines().count();
    let mut rows = vec![
        ratatui::text::Line::default(),
        thought_head(thought_foldable(text), true, n, w, shimmer_ms),
    ];
    for line in text.lines().skip(n.saturating_sub(THOUGHT_COLLAPSED_LINES)) {
        push_thought_body(&mut rows, line, w);
    }
    rows.push(ratatui::text::Line::default());
    rows
}

/// Head row of a thinking block: `✻ Thinking` in `THINK`, the fold
/// chevron ` ▸|▾` faint (foldable blocks only — no dead indicator),
/// and while collapsed a right-aligned faint `{lines} lines` count
/// (dropped when the row has no room). `shimmer_ms` sweeps a light
/// band across the label (the live head); `None` paints it flat.
fn thought_head(
    foldable: bool,
    open: bool,
    lines: usize,
    w: usize,
    shimmer_ms: Option<u128>,
) -> ratatui::text::Line<'static> {
    use ratatui::style::Style;
    use ratatui::text::Span;
    use unicode_width::UnicodeWidthStr;
    let think = Style::new().fg(crate::palette::THINK);
    let faint = Style::new().fg(crate::palette::FAINT);
    let mut spans = vec![Span::styled(format!("{} ", crate::icons::THOUGHT), think)];
    match shimmer_ms {
        Some(ms) => spans.extend(crate::fx::shimmer(
            "Thinking",
            crate::palette::THINK,
            crate::palette::STRONG,
            ms,
            Style::new(),
        )),
        None => spans.push(Span::styled("Thinking", think)),
    }
    if foldable {
        spans.push(Span::styled(if open { " ▾" } else { " ▸" }, faint));
        if !open {
            let count = format!("{lines} lines");
            let used: usize = spans.iter().map(|s| s.content.width()).sum();
            let pad = w.saturating_sub(used + count.width());
            if pad > 0 {
                spans.push(Span::raw(" ".repeat(pad)));
                spans.push(Span::styled(count, faint));
            }
        }
    }
    ratatui::text::Line::from(spans)
}

/// One source line of thinking as body rows: `│ ` rail (`RULE`) plus
/// the prose in [`crate::palette::THOUGHT`], word-wrapped at W-2.
fn push_thought_body(rows: &mut Vec<ratatui::text::Line<'static>>, line: &str, w: usize) {
    use ratatui::text::Span;
    let room = w.saturating_sub(2).max(1);
    for seg in wrap_rows(line, room) {
        rows.push(ratatui::text::Line::from(vec![
            Span::styled("│ ", crate::palette::RULE_STYLE),
            Span::styled(trunc_cols(&seg.text, room), crate::palette::THOUGHT),
        ]));
    }
}

/// The collapsed band's "more" row: `│ ⋯ +{hidden} more`, drawn only
/// while lines are actually hidden.
fn thought_more_row(hidden: usize) -> ratatui::text::Line<'static> {
    use ratatui::text::Span;
    ratatui::text::Line::from(vec![
        Span::styled("│ ", crate::palette::RULE_STYLE),
        Span::styled(
            format!("⋯ +{hidden} more"),
            ratatui::style::Style::new().fg(crate::palette::FAINT),
        ),
    ])
}

/// One body row of a tool call.
#[derive(Debug, Clone, PartialEq)]
enum CallRow {
    /// A note line on the thread, faint.
    Note(String),
    /// A line of expanded output on the state panel, in the given ink.
    Output(String, ratatui::style::Color),
}

/// Cached rows for one finished call, timeline style: the header row
/// ([`tool_call_header`]) then its body threaded down column 0 — `│`
/// on every body row but the last, which turns the thread with `╰`
/// (`RULE`). Note rows read `│ {note}` faint; expanded output rows put
/// a panel from column 2 to W on `BG_TOOL` (ok) / `BG_ERR_TINT`
/// (failed), output in `INK_DIM`, the spill pointer in `META`. No box
/// borders, no header tint. Rows clip at the pane edge, never wrap.
fn tool_call_rows(call: &ToolCall, width: u16) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::style::Style;
    use ratatui::text::Span;
    use unicode_width::UnicodeWidthStr;
    let w = width as usize;
    let body = tool_card_body(call, width);
    let panel = Style::new().bg(if call.ok {
        crate::palette::BG_TOOL
    } else {
        crate::palette::BG_ERR_TINT
    });
    let mut rows = Vec::with_capacity(1 + body.len());
    rows.push(tool_call_header(call, w));
    let last = body.len().saturating_sub(1);
    for (i, row) in body.into_iter().enumerate() {
        let turn = i == last;
        rows.push(match row {
            CallRow::Note(text) => thread_row(turn, text),
            CallRow::Output(text, fg) => {
                // `│ ` + panel(` {text}{pad} `) = W
                let room = w.saturating_sub(4).max(1);
                let pad = room.saturating_sub(text.width());
                ratatui::text::Line::from(vec![
                    Span::styled(if turn { "╰" } else { "│" }, crate::palette::RULE_STYLE),
                    Span::raw(" "),
                    Span::styled(" ", panel),
                    Span::styled(text, panel.fg(fg)),
                    Span::styled(" ".repeat(pad + 1), panel),
                ])
            }
        });
    }
    rows
}

/// One thread row under a call header: `│ {text}` (or `╰ {text}` when
/// `turn`, the body's last row) — rail in `RULE`, text faint.
fn thread_row(turn: bool, text: String) -> ratatui::text::Line<'static> {
    use ratatui::text::Span;
    ratatui::text::Line::from(vec![
        Span::styled(if turn { "╰ " } else { "│ " }, crate::palette::RULE_STYLE),
        Span::styled(text, ratatui::style::Style::new().fg(crate::palette::FAINT)),
    ])
}

/// Cached row count of one call: the header plus its body rows. The
/// single height authority shared by the renderer and
/// [`Transcript::row_ref_at`]'s click mapping.
fn tool_card_height(call: &ToolCall, width: u16) -> usize {
    1 + tool_card_body(call, width).len()
}

/// A call's body rows: one note row per note line (the collapsed
/// body), plus when expanded the excerpt hard-wrapped onto the panel
/// and the spill pointer. A call that produced nothing readable gets no
/// body at all — its header stands alone.
fn tool_card_body(call: &ToolCall, width: u16) -> Vec<CallRow> {
    let w = width as usize;
    let note_room = w.saturating_sub(2).max(1);
    let mut body: Vec<CallRow> = call
        .note
        .lines()
        .map(|l| CallRow::Note(trunc_cols(l, note_room)))
        .collect();
    let expandable = !call.excerpt.is_empty() || call.spill.is_some();
    if !call.expanded || !expandable {
        return body;
    }
    // `│ ` thread + one column of panel air each side
    let room = w.saturating_sub(4).max(1);
    for line in call.excerpt.lines() {
        for seg in wrap_cols(line, room) {
            body.push(CallRow::Output(
                trunc_cols(&seg, room),
                crate::palette::INK_DIM,
            ));
        }
    }
    if call.spill.is_some() {
        body.push(CallRow::Output(
            trunc_cols("… full output parked in a spill file — /spills", room),
            crate::palette::META,
        ));
    }
    body
}

/// The bash hand leads failed output with `(exit N)` as the excerpt's
/// first line (ka-engine `hands/bash.rs`); surface it as `⏎ exit N` in
/// the call header's right cluster.
fn bash_exit_code(note: &str) -> Option<i32> {
    note.lines()
        .next()?
        .trim()
        .strip_prefix("(exit ")?
        .strip_suffix(')')?
        .parse()
        .ok()
}

/// A finished call's header row: `{✓|✗} {icon} {tool}  {detail}{ ▸|▾}`
/// left, `{⏎ exit N}  {dur}` right-aligned. The duration only earns
/// cells once it measured at least a tenth of a second (replayed calls
/// carry none); the exit note appears only on failed bash calls; the
/// chevron only on calls with something to reveal (excerpt or spill).
fn tool_call_header(call: &ToolCall, w: usize) -> ratatui::text::Line<'static> {
    use ratatui::style::Style;
    use ratatui::text::Span;
    let status = if call.ok {
        Span::styled("✓", Style::new().fg(crate::palette::OK))
    } else {
        Span::styled("✗", Style::new().fg(crate::palette::ERR))
    };
    let mut cluster = Vec::with_capacity(3);
    if !call.ok
        && let Some(n) = bash_exit_code(&call.note)
    {
        cluster.push(Span::styled(
            format!("⏎ exit {n}"),
            Style::new().fg(crate::palette::ERR),
        ));
    }
    if let Some(d) = call.dur.filter(|d| *d >= 0.1) {
        if !cluster.is_empty() {
            cluster.push(Span::raw("  "));
        }
        cluster.push(Span::styled(
            fmt_dur(d),
            Style::new().fg(crate::palette::FAINT),
        ));
    }
    let fold = (!call.excerpt.is_empty() || call.spill.is_some()).then_some(if call.expanded {
        " ▾"
    } else {
        " ▸"
    });
    timeline_head(status, &call.head, fold, cluster, w)
}

/// The one timeline header shape shared by finished and running calls:
/// `{status} {icon} {tool}  {detail}{fold}` left, `cluster` right-
/// aligned after at least two columns of air. `head` is the call head
/// (`→ {tool} · {detail}`, the live `→ ` optional). Icon and tool in
/// `TOOL` (tool bold), detail `INK_DIM`, chevron faint. The detail
/// truncates first, then the tool name; the chevron always survives.
fn timeline_head(
    status: ratatui::text::Span<'static>,
    head: &str,
    fold: Option<&'static str>,
    cluster: Vec<ratatui::text::Span<'static>>,
    w: usize,
) -> ratatui::text::Line<'static> {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::Span;
    use unicode_width::UnicodeWidthStr;
    let inner = head.strip_prefix("→ ").unwrap_or(head);
    let (tool, detail) = inner.split_once(" · ").unwrap_or((inner, ""));
    let icon = crate::icons::tool_icon(tool);
    let fold = fold.unwrap_or("");
    let cluster_w: usize = cluster.iter().map(|s| s.content.width()).sum();
    // everything left of the cluster and its two-column gap
    let budget = if cluster_w == 0 {
        w
    } else {
        w.saturating_sub(cluster_w + 2)
    };
    let lead_w = status.content.width() + 1 + icon.width() + 1;
    let tool = trunc_cols(tool, budget.saturating_sub(lead_w + fold.width()).max(1));
    let mut used = lead_w + tool.width();
    let detail_room = budget.saturating_sub(used + fold.width() + 2);
    let tool_style = Style::new().fg(crate::palette::TOOL);
    let mut spans = vec![
        status,
        Span::raw(" "),
        Span::styled(icon, tool_style),
        Span::raw(" "),
        Span::styled(tool, tool_style.add_modifier(Modifier::BOLD)),
    ];
    if !detail.is_empty() && detail_room >= 2 {
        let detail = trunc_cols(detail, detail_room);
        used += 2 + detail.width();
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            detail,
            Style::new().fg(crate::palette::INK_DIM),
        ));
    }
    if !fold.is_empty() {
        used += fold.width();
        spans.push(Span::styled(fold, Style::new().fg(crate::palette::FAINT)));
    }
    if cluster_w > 0 {
        spans.push(Span::raw(
            " ".repeat(w.saturating_sub(used + cluster_w).max(2)),
        ));
        spans.extend(cluster);
    }
    ratatui::text::Line::from(spans)
}

/// Split one line into width-fitting segments of at most `room` display
/// columns. Unlike [`trunc_cols`] nothing is lost — the tail continues
/// on the next segment.
fn wrap_cols(line: &str, room: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let room = room.max(1);
    let mut segs: Vec<String> = Vec::new();
    let (mut cur, mut cur_w) = (String::new(), 0usize);
    for c in line.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if cur_w > 0 && cur_w + cw > room {
            segs.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur.push(c);
        cur_w += cw;
    }
    segs.push(cur);
    segs
}

/// Viewport window over `total` rendered rows for `visible` rows given the
/// scroll anchor (None = pinned to tail). Returns `(start_row, pinned)`;
/// an anchor at or past the tail re-pins.
fn window_range(total: usize, visible: usize, scroll: Option<usize>) -> (usize, bool) {
    if visible == 0 || total <= visible {
        return (0, true);
    }
    let max_start = total - visible;
    match scroll {
        None => (max_start, true),
        Some(anchor) => {
            let anchor = anchor.min(max_start);
            (anchor, anchor >= max_start)
        }
    }
}

/// The scrollbar rail's published geometry: (rect, total rows, visible
/// rows, thumb position, thumb length).
type RailZone = (ratatui::layout::Rect, usize, usize, usize, usize);

/// Scrollbar rail geometry for a track of `track_h` cells: returns
/// `(thumb_pos, thumb_len)` for a viewport showing `visible` of `total`
/// rows with the window anchored at `start`. The thumb keeps at least
/// one cell and never runs past the track end; `(0, 0)` means no rail.
fn rail_thumb(track_h: usize, total: usize, visible: usize, start: usize) -> (usize, usize) {
    if track_h == 0 || total <= visible {
        return (0, 0);
    }
    let len = (visible * track_h / total).max(1);
    let pos = (start * track_h / total).min(track_h - len);
    (pos, len)
}

/// Scroll anchor for a rail drag: the thumb's center row follows the
/// pointer (minus the grab offset inside the thumb), scaled from track
/// space to row space and clamped. `None` at (or past) the maximum —
/// the tail re-pins.
fn rail_drag_anchor(
    center: usize,
    thumb_len: usize,
    track_h: usize,
    total: usize,
    visible: usize,
) -> Option<usize> {
    let max_anchor = total.saturating_sub(visible);
    if track_h <= thumb_len {
        return None; // degenerate track: the tail stays pinned
    }
    let anchor = center.saturating_sub(thumb_len / 2) * max_anchor / (track_h - thumb_len);
    Some(anchor.min(max_anchor)).filter(|a| *a < max_anchor)
}

/// Visible transcript rows for a terminal height: the header bar, the
/// row of air under it, the composer, the row of air under it, and the
/// status line are the rows carved out of the viewport (the transcript
/// has no border row).
fn visible_rows(term_h: u16, input_h: u16) -> usize {
    term_h.saturating_sub(input_h + 4) as usize
}
/// Cursor `(row, col)` in `text` for a char-index cursor position.
fn cursor_row_col(text: &str, cursor_chars: usize) -> (usize, usize) {
    let before: String = text.chars().take(cursor_chars).collect();
    let row = before.matches('\n').count();
    let col = before.chars().rev().take_while(|&c| c != '\n').count();
    (row, col)
}

/// Input box height: borders + one row, growing one row per extra line
/// up to a five-row cap (longer drafts clip).
fn input_height(row_count: usize) -> u16 {
    3 + row_count.saturating_sub(1).min(5) as u16
}

/// Rows the input area needs: an open picker (mode tiers or thinking
/// levels) borrows the box for its rows; otherwise the draft wraps to
/// the box width. (Permission asks render as a centered modal and no
/// longer borrow it.)
fn input_area_rows(picker: Option<&InputPicker>, draft: &str, width: usize) -> usize {
    match picker {
        Some(InputPicker::Mode(_)) => MODE_CHOICES.len(),
        Some(InputPicker::Thinking(tp)) => tp.choices.len(),
        None => wrap_rows(draft, width).len(),
    }
}

/// One wrapped display row of the draft: the row's text plus the char
/// index in the draft where the row starts (cursor mapping).
#[derive(Debug, Clone, PartialEq, Eq)]
struct VisualRow {
    text: String,
    start: usize,
}

/// Wrap a draft into display rows at `width` display columns
/// (unicode-width aware): explicit newlines always break, long lines
/// break at the last space that fits, over-long tokens hard-break.
/// The draft itself is never mutated — this is a display fold.
fn wrap_rows(text: &str, width: usize) -> Vec<VisualRow> {
    let width = width.max(1);
    let mut out: Vec<VisualRow> = Vec::new();
    let mut base = 0usize; // char index where the current line starts
    for line in text.split('\n') {
        // row = (char offset within the line, char)
        let mut row: Vec<(usize, char)> = Vec::new();
        let mut cols = 0usize;
        for (i, ch) in line.chars().enumerate() {
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if cols + w > width && !row.is_empty() {
                // prefer breaking at the last space within the row
                // (never at position 0 — that would loop forever)
                match row.iter().rposition(|(_, c)| *c == ' ').filter(|&p| p > 0) {
                    Some(p) => {
                        let carry: Vec<(usize, char)> = row[p + 1..].to_vec();
                        row.truncate(p);
                        let start = base + row.first().map(|(i, _)| *i).unwrap_or(0);
                        out.push(VisualRow {
                            text: row.iter().map(|(_, c)| c).collect(),
                            start,
                        });
                        cols = carry
                            .iter()
                            .map(|(_, c)| unicode_width::UnicodeWidthChar::width(*c).unwrap_or(0))
                            .sum();
                        row = carry;
                    }
                    None => {
                        let start = base + row.first().map(|(i, _)| *i).unwrap_or(0);
                        out.push(VisualRow {
                            text: row.iter().map(|(_, c)| c).collect(),
                            start,
                        });
                        row.clear();
                        cols = 0;
                    }
                }
            }
            row.push((i, ch));
            cols += w;
        }
        let start = base + row.first().map(|(i, _)| *i).unwrap_or(0);
        out.push(VisualRow {
            text: row.iter().map(|(_, c)| c).collect(),
            start,
        });
        base += line.chars().count() + 1; // +1 for the newline
    }
    out
}

/// The visual row containing char position `cursor`: the last row
/// whose start is at or before it.
fn visual_cursor_row(rows: &[VisualRow], cursor: usize) -> usize {
    rows.iter().rposition(|r| r.start <= cursor).unwrap_or(0)
}

/// Draft text width of the composer at terminal width `term_w`:
/// 2 outer margin cols + 2 borders + 2 horizontal padding + the 2-col
/// `❯ ` lead ([`COMPOSER_LEAD`]). The draft wraps at exactly this width
/// and the cursor math adds the lead back.
fn input_inner_w(term_w: u16) -> usize {
    term_w.saturating_sub(6 + COMPOSER_LEAD) as usize
}

/// Max rows of diff detail shown in an ask form. The input box grows
/// at most five rows past its base (six content rows total), and the
/// options row must stay visible: the detail budget is whatever is
/// left after the question lines and the options row, never above 8.
const ASK_DETAIL_MAX: usize = 8;

/// Rows of detail the ask form can afford for `question_lines` lines.
/// `ask_detail_rows` may add a trailer, so one row of headroom is kept.
fn ask_detail_budget(question_lines: usize) -> usize {
    ASK_DETAIL_MAX
        .min(6usize.saturating_sub(question_lines + 1))
        .saturating_sub(1)
}

/// Body rows of the permission-ask modal, `width` cols wide: the
/// question wrapped in bold GOLD_HI, the budgeted diff detail, then one
/// option per row as a FAINT `N ` key cap plus its label. The selected
/// row leads with the `▌ ` pointer and wears the selection style;
/// unselected rows lead two blank cols so the caps align.
fn ask_modal_body(ask: &PendingAsk, width: usize) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line as TuiLine, Span};
    use unicode_width::UnicodeWidthStr;
    let strong = Style::new()
        .fg(crate::palette::STRONG)
        .add_modifier(Modifier::BOLD);
    let mut rows: Vec<TuiLine<'static>> = wrap_rows(&ask.question, width)
        .into_iter()
        .map(|r| TuiLine::from(Span::styled(r.text, strong)))
        .collect();
    let q_lines = ask.question.split('\n').count();
    let budget = ask_detail_budget(q_lines);
    if budget > 0
        && let Some(detail) = &ask.detail
    {
        rows.extend(ask_detail_rows(detail, budget));
    }
    let sel_bg = Style::new().bg(crate::palette::SEL_BG);
    for (i, opt) in ask.options.iter().enumerate() {
        let cap = format!("{} ", i + 1);
        if i == ask.selected {
            let room = width.saturating_sub(2 + cap.width());
            rows.push(TuiLine::from(vec![
                Span::styled(
                    format!("{} ", crate::icons::RAIL),
                    sel_bg.fg(crate::palette::ACCENT),
                ),
                Span::styled(cap, sel_bg.fg(crate::palette::FAINT)),
                Span::styled(pad_to_width(opt.clone(), room), selection_style()),
            ]));
        } else {
            rows.push(TuiLine::from(vec![
                Span::raw("  "),
                Span::styled(cap, crate::palette::FAINT),
                Span::raw(opt.clone()),
            ]));
        }
    }
    rows
}

/// Colorized rows for an ask's diff detail: additions in OK, removals
/// in ERR, hunk headers in META, file headers in FAINT, context plain.
/// Clamped to `max` rows with a `… +N more` trailer.
fn ask_detail_rows(detail: &str, max: usize) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::style::Style;
    use ratatui::text::Span;
    let mut rows: Vec<ratatui::text::Line<'static>> = detail
        .lines()
        .map(|l| {
            let style = if l.starts_with("+++") || l.starts_with("---") {
                Style::new().fg(crate::palette::FAINT)
            } else if l.starts_with("@@") {
                Style::new().fg(crate::palette::META)
            } else if l.starts_with('+') {
                Style::new().fg(crate::palette::OK)
            } else if l.starts_with('-') {
                Style::new().fg(crate::palette::ERR)
            } else {
                Style::new()
            };
            ratatui::text::Line::from(vec![ratatui::text::Span::styled(l.to_string(), style)])
        })
        .collect();
    if rows.len() > max {
        let more = rows.len() - max;
        rows.truncate(max);
        rows.push(ratatui::text::Line::from(vec![Span::styled(
            format!("… +{more} more"),
            Style::new().fg(crate::palette::FAINT),
        )]));
    }
    rows
}

/// The composer title while a queue is waiting: `N queued` (None with
/// an empty queue — the action hints ride the bottom border).
fn busy_input_title(queued: usize) -> Option<String> {
    (queued > 0).then(|| format!("{queued} queued"))
}

/// Take the queue head (FIFO) to auto-send on turn settle.
fn pop_queue_head(queue: &mut Vec<String>) -> Option<String> {
    (!queue.is_empty()).then(|| queue.remove(0))
}

/// Queue one draft for after the current turn — the `+` prefix and
/// alt+q share this path. Blank drafts never queue; slash drafts that
/// cannot run from a queue refuse with a visible reason (see
/// [`queue_refusal`]) instead of silently replaying as a prompt.
fn queue_draft(queue: &mut Vec<String>, draft: &str, transcript: &mut Transcript) {
    let item = draft.trim();
    if item.is_empty() {
        return;
    }
    if let Some(reason) = queue_refusal(item) {
        transcript.push_separated(Line::Warn(reason));
        return;
    }
    queue.push(item.to_string());
    transcript.push_separated(Line::Info(format!(
        "⏳ queued · {} waiting for this turn to end",
        queue.len()
    )));
}

/// May this draft ride the queue? Plain text always queues. Slash
/// commands queue only when they carry an engine event and need no
/// interactive surface (`/mode free`, `/model x`, `/compact`, custom
/// commands): picker-openers (`/model` bare), `/quit`, follow-up
/// carriers, and unknown names refuse with a reason — the drain
/// replays them through the slash dispatcher, never as a prompt.
fn queue_refusal(draft: &str) -> Option<String> {
    let item = draft.trim();
    if !item.starts_with('/') {
        return None;
    }
    // /skill: lines never reach the slash dispatcher (skill_invocation
    // intercepts them live) — refuse with an accurate reason instead
    // of "unknown command"
    if item.starts_with("/skill:") || item == "/skill" {
        return Some("skills run immediately — press enter".to_string());
    }
    let head = item.split(' ').next().unwrap_or_default();
    match slash_command(item) {
        None => Some(format!("{head}: unknown command — nothing queued")),
        Some(cmd) => {
            if cmd.quit {
                Some(format!("{head} runs immediately — press enter"))
            } else if cmd.modal.is_some() {
                Some(format!("{head} opens a picker — run it after the turn"))
            } else if cmd.followup.is_some() {
                Some(format!("{head} sends a follow-up prompt — run it directly"))
            } else if cmd.event.is_none() {
                Some(format!("{head} only prints a note — nothing to run"))
            } else {
                None
            }
        }
    }
}

/// Braille spinner frame for an elapsed-milliseconds clock (80 ms per
/// frame).
fn spin_frame(elapsed_ms: u128) -> char {
    const F: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    F[(elapsed_ms / 80) as usize % F.len()]
}

/// The transient live working-indicator row: `{spinner} ` (ACCENT) +
/// a shimmering `Working` (META under a GOLD_HI light band) + faint
/// ` · {s}s · esc to interrupt` while a turn streams; a static WARN
/// `◆ waiting for approval · {s}s` while a permission ask holds the
/// turn. Never cached — rebuilt every frame, a pure function of the
/// clock.
fn working_row(
    ask: Option<&PendingAsk>,
    busy_since: Option<Instant>,
    now: Instant,
) -> ratatui::text::Line<'static> {
    use ratatui::style::Style;
    use ratatui::text::Span;
    let ms = busy_since.map_or(0, |t0| now.saturating_duration_since(t0).as_millis());
    let secs = ms as f64 / 1000.0;
    // span-level styling: Paragraph paints span styles, not line styles
    if ask.is_some() {
        return ratatui::text::Line::from(vec![Span::styled(
            format!("◆ waiting for approval · {secs:.1}s"),
            Style::new().fg(crate::palette::WARN),
        )]);
    }
    let mut spans = vec![Span::styled(
        format!("{} ", spin_frame(ms)),
        crate::palette::ACCENT_STYLE,
    )];
    spans.extend(crate::fx::shimmer(
        "Working",
        crate::palette::META,
        crate::palette::STRONG,
        ms,
        Style::new(),
    ));
    spans.push(Span::styled(
        format!(" · {secs:.1}s · esc to interrupt"),
        Style::new().fg(crate::palette::FAINT),
    ));
    ratatui::text::Line::from(spans)
}

/// The live (never cached) region under the transcript cache, rebuilt
/// every frame at the render width `w`: the thinking tail band (own
/// spacer rows, shimmering head), the running tool call, the streaming
/// assistant markdown (indented like its cached twin, the `▌` cursor
/// riding its last row), then the working row between air rows.
///
/// Air mirrors what the flush will cache, so hand-offs land in place:
/// the thinking band is row-for-row its flushed tail; the running call
/// gets no leading air (it follows a just-flushed thinking band's
/// spacer or merges into the preceding tool block); the streaming text
/// gets the one blank [`separate_before`] will push ahead of the
/// cached assistant block — skipped right after the thinking band,
/// whose trailing spacer already is that air.
fn live_region_rows(
    lb: &LiveBlock,
    ask: Option<&PendingAsk>,
    busy_since: Option<Instant>,
    now: Instant,
    w: usize,
) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::text::Line as TuiLine;
    // one blank row of air unless the tail already is one
    fn air(rows: &mut Vec<TuiLine<'static>>) {
        if rows.last().is_none_or(|r| !r.spans.is_empty()) {
            rows.push(TuiLine::default());
        }
    }
    let ms = busy_since.map_or(0, |t0| now.saturating_duration_since(t0).as_millis());
    let mut rows: Vec<TuiLine<'static>> = Vec::new();
    if !lb.thought.trim().is_empty() {
        rows.extend(thought_rows_tail(&lb.thought, w, Some(ms)));
    }
    rows.extend(tool_live_rows(
        &lb.tool_header,
        lb.live_tool.as_ref(),
        w,
        now,
    ));
    if !lb.md.is_empty() {
        air(&mut rows);
        // the markdown cache stays cursor-free: clone, trim room for the
        // cursor, then append it
        let mut md = lb.md.clone();
        if let Some(last) = md.last_mut() {
            live_cursor(last, w.saturating_sub(ASSISTANT_INDENT));
        }
        indent_assistant(&mut md);
        rows.extend(md);
    }
    // transient tail: the working indicator between air rows, so it
    // never crowds the live blocks or the composer
    air(&mut rows);
    rows.push(working_row(ask, busy_since, now));
    rows.push(TuiLine::default());
    rows
}

fn live_stale(cached_at: Instant, now: Instant) -> bool {
    now.duration_since(cached_at).as_millis() >= 80
}

/// Append the streaming cursor `▌` to a live markdown row that must fit
/// `width` columns: code panels and rules pad to the full fold width, so
/// the row first gives up trailing whitespace (then, if a full line of
/// text leaves no column, its last glyph) — the cursor never clips off
/// the pane edge.
fn live_cursor(row: &mut ratatui::text::Line<'static>, width: usize) {
    use ratatui::text::Span;
    use unicode_width::UnicodeWidthStr;
    let used = |row: &ratatui::text::Line<'static>| -> usize {
        row.spans.iter().map(|s| s.content.width()).sum()
    };
    while used(row) >= width.max(1) {
        let Some(last) = row.spans.last_mut() else {
            return;
        };
        if last.content.to_mut().pop().is_none() {
            row.spans.pop();
        }
    }
    row.spans
        .push(Span::styled("▌", crate::palette::ACCENT_STYLE));
}

/// Format a duration for turn reports: sub-minute `3.2s`, else `m:ss`.
fn fmt_dur(secs: f64) -> String {
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let m = (secs / 60.0).floor() as u64;
        let ss = (secs % 60.0).floor() as u64;
        format!("{m}:{ss:02}")
    }
}

/// Format a token count: 999, 1k, 1.2k, 123k.
fn fmt_tok(n: u64) -> String {
    if n < 1000 {
        format!("{n}")
    } else {
        let v = format!("{:.1}", n as f64 / 1000.0);
        let v = v.strip_suffix(".0").unwrap_or(&v);
        format!("{v}k")
    }
}

/// Trailing turn-report fields: elapsed, tokens, cache, cost — each only
/// when non-zero.
fn usage_tail(u: &ka_protocol::Usage, dur: f64) -> String {
    let mut tail = format!(" · {}", fmt_dur(dur));
    if u.input + u.output > 0 {
        tail.push_str(&format!(
            " · {} in · {} out",
            fmt_tok(u.input),
            fmt_tok(u.output)
        ));
    }
    if u.cache_read > 0 {
        tail.push_str(&format!(" · {} cache", fmt_tok(u.cache_read)));
    }
    if u.cost > 0.0 {
        tail.push_str(&format!(" · ${:.4}", u.cost));
    }
    tail
}

/// Base64-encode a string's UTF-8 bytes (standard alphabet, `=` padding).
/// Local helper — the workspace has no base64 dependency.
fn b64encode(data: impl AsRef<[u8]>) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = data.as_ref();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHA[((n >> 18) & 63) as usize] as char);
        out.push(ALPHA[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHA[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHA[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Scroll one page up from the current anchor (pinned counts from the
/// tail). Keeps a two-row overlap for reading continuity.
fn page_up(scroll: &mut Option<usize>, total: usize, visible: usize) {
    if visible == 0 || total <= visible {
        *scroll = None;
        return;
    }
    let max_anchor = total - visible;
    let step = visible.saturating_sub(2).max(1);
    let cur = scroll.unwrap_or(max_anchor);
    *scroll = Some(cur.saturating_sub(step).min(max_anchor));
}

/// Scroll one page down; reaching the tail re-pins (None).
fn page_down(scroll: &mut Option<usize>, total: usize, visible: usize) {
    let Some(anchor) = *scroll else { return };
    if visible == 0 {
        *scroll = None;
        return;
    }
    let max_anchor = total.saturating_sub(visible);
    let step = visible.saturating_sub(2).max(1);
    *scroll = if anchor + step >= max_anchor {
        None
    } else {
        Some(anchor + step)
    };
}

/// Scroll a few lines up from the current anchor (pinned counts from the
/// tail). Wheel granularity — three lines per notch.
fn line_up(scroll: &mut Option<usize>, total: usize, visible: usize) {
    if visible == 0 || total <= visible {
        *scroll = None;
        return;
    }
    let max_anchor = total - visible;
    let cur = scroll.unwrap_or(max_anchor);
    *scroll = Some(cur.saturating_sub(WHEEL_STEP).min(max_anchor));
}

/// Scroll a few lines down; reaching the tail re-pins (None).
fn line_down(scroll: &mut Option<usize>, total: usize, visible: usize) {
    let Some(anchor) = *scroll else { return };
    if visible == 0 {
        *scroll = None;
        return;
    }
    let max_anchor = total.saturating_sub(visible);
    *scroll = if anchor + WHEEL_STEP >= max_anchor {
        None
    } else {
        Some(anchor + WHEEL_STEP)
    };
}

/// Transcript rows per mouse-wheel notch.
const WHEEL_STEP: usize = 3;

/// A pending self-update, from the CLI's startup check to the TUI
/// (`ka update` remains the only install path — this only announces).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateNotice {
    /// Running version (e.g. `0.2.9 (abc1234)`).
    pub current: String,
    /// Newest release tag (e.g. `v0.2.10`).
    pub latest: String,
}

/// Footer state shown under the editor.
#[derive(Debug, Clone, Default)]
pub struct Meters {
    /// Active model selector.
    pub model: String,
    /// Permission mode.
    pub mode: String,
    /// Mode icon glyph; rendered before the word in status surfaces.
    pub mode_icon: String,
    /// Active session (strand) id.
    pub session: String,
    /// Reasoning effort.
    pub effort: String,
    /// Whether the active model exposes reasoning control (gates the
    /// status-bar effort segment).
    pub effort_supported: bool,
    /// Context usage / window.
    pub context: (u64, u64),
    /// Turn cost.
    pub cost: f64,
    /// Cache-hit rate (0-1) when known.
    pub cache_hit: Option<f32>,
    /// Finished turns this session.
    pub turns: u64,
    /// Feature specs currently disabled (`⊘ n` in the status bar; 0
    /// hides the segment).
    pub toggles: usize,
    /// Cumulative input tokens seen (incl. cache reads/writes).
    pub tokens_in: u64,
    /// Cumulative output tokens.
    pub tokens_out: u64,
    /// Cumulative cache-read tokens.
    pub cache_read: u64,
    /// Accumulated busy seconds across finished turns.
    pub elapsed: f64,
    /// Newest release tag when a self-update is pending (the `⤴` badge;
    /// None hides the segment).
    pub update: Option<String>,
}

/// Bootstrap inventory for the sidebar (the [`Event::Inventory`] payload,
/// kept beside the transcript card it also renders).
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    /// Visible tool names (feature toggles hide the rest).
    pub tools: Vec<String>,
    /// Per configured MCP server: name, connect ok, tool count.
    pub mcp: Vec<ka_protocol::McpSummary>,
    /// Discovered subagent names.
    pub agents: Vec<String>,
    /// Discovered skill names.
    pub skills: Vec<String>,
    /// Advertised MCP prompts (`server/name (args)`).
    pub prompts: Vec<String>,
    /// Specs currently disabled by feature toggles (shared grammar).
    pub disabled: Vec<String>,
}

/// Data behind the bottom-strip popups. Session figures come from the
/// footer meters at render time; this state carries the rest.
#[derive(Debug, Clone, Default)]
pub struct SidebarState {
    /// Bootstrap inventory.
    pub inventory: Inventory,
    /// Superset of tool names ever offered (toggled-off tools vanish
    /// from the inventory but must stay re-enableable from /features).
    pub known_tools: Vec<String>,
    /// Live sandbox-mode override (None = follows config).
    pub sandbox: Option<String>,
    /// Live todo list ([`Event::Todos`]; whole-list replacement).
    pub todos: Vec<ka_protocol::TodoItem>,
    /// Working directory, display-shortened.
    pub cwd: String,
    /// Git branch when cheaply detectable at startup.
    pub branch: Option<String>,
    /// The active session's display title ([`Event::Title`]; stored
    /// record or auto-generated). None until the engine announces one.
    pub title: Option<String>,
}

/// Clickable strip buttons on the composer's bottom border, recorded at
/// render time from the exact drawn cells so the mouse handler can
/// hit-test without duplicating the layout math (capture mode only —
/// native mode uses the keyboard shortcuts). A button the border had
/// no room for keeps an empty rect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct StripZones {
    /// The todos button.
    todos: ratatui::layout::Rect,
    /// The skills button.
    skills: ratatui::layout::Rect,
    /// The info button.
    info: ratatui::layout::Rect,
}

/// Which strip button a click landed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StripButton {
    Todos,
    Skills,
    Info,
}

impl StripZones {
    /// Does a terminal-cell click land on a button? Returns the button.
    fn hit(&self, x: u16, y: u16) -> Option<StripButton> {
        let pos = ratatui::layout::Position { x, y };
        if self.todos.contains(pos) {
            Some(StripButton::Todos)
        } else if self.skills.contains(pos) {
            Some(StripButton::Skills)
        } else if self.info.contains(pos) {
            Some(StripButton::Info)
        } else {
            None
        }
    }
}

/// Clickable ▲▼ jump targets at the right end of the header bar
/// (recorded at render time from the drawn cells, like [`StripZones`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TitleArrows {
    /// ▲ — jump to the previous user message.
    up: ratatui::layout::Rect,
    /// ▼ — jump to the next user message.
    down: ratatui::layout::Rect,
}

impl TitleArrows {
    /// Does a terminal-cell click land on one of the arrows? Returns
    /// `Some(true)` for ▲ (previous), `Some(false)` for ▼ (next).
    fn hit(&self, x: u16, y: u16) -> Option<bool> {
        let pos = ratatui::layout::Position { x, y };
        if self.up.contains(pos) {
            Some(true)
        } else if self.down.contains(pos) {
            Some(false)
        } else {
            None
        }
    }
}

/// Inner text width of the transcript column: the terminal narrows by
/// one margin column on each side and the paragraph's side padding.
/// Both the cache (`Transcript::set_width`) and the live surface derive
/// from this so they always agree.
fn transcript_width(term_w: u16) -> u16 {
    term_w.saturating_sub(4) // 2 margin cols + the paragraph's side padding
}

/// Truncate a string to `max_cols` display columns (unicode-width aware),
/// marking the cut with `…`.
fn trunc_cols(s: &str, max_cols: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if s.width() <= max_cols {
        return s.to_string();
    }
    let keep = max_cols.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > keep {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// Shorten a cwd for one sidebar row: the last two components.
fn shorten_cwd(cwd: &std::path::Path) -> String {
    let parts: Vec<_> = cwd
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    match parts.len() {
        0 => String::new(),
        1 => parts[0].clone(),
        n => format!("…/{}/{}", parts[n - 2], parts[n - 1]),
    }
}

/// One-shot git branch detection for the sidebar info section (the
/// engine's per-turn snapshot never crosses the protocol; this is the
/// cheap local equivalent — empty when not a repo). `symbolic-ref`
/// covers fresh repos with no commits yet, where `rev-parse HEAD`
/// fails.
fn detect_branch() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .ok()?;
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !branch.is_empty()).then_some(branch)
}
/// Popup rows for the todos strip button: done items struck through,
/// the first pending item accented as "next".
fn todos_rows(sidebar: &SidebarState, width: usize) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line as TuiLine, Span};
    if sidebar.todos.is_empty() {
        return vec![TuiLine::styled(
            "(no live todo list — the todo hand drives this)".to_string(),
            crate::palette::META,
        )];
    }
    let done_style = Style::new()
        .fg(crate::palette::FAINT)
        .add_modifier(Modifier::CROSSED_OUT);
    let first_pending = sidebar
        .todos
        .iter()
        .position(|t| t.state == ka_protocol::TodoState::Pending);
    sidebar
        .todos
        .iter()
        .enumerate()
        .map(|(i, t)| match t.state {
            ka_protocol::TodoState::Done => TuiLine::from(vec![
                Span::styled("\u{2713} ", done_style),
                Span::styled(trunc_cols(&t.text, width.saturating_sub(2)), done_style),
            ]),
            // the first pending item is "next": filled accent bullet
            ka_protocol::TodoState::Pending if Some(i) == first_pending => TuiLine::from(vec![
                Span::styled("\u{25cf} ".to_string(), crate::palette::ACCENT_BOLD),
                Span::styled(
                    trunc_cols(&t.text, width.saturating_sub(2)),
                    crate::palette::ACCENT_BOLD,
                ),
            ]),
            // later pending items stay quiet: hollow bullet, plain text
            ka_protocol::TodoState::Pending => TuiLine::from(format!(
                "\u{25cb} {}",
                trunc_cols(&t.text, width.saturating_sub(2))
            )),
        })
        .collect()
}

/// Popup rows for the skills strip button: the discovery inventory —
/// skills, agents, MCP servers — one labeled section each.
fn inventory_rows(sidebar: &SidebarState, width: usize) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::text::{Line as TuiLine, Span};
    // one glyph per section header, accent bold
    let header = |icon: &str, name: &str| {
        TuiLine::styled(format!("{icon} {name}"), crate::palette::ACCENT_BOLD)
    };
    let plain = |s: String| TuiLine::from(s);
    let names = |icon: &str, label: &str, items: &[String]| {
        std::iter::once(header(icon, label))
            .chain(items.iter().map(|s| plain(trunc_cols(s, width))))
            .collect::<Vec<_>>()
    };
    let mut out = names(
        crate::icons::STRIP_SKILLS,
        "skills",
        &sidebar.inventory.skills,
    );
    out.extend(names(
        crate::icons::AGENTS,
        "agents",
        &sidebar.inventory.agents,
    ));
    // `mcp.` prefix-matches the per-tool mcp icon (🔌)
    out.push(header(crate::icons::tool_icon("mcp."), "mcp"));
    for m in &sidebar.inventory.mcp {
        let row = if m.ok {
            TuiLine::from(vec![
                Span::raw(format!("{} ", trunc_cols(&m.name, width.saturating_sub(4)))),
                Span::styled("\u{2713}".to_string(), crate::palette::OK),
                Span::raw(format!(" {}", m.tools)),
            ])
        } else {
            TuiLine::from(vec![
                Span::raw(format!("{} ", trunc_cols(&m.name, width.saturating_sub(4)))),
                Span::styled("\u{2717}".to_string(), crate::palette::ERR),
            ])
        };
        out.push(row);
    }
    let headers = [
        format!("{} skills", crate::icons::STRIP_SKILLS),
        format!("{} agents", crate::icons::AGENTS),
        format!("{} mcp", crate::icons::tool_icon("mcp.")),
    ];
    if out.iter().all(|l| {
        l.spans.is_empty() || {
            let s: String = l.spans.iter().map(|s| s.content.clone()).collect();
            headers.contains(&s)
        }
    }) {
        out.insert(
            0,
            TuiLine::styled(
                "(nothing discovered — skills, agents, MCP servers land here)".to_string(),
                crate::palette::META,
            ),
        );
    }
    out
}

/// One row of the /features panel. Header rows organize; every other
/// row maps to one `SetFeature`/`SetSandbox` command.
#[derive(Debug, Clone)]
pub struct FeatureRow {
    /// The spec string (`agents`, `mcp:github`, `tool:bash`, …) — or
    /// `"sandbox"` for the sandbox-mode row.
    pub spec: String,
    /// Display label beside the state glyph.
    pub label: String,
    /// Section header row (not selectable).
    pub header: bool,
    /// Current state: on = enabled.
    pub on: bool,
}

/// The /features panel rows: the seven feature words, the sandbox mode,
/// every base tool, then per-item sections (MCP servers, skills,
/// agents). State comes from the live inventory's disabled set.
fn feature_rows(sidebar: &SidebarState) -> Vec<FeatureRow> {
    let mut out = Vec::new();
    let header = |out: &mut Vec<FeatureRow>, label: &str| {
        out.push(FeatureRow {
            spec: String::new(),
            label: label.to_string(),
            header: true,
            on: true,
        });
    };
    let on = |out: &mut Vec<FeatureRow>, spec: &str, label: String| {
        let disabled = sidebar.inventory.disabled.iter().any(|d| d == spec);
        out.push(FeatureRow {
            spec: spec.to_string(),
            label,
            header: false,
            on: !disabled,
        });
    };
    header(&mut out, "features");
    on(&mut out, "agents", "subagents (delegate + tasks)".into());
    on(
        &mut out,
        "skills",
        "skills block in the system prompt".into(),
    );
    on(&mut out, "mcp", "every MCP server".into());
    on(&mut out, "hooks", "config + .ka/hooks hooks".into());
    on(&mut out, "web", "web search + fetch".into());
    on(&mut out, "lsp", "LSP navigation tier".into());
    on(&mut out, "debug", "debug (DAP) probe".into());
    header(&mut out, "sandbox");
    let mode = sidebar.sandbox.clone().unwrap_or_else(|| "off".into());
    out.push(FeatureRow {
        spec: "sandbox".to_string(),
        label: format!("bash sandbox (now {mode}; ⏎ cycles off↔fs)"),
        header: false,
        on: mode == "fs",
    });
    header(&mut out, "tools");
    for t in &sidebar.known_tools {
        on(&mut out, &format!("tool:{t}"), t.clone());
    }
    header(&mut out, "mcp servers");
    for m in &sidebar.inventory.mcp {
        let count = if m.ok {
            format!(" ({} tools)", m.tools)
        } else {
            String::new()
        };
        on(
            &mut out,
            &format!("mcp:{}", m.name),
            format!("{}{count}", m.name),
        );
    }
    header(&mut out, "skills");
    for s in &sidebar.inventory.skills {
        on(&mut out, &format!("skill:{s}"), s.clone());
    }
    header(&mut out, "agents");
    for a in &sidebar.inventory.agents {
        on(&mut out, &format!("agent:{a}"), a.clone());
    }
    out
}

/// Popup rows for the info strip button: cwd ⎇ branch, session, model,
/// effort, cost, context — the old sidebar's session + info sections.
fn info_rows(
    sidebar: &SidebarState,
    meters: &Meters,
    width: usize,
) -> Vec<ratatui::text::Line<'static>> {
    let plain = |s: String| ratatui::text::Line::from(s);
    let mut rows = Vec::new();
    match &sidebar.branch {
        Some(b) => rows.push(plain(trunc_cols(
            &format!("{} {} {b}", sidebar.cwd, crate::icons::BRANCH),
            width,
        ))),
        None if !sidebar.cwd.is_empty() => {
            rows.push(plain(trunc_cols(&sidebar.cwd, width)));
        }
        _ => {}
    }
    let (used, window) = meters.context;
    let ctx_row = if window > 0 {
        format!(
            "ctx {used}/{window} {}%",
            (used as f64 / window as f64 * 100.0) as u64
        )
    } else {
        format!("ctx {used}")
    };
    for row in [
        Some(match &meters.update {
            Some(latest) => format!("ka {} · \u{2934} {latest}", env!("CARGO_PKG_VERSION")),
            None => format!("ka {}", env!("CARGO_PKG_VERSION")),
        }),
        short_session(&meters.session).map(|t| format!("session #{t}")),
        (!meters.model.is_empty()).then(|| format!("model {}", meters.model)),
        (!meters.effort.is_empty()).then(|| format!("effort {}", effort_segment(&meters.effort))),
        (meters.cost > 0.0).then(|| format!("cost ${:.4}", meters.cost)),
        (used > 0).then_some(ctx_row),
    ]
    .into_iter()
    .flatten()
    {
        rows.push(plain(trunc_cols(&row, width)));
    }
    if rows.is_empty() {
        rows.push(plain("(no session facts yet)".to_string()));
    }
    rows
}

/// A pending ask (permission dialog).
#[derive(Debug, Clone)]
pub struct PendingAsk {
    /// Ask id.
    pub id: AskId,
    /// Question text.
    pub question: String,
    /// Selectable options.
    pub options: Vec<String>,
    /// Selected option index.
    pub selected: usize,
    /// Optional rendered detail (e.g. a unified diff) above the options.
    pub detail: Option<String>,
}

/// Agent summaries injected by the CLI for `/agents` (ka-term stays
/// ka-engine-free).
pub static AGENTS: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();

/// Skill names from the latest engine inventory — feeds the `/skill:`
/// autocomplete popup. A RwLock (not OnceLock): the inventory re-emits
/// on every feature-toggle change.
pub static SKILLS: std::sync::RwLock<Vec<String>> = std::sync::RwLock::new(Vec::new());

/// Notification settings injected by the CLI ([tui] bell / notify):
/// the bell rings on turn completion and permission asks; the command
/// (when set) runs once per finished turn with a small JSON payload on
/// stdin — `notify-send ka "done"` is the canonical use.
pub static NOTIFY: std::sync::OnceLock<NotifySettings> = std::sync::OnceLock::new();

/// What an opt-in integration (Herdr) gets to see: the TUI's activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentActivity {
    /// Ready for input.
    Idle,
    /// A turn is streaming.
    Working,
    /// A permission ask holds the turn.
    Blocked,
}

/// One activity report: the state plus the active strand id (empty while
/// the engine has not named one yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityReport {
    /// The state to surface.
    pub activity: AgentActivity,
    /// Active strand id — the integration's resume handle.
    pub session: String,
}

/// One message for an [`ActivitySink`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentReport {
    /// A state (and session) change.
    Report(ActivityReport),
    /// The agent is quitting: clear the pane's agent.
    Release,
    /// Stop the consumer (sent after a release; used by tests too).
    Stop,
}

/// Latest-wins hand-off between the TUI loop and an out-of-process
/// integration: publishing replaces any unconsumed report, so a slow
/// reporter can never replay stale states (and never grows a queue).
#[derive(Debug, Default)]
pub struct ActivitySink {
    slot: std::sync::Mutex<Option<AgentReport>>,
    ready: std::sync::Condvar,
}

impl ActivitySink {
    /// A fresh, empty sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the pending report and wake the consumer.
    pub fn publish(&self, report: AgentReport) {
        if let Ok(mut slot) = self.slot.lock() {
            *slot = Some(report);
        }
        self.ready.notify_one();
    }

    /// Block until a report is published, then take it.
    pub fn wait(&self) -> Option<AgentReport> {
        let mut slot = self.slot.lock().ok()?;
        loop {
            if let Some(report) = slot.take() {
                return Some(report);
            }
            slot = self.ready.wait(slot).ok()?;
        }
    }
}

/// Installed once by the CLI when the integration is enabled.
static ACTIVITY: std::sync::OnceLock<std::sync::Arc<ActivitySink>> = std::sync::OnceLock::new();

/// Install the sink the TUI publishes activity to. No-op after the first
/// call, and the TUI runs unchanged when nothing is installed.
pub fn set_activity_sink(sink: std::sync::Arc<ActivitySink>) {
    let _ = ACTIVITY.set(sink);
}

/// The state a turn holds: a permission ask outranks a running turn.
fn activity_of(busy: bool, blocked: bool) -> AgentActivity {
    if blocked {
        AgentActivity::Blocked
    } else if busy {
        AgentActivity::Working
    } else {
        AgentActivity::Idle
    }
}

/// What the CLI resolves from `[tui]` config before `run`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotifySettings {
    /// Ring the terminal bell on turn completion and asks.
    pub bell: bool,
    /// Optional shell command; JSON `{"event","stop"}` on stdin.
    pub command: Option<String>,
}

/// Fire turn-completion/ask notifications: bell byte first (terminals
/// mute it by user choice), then the optional command detached.
fn fire_notifications(event: &str, stop: &str) {
    let settings = NOTIFY.get().cloned().unwrap_or_default();
    if settings.bell {
        let _ = std::io::Write::write_all(&mut std::io::stdout(), b"\x07");
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
    let Some(command) = settings.command else {
        return;
    };
    // fixed-vocabulary payload: no user text, no escaping needed
    let payload = format!("{{\"event\":\"{event}\",\"stop\":\"{stop}\"}}");
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let spawned = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn();
        if let Ok(mut child) = spawned {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(payload.as_bytes()).await;
            }
            let _ = child.wait().await;
        }
    });
}

/// Short human tag for a strand id: the first 8 chars of its random tail.
pub fn short_session(id: &str) -> Option<&str> {
    id.split_once('-')
        .map(|(_, tail)| &tail[..tail.len().min(8)])
}

/// Relative age of an RFC3339 timestamp against `now_secs` (epoch).
fn rel_age_at(ts: &str, now_secs: i64) -> String {
    let parts: Vec<&str> = ts.split(['-', 'T', ':', 'Z', '+', '.']).collect();
    let nums: Vec<i64> = parts
        .iter()
        .take(6)
        .map(|p| p.parse::<i64>().ok())
        .collect::<Option<Vec<i64>>>()
        .unwrap_or_default();
    if nums.len() < 6 {
        return ts.to_string();
    }
    let (y, mo, d, h, mi, s) = (nums[0], nums[1], nums[2], nums[3], nums[4], nums[5]);
    // days since epoch: Howard Hinnant's civil algorithm
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + h * 3600 + mi * 60 + s;
    let diff = now_secs - secs;
    if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86400 {
        format!("{}h ago", diff / 3600)
    } else if diff < 604800 {
        format!("{}d ago", diff / 86400)
    } else {
        format!("{}w ago", diff / 604800)
    }
}

/// Relative age against the wall clock.
pub fn rel_age(ts: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    rel_age_at(ts, now)
}

/// Rows for the `/usage` popup: session figures, recent strands, total.
fn usage_rows(meters: &Meters, summaries: &[ka_strand::StrandSummary]) -> Vec<String> {
    let hit = if meters.tokens_in > 0 {
        format!(
            "{:.0}%",
            meters.cache_read as f32 / meters.tokens_in as f32 * 100.0
        )
    } else {
        "—".to_string()
    };
    let mut rows = vec![
        "▸ session".to_string(),
        format!(
            "{} turns · {} in / {} out · cache {} · hit {hit}",
            meters.turns,
            fmt_tok(meters.tokens_in),
            fmt_tok(meters.tokens_out),
            fmt_tok(meters.cache_read),
        ),
        format!("${:.4} · {} busy", meters.cost, fmt_dur(meters.elapsed)),
        String::new(),
        "▸ recent sessions".to_string(),
    ];
    for s in summaries.iter().take(8) {
        let title: String = s.title.chars().take(34).collect();
        rows.push(format!(
            "{title} · {} · {} tok · ${:.2}",
            rel_age(&s.ts),
            fmt_tok(s.tokens),
            s.cost,
        ));
    }
    if summaries.is_empty() {
        rows.push("(no recorded sessions)".to_string());
    }
    let tot_tok: u64 = summaries.iter().map(|s| s.tokens).sum();
    let tot_cost: f64 = summaries.iter().map(|s| s.cost).sum();
    rows.push(String::new());
    rows.push(format!(
        "▸ total · {} sessions · {} tok · ${tot_cost:.2}",
        summaries.len(),
        fmt_tok(tot_tok),
    ));
    rows
}

/// Rows for the `/context` popup: one bar per component scaled to the
/// window, plus a used/free footer.
fn context_rows(parts: &[ka_protocol::ContextPart], window: u64) -> Vec<String> {
    const BAR_W: usize = 20;
    let used: u64 = parts.iter().map(|p| p.tokens).sum();
    let mut rows = vec!["▸ context".to_string()];
    for p in parts {
        let filled = if window > 0 {
            ((p.tokens as f64 / window as f64) * BAR_W as f64).round() as usize
        } else {
            0
        }
        .min(BAR_W);
        let bar = format!("{}{}", "█".repeat(filled), "░".repeat(BAR_W - filled));
        let pct = (p.tokens * 100).checked_div(used).unwrap_or(0);
        rows.push(format!(
            "{:<10} {bar} {:>7} ({pct}%)",
            p.name,
            fmt_tok(p.tokens)
        ));
    }
    rows.push(String::new());
    let free = window.saturating_sub(used);
    let pct = (used * 100).checked_div(window).unwrap_or(0);
    rows.push(format!(
        "▸ used {} of {} ({pct}%) · free {}",
        fmt_tok(used),
        fmt_tok(window),
        fmt_tok(free)
    ));
    rows
}

/// The session cwd handed to [`run`]: the plan path must anchor where
/// the ENGINE anchors (its resolved cwd, honoring the `cwd` config
/// override), not wherever the process happens to stand.
pub static PLAN_CWD: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// The plan file path for this session: root-anchored (nearest `.git`
/// ancestor of the session cwd, else the session cwd), matching the
/// plan-mode system prompt and the engine's writable-path gate. Falls
/// back to the process cwd before [`run`] hands the session cwd over.
fn plan_file_path() -> std::path::PathBuf {
    let base = PLAN_CWD
        .get()
        .cloned()
        .or_else(|| std::env::current_dir().ok());
    match base {
        Some(cwd) => ka_engine::project_root(&cwd).join(".ka/plans/plan.md"),
        None => std::path::PathBuf::from(".ka/plans/plan.md"),
    }
}

/// The shared follow-up prompt that starts a build turn from the plan
/// file (both `/build` and `/approve`). The plan file anchors at the
/// root project (nearest `.git` ancestor, else cwd), matching the
/// plan-mode system prompt.
fn build_followup() -> String {
    format!(
        "Switching to build mode. Read {} and implement it step by \
step now; verify each step.",
        plan_file_path().display()
    )
}

/// True when the plan file exists and was written after `/plan` started
/// its turn: a fresh plan on disk → suggest `/approve`.
fn plan_drafted(plan_started: Option<std::time::SystemTime>, plan_path: &std::path::Path) -> bool {
    let Some(started) = plan_started else {
        return false;
    };
    plan_path
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .is_some_and(|mt| mt >= started)
}

/// Session picker (/session, /resume): newest strands + a fresh-session
/// row, filtered by the typed substring.
#[derive(Debug, Clone)]
pub struct SessionPicker {
    /// Newest-first sessions for the cwd.
    pub sessions: Vec<ka_strand::StrandSummary>,
    /// Selected row (0 = new session).
    pub selected: usize,
    /// Typed filter (matches title or id).
    pub filter: String,
    /// The active session id (tagged in the list).
    pub current: Option<String>,
}

impl SessionPicker {
    /// Rows after filtering: (label, detail). Row 0 is always new-session.
    pub fn rows(&self) -> Vec<(String, String)> {
        let mut rows = vec![("(new session)".to_string(), "start fresh".to_string())];
        let f = self.filter.to_lowercase();
        for s in &self.sessions {
            if !f.is_empty()
                && !s.title.to_lowercase().contains(&f)
                && !s.id.to_lowercase().contains(&f)
            {
                continue;
            }
            let tag = short_session(&s.id).unwrap_or("?");
            let here = if self.current.as_ref() == Some(&s.id) {
                " · current"
            } else {
                ""
            };
            let mut detail = format!("{} msgs · {}", s.messages, rel_age(&s.ts));
            // only pay for digits the session earned: sub-cent costs and
            // zero token counts add no separators
            if s.cost >= 0.005 {
                detail.push_str(&format!(" · ${:.2}", s.cost));
            }
            if s.tokens > 0 {
                detail.push_str(&format!(" · {} tok", fmt_tok(s.tokens)));
            }
            rows.push((format!("#{tag}  {}{here}", s.title), detail));
        }
        rows
    }

    /// The session id the selected row switches to (None = new session).
    pub fn pick(&self) -> Option<String> {
        let rows = self.rows();
        if self.selected == 0 {
            return None;
        }
        let visible: Vec<&ka_strand::StrandSummary> = self
            .sessions
            .iter()
            .filter(|s| {
                let f = self.filter.to_lowercase();
                f.is_empty()
                    || s.title.to_lowercase().contains(&f)
                    || s.id.to_lowercase().contains(&f)
            })
            .collect();
        // selected counts only session rows after row 0
        let idx = self.selected.saturating_sub(1);
        let _ = rows;
        visible.get(idx).map(|s| s.id.clone())
    }
}

/// Settings panel (/settings): live engine options + provider registry.
#[derive(Debug, Clone)]
pub struct SettingsPanel {
    /// Model selector (editable).
    pub model: String,
    /// Permission mode.
    pub mode: ka_protocol::Mode,
    /// Effort (None = provider default).
    pub effort: Option<ka_protocol::Effort>,
    /// Selected row index.
    pub selected: usize,
    /// Inline edit buffer while editing the model.
    pub edit: Option<String>,
    /// Providers injected by the CLI.
    pub providers: Vec<ProviderInfo>,
    /// User config path (informational).
    pub config_path: String,
}

impl SettingsPanel {
    /// The editable row count (model, mode, effort).
    pub const ROWS: usize = 3;

    /// Cycle effort none → low → medium → high → none.
    pub fn cycle_effort(&mut self) -> ka_protocol::Effort {
        use ka_protocol::Effort;
        self.effort = match self.effort {
            None | Some(Effort::Off) => Some(Effort::Low),
            Some(Effort::Low) => Some(Effort::Medium),
            Some(Effort::Medium) => Some(Effort::High),
            Some(Effort::High) => Some(Effort::Max),
            Some(Effort::Max) => None,
        };
        self.effort.unwrap_or(Effort::Medium)
    }
}

/// The permission tiers `/mode` offers, in picker order:
/// (mode, label, one-line description).
const MODE_CHOICES: [(ka_protocol::Mode, &str, &str); 4] = [
    (
        ka_protocol::Mode::Ask,
        "ask",
        "ask before every edit and command",
    ),
    (
        ka_protocol::Mode::AcceptEdits,
        "accept edits",
        "auto-apply file edits, ask for commands",
    ),
    (ka_protocol::Mode::Allow, "allow", "no prompts"),
    (
        ka_protocol::Mode::Readonly,
        "readonly",
        "read-only planning (build with /build)",
    ),
];

/// Footer label for a mode — the canonical lowercase word (`ask`,
/// `accept_edits`, `allow`, `readonly`).
fn mode_label(mode: ka_protocol::Mode) -> &'static str {
    crate::icons::mode_word(mode)
}

/// Parse a footer label back to a mode (settings panel bootstrap). The
/// friendly words are canonical; the older slugs keep parsing.
fn mode_from_label(label: &str) -> ka_protocol::Mode {
    match label {
        "ask" | "guarded" | "needs-approval" | "needs_approval" => ka_protocol::Mode::Ask,
        "accept edits" | "accept-edits" | "accept_edits" => ka_protocol::Mode::AcceptEdits,
        "allow" | "full access" | "full-access" | "full_access" | "free" => {
            ka_protocol::Mode::Allow
        }
        "readonly" | "read-only" | "read_only" | "plan" => ka_protocol::Mode::Readonly,
        _ => ka_protocol::Mode::Ask,
    }
}

/// The `/mode` picker: borrows the input box like the permission form
/// does; ↑↓/1-4 navigate, Enter confirms, Esc cancels.
#[derive(Debug, Clone)]
pub struct ModePicker {
    /// Selected row index.
    pub selected: usize,
}

impl ModePicker {
    /// A picker preselecting the given mode's row.
    pub fn for_mode(mode: ka_protocol::Mode) -> Self {
        Self {
            selected: MODE_CHOICES
                .iter()
                .position(|(m, _, _)| *m == mode)
                .unwrap_or(0),
        }
    }

    /// The mode the selected row stands for.
    pub fn pick(&self) -> ka_protocol::Mode {
        MODE_CHOICES[self.selected.min(MODE_CHOICES.len() - 1)].0
    }
}

/// Parse a reasoning-level word (`off|low|medium|high|max`).
fn parse_effort(word: &str) -> Option<ka_protocol::Effort> {
    match word {
        "off" => Some(ka_protocol::Effort::Off),
        "low" => Some(ka_protocol::Effort::Low),
        "medium" => Some(ka_protocol::Effort::Medium),
        "high" => Some(ka_protocol::Effort::High),
        "max" => Some(ka_protocol::Effort::Max),
        _ => None,
    }
}

/// Footer/picker label for an effort level.
fn effort_label(level: ka_protocol::Effort) -> &'static str {
    match level {
        ka_protocol::Effort::Off => "off",
        ka_protocol::Effort::Low => "low",
        ka_protocol::Effort::Medium => "medium",
        ka_protocol::Effort::High => "high",
        ka_protocol::Effort::Max => "max",
    }
}

/// The `/thinking` picker's rows: Off first, then every level the
/// model exposes (unparseable and duplicate entries skipped).
fn thinking_choices(efforts: &[String]) -> Vec<ka_protocol::Effort> {
    let mut out = vec![ka_protocol::Effort::Off];
    for e in efforts {
        if let Some(level) = parse_effort(e)
            && !out.contains(&level)
        {
            out.push(level);
        }
    }
    out
}

/// The `/thinking` picker: borrows the input box like `/mode`;
/// ↑↓/1-9 navigate, Enter applies + persists, Esc cancels.
#[derive(Debug, Clone)]
pub struct ThinkingPicker {
    /// Selected row index.
    pub selected: usize,
    /// Rows on offer (off + the model's levels).
    pub choices: Vec<ka_protocol::Effort>,
}

impl ThinkingPicker {
    /// A picker preselecting the current level's row (or Off).
    pub fn for_effort(current: Option<ka_protocol::Effort>, efforts: &[String]) -> Self {
        let choices = thinking_choices(efforts);
        Self {
            selected: current
                .and_then(|c| choices.iter().position(|l| *l == c))
                .unwrap_or(0),
            choices,
        }
    }

    /// The level the selected row stands for.
    pub fn pick(&self) -> ka_protocol::Effort {
        self.choices[self.selected.min(self.choices.len() - 1)]
    }
}

/// The input-box picker slot: the `/mode` permission tiers or the
/// `/thinking` reasoning levels.
#[derive(Debug, Clone)]
pub enum InputPicker {
    Mode(ModePicker),
    Thinking(ThinkingPicker),
}

/// A model row for the model picker (built by the CLI from the catalog;
/// ka-term stays catalog-free).
#[derive(Debug, Clone)]
pub struct ModelInfo {
    /// Full selector id (`vendor/model`).
    pub id: String,
    /// Wire label for display.
    pub wire: String,
    /// Context window (0 = unknown).
    pub context: u32,
    /// Env var holding the API key (empty = keyless).
    pub key_env: String,
    /// Whether the key is present in this process.
    pub key_set: bool,
    /// Vendor docs URL for the key prompt (empty = unknown).
    pub doc_url: String,
    /// USD per mtok input (0 = unknown).
    pub price_in: f64,
    /// USD per mtok output (0 = unknown).
    pub price_out: f64,
    /// Price is real published pricing (footer shows cost).
    pub priced: bool,
    /// Subscription plan (no per-token cost).
    pub plan: bool,
    /// Reasoning-effort levels the model exposes (empty = none).
    pub efforts: Vec<String>,
}
/// The `/model` picker state.
#[derive(Debug, Clone)]
pub struct ModelPicker {
    pub models: Vec<ModelInfo>,
    /// Vendor lock (stage two of `/provider`, or `/model <vendor>`):
    /// when set, only this vendor's models are listed and Esc returns
    /// to the provider stage.
    pub vendor: Option<String>,
    /// Selected row.
    pub selected: usize,
    /// Typed filter (substring over the id).
    pub filter: String,
}

/// One picker row for a model. Must stay on a single line under the
/// picker's row budget (62 display cols: 68 wide minus borders,
/// padding, and the 2-col selection pointer) or Paragraph wraps and the
/// tail models clip off the picker — the exact bug that hid installed
/// ollama models behind two-line rows.
fn model_row(m: &ModelInfo, ctx: &str, key: &str) -> String {
    use unicode_width::UnicodeWidthStr;
    // reasoning levels ride last as the effort battery (`✻◔◑◕`); a
    // level with no battery glyph falls back to its name, so nothing is
    // hidden — the id column absorbs any extra width
    let think = if m.efforts.is_empty() {
        String::new()
    } else {
        let mut s = format!(" {}", crate::icons::THOUGHT);
        for e in &m.efforts {
            match crate::icons::effort_glyph(e) {
                Some(g) => s.push_str(g),
                None => {
                    s.push(' ');
                    s.push_str(e);
                }
            }
        }
        s
    };
    let wire = m
        .wire
        .trim_end_matches("_messages")
        .trim_end_matches("_chat");
    let price = if m.plan {
        "plan".to_string()
    } else if m.priced {
        let trim = |v: f64| {
            if (v - v.round()).abs() < f64::EPSILON {
                format!("{}", v.round() as u64)
            } else {
                format!("{v}")
            }
        };
        format!("${}/${}", trim(m.price_in), trim(m.price_out))
    } else {
        "-".to_string()
    };
    // 26 + 1 + 5 + 1 + 10 + 1 + 9 + key (≤2) + battery (≤6) = 61
    let id_w = 26usize
        .saturating_sub(think.width().saturating_sub(6))
        .max(8);
    let id: String = m.id.chars().take(id_w).collect();
    format!("{id:<id_w$} {ctx:>5} {price:<10} {wire:<9}{key:<2}{think}")
}

impl ModelPicker {
    /// Rows after filtering: only models of configured providers
    /// (keyless, or key present) — narrowed by a vendor lock, with the
    /// substring filter applied last. Models of unconfigured providers
    /// stay reachable by typing a `vendor/model` selector (see
    /// [`pick`](Self::pick)).
    pub fn rows(&self) -> Vec<&ModelInfo> {
        let f = self.filter.to_lowercase();
        self.models
            .iter()
            .filter(|m| {
                Self::configured(m)
                    && self
                        .vendor
                        .as_deref()
                        .is_none_or(|v| m.id.split('/').next() == Some(v))
                    && (f.is_empty() || m.id.to_lowercase().contains(&f))
            })
            .collect()
    }

    /// A model of a configured provider: keyless, or its key is set.
    fn configured(m: &ModelInfo) -> bool {
        m.key_env.is_empty() || m.key_set
    }

    /// The selector Enter applies: the selected row's id, or — when the
    /// filter matched nothing — the raw filter (custom provider/model).
    pub fn pick(&self) -> Option<String> {
        let rows = self.rows();
        if rows.is_empty() {
            return (!self.filter.trim().is_empty()).then(|| self.filter.trim().to_string());
        }
        rows.get(self.selected).map(|m| m.id.clone())
    }
}
/// The `/provider` picker: choose a provider to connect before any of
/// its models can be selected.
#[derive(Debug, Clone)]
pub struct ProviderPicker {
    pub providers: Vec<ProviderInfo>,
    /// `(vendor, model count)` pairs backing the per-row detail line.
    pub counts: Vec<(String, usize)>,
    /// Selected row.
    pub selected: usize,
    /// Typed filter (substring over name or env var).
    pub filter: String,
}

impl ProviderPicker {
    /// Build the picker with per-vendor model counts from the catalog.
    pub fn new(providers: Vec<ProviderInfo>, models: &[ModelInfo]) -> Self {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for m in models {
            if let Some(vendor) = m.id.split('/').next() {
                match counts.iter_mut().find(|(v, _)| v == vendor) {
                    Some((_, n)) => *n += 1,
                    None => counts.push((vendor.to_string(), 1)),
                }
            }
        }
        Self {
            providers,
            counts,
            selected: 0,
            filter: String::new(),
        }
    }

    /// Rows after filtering: `(provider, detail)` pairs, detail reading
    /// `ENV ✓|✗ · n models` or `keyless · local · n models`.
    pub fn rows(&self) -> Vec<(&ProviderInfo, String)> {
        let f = self.filter.to_lowercase();
        self.providers
            .iter()
            .filter(|p| {
                f.is_empty()
                    || p.name.to_lowercase().contains(&f)
                    || p.env_var.to_lowercase().contains(&f)
            })
            .map(|p| {
                let n = self
                    .counts
                    .iter()
                    .find(|(v, _)| *v == p.name)
                    .map_or(0, |(_, n)| *n);
                let key = if p.env_var.is_empty() {
                    "keyless · local".to_string()
                } else if p.key_set {
                    format!("{} ✓", p.env_var)
                } else {
                    format!("{} ✗", p.env_var)
                };
                (
                    p,
                    format!("{key} · {n} model{}", if n == 1 { "" } else { "s" }),
                )
            })
            .collect()
    }

    /// The provider Enter connects (or, when already keyed, whose model
    /// list Enter opens).
    pub fn pick(&self) -> Option<&ProviderInfo> {
        self.rows().get(self.selected).map(|(p, _)| *p)
    }
}

/// Mark every model (by `key_env`) and provider (by `env_var`) keyed by
/// `env_var` as connected, so pickers reflect a key saved this session.
fn mark_key_set(models: &mut [ModelInfo], providers: &mut [ProviderInfo], env_var: &str) {
    for m in models.iter_mut() {
        if m.key_env == env_var {
            m.key_set = true;
        }
    }
    for p in providers.iter_mut() {
        if p.env_var == env_var {
            p.key_set = true;
        }
    }
}

/// What Enter does on a `/tree` row.
#[derive(Debug, Clone, PartialEq)]
pub enum TreeAction {
    /// Non-selectable section header.
    Header,
    /// Attach the strand with this id.
    Attach(String),
    /// Fork the current strand before the nth-last user input — the
    /// picked input and everything after it drop from the fork. The
    /// prompt prefills the composer after the fork (⏎ and `e` alike),
    /// so the user can resend, edit, or delete it.
    Fork { turns: u32, prompt: String },
}

/// Which modal is open (drawn above everything).
#[derive(Debug, Clone)]
pub enum Modal {
    /// Session picker.
    Session(SessionPicker),
    /// Settings panel.
    Settings(SettingsPanel),
    /// Feature-toggle panel (/features, ctrl+T): every toggleable
    /// capability with a live on/off state; ⏎ flips the selection.
    Features {
        /// Panel rows (section headers + toggleable specs).
        entries: Vec<FeatureRow>,
        /// Selected row index (always a toggleable row).
        selected: usize,
    },
    /// Model picker.
    Model(ModelPicker),
    /// Provider picker (`/provider`).
    Provider(ProviderPicker),
    /// API key prompt for a provider.
    Key(KeyPrompt),
    /// Spill-file viewer (/spills).
    Spills {
        /// Spilled-output file paths, oldest first.
        items: Vec<String>,
        /// Selected row index.
        selected: usize,
    },
    /// MCP prompt picker (/prompt): `server/name (args)` rows.
    Prompts {
        /// Prompt rows in inventory order.
        items: Vec<String>,
        /// Selected row index.
        selected: usize,
    },
    /// Double-Esc rewind menu: pick a past user message to rewind to
    /// (or edit & resend).
    Rewind {
        /// (turns-from-end, prompt text); 1 = most recent.
        items: Vec<(usize, String)>,
        /// Selected row.
        selected: usize,
    },
    /// Memory viewer (/memory): project + user memory files, plus the
    /// staged-inbox review flow (⏎ accept → project, u → user,
    /// d discard).
    Memory {
        /// Rendered rows (path header + content lines).
        rows: Vec<String>,
        /// Staged memory proposals from `.ka/memory/inbox.md`.
        inbox: Vec<String>,
        /// Selected inbox row.
        selected: usize,
    },
    /// Usage dashboard (/usage): session + recent session rows.
    Usage {
        /// Rendered rows (section headers + figures + total).
        rows: Vec<String>,
    },
    /// Context breakdown (/context): rendered rows.
    Context {
        /// Rendered rows (bars + footer).
        rows: Vec<String>,
    },
    /// Strand tree (/tree): the session's family plus fork points.
    Tree {
        /// Rendered rows (`title · date · N msgs` sessions, `⑂ prompt`
        /// fork points, dim section headers).
        items: Vec<String>,
        /// Row actions aligned with `items`.
        actions: Vec<TreeAction>,
        /// Selected row index.
        selected: usize,
    },
    /// /tasks picker: background task/job/DAP rows; Enter pages the
    /// selected task's full result.
    Tasks {
        /// Row label plus its parseable task id (None for job-/dap rows).
        entries: Vec<(Option<u64>, String)>,
        /// Selected row index.
        selected: usize,
    },
    /// Full result of one background task (the pager over
    /// `Command::TaskDetail`).
    TaskDetail {
        /// Task id (roster `t-<id>`).
        id: u64,
        /// Uncapped result text.
        text: String,
        /// Scroll anchor (None = pinned to the tail).
        scroll: Option<usize>,
    },
    /// /debug overlay: live DAP session roster rows.
    Debug {
        /// Rendered roster rows.
        rows: Vec<String>,
        /// Scroll anchor (None = pinned to the tail).
        scroll: Option<usize>,
    },
    /// Live delegate todo list (bottom-strip button / alt+O).
    Todos {
        /// Rendered todo rows.
        rows: Vec<ratatui::text::Line<'static>>,
    },
    /// Discovered skills, agents, MCP servers (bottom strip / ctrl+T).
    Skills {
        /// Rendered inventory rows.
        rows: Vec<ratatui::text::Line<'static>>,
    },
    /// Session facts: cwd:branch, session, model, effort, cost, ctx.
    Info {
        /// Rendered fact rows.
        rows: Vec<ratatui::text::Line<'static>>,
    },
    /// Help overlay.
    Help,
}

/// Which modal the next roster event should open instead of rendering
/// transcript rows (set by `/tasks` and `/debug`, consumed once).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingModal {
    Tasks,
    Debug,
}

/// Visible rows in the TaskDetail/Debug pager modals — the render
/// height cap (30) minus the box chrome (border 2 + padding 2 + 1).
/// The key side shrinks this to the transcript pane the same way the
/// renderer's `.min(modal_area.height)` does, so scrolling always
/// matches what is on screen.
const PAGER_VISIBLE: usize = 25;

/// The pager's window height for a transcript pane `view_rows` tall —
/// the same number the renderer derives from its clamped box height,
/// so page keys never step past what is actually shown.
fn pager_visible(view_rows: usize) -> usize {
    PAGER_VISIBLE.min(view_rows.saturating_sub(5)).max(1)
}

/// Task id off a /tasks roster row: `t-<id>  …` → `Some(id)`, any
/// other row shape (job-`, dap, placeholders) → None.
fn task_id_of_row(row: &str) -> Option<u64> {
    let row = row.trim();
    let digits = row.strip_prefix("t-")?;
    let end = digits.find(char::is_whitespace).unwrap_or(digits.len());
    digits[..end].parse().ok()
}

/// Push the terminal sequences for one mouse mode. Captured = SGR
/// button reporting plus any-motion tracking (wheel scrolls, click
/// zones live, ⇧drag selects, hover highlights what a click would
/// touch); native = capture off + DECSET 1007, so the wheel still
/// arrives — as ↑/↓ — while plain drag selection is the terminal's
/// own (and no motion events reach ka, so hover is capture-only).
fn apply_mouse_mode(mouse_captured: bool) {
    let mut out = std::io::stdout();
    if mouse_captured {
        let _ = std::io::Write::write_all(&mut out, b"\x1b[?1007l\x1b[?1003h");
        let _ = std::io::Write::flush(&mut out);
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    } else {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
        let _ = std::io::Write::write_all(&mut out, b"\x1b[?1003l\x1b[?1007h");
        let _ = std::io::Write::flush(&mut out);
    }
}

/// The `[tui] mouse` value for a capture flag (SaveSettings payload) —
/// `"capture"` (default) or `"native"`, matching the config docs.
fn mouse_setting(captured: bool) -> Option<String> {
    Some(if captured {
        "capture".to_string()
    } else {
        "native".to_string()
    })
}

/// The Ctrl+M toggle toast: mode names match the persisted
/// `[tui] mouse` values.
fn mouse_mode_text(captured: bool) -> String {
    format!(
        "mouse {} — {}",
        if captured { "capture" } else { "native" },
        if captured {
            "everything clickable; hover previews what a click touches; drag over the transcript selects & copies; ⇧drag = native selection; ↑/↓ = prompt history"
        } else {
            "plain drag selects natively; the wheel scrolls the chat (kitty: no alternate-scroll — Ctrl+M back to capture for the wheel); ↑/↓ scroll, Ctrl+P/N = prompt history"
        }
    )
}

/// Key handling shared by the pager modals (TaskDetail, Debug): the
/// page keys scroll the window, Home jumps to the top, End re-pins to
/// the tail; any other key closes. Returns true when the modal should
/// close. `visible` mirrors the renderer's window height.
fn pager_keys(
    code: crossterm::event::KeyCode,
    total: usize,
    scroll: &mut Option<usize>,
    visible: usize,
) -> bool {
    use crossterm::event::KeyCode;
    match code {
        KeyCode::PageUp => {
            page_up(scroll, total, visible);
            false
        }
        KeyCode::PageDown => {
            page_down(scroll, total, visible);
            false
        }
        KeyCode::Home => {
            if total > visible {
                *scroll = Some(0);
            }
            false
        }
        KeyCode::End => {
            *scroll = None;
            false
        }
        _ => true,
    }
}

/// API key entry for a provider's env var.
#[derive(Debug, Clone)]
pub struct KeyPrompt {
    /// Env var the engine reads (e.g. `ZHIPU_API_KEY`).
    pub env_var: String,
    /// Vendor prefix for display.
    pub provider: String,
    /// Where to get a key (vendor docs).
    pub doc_url: String,
    /// Entered (masked) key value.
    pub input: String,
    /// Vendor to drill into (its model list) once the key is saved.
    pub drill: Option<String>,
    /// Model selector to apply once the key saves (the unified
    /// `/model` pick on a keyed-but-unset vendor).
    pub pending_model: Option<String>,
}

/// Kitty keyboard-protocol flags pushed at startup.
/// `DISAMBIGUATE_ESCAPE_CODES` keeps lone Esc distinct from sequences;
/// `REPORT_EVENT_TYPES` lets key releases be filtered; and
/// `REPORT_ALL_KEYS_AS_ESCAPE_CODES` promotes Enter to CSI-u so
/// modifiers survive — without it, terminals never report Shift+Enter
/// distinctly (it arrives as plain `\r`), so the draft sends instead of
/// breaking the line.
///
/// `REPORT_ALTERNATE_KEYS` is what makes shifted symbols type: the
/// `CSI u` main codepoint is *always* the un-shifted key, so with only
/// the flags above `?` (Shift+`'` on a Spanish/Latin American layout)
/// arrives as `CSI 39;2u` and types `'`. The flag adds the shifted
/// codepoint in the alternate field (`CSI 39:63;2u`), which crossterm
/// resolves back into the layout's own character.
fn startup_keyboard_flags() -> crossterm::event::KeyboardEnhancementFlags {
    use crossterm::event::KeyboardEnhancementFlags as F;
    F::DISAMBIGUATE_ESCAPE_CODES
        | F::REPORT_EVENT_TYPES
        | F::REPORT_ALL_KEYS_AS_ESCAPE_CODES
        | F::REPORT_ALTERNATE_KEYS
}

/// Run the TUI over an engine handle. Blocks until exit.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    mut commands: mpsc::Sender<Command>,
    mut events: mpsc::Receiver<Event>,
    initial_model: &str,
    providers: Vec<ProviderInfo>,
    models: Vec<ModelInfo>,
    agents: Vec<(String, String)>,
    header_glyph: &str,
    notify: NotifySettings,
    mouse_capture: bool,
    fresh: bool,
    session_cwd: std::path::PathBuf,
    mut updates: mpsc::Receiver<UpdateNotice>,
) -> std::io::Result<Exit> {
    let _ = AGENTS.set(agents.clone());
    let _ = NOTIFY.set(notify);
    let _ = PLAN_CWD.set(session_cwd);
    // Register raw mode in THIS crate's crossterm before ratatui flips
    // it through its own (0.28) copy: crossterm's parser decides whether
    // `\n` means Enter or Ctrl+J by reading its own per-crate raw-mode
    // flag, and only the copy that called enable_raw_mode knows. Without
    // this, every lone \n sends the draft instead of breaking the line,
    // and run_external restores a raw state for editors instead of the
    // cooked one it started from.
    let _ = crossterm::terminal::enable_raw_mode();
    let mut terminal = ratatui::init();
    // Kitty keyboard protocol: Shift+Enter as a distinct key + bracketed
    // paste. Best effort — hosts without support degrade to plain Enter;
    // Ctrl+J always works as the newline fallback. Mouse mode defaults
    // to CAPTURE: wheel reporting, ▲▼/skills-header click zones, ⇧drag
    // selects, sidebar visible. `[tui] mouse = "native"` opts out (plain
    // drag selects/pastes natively, wheel scrolls via DECSET 1007).
    // Hybrid: while a popup or modal is open a native session transiently
    // enables capture so overlay clicks (✕ chip, click-outside) work, and
    // releases it on close. Ctrl+M toggles the base at runtime
    // (kitty-protocol terminals only — plain terminals read Ctrl+M as
    // Enter) and persists it.
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::PushKeyboardEnhancementFlags(startup_keyboard_flags()),
        crossterm::event::EnableBracketedPaste,
    );
    // capture is the default; `[tui] mouse = "native"` starts native —
    // plain drag selection, full-width chat, wheel via alternate-scroll.
    // Ctrl+M toggles and persists either way.
    if mouse_capture {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    } else {
        // native mode: alternate scroll translates the wheel to ↑/↓,
        // which scroll the chat there
        let _ = std::io::Write::write_all(&mut std::io::stdout(), b"\x1b[?1007h");
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
    let result = app(
        &mut terminal,
        &mut commands,
        &mut events,
        &mut updates,
        initial_model,
        providers,
        models,
        agents,
        header_glyph,
        mouse_capture,
        fresh,
    )
    .await;
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::PopKeyboardEnhancementFlags,
        crossterm::event::DisableBracketedPaste,
        crossterm::event::DisableMouseCapture
    );
    // native sessions leave DECSET 1007 set and capture sessions leave
    // 1003 (any-motion) on; release both so the next alt-screen app
    // starts from a clean slate
    let _ = std::io::Write::write_all(&mut std::io::stdout(), b"\x1b[?1003l\x1b[?1007l");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    ratatui::restore();
    // Two linked crossterm copies keep separate raw-mode snapshots: our
    // enable at the top of this function runs first (saving cooked
    // state), then ratatui::init enables raw again through ratatui's own
    // 0.28 copy — whose snapshot is therefore already-raw state.
    // ratatui::restore only unwinds that copy, so disabling through THIS
    // crate's copy here is what restores cooked mode; without it the
    // shell is left unable to echo keystrokes.
    let _ = crossterm::terminal::disable_raw_mode();
    let (exit, resume) = result?;
    if let Some(hint) = resume {
        println!();
        println!("{hint}");
    }
    Ok(exit)
}

#[allow(clippy::too_many_arguments)]
async fn app(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    commands: &mut mpsc::Sender<Command>,
    events: &mut mpsc::Receiver<Event>,
    updates: &mut mpsc::Receiver<UpdateNotice>,
    initial_model: &str,
    mut providers: Vec<ProviderInfo>,
    mut models: Vec<ModelInfo>,
    agents: Vec<(String, String)>,
    header_glyph: &str,
    mut mouse_captured: bool,
    mut fresh: bool,
) -> std::io::Result<(Exit, Option<String>)> {
    use crossterm::event::{Event as TermEvent, KeyCode, KeyModifiers};
    let _ = agents.clone();

    let mut scroll: Option<usize> = None;
    let mut transcript = Transcript::default();

    let mut view_rows;
    let mut input = InputBuffer::default();
    let mut meters = Meters {
        model: initial_model.to_string(),
        // default mode is Allow (yolo default); the bootstrap
        // ModeChanged event re-renders this with the true state
        mode: mode_label(ka_protocol::Mode::Allow).to_string(),
        ..Default::default()
    };
    // the reasoning segment must be visible from the first frame: the
    // bootstrap ModelChanged/EffortChanged events land only once the
    // engine finishes starting, so seed both fields from the catalog
    // list here (the events correct them moments later)
    meters.effort_supported = models
        .iter()
        .find(|m| m.id == initial_model)
        .or_else(|| models.iter().find(|m| initial_model.starts_with(&m.id)))
        .is_some_and(|m| !m.efforts.is_empty());
    if meters.effort_supported && meters.effort.is_empty() {
        meters.effort = "off".to_string();
    }
    let mut busy = false;
    let mut busy_since: Option<Instant> = None;
    let mut quit_armed: Option<Instant> = None;
    // double-Esc (rewind menu) tracking
    let mut last_esc: Option<Instant> = None;
    let mut last_user: Option<String> = None;
    let mut last_error: Option<String> = None;
    let mut live_cache: Option<(String, u16, Vec<ratatui::text::Line<'static>>, Instant)> = None;
    let mut turn_ended = false;
    let mut pending: Option<PendingAsk> = None;
    // user-queued prompts (`+` or alt+q while busy), held locally and auto-sent
    // FIFO, one per turn settle (see the TurnFinished arm below)
    let mut queue: Vec<String> = Vec::new();
    let mut turn_produced = false;
    let mut turn_usage: Option<(u64, u64, u64)> = None; // (input+cache_read, total_in_seen, output)
    let mut plan_started: Option<std::time::SystemTime> = None;
    let mut current_assistant = String::new();
    let mut spills: Vec<String> = Vec::new();
    let mut current_thought = String::new();
    let mut current_tool = String::new();
    let mut live_tool: Option<LiveTool> = None;
    // the most recent finished call's (entry, call) slot: Ctrl+O and
    // click-to-expand toggle its inline output
    let mut last_tool_ref: Option<(usize, usize)> = None;
    // transient action feedback: (message, shown-at); auto-expires
    let mut toast: Option<(String, Instant)> = None;
    // pending image attachment: staged by `/image <path>`, consumed by
    // the next sent prompt
    let mut pending_image: Option<ka_protocol::ImagePart> = None;
    let mut sidebar = SidebarState {
        cwd: shorten_cwd(&std::env::current_dir().unwrap_or_default()),
        branch: detect_branch(),
        ..Default::default()
    };
    let mut exit = None;
    let mut slash_popup: Option<SlashPopup> = None;
    let mut path_popup: Option<PathPopup> = None;
    // last /find query + the row to resume a bare /find after
    let mut find_last: Option<(String, usize)> = None;
    let mut modal: Option<Modal> = None;
    // /tasks and /debug set this so the roster event opens the modal
    // instead of rendering transcript rows; consumed once
    let mut pending_modal: Option<PendingModal> = None;
    let mut picker: Option<InputPicker> = None;
    // clickable sidebar regions + ▲▼ title-row jump targets, refreshed
    // every frame by render()
    let strip_zone: std::cell::Cell<Option<StripZones>> = std::cell::Cell::new(None);
    let title_arrows: std::cell::Cell<Option<TitleArrows>> = std::cell::Cell::new(None);
    // the modal overlay rect and the slash/path/mode-picker popup rect,
    // refreshed every frame by render(): click-outside and the ✕ chip
    // close routing reads them
    let modal_zone: std::cell::Cell<Option<ratatui::layout::Rect>> = std::cell::Cell::new(None);
    let popup_zone: std::cell::Cell<Option<ratatui::layout::Rect>> = std::cell::Cell::new(None);
    let tx_content: std::cell::Cell<Option<(ratatui::layout::Rect, usize)>> =
        std::cell::Cell::new(None);
    // the scrollbar rail's geometry (rect, total rows, visible rows,
    // thumb position, thumb length), refreshed every frame by
    // render(): the mouse router reads it for thumb drags and track
    // paging
    let rail_zone: std::cell::Cell<Option<RailZone>> = std::cell::Cell::new(None);
    // grab offset inside the thumb while a rail drag is active
    let mut rail_drag: Option<usize> = None;
    // the transcript width the last render actually used (0 until the
    // first frame): the tick-side markdown cache keys on it so cache
    // and frame can never disagree about the band/surface width
    let tx_width: std::cell::Cell<u16> = std::cell::Cell::new(0);
    let mut term_events = crossterm::event::EventStream::new();
    // 80 ms: the spinner's frame step, fast enough for a smooth comet
    let mut spin = tokio::time::interval(Duration::from_millis(80));
    // last mouse mode actually applied to the terminal — the loop owns
    // every capture transition (base toggle, overlay open/close,
    // run_external restore) through this single reconcile point
    let mut applied_capture: Option<bool> = None;
    // left-drag selection over the transcript: `press` is an
    // undispatched click anchor, `sel` the picked text span once the
    // drag passes click tolerance — (row, column) anchors in reading
    // order, columns relative to the content area's left edge
    let mut press: Option<(u16, u16)> = None;
    let mut sel: Option<((usize, u16), (usize, u16))> = None;
    // what the pointer currently hovers (capture mode; motion events)
    let mut hover: Option<Hover> = None;
    let mut was_overlay_open = false;
    // last (activity, session) published to an opt-in sink
    let mut reported: Option<(AgentActivity, String)> = None;

    while exit.is_none() {
        // Hybrid mouse: base native leaves the mouse to the terminal
        // (plain drag selects, wheel via alternate-scroll); while any
        // overlay is open the app transiently owns the mouse so its
        // clicks work (✕ chip, click-outside, items). Base capture
        // (Ctrl+M / [tui] mouse = "capture") overrides — always on.
        let overlay_open =
            modal.is_some() || slash_popup.is_some() || path_popup.is_some() || picker.is_some();
        if overlay_open && !was_overlay_open {
            // an overlay TAKING the pointer drops any pending click,
            // selection, or hover with it — but only on the transition:
            // wiping every iteration would kill the ✕-chip hover the
            // Moved handler sets under the open overlay
            press = None;
            sel = None;
            hover = None;
        }
        was_overlay_open = overlay_open;
        let capture_now = mouse_captured || overlay_open;
        if applied_capture != Some(capture_now) {
            applied_capture = Some(capture_now);
            apply_mouse_mode(capture_now);
            // a release to native also ends hover tracking (no more
            // motion events arrive to refresh or clear it)
            if !capture_now {
                hover = None;
            }
        }
        let busy_now = busy;
        let ask = pending.clone();
        // opt-in integrations (Herdr) see one report per state/session
        // change; with no sink installed this is a no-op
        let activity = activity_of(busy_now, ask.is_some());
        let session_now = meters.session.clone();
        if reported.as_ref() != Some(&(activity, session_now.clone())) {
            if let Some(sink) = ACTIVITY.get() {
                sink.publish(AgentReport::Report(ActivityReport {
                    activity,
                    session: session_now.clone(),
                }));
            }
            reported = Some((activity, session_now));
        }
        let input_snapshot = input.text.clone();
        let rsearch = input.search_title();
        let (term_w, term_h) = match terminal.size() {
            Ok(s) => (s.width, s.height),
            Err(_) => (80, 24),
        };
        // the markdown cache keys on the width render actually used,
        // folded exactly like a cached assistant block (W minus its
        // indent), so the flush never reflows; one tick of staleness
        // after a resize is fine
        let last_w = tx_width.get();
        let md_width = assistant_md_width(if last_w == 0 {
            transcript_width(term_w)
        } else {
            last_w
        });
        let input_h = input_height(input_area_rows(
            picker.as_ref(),
            &input.text,
            input_inner_w(term_w),
        ));
        let live = if busy_now {
            let now = Instant::now();
            let stale = live_cache
                .as_ref()
                .map(|(_, _, _, at)| live_stale(*at, now))
                .unwrap_or(true);
            let changed = live_cache
                .as_ref()
                .is_none_or(|(t, w, _, _)| t != &current_assistant || *w != md_width);
            if changed && (stale || turn_ended) {
                live_cache = Some((
                    current_assistant.clone(),
                    md_width,
                    crate::markdown::render(&current_assistant, md_width),
                    now,
                ));
            }
            live_cache.as_ref().map(|(_, _, rows, _)| LiveBlock {
                thought: current_thought.clone(),
                tool_header: current_tool.clone(),
                live_tool: live_tool.clone(),
                md: rows.clone(),
            })
        } else {
            live_cache = None;
            None
        };
        view_rows = visible_rows(term_h, input_h);
        turn_ended = false;
        let cursor = input.cursor;
        terminal.draw(|frame| {
            render(
                frame,
                &mut transcript,
                scroll,
                &input_snapshot,
                cursor,
                busy_now,
                busy_since,
                Instant::now(),
                queue.len(),
                ask.as_ref(),
                live.as_ref(),
                slash_popup.as_ref(),
                path_popup.as_ref(),
                rsearch.as_deref(),
                modal.as_ref(),
                picker.as_ref(),
                &meters,
                &sidebar,
                fresh,
                mouse_captured,
                header_glyph,
                &strip_zone,
                toast
                    .as_ref()
                    .filter(|(_, at)| at.elapsed() < TOAST_TTL)
                    .map(|(m, _)| m.as_str()),
                &title_arrows,
                &modal_zone,
                &popup_zone,
                &tx_content,
                &rail_zone,
                &tx_width,
                sel,
                hover,
            );
        })?;

        // streamed thinking blocks stay expanded while visible; once
        // fully scrolled out of the window they collapse to the 3-row
        // band. Deferred while the user holds a scroll anchor: the
        // height change is only invisible when the view is tail-pinned
        // (a wrapped band settling under a scrolled view would shift
        // content); the TurnStarted backstop still settles everything.
        if scroll.is_none()
            && let Some((_, first_visible)) = tx_content.get()
        {
            transcript.collapse_streamed_above(first_visible);
        }

        tokio::select! {
            biased;
            maybe_term = term_events.next() => {
                if let Some(Ok(TermEvent::Key(key))) = maybe_term {
                    if key.kind == crossterm::event::KeyEventKind::Release {
                        continue;
                    }
                    // Ctrl+C, ahead of every other capture (asks, modals,
                    // popups all swallow chars): busy aborts the run; a
                    // reverse search is cancelled; a non-empty input is
                    // cleared; an empty input quits on the second press
                    if (key.code, key.modifiers) == (KeyCode::Char('c'), KeyModifiers::CONTROL) {
                        if busy {
                            let _ = commands.send(Command::Abort).await;
                            quit_armed = None;
                        } else if input.searching() {
                            input.search_cancel();
                            quit_armed = None;
                        } else if !input.text.is_empty() {
                            input.clear_draft();
                            slash_popup = update_suggestions(&input.text);
                            quit_armed = None;
                        } else if quit_armed
                            .is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(3))
                        {
                            exit = Some(Exit::Quit);
                        } else {
                            quit_armed = Some(Instant::now());
                            pop_toast(&mut toast, "press ctrl+c again to exit");
                        }
                        continue;
                    }
                    // any other keypress breaks a quit arm
                    quit_armed = None;
                    // Ask dialog captures input first
                    if let Some(ask) = pending.as_mut() {
                        match key.code {
                            KeyCode::Up | KeyCode::Left => {
                                ask.selected = ask.selected.saturating_sub(1);
                            }
                            KeyCode::Down | KeyCode::Right => {
                                if ask.selected + 1 < ask.options.len() {
                                    ask.selected += 1;
                                }
                            }
                            KeyCode::Enter | KeyCode::Char(' ') => {
                                let (id, choice) = (ask.id.clone(), ask.selected);
                                let label =
                                    ask.options.get(choice).cloned().unwrap_or_default();
                                pending = None;
                                transcript
                                    .push_separated(Line::Info(format!("· {label}")));
                                let _ = commands.send(Command::Answer { question: id, choice }).await;
                            }
                            KeyCode::Esc => {
                                let id = ask.id.clone();
                                let deny = ask.options.len().saturating_sub(1);
                                let label = ask.options.get(deny).cloned().unwrap_or_default();
                                pending = None;
                                transcript.push_separated(Line::Info(format!("· {label}")));
                                let _ = commands.send(Command::Answer { question: id, choice: deny }).await;
                            }
                            // direct pick: 1..9 answers that option at once
                            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                                let idx = (c as u8 - b'1') as usize;
                                if idx < ask.options.len() {
                                    let id = ask.id.clone();
                                    let label = ask.options[idx].clone();
                                    pending = None;
                                    transcript
                                        .push_separated(Line::Info(format!("· {label}")));
                                    let _ =
                                        commands.send(Command::Answer { question: id, choice: idx }).await;
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }
                    // The input-box picker captures input next: /mode
                    // tiers or /thinking levels, both borrowing the
                    // input box exactly like the permission ask does
                    if let Some(open) = picker.as_mut() {
                        match open {
                            InputPicker::Mode(pk) => match key.code {
                                KeyCode::Esc => picker = None,
                                KeyCode::Up => pk.selected = pk.selected.saturating_sub(1),
                                KeyCode::Down => {
                                    if pk.selected + 1 < MODE_CHOICES.len() {
                                        pk.selected += 1;
                                    }
                                }
                                // direct pick: 1..4 jump to that row
                                KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                                    let idx = (c as u8 - b'1') as usize;
                                    if idx < MODE_CHOICES.len() {
                                        pk.selected = idx;
                                    }
                                }
                                KeyCode::Enter => {
                                    let mode = pk.pick();
                                    picker = None;
                                    let _ = commands.send(Command::SetMode { mode }).await;
                                    // persist like the model pick does: mode wins,
                                    // the other settings keep their saved values
                                    let _ = commands
                                        .send(Command::SaveSettings {
                                            model: None,
                                            effort: None,
                                            mode: Some(mode),
                                            mouse: None,
                                        features: None,
                                        })
                                        .await;
                                }
                                _ => {}
                            },
                            InputPicker::Thinking(tk) => match key.code {
                                KeyCode::Esc => picker = None,
                                KeyCode::Up => tk.selected = tk.selected.saturating_sub(1),
                                KeyCode::Down => {
                                    if tk.selected + 1 < tk.choices.len() {
                                        tk.selected += 1;
                                    }
                                }
                                // direct pick: 1..9 jump to that row
                                KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                                    let idx = (c as u8 - b'1') as usize;
                                    if idx < tk.choices.len() {
                                        tk.selected = idx;
                                    }
                                }
                                KeyCode::Enter => {
                                    let level = tk.pick();
                                    picker = None;
                                    let _ = commands.send(Command::SetEffort { level }).await;
                                    // live + persist, mirroring the /model pick
                                    let _ = commands
                                        .send(Command::SaveSettings {
                                            model: None,
                                            effort: Some(level),
                                            mode: None,
                                            mouse: None,
                                        features: None,
                                        })
                                        .await;
                                    pop_toast(&mut toast, format!("{} {}", crate::icons::THOUGHT, effort_label(level)));
                                }
                                _ => {}
                            },
                        }
                        continue;
                    }
                    // Modal (session picker / settings) captures input next
                    if let Some(open) = modal.as_mut() {
                        match open {
                            Modal::Session(picker) => match key.code {
                                KeyCode::Esc => modal = None,
                                KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
                                KeyCode::Down => {
                                    let n = picker.rows().len();
                                    if picker.selected + 1 < n {
                                        picker.selected += 1;
                                    }
                                }
                                KeyCode::Backspace => {
                                    picker.filter.pop();
                                    picker.selected = 0;
                                }
                                KeyCode::Enter => {
                                    let id = picker.pick();
                                    let target = id.unwrap_or_else(|| "new".to_string());
                                    fresh = target == "new";
                                    let _ = commands
                                        .send(Command::SwitchStrand { id: target })
                                        .await;
                                    modal = None;
                                    busy = true;
                                }
                                KeyCode::Char(c) => picker.filter.push(c),
                                _ => {}
                            },
                            Modal::Help => {
                                if key.code == KeyCode::Esc || key.code == KeyCode::Enter {
                                    modal = None;
                                }
                            }
                            Modal::Key(prompt) => match key.code {
                                KeyCode::Esc => modal = None,
                                KeyCode::Backspace => {
                                    prompt.input.pop();
                                }
                                KeyCode::Enter => {
                                    let value = prompt.input.trim().to_string();
                                    let drill = prompt.drill.clone();
                                    let env_var = prompt.env_var.clone();
                                    let pending_model = prompt.pending_model.clone();
                                    let apply = pending_model.filter(|_| !value.is_empty());
                                    if !value.is_empty() {
                                        let _ = commands
                                            .send(Command::SaveApiKey {
                                                env_var: env_var.clone(),
                                                value,
                                            })
                                            .await;
                                        // the saved key resolves in this
                                        // process now; reflect it in both
                                        // lists so ✓/✗ marks stay truthful
                                        mark_key_set(&mut models, &mut providers, &env_var);
                                        pop_toast(&mut toast, "✓ key saved");
                                    }
                                    if let Some(selector) = apply {
                                        // the key unblocked this unified
                                        // `/model` pick: apply it now
                                        let _ = commands
                                            .send(Command::SetModel {
                                                selector: selector.clone(),
                                            })
                                            .await;
                                        let _ = commands
                                            .send(Command::SaveSettings {
                                                model: Some(selector),
                                                effort: None,
                                                mode: None,
                                                mouse: None,
                                            features: None,
                                            })
                                            .await;
                                        modal = None;
                                    } else {
                                        modal = drill.map(|vendor| {
                                            Modal::Model(ModelPicker {
                                                models: models.clone(),
                                                vendor: Some(vendor),
                                                selected: 0,
                                                filter: String::new(),
                                            })
                                        });
                                    }
                                }
                                KeyCode::Char(c) => prompt.input.push(c),
                                _ => {}
                            },
                            Modal::Provider(picker) => match key.code {
                                KeyCode::Esc => modal = None,
                                KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
                                KeyCode::Down => {
                                    let n = picker.rows().len();
                                    if picker.selected + 1 < n {
                                        picker.selected += 1;
                                    }
                                }
                                KeyCode::Backspace => {
                                    picker.filter.pop();
                                    picker.selected = 0;
                                }
                                KeyCode::Enter => {
                                    let picked = picker.pick().cloned();
                                    match picked {
                                        // connected (or keyless): list its models
                                        Some(p) if p.env_var.is_empty() || p.key_set => {
                                            modal = Some(Modal::Model(ModelPicker {
                                                models: models.clone(),
                                                vendor: Some(p.name),
                                                selected: 0,
                                                filter: String::new(),
                                            }));
                                        }
                                        // unconfigured keyed provider: ask
                                        // for the key, then drill into its
                                        // models once it saves
                                        Some(p) => {
                                            let doc_url = models
                                                .iter()
                                                .find(|m| {
                                                    m.id.split('/').next()
                                                        == Some(p.name.as_str())
                                                        && !m.key_env.is_empty()
                                                })
                                                .map(|m| m.doc_url.clone())
                                                .unwrap_or_default();
                                            modal = Some(Modal::Key(KeyPrompt {
                                                env_var: p.env_var,
                                                provider: p.name.clone(),
                                                doc_url,
                                                input: String::new(),
                                                drill: Some(p.name),
                                                pending_model: None,
                                            }));
                                        }
                                        None => modal = None,
                                    }
                                }
                                KeyCode::Char(c) => {
                                    picker.filter.push(c);
                                    picker.selected = 0;
                                }
                                _ => {}
                            },
                            Modal::Model(picker) => match key.code {
                                KeyCode::Esc => {
                                    // a vendor-locked list returns to the
                                    // provider stage; the plain list closes
                                    modal = picker.vendor.as_deref().map(|_| {
                                        Modal::Provider(ProviderPicker::new(
                                            providers.clone(),
                                            &models,
                                        ))
                                    });
                                }
                                KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
                                KeyCode::Down => {
                                    let n = picker.rows().len();
                                    if picker.selected + 1 < n {
                                        picker.selected += 1;
                                    }
                                }
                                KeyCode::Backspace => {
                                    picker.filter.pop();
                                    picker.selected = 0;
                                }
                                KeyCode::Enter => {
                                    if let Some(selector) = picker.pick() {
                                        // a keyed model without its key
                                        // asks for one first; the pick
                                        // applies once the key saves
                                        let missing = picker
                                            .models
                                            .iter()
                                            .find(|m| m.id == selector)
                                            .is_some_and(|m| {
                                                !m.key_set && !m.key_env.is_empty()
                                            });
                                        if missing {
                                            if let Some(m) =
                                                picker.models.iter().find(|m| m.id == selector)
                                            {
                                                modal = Some(Modal::Key(KeyPrompt {
                                                    env_var: m.key_env.clone(),
                                                    provider: selector
                                                        .split('/')
                                                        .next()
                                                        .unwrap_or("")
                                                        .to_string(),
                                                    doc_url: m.doc_url.clone(),
                                                    input: String::new(),
                                                    drill: None,
                                                    pending_model: Some(selector.clone()),
                                                }));
                                                continue;
                                            }
                                        }
                                        let _ = commands
                                            .send(Command::SetModel {
                                                selector: selector.clone(),
                                            })
                                            .await;
                                        // the pick also becomes the default for
                                        // future conversations
                                        let _ = commands
                                            .send(Command::SaveSettings {
                                                model: Some(selector),
                                                effort: None,
                                                mode: None,
                                                mouse: None,
                                            features: None,
                                            })
                                            .await;
                                    }
                                    modal = None;
                                }
                                KeyCode::Char(c) => {
                                    picker.filter.push(c);
                                    picker.selected = 0;
                                }
                                _ => {}
                            },
            Modal::Settings(panel) => {
                                let editing = panel.edit.is_some();
                                match key.code {
                                    KeyCode::Esc => {
                                        if editing {
                                            panel.edit = None;
                                        } else {
                                            modal = None;
                                        }
                                    }
                                    KeyCode::Up if !editing => {
                                        panel.selected = panel.selected.saturating_sub(1)
                                    }
                                    KeyCode::Down if !editing => {
                                        if panel.selected + 1 < SettingsPanel::ROWS {
                                            panel.selected += 1;
                                        }
                                    }
                                    KeyCode::Backspace if editing => {
                                        if let Some(edit) = panel.edit.as_mut() {
                                            edit.pop();
                                        }
                                    }
                                    KeyCode::Char(c) if editing => {
                                        if let Some(edit) = panel.edit.as_mut() {
                                            edit.push(c);
                                        }
                                    }
                                    KeyCode::Enter => match panel.selected {
                                        0 => {
                                            if editing {
                                                let value =
                                                    panel.edit.clone().unwrap_or_default();
                                                if !value.trim().is_empty() {
                                                    panel.model = value.trim().to_string();
                                                    let _ = commands
                                                        .send(Command::SetModel {
                                                            selector: panel.model.clone(),
                                                        })
                                                        .await;
                                                }
                                                panel.edit = None;
                                            } else {
                                                panel.edit = Some(panel.model.clone());
                                            }
                                        }
                                        1 => {
                                            // hand off to the /mode picker: it
                                            // borrows the input box, so the
                                            // settings panel closes first
                                            let mode = panel.mode;
                                            modal = None;
                                            picker = Some(InputPicker::Mode(ModePicker::for_mode(mode)));
                                        }
                                        _ => {
                                            let level = panel.cycle_effort();
                                            let _ = commands
                                                .send(Command::SetEffort { level })
                                                .await;
                                        }
                                    },
                                    KeyCode::Char('s') if !editing => {
                                        // "(canned)" is the no-model placeholder,
                                        // not a selector — persisting it would
                                        // poison every later launch
                                        let model = (panel.model != "(canned)")
                                            .then(|| panel.model.clone());
                                        let _ = commands
                                            .send(Command::SaveSettings {
                                                model,
                                                effort: panel.effort,
                                                mode: Some(panel.mode),
                                                mouse: None,
                                            features: None,
                                            })
                                            .await;
                                    }
                                    _ => {}
                                }
                            }
                            Modal::Rewind { items, selected } => {
                                match key.code {
                                    KeyCode::Esc => modal = None,
                                    KeyCode::Up => {
                                        *selected = selected.saturating_sub(1);
                                    }
                                    KeyCode::Down => {
                                        if *selected + 1 < items.len() {
                                            *selected += 1;
                                        }
                                    }
                                    KeyCode::Enter | KeyCode::Char('r') | KeyCode::Char('e') => {
                                        let edit = key.code == KeyCode::Char('e');
                                        let (turns, prompt) = items[*selected].clone();
                                        modal = None;
                                        let _ = commands
                                            .send(Command::Rewind { turns: turns as u32 })
                                            .await;
                                        transcript.rewind_user(turns);
                                        if edit {
                                            input.text = prompt;
                                            input.cursor = input.text.chars().count();
                                            transcript.push_separated(Line::Info(
                                                "✎ edit & resend — ⏎ sends when ready"
                                                    .into(),
                                            ));
                                        } else {
                                            transcript.push_separated(Line::Info(format!(
                                                "⏪ rewound {turns} turn(s) — files unchanged (/undo restores edits)"
                                            )));
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            Modal::Memory {
                                rows,
                                inbox,
                                selected,
                            } => {
                                match key.code {
                                    KeyCode::Esc => modal = None,
                                    KeyCode::Up if !inbox.is_empty() => {
                                        *selected = selected.saturating_sub(1);
                                    }
                                    KeyCode::Down if !inbox.is_empty() => {
                                        *selected = (*selected + 1).min(inbox.len() - 1);
                                    }
                                    KeyCode::Enter
                                    | KeyCode::Char('a')
                                    | KeyCode::Char('u')
                                    | KeyCode::Char('d')
                                        if !inbox.is_empty() =>
                                    {
                                        let cwd = std::env::current_dir()
                                            .unwrap_or_else(|_| std::path::PathBuf::from("."));
                                        let line = inbox.remove(*selected);
                                        if key.code != KeyCode::Char('d') {
                                            let user = key.code == KeyCode::Char('u');
                                            // best-effort: a failed append leaves the
                                            // note staged for a later retry
                                            if accept_memory_note(&cwd, &line, user).is_ok() {
                                                write_memory_inbox(&cwd, inbox);
                                            } else {
                                                inbox.insert(*selected, line);
                                            }
                                        } else {
                                            write_memory_inbox(&cwd, inbox);
                                        }
                                        if *selected >= inbox.len() {
                                            *selected = inbox.len().saturating_sub(1);
                                        }
                                        *rows = memory_modal_rows(&cwd);
                                    }
                                    _ => {}
                                }
                            }
                                            Modal::Usage { .. } => {
                                                modal = None;
                                            }
                                            Modal::Todos { .. } | Modal::Skills { .. } | Modal::Info { .. } => {
                                                modal = None;
                                            }
                            Modal::Context { .. } => {
                                modal = None;
                            }
                            Modal::Features { entries, selected } => match key.code {
                                KeyCode::Esc => modal = None,
                                KeyCode::Up => {
                                    while *selected > 0 {
                                        *selected -= 1;
                                        if !entries[*selected].header {
                                            break;
                                        }
                                    }
                                }
                                KeyCode::Down => {
                                    while *selected + 1 < entries.len() {
                                        *selected += 1;
                                        if !entries[*selected].header {
                                            break;
                                        }
                                    }
                                }
                                // ⏎ (or space) flips the selected row; the
                                // optimistic flip lands instantly and the
                                // engine's FeaturesChanged keeps the state
                                // truthful even if the command raced
                                KeyCode::Enter | KeyCode::Char(' ') => {
                                    if let Some(row) = entries.get(*selected).cloned() {
                                        if row.spec == "sandbox" {
                                            let mode =
                                                if row.on { "off" } else { "fs" }.to_string();
                                            let _ = commands
                                                .send(Command::SetSandbox { mode })
                                                .await;
                                            if let Some(r) = entries.get_mut(*selected) {
                                                r.on = !row.on;
                                            }
                                        } else if let Ok(spec) =
                                            row.spec.parse::<ka_protocol::FeatureSpec>()
                                        {
                                            let _ = commands
                                                .send(Command::SetFeature {
                                                    spec,
                                                    enabled: !row.on,
                                                })
                                                .await;
                                            if let Some(r) = entries.get_mut(*selected) {
                                                r.on = !row.on;
                                            }
                                        }
                                    }
                                }
                                // `s` persists the current disabled set as
                                // the user-layer default ([features] disable)
                                KeyCode::Char('s') => {
                                    let disabled = sidebar.inventory.disabled.clone();
                                    let _ = commands
                                        .send(Command::SaveSettings {
                                            model: None,
                                            effort: None,
                                            mode: None,
                                            mouse: None,
                                            features: Some(disabled),
                                        })
                                        .await;
                                    transcript.push_separated(Line::Info(
                                        "saved feature toggles as the default (user ka.toml)".into(),
                                    ));
                                }
                                _ => {}
                            },
                            Modal::Tree {
                                items,
                                actions,
                                selected,
                            } => match key.code {
                                KeyCode::Esc => modal = None,
                                // headers are scenery: navigation skips
                                KeyCode::Up => {
                                    if let Some(prev) = (0..*selected)
                                        .rev()
                                        .find(|&i| !matches!(actions[i], TreeAction::Header))
                                    {
                                        *selected = prev;
                                    }
                                }
                                KeyCode::Down => {
                                    if let Some(next) = (*selected + 1..items.len())
                                        .find(|&i| !matches!(actions[i], TreeAction::Header))
                                    {
                                        *selected = next;
                                    }
                                }
                                KeyCode::Enter | KeyCode::Char('e') => {
                                    let edit = key.code == KeyCode::Char('e');
                                    match actions.get(*selected).cloned() {
                                        Some(TreeAction::Attach(id)) if !edit => {
                                            fresh = id == "new";
                                            let _ = commands
                                                .send(Command::SwitchStrand { id })
                                                .await;
                                            busy = true;
                                            // close on attach: a stale tree
                                            // over the replayed session
                                            // swallows the next keys
                                            modal = None;
                                        }
                                        Some(TreeAction::Fork { turns, prompt }) => {
                                            // mid-turn the turn's command
                                            // drain swallows ForkStrand —
                                            // fork only from idle, like
                                            // /update
                                            if busy {
                                                transcript.push_separated(Line::Warn(
                                                    "⏳ turn running — esc to abort first"
                                                        .into(),
                                                ));
                                                continue;
                                            }
                                            // branch point: the current
                                            // strand stays intact, the
                                            // fork child continues from
                                            // before the picked input
                                            let _ = commands
                                                .send(Command::ForkStrand { turns })
                                                .await;
                                            busy = true;
                                            modal = None;
                                            // going back lands the picked
                                            // prompt in the composer: the
                                            // user decides whether to reuse,
                                            // edit, or delete it. An existing
                                            // draft is parked in the kill ring
                                            // (Ctrl+Y) rather than dropped.
                                            if !input.text.is_empty() {
                                                input.clear_draft();
                                            }
                                            input.text = prompt;
                                            input.cursor = input.text.chars().count();
                                            transcript.push_separated(Line::Info(
                                                if edit {
                                                    "✎ forked — edit & resend, ⏎ sends when ready"
                                                        .to_string()
                                                } else {
                                                    format!(
                                                        "⑂ forked {turns} turn(s) back — prompt in the box; original intact in /tree"
                                                    )
                                                },
                                            ));
                                        }
                                        _ => {}
                                    }
                                }
                                _ => {}
                            },
                            Modal::Prompts { items, selected } => match key.code {
                                KeyCode::Esc => modal = None,
                                KeyCode::Up => {
                                    *selected = selected.saturating_sub(1);
                                }
                                KeyCode::Down => {
                                    if *selected + 1 < items.len() {
                                        *selected += 1;
                                    }
                                }
                                KeyCode::Enter => {
                                    if let Some(row) = items.get(*selected).cloned() {
                                        let (spec, args) = parse_prompt_row(&row);
                                        modal = None;
                                        if args.is_empty() {
                                            if let Some((server, name)) = spec {
                                                let _ = commands
                                                    .send(Command::CallPrompt {
                                                        server,
                                                        name,
                                                        args: Default::default(),
                                                    })
                                                    .await;
                                                busy = true;
                                            }
                                        } else {
                                            // arg-picking: prefill the input; the
                                            // user appends key=value pairs
                                            if let Some((server, name)) = spec {
                                                input.text =
                                                    format!("/prompt {server}/{name} ");
                                                input.cursor = input.text.chars().count();
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            },
                            Modal::Spills { items, selected } => match key.code {
                                KeyCode::Esc => modal = None,
                                KeyCode::Up => {
                                    *selected = selected.saturating_sub(1);
                                }
                                KeyCode::Down => {
                                    if *selected + 1 < items.len() {
                                        *selected += 1;
                                    }
                                }
                                KeyCode::Enter => {
                                    if let Some(path) = items.get(*selected).cloned() {
                                        modal = None;
                                        if !std::path::Path::new(&path).exists() {
                                            transcript.push_separated(Line::Info(format!(
                                                "spill file is gone: {path}"
                                            )));
                                        } else {
                                            let opened =
                                                match std::env::var("PAGER")
                                                    .ok()
                                                    .filter(|p| !p.is_empty())
                                                {
                                                    Some(pager) => run_external(
                                                        &pager,
                                                        &[path.as_str()],
                                                        terminal,
                                                        capture_now,
                                                    ),
                                                    None => run_external(
                                                        "less",
                                                        &["-R", path.as_str()],
                                                        terminal,
                                                        capture_now,
                                                    ),
                                                };
                                            if let Err(e) = opened {
                                                transcript.push_separated(Line::Info(format!("pager failed: {e}")));
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            },
                            Modal::Tasks { entries, selected } => match key.code {
                                KeyCode::Esc => modal = None,
                                KeyCode::Up => {
                                    *selected = selected.saturating_sub(1);
                                }
                                KeyCode::Down => {
                                    if *selected + 1 < entries.len() {
                                        *selected += 1;
                                    }
                                }
                                KeyCode::Enter => {
                                    // page the selected task's full result;
                                    // the modal stays open and is replaced
                                    // when TaskDetail arrives
                                    if let Some((Some(id), _)) = entries.get(*selected) {
                                        let _ = commands.send(Command::TaskDetail { id: *id }).await;
                                    }
                                }
                                _ => {}
                            },
                            Modal::TaskDetail { text, scroll, .. } => {
                                let visible = pager_visible(view_rows);
                                if pager_keys(key.code, text.lines().count(), scroll, visible) {
                                    modal = None;
                                }
                            }
                            Modal::Debug { rows, scroll } => {
                                let visible = pager_visible(view_rows);
                                if pager_keys(key.code, rows.len(), scroll, visible) {
                                    modal = None;
                                }
                            }
                        }
                        continue;
                    }
                    // Path-completion popup keys. Mutually exclusive with the
                    // slash popup; unlisted keys close the popup and fall
                    // through to the main match.
                    if path_popup.is_some() {
                        enum PathKey {
                            Navigate,
                            Accept,
                            Backspace,
                            Char(char),
                            Close,
                            Fallthrough,
                        }
                        let intent = match key.code {
                            KeyCode::Up | KeyCode::Down => PathKey::Navigate,
                            KeyCode::Tab | KeyCode::Enter => PathKey::Accept,
                            KeyCode::Esc => PathKey::Close,
                            KeyCode::Backspace => PathKey::Backspace,
                            KeyCode::Char(c) => {
                                if key
                                    .modifiers
                                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                                {
                                    // shortcuts like Ctrl+C must not be eaten
                                    // as literal input: close and fall through
                                    PathKey::Fallthrough
                                } else {
                                    PathKey::Char(c)
                                }
                            }
                            _ => PathKey::Fallthrough,
                        };
                        match intent {
                            PathKey::Navigate => {
                                if let Some(pp) = path_popup.as_mut() {
                                    match key.code {
                                        KeyCode::Up => {
                                            pp.selected = pp.selected.saturating_sub(1)
                                        }
                                        _ => {
                                            if pp.selected + 1 < pp.entries.len() {
                                                pp.selected += 1;
                                            }
                                        }
                                    }
                                }
                            }
                            PathKey::Accept => {
                                let snapshot = path_popup.as_ref().and_then(|pp| {
                                    pp.entries.get(pp.selected).cloned().map(|e| {
                                        (e, pp.token_start, pp.prefix.clone(), pp.mentions)
                                    })
                                });
                                match snapshot {
                                    None => path_popup = None,
                                    Some(((name, is_dir), tok_start, prefix, mentions)) => {
                                        let tok_len = path_token(&input.text, input.cursor)
                                            .map(|(_, t)| t.chars().count())
                                            .unwrap_or(0);
                                        let suffix = if is_dir {
                                            "/"
                                        } else if mentions {
                                            // a finished mention reads like a word
                                            " "
                                        } else {
                                            ""
                                        };
                                        let insert = format!("{prefix}{name}{suffix}");
                                        let (text, cursor) = complete_token(
                                            &input.text,
                                            tok_start,
                                            tok_len,
                                            &insert,
                                        );
                                        input.text = text;
                                        input.cursor = cursor;
                                        if is_dir {
                                            // descend: list the entered directory
                                            let word =
                                                path_token(&input.text, input.cursor)
                                                    .map(|(_, t)| t)
                                                    .unwrap_or_default();
                                            let (matches, new_prefix) = if mentions {
                                                let rel =
                                                    word.strip_prefix('@').unwrap_or(&word);
                                                let root = std::env::current_dir()
                                                    .unwrap_or_default()
                                                    .join(rel);
                                                (walk_files(&root, WALK_CAP), format!("@{rel}"))
                                            } else {
                                                let (dir2, base2, _) =
                                                    split_path_token(&word);
                                                (
                                                    list_matches(&dir2, &base2),
                                                    dir2,
                                                )
                                            };
                                            path_popup = if matches.is_empty() {
                                                None
                                            } else {
                                                Some(PathPopup {
                                                    entries: matches,
                                                    selected: 0,
                                                    token_start: tok_start,
                                                    prefix: new_prefix,
                                                    mentions,
                                                })
                                            };
                                        } else {
                                            path_popup = None;
                                        }
                                    }
                                }
                            }
                            PathKey::Backspace => {
                                path_popup = None;
                                input.backspace();
                                slash_popup = update_suggestions(&input.text);
                            }
                            PathKey::Char(c) => {
                                path_popup = None;
                                input.insert(c);
                                slash_popup = update_suggestions(&input.text);
                            }
                            PathKey::Close => path_popup = None,
                            PathKey::Fallthrough => path_popup = None,
                        }
                        if !matches!(intent, PathKey::Fallthrough) {
                            continue;
                        }
                    }
                    match (key.code, key.modifiers) {
                        (KeyCode::Char('l'), KeyModifiers::CONTROL) => {
                            // viewport wipe; transcript state untouched
                            let _ = terminal.clear();
                            continue;
                        }
                        (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
                            // skills & inventory popup (button on the strip)
                            modal = Some(Modal::Skills {
                                rows: inventory_rows(&sidebar, 64),
                            });
                            continue;
                        }
                        (KeyCode::Char('o'), KeyModifiers::CONTROL) => {
                            // expand/collapse the last tool call in place
                            match last_tool_ref {
                                Some((e, c)) => transcript.toggle_tool_call(e, c),
                                None => transcript
                                    .push_separated(Line::Info("· no tool output yet".into())),
                            }
                            continue;
                        }
                        (KeyCode::Char('o'), KeyModifiers::ALT) => {
                            // todos popup (button on the strip) — works
                            // mid-turn too, like Ctrl+T
                            modal = Some(Modal::Todos {
                                rows: todos_rows(&sidebar, 64),
                            });
                            continue;
                        }
                        (KeyCode::Char('i'), KeyModifiers::ALT) => {
                            // session info popup (button on the strip)
                            modal = Some(Modal::Info {
                                rows: info_rows(&sidebar, &meters, 64),
                            });
                            continue;
                        }
                        (KeyCode::Char('t'), KeyModifiers::ALT) => {
                            // fold/unfold thinking blocks in the transcript
                            transcript.set_thoughts_open(!transcript.thoughts_open);
                            continue;
                        }
                        (KeyCode::Char('v'), KeyModifiers::CONTROL) => {
                            // ctrl+v reads the system clipboard into
                            // the input; terminals that intercept
                            // ctrl+v deliver it as bracketed paste
                            // instead
                            if modal.is_none() && pending.is_none() {
                                let text = clipboard_text().await;
                                input.insert_str(&text);
                            }
                        }
                        // Reverse history search: while active it captures
                        // editing keys, so its arms sit ahead of the
                        // standard Esc/Enter/Backspace/char handling.
                        (KeyCode::Char('m'), KeyModifiers::CONTROL)
                            if !busy && slash_popup.is_none() && path_popup.is_none() =>
                        {
                            // Ctrl+M: the runtime mouse toggle — capture
                            // (dashboard: sidebar + click zones + ⇧drag)
                            // ⇄ native (text mode: full-width chat, plain
                            // drag selects, wheel via alternate-scroll).
                            // kitty-protocol terminals deliver Ctrl+M as a
                            // distinct key; plain terminals read it as
                            // Enter, so there `[tui] mouse = "capture"`
                            // is the way back. The choice persists.
                            mouse_captured = !mouse_captured;
                            rail_drag = None;
                            // the loop-top reconcile applies the new
                            // effective mode (base toggle or transient)
                            let _ = commands
                                .send(Command::SaveSettings {
                                    model: None,
                                    effort: None,
                                    mode: None,
                                    mouse: mouse_setting(mouse_captured),
                                features: None,
                                })
                                .await;
                            pop_toast(&mut toast, mouse_mode_text(mouse_captured));
                        }
                        (KeyCode::Char('r'), KeyModifiers::CONTROL)
                            if input.searching()
                                || (!busy && slash_popup.is_none() && path_popup.is_none()) =>
                        {
                            if input.searching() {
                                input.search_next();
                            } else {
                                input.search_start();
                            }
                        }
                        (KeyCode::Esc, _) if input.searching() => input.search_cancel(),
                        (KeyCode::Enter, _) if input.searching() => input.search_accept(),
                        (KeyCode::Backspace, _) if input.searching() => input.search_backspace(),
                        (KeyCode::Tab, _) if input.searching() => {}
                        (KeyCode::Up, _) if input.searching() => {}
                        (KeyCode::Down, _) if input.searching() => {}
                        (KeyCode::Esc, _) if input.searching() => input.search_cancel(),
                        // a popup or path-completion overlay owns Esc:
                        // close it, never abort a running turn
                        (KeyCode::Esc, _)
                            if slash_popup.is_some() || path_popup.is_some() =>
                        {
                            slash_popup = None;
                            path_popup = None;
                            continue;
                        }
                        (KeyCode::Esc, _) if busy => {
                            let _ = commands.send(Command::Abort).await;
                        }
                        (KeyCode::Esc, _)
                            if !busy
                                && slash_popup.is_none()
                                && path_popup.is_none()
                                && pending.is_none()
                                && input.text.is_empty() =>
                        {
                            // double-Esc on an empty idle input opens the
                            // rewind menu (claude-code muscle memory)
                            let now = Instant::now();
                            let doubled = last_esc
                                .is_some_and(|t| now.duration_since(t) < Duration::from_millis(600));
                            if doubled {
                                last_esc = None;
                                let items: Vec<(usize, String)> = transcript
                                    .entries()
                                    .iter()
                                    .rev()
                                    .filter_map(|l| match l {
                                        Line::User(t) if !t.trim_start().starts_with('/') => {
                                            Some(t.clone())
                                        }
                                        _ => None,
                                    })
                                    .take(20)
                                    .enumerate()
                                    .map(|(i, t)| (i + 1, t))
                                    .collect();
                                if !items.is_empty() {
                                    modal = Some(Modal::Rewind {
                                        items,
                                        selected: 0,
                                    });
                                }
                            } else {
                                last_esc = Some(now);
                            }
                        }
                        (KeyCode::Esc, _) if scroll.is_some() => scroll = None,
                        (KeyCode::Enter, KeyModifiers::SHIFT) => input.newline(),
                        (KeyCode::Char('j'), KeyModifiers::CONTROL) => input.newline(),
                        (KeyCode::Enter, _) => {
                            let text = input.take();
                            // a submitted line invalidates any completion popup
                            slash_popup = None;
                            path_popup = None;
                            if text.trim().is_empty() {
                                continue;
                            }
                            if text.trim() == "/find" || text.trim().starts_with("/find ") {
                                // transcript search: purely local, no engine
                                // roundtrip. Fresh query starts at the scroll
                                // anchor (tail viewport top when pinned); a
                                // bare /find resumes just past the last hit.
                                let fresh = text
                                    .trim()
                                    .strip_prefix("/find")
                                    .map(str::trim)
                                    .filter(|q| !q.is_empty());
                                let (q, from) = match fresh {
                                    Some(q) => {
                                        let total = transcript.total_rows();
                                        let anchor = scroll
                                            .unwrap_or_else(|| total.saturating_sub(view_rows.min(total)));
                                        (q.to_string(), anchor)
                                    }
                                    None => match &find_last {
                                        Some((q, row)) => (q.clone(), row + 1),
                                        None => {
                                            transcript.push_separated(Line::Info("no previous /find".into()));
                                            continue;
                                        }
                                    },
                                };
                                match transcript.find_from(from, &q) {
                                    Some((_, offset)) => {
                                        scroll = Some(offset);
                                        find_last = Some((q, offset));
                                    }
                                    None => {
                                        transcript.push_separated(Line::Info(format!("no matches for '{q}'")));
                                        // resume past the searched-from row so a
                                        // later /find re-scans only fresh rows
                                        find_last = Some((q, from));
                                    }
                                }
                                continue;
                            }
                            if text.trim() == "/features" || text.trim().starts_with("/features ") {
                                // feature toggles: the panel, or direct
                                // `/features on|off spec[,spec…]`. Local (like
                                // /find): the engine command is assembled
                                // here, one SetFeature per spec.
                                let args = text.trim().strip_prefix("/features").map(str::trim).unwrap_or("");
                                if args.is_empty() {
                                    modal = Some(Modal::Features {
                                        entries: feature_rows(&sidebar),
                                        selected: 0,
                                    });
                                } else {
                                    let mut parts = args.split_whitespace();
                                    match (parts.next(), parts.next()) {
                                        (Some(action), Some(list))
                                            if action == "on" || action == "off" =>
                                        {
                                            let enabled = action == "on";
                                            if busy {
                                                transcript.push_separated(Line::Info(
                                                    "⏳ turn running — toggles apply when it settles".into(),
                                                ));
                                            }
                                            let mut sent = Vec::new();
                                            let mut bad = Vec::new();
                                            for spec in list.split(',').map(str::trim) {
                                                match spec
                                                    .parse::<ka_protocol::FeatureSpec>()
                                                {
                                                    Ok(spec) => {
                                                        let _ = commands
                                                            .send(Command::SetFeature {
                                                                spec: spec.clone(),
                                                                enabled,
                                                            })
                                                            .await;
                                                        sent.push(spec.to_string());
                                                    }
                                                    Err(_) => bad.push(spec.to_string()),
                                                }
                                            }
                                            if !sent.is_empty() {
                                                transcript.push_separated(Line::Info(format!(
                                                    "{} {}",
                                                    if enabled { "⊘ off:" } else { "⊙ on:" },
                                                    sent.join(", ")
                                                )));
                                            }
                                            if !bad.is_empty() {
                                                transcript.push_separated(Line::Warn(format!(
                                                    "unknown feature spec(s): {} — try /features for the panel",
                                                    bad.join(", ")
                                                )));
                                            }
                                        }
                                        _ => {
                                            transcript.push_separated(Line::Info(
                                                "usage: /features [on|off spec[,spec…]] — bare /features opens the panel"
                                                    .into(),
                                            ));
                                        }
                                    }
                                }
                                continue;
                            }
                            if text.trim() == "/sandbox" || text.trim().starts_with("/sandbox ") {
                                let arg = text
                                    .trim()
                                    .strip_prefix("/sandbox")
                                    .map(str::trim)
                                    .unwrap_or("");
                                match arg {
                                    "off" | "fs" => {
                                        if busy {
                                            transcript.push_separated(Line::Info(
                                                "⏳ turn running — the sandbox swap applies when it settles"
                                                    .into(),
                                            ));
                                        }
                                        let _ = commands
                                            .send(Command::SetSandbox { mode: arg.to_string() })
                                            .await;
                                        transcript.push_separated(Line::Info(format!(
                                            "sandbox {arg}"
                                        )));
                                    }
                                    _ => {
                                        transcript.push_separated(Line::Info(
                                            "usage: /sandbox off|fs".into(),
                                        ));
                                    }
                                }
                                continue;
                            }
                            if text.trim() == "/retry" {
                                if busy {
                                    transcript.push_separated(Line::Warn(
                                        "⏳ turn running — esc to abort first".into(),
                                    ));
                                } else if let Some(p) = last_user.clone() {
                                    // retry = a fresh turn with the same prompt
                                    transcript.push_separated(Line::User(p.clone()));
                                    busy = true;
                                    let _ = commands.send(Command::Prompt { text: p, schema: None, images: Vec::new(), allowed_tools: None, model: None, skills: Vec::new() }).await;
                                } else {
                                    transcript.push_separated(Line::Info("nothing to retry yet".into()));
                                }
                                continue;
                            }
                            if text.trim() == "/context" {
                                // mid-turn the engine cannot serve the
                                // breakdown (the turn owns the command
                                // channel) — gate it like /retry
                                if busy {
                                    transcript.push_separated(Line::Warn(
                                        "⏳ turn running — /context works when idle".into(),
                                    ));
                                } else {
                                    let _ = commands.send(Command::ContextBreakdown).await;
                                }
                                continue;
                            }
                            if text.trim() == "/copy" {
                                let last = transcript
                                    .entries()
                                    .iter()
                                    .rev()
                                    .find_map(|l| match l {
                                        Line::Assistant(s) => Some(s.clone()),
                                        _ => None,
                                    });
                                match last {
                                    None => transcript.push_separated(Line::Info(
                                        "no assistant reply to copy".into(),
                                    )),
                                    Some(s) => {
                                        // OSC52 into the clipboard; the ack is a toast
                                        let mut stdout = std::io::stdout().lock();
                                        let _ = stdout.write_all(b"\x1b]52;c;");
                                        let _ = stdout.write_all(b64encode(&s).as_bytes());
                                        let _ = stdout.write_all(b"\x07");
                                        let _ = stdout.flush();
                                        pop_toast(&mut toast, "✓ copied last reply");
                                    }
                                }
                                continue;
                            }
                            // /image: stage an attachment instead of a prompt
                            if text.starts_with("/image ") || text == "/image" {
                                match handle_image_command(&text, &mut pending_image) {
                                    Some(Ok(note)) => {
                                        pop_toast(&mut toast, note);
                                    }
                                    Some(Err(e)) => transcript.push_separated(Line::Info(e)),
                                    None => {}
                                }
                                input.text.clear();
                                input.cursor = 0;
                                continue;
                            }
                            // /clip: paste a clipboard image as the
                            // next prompt's attachment
                            if text.trim() == "/clip" {
                                input.text.clear();
                                input.cursor = 0;
                                let staged = match clip_image_bytes().await {
                                    Ok(bytes) => match image_part_from_bytes(&bytes) {
                                        Ok(part) => {
                                            let kb = bytes.len() / 1024;
                                            let note = format!(
                                                "[clip attachment · {} · {kb}KB]",
                                                part.media_type
                                            );
                                            pending_image = Some(part);
                                            Ok(note)
                                        }
                                        Err(e) => Err(e),
                                    },
                                    Err(e) => Err(e),
                                };
                                match staged {
                                    Ok(note) => pop_toast(&mut toast, note),
                                    Err(e) => transcript.push_separated(Line::Info(e)),
                                }
                                continue;
                            }
                            // /update: exit into the updater, which
                            // re-execs ka on the same session — only
                            // from an idle app, never mid-turn
                            if text.split_whitespace().next() == Some("/update") {
                                if text.trim() != "/update" {
                                    transcript.push_separated(Line::Warn(
                                        "usage: /update (no arguments)".into(),
                                    ));
                                    continue;
                                }
                                transcript.push_separated(Line::User(text));
                                if busy || pending.is_some() || !queue.is_empty() {
                                    transcript.push_separated(Line::Warn(
                                        "update waits for idle — finish or stop the turn first"
                                            .into(),
                                    ));
                                } else {
                                    exit = Some(Exit::Update {
                                        session: meters.session.clone(),
                                    });
                                }
                                continue;
                            }
                            // /skill:<name> [args]: explicit invocation —
                            // intercepted before slash dispatch so the
                            // literal line never runs as a prompt
                            if let Some(outcome) = skill_invocation(&text, &sidebar.inventory) {
                                match outcome {
                                    Ok(cmd) => {
                                        transcript.push_separated(Line::User(text.clone()));
                                        busy = true;
                                        let _ = commands.send(cmd).await;
                                    }
                                    Err(msg) => transcript.push_separated(Line::Warn(msg)),
                                }
                                continue;
                            }
                            if let Some(cmd) = slash_command(&text) {
                                transcript.push_separated(Line::User(text));
                                if matches!(
                                    cmd.event,
                                    Some(Command::SetMode {
                                        mode: ka_protocol::Mode::Readonly,
                                    })
                                ) {
                                    plan_started = Some(std::time::SystemTime::now());
                                }
                                if let Some(note) = cmd.note {
                                    // usage errors warn; other notes stay muted
                                    transcript.push_separated(if note.starts_with("usage:") {
                                        Line::Warn(note)
                                    } else {
                                        Line::Info(note)
                                    });
                                }
                                        if let Some(kind) = cmd.modal.clone() {
                                            match &kind {
                                                ModalKind::Mode => {
                                                    // the picker borrows the input
                                                    // box, preselecting the mode
                                                    // shown in the footer
                                                    picker = Some(InputPicker::Mode(ModePicker::for_mode(
                                                        mode_from_label(&meters.mode),
                                                    )));
                                                }
                                                ModalKind::Thinking => {
                                                    // /thinking: preselect the
                                                    // footer level; a model with
                                                    // no reasoning control just
                                                    // says so — no picker
                                                    let current = meters
                                                        .model
                                                        .trim_start_matches('+')
                                                        .split('@')
                                                        .next()
                                                        .unwrap_or("")
                                                        .to_string();
                                                    let efforts = models
                                                        .iter()
                                                        .find(|m| m.id == current)
                                                        .or_else(|| {
                                                            models
                                                                .iter()
                                                                .find(|m| current.starts_with(&m.id))
                                                        })
                                                        .map(|m| m.efforts.clone())
                                                        .unwrap_or_default();
                                                    if efforts.is_empty() {
                                                        transcript.push_separated(Line::Info(
                                                            "current model exposes no reasoning control"
                                                                .into(),
                                                        ));
                                                    } else {
                                                        picker = Some(InputPicker::Thinking(
                                                            ThinkingPicker::for_effort(
                                                                parse_effort(&meters.effort),
                                                                &efforts,
                                                            ),
                                                        ));
                                                    }
                                                }
                                                ModalKind::Key => {
                // /key: open the prompt for the current model's provider
                let current = meters
                    .model
                    .trim_start_matches('+')
                    .split('@')
                    .next()
                    .unwrap_or("")
                    .to_string();
                let vendor = current.split('/').next().unwrap_or("").to_string();
                let info = models
                    .iter()
                    .find(|m| m.id == current)
                    .or_else(|| models.iter().find(|m| current.starts_with(&m.id)));
                let prompt = info.and_then(|m| {
                    (!m.key_env.is_empty()).then(|| KeyPrompt {
                        env_var: m.key_env.clone(),
                        provider: vendor.clone(),
                        doc_url: m.doc_url.clone(),
                        input: String::new(),
                        drill: None,
                        pending_model: None,
                    })
                });
                match prompt {
                    Some(p) => modal = Some(Modal::Key(p)),
                    None => transcript.push_separated(Line::Warn("no api key variable is known for this model".into())),
                }
                                                }
                                                _ => {
                modal = Some(match kind {
                    ModalKind::Session => {
                                            let sessions = std::env::current_dir()
                                                .ok()
                                                .and_then(|cwd| ka_strand::list(&cwd).ok())
                                                .unwrap_or_default();
                                            Modal::Session(SessionPicker {
                                                sessions,
                                                selected: 0,
                                                filter: String::new(),
                                                current: (!meters.session.is_empty())
                                                    .then(|| meters.session.clone()),
                                            })
                                        }
                                        ModalKind::Help => Modal::Help,
                                        ModalKind::Model(vendor) => Modal::Model(ModelPicker {
                                            models: models.clone(),
                                            vendor: vendor.clone(),
                                            selected: 0,
                                            filter: String::new(),
                                        }),
                                        ModalKind::Provider => Modal::Provider(
                                            ProviderPicker::new(providers.clone(), &models),
                                        ),
                                        ModalKind::Settings => Modal::Settings(SettingsPanel {
                                            model: meters.model.clone(),
                                            mode: mode_from_label(&meters.mode),
                                            effort: None,
                                            selected: 0,
                                            edit: None,
                                            providers: providers.clone(),
                                            config_path: ka_config_path(),
                                        }),
                                        ModalKind::Spills => Modal::Spills {
                                            items: spills.clone(),
                                            selected: 0,
                                        },
                                        ModalKind::Prompts => Modal::Prompts {
                                            items: sidebar.inventory.prompts.clone(),
                                            selected: 0,
                                        },
                                        ModalKind::Memory => {
                                            let cwd = std::env::current_dir()
                                                .unwrap_or_else(|_| std::path::PathBuf::from("."));
                                            // safe mode: memory is not loaded into
                                            // the session — the viewer must not
                                            // pretend otherwise
                                            let rows = if ka_engine::conventions::bare_mode() {
                                                vec![
                                                    "(no memory files)".to_string(),
                                                    "safe mode: memory tiers are not loaded"
                                                        .to_string(),
                                                ]
                                            } else {
                                                memory_modal_rows(&cwd)
                                            };
                                            let inbox = if ka_engine::conventions::bare_mode() {
                                                Vec::new()
                                            } else {
                                                read_memory_inbox(&cwd)
                                            };
                                            Modal::Memory {
                                                rows,
                                                inbox,
                                                selected: 0,
                                            }
                                        }
                                        ModalKind::Usage => {
                                            let sessions = std::env::current_dir()
                                                .ok()
                                                .and_then(|cwd| ka_strand::list(&cwd).ok())
                                                .unwrap_or_default();
                                            Modal::Usage {
                                                rows: usage_rows(&meters, &sessions),
                                            }
                                        }
                                        ModalKind::Tree => {
                                            let cwd = std::env::current_dir()
                                                .unwrap_or_else(|_| std::path::PathBuf::from("."));
                                            let all = ka_strand::list(&cwd).unwrap_or_default();
                                            let current = if meters.session.is_empty() {
                                                None
                                            } else {
                                                Some(meters.session.clone())
                                            };
                                            let inner_w = 110usize.saturating_sub(4);
                                            // the session's user inputs,
                                            // oldest first, read from the
                                            // CURRENT STRAND: ForkStrand
                                            // counts strand user messages,
                                            // so fork depths must count the
                                            // same list — transcript
                                            // Line::User rows drift (steers
                                            // never persist, /plan-style
                                            // followups persist without an
                                            // echo, rewinds keep stale
                                            // records)
                                            let history: Vec<String> = current
                                                .as_deref()
                                                .map(|id| {
                                                    all.iter()
                                                        .find(|s| s.id == id)
                                                        .and_then(|s| {
                                                            ka_strand::read(&s.path).ok()
                                                        })
                                                        .unwrap_or_default()
                                                })
                                                .map(|records| {
                                                    records
                                                        .iter()
                                                        .filter_map(|r| match r {
                                                            ka_strand::Record::Message {
                                                                role:
                                                                    ka_strand::Role::User,
                                                                content,
                                                                ..
                                                            } => Some(content.clone()),
                                                            _ => None,
                                                        })
                                                        .rev()
                                                        .take(20)
                                                        .collect::<Vec<_>>()
                                                        .into_iter()
                                                        .rev()
                                                        .collect()
                                                })
                                                .unwrap_or_default();
                                            let (items, actions) = tree_modal_rows(
                                                &all,
                                                current.as_deref(),
                                                &history,
                                                inner_w,
                                                // fork-point labels need the
                                                // strands' records; missing
                                                // files read as empty and
                                                // simply drop the label
                                                |id| {
                                                    all.iter()
                                                        .find(|s| s.id == id)
                                                        .and_then(|s| ka_strand::read(&s.path).ok())
                                                        .unwrap_or_default()
                                                },
                                            );
                                            Modal::Tree {
                                                items,
                                                actions,
                                                selected: 0,
                                            }
                                        }
                                        // Mode and Thinking borrow the
                                        // input box, Key the outer arm
                                        // above: none ever becomes a
                                        // modal
                                        ModalKind::Mode | ModalKind::Key | ModalKind::Thinking => {
                                            Modal::Help
                                        }
                                        // unreachable in practice: /tasks and
                                        // /debug send events, and the modal is
                                        // built from the event payload — these
                                        // arms only satisfy exhaustiveness
                                        ModalKind::Tasks => Modal::Tasks {
                                            entries: Vec::new(),
                                            selected: 0,
                                        },
                                        ModalKind::Debug => Modal::Debug {
                                            rows: Vec::new(),
                                            scroll: None,
                                        },
                                    });
                            }
                                            }
                        }
                                // /tasks and /debug flag the roster event to
                                // open its modal instead of transcript rows
                                if modal.is_none() {
                                    match cmd.event {
                                        Some(Command::ListTasks) => {
                                            pending_modal = Some(PendingModal::Tasks)
                                        }
                                        Some(Command::DebugRoster) => {
                                            pending_modal = Some(PendingModal::Debug)
                                        }
                                        _ => {}
                                    }
                                }
                                if let Some(evt) = cmd.event {
                                    let mut is_switch = false;
                                    if let Command::SwitchStrand { id } = &evt {
                                        is_switch = true;
                                        fresh = id == "new";
                                    }
                                    let _ = commands.send(evt).await;
                                    if is_switch {
                                        busy = true;
                                    }
                                }
                                if let Some(follow) = cmd.followup {
                                    transcript.push_separated(Line::Info("(mode set; starting)".into()));
                                    busy = true;
                                    let _ = commands
                                        .send(Command::Prompt { text: follow, schema: None, images: Vec::new(), allowed_tools: None, model: None, skills: Vec::new() })
                                        .await;
                                }
                                if cmd.quit {
                                    exit = Some(Exit::Quit);
                                }
                                continue;
                            }
                            // '+'-prefix while busy queues the draft
                            // locally; the head auto-sends when the turn
                            // settles (TurnFinished arm in the event loop)
                            if busy {
                                if let Some(queued) = text.strip_prefix('+') {
                                    queue_draft(&mut queue, queued, &mut transcript);
                                    continue;
                                }
                            }
                            // `!` passthrough: run a shell command directly
                            // (no turn, no gate — the user typed it); output
                            // lands in the transcript and rides the next
                            // prompt as context
                            if let Some(shell_cmd) = text
                                .strip_prefix('!')
                                .map(str::trim)
                                .filter(|c| !c.is_empty())
                            {
                                let command = shell_cmd.to_string();
                                transcript.push_separated(Line::Info(format!("» {command}")));
                                let _ = commands.send(Command::Shell { command }).await;
                                continue;
                            }
                            transcript.push_separated(Line::User(text.clone()));
                            let cmd = if busy {
                                transcript.push_separated(Line::Info(
                                    "steering this turn".into(),
                                ));
                                Command::Steer { text }
                            } else {
                                // plain new turn: the /retry target; a
                                // staged /image attachment rides along
                                last_user = Some(text.clone());
                                if let Some(img) = &pending_image {
                                    let kb = img.data.len() * 3 / 4 / 1024;
                                    transcript.push_separated(Line::Info(format!(
                                        "[img attachment · {} · {kb}KB]",
                                        img.media_type
                                    )));
                                }
                                Command::Prompt {
                                    allowed_tools: None,
                                    model: None,
                                    text,
                                    schema: None,
                                    images: pending_image.take().into_iter().collect(),
                                    skills: Vec::new(),
                                }
                            };
                            busy = true;
                            let _ = commands.send(cmd).await;
                        }
                        (KeyCode::PageUp, _) => {
                            page_up(&mut scroll, transcript.total_rows(), view_rows);
                        }
                        (KeyCode::PageDown, _) => {
                            page_down(&mut scroll, transcript.total_rows(), view_rows);
                        }
                        (KeyCode::Tab, _) => {
                            if let Some(popup) = slash_popup.as_mut() {
                                if let Some((name, _)) = popup.items.get(popup.selected).cloned() {
                                    input.text = complete_slash(&name);
                                    input.cursor = input.text.chars().count();
                                    slash_popup = update_suggestions(&input.text);
                                }
                            } else if path_popup.is_none() {
                                // @-mention completion on an @-led token
                                // (longer than the bare sigil)
                                if let Some((start, token)) =
                                    path_token(&input.text, input.cursor)
                                        .filter(|(_, t)| mention_token(t).is_some())
                                {
                                    let query = mention_token(&token).unwrap_or_default();
                                    let matches = mention_matches(
                                        &walk_files(&std::env::current_dir().unwrap_or_default(), WALK_CAP),
                                        query,
                                    );
                                    if !matches.is_empty() {
                                        path_popup = Some(PathPopup {
                                            entries: matches,
                                            selected: 0,
                                            token_start: start,
                                            prefix: "@".to_string(),
                                            mentions: true,
                                        });
                                    }
                                } else {
                                    // file-path completion on a path-like token
                                    // (slash completion keeps priority for
                                    // `/`-led single-token input)
                                    let attempt = path_token(&input.text, input.cursor).and_then(
                                        |(start, token)| {
                                            let pathish = !token.is_empty()
                                                && (token.starts_with('.')
                                                    || token.starts_with('~')
                                                    || token.starts_with('/')
                                                    || token.contains('/'))
                                                && token != "."
                                                && token != "..";
                                            if !pathish {
                                                return None;
                                            }
                                            let (dir_part, base, _is_abs) =
                                                split_path_token(&token);
                                            // bare "~" lists $HOME, so `~` Tab
                                            // completes to `~/notes.txt`
                                            let (dir_part, base) =
                                                if dir_part.is_empty() && token == "~" {
                                                    ("~/".to_string(), String::new())
                                                } else {
                                                    (dir_part, base)
                                                };
                                            Some((start, token, dir_part, base))
                                        },
                                    );
                                    if let Some((start, token, dir_part, base)) = attempt {
                                        let matches = list_matches(&dir_part, &base);
                                        if matches.len() == 1 && !matches[0].1 {
                                            // one file: complete inline, silently
                                            let insert = format!("{dir_part}{}", matches[0].0);
                                            let (text, cursor) = complete_token(
                                                &input.text,
                                                start,
                                                token.chars().count(),
                                                &insert,
                                            );
                                            input.text = text;
                                            input.cursor = cursor;
                                        } else if !matches.is_empty() {
                                            // a directory (or many): offer the popup
                                            path_popup = Some(PathPopup {
                                                entries: matches,
                                                selected: 0,
                                                token_start: start,
                                                prefix: dir_part,
                                                mentions: false,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                        (KeyCode::Up, _) if slash_popup.is_some() => {
                            if let Some(popup) = slash_popup.as_mut() {
                                popup.selected = popup.selected.saturating_sub(1);
                            }
                        }
                        (KeyCode::Down, _) if slash_popup.is_some() => {
                            if let Some(popup) = slash_popup.as_mut() {
                                if popup.selected + 1 < popup.items.len() {
                                    popup.selected += 1;
                                }
                            }
                        }
                        (KeyCode::Up, _)
                            if !busy && slash_popup.is_none() && input.text.contains('\n') =>
                        {
                            input.move_up();
                        }
                        // Ctrl+↑/↓: jump between user messages — the
                        // keyboard twin of the ▲▼ arrows, works in both
                        // mouse modes
                        (KeyCode::Up, KeyModifiers::CONTROL) => {
                            let rows = transcript.user_entry_rows();
                            let total = transcript.total_rows();
                            jump_to_user_message(&mut scroll, &rows, total, view_rows, true);
                        }
                        (KeyCode::Down, KeyModifiers::CONTROL) => {
                            let rows = transcript.user_entry_rows();
                            let total = transcript.total_rows();
                            jump_to_user_message(&mut scroll, &rows, total, view_rows, false);
                        }
                        // arrows are mode-dependent: captured = prompt
                        // history; native (default) = scroll the chat
                        // (the wheel arrives as ↑/↓ there via
                        // alternate-scroll)
                        (KeyCode::Up, _) if !busy && !mouse_captured => {
                            line_up(
                                &mut scroll,
                                transcript.total_rows(),
                                view_rows,
                            );
                        }
                        (KeyCode::Down, _) if !busy && !mouse_captured => {
                            line_down(
                                &mut scroll,
                                transcript.total_rows(),
                                view_rows,
                            );
                        }
                        (KeyCode::Up, _) if !busy => input.history_prev(),
                        (KeyCode::Down, _)
                            if !busy && slash_popup.is_none() && input.text.contains('\n') =>
                        {
                            input.move_down();
                        }
                        (KeyCode::Down, _) if !busy => input.history_next(),
                        // Ctrl+P / Ctrl+N: prompt history in every mode
                        // (readline muscle memory; the only history keys
                        // in native mode)
                        (KeyCode::Char('p'), KeyModifiers::CONTROL)
                            if !busy && slash_popup.is_none() && path_popup.is_none() =>
                        {
                            input.history_prev();
                        }
                        (KeyCode::Char('n'), KeyModifiers::CONTROL)
                            if !busy && slash_popup.is_none() && path_popup.is_none() =>
                        {
                            input.history_next();
                        }
                        // Line-editing keys. Alt combos rely on the kitty
                        // keyboard protocol; terminals that ignore it deliver
                        // Esc-then-key, which at idle degrades to a harmless
                        // no-op or a typed char — busy-turn Esc abort is
                        // unchanged.
                        (KeyCode::Char('a'), KeyModifiers::CONTROL) => input.line_home(),
                        (KeyCode::Char('e'), KeyModifiers::CONTROL) => input.line_end(),
                        (KeyCode::Char('b'), KeyModifiers::CONTROL) => input.left(),
                        (KeyCode::Char('f'), KeyModifiers::CONTROL) => input.right(),
                        (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                            input.delete_forward();
                            slash_popup = update_suggestions(&input.text);
                        }
                        (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                            let del = input.delete_to_line_start();
                            input.kill_push(del);
                            slash_popup = update_suggestions(&input.text);
                        }
                        (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
                            let del = input.delete_to_line_end();
                            input.kill_push(del);
                            slash_popup = update_suggestions(&input.text);
                        }
                        (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                            let del = input.delete_word_backward();
                            input.kill_push(del);
                            slash_popup = update_suggestions(&input.text);
                        }
                        (KeyCode::Backspace, m)
                            if m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            let del = input.delete_word_backward();
                            input.kill_push(del);
                            slash_popup = update_suggestions(&input.text);
                        }
                        (KeyCode::Char('y'), KeyModifiers::CONTROL) => {
                            input.yank();
                            slash_popup = update_suggestions(&input.text);
                        }
                        (KeyCode::Char('q'), KeyModifiers::CONTROL) => {
                            // recall: pop the LAST queued item back into
                            // the draft to edit and re-queue
                            if let Some(item) = queue.pop() {
                                input.text = item;
                                input.cursor = input.text.chars().count();
                                slash_popup = update_suggestions(&input.text);
                            }
                        }
                        (KeyCode::Char('q'), KeyModifiers::ALT) => {
                            // alt+q: queue the whole draft for after this
                            // turn — the `+` prefix without the retyping
                            // (ctrl+q pops it back). Slash drafts queue
                            // when they can run unattended; queue_refusal
                            // explains the ones that can't
                            let draft = input.text.trim().to_string();
                            if draft.is_empty() {
                                // blank draft: nothing to queue
                            } else if !busy {
                                pop_toast(&mut toast, "nothing running — enter sends now");
                            } else if let Some(reason) = queue_refusal(&draft) {
                                pop_toast(&mut toast, &reason);
                            } else {
                                input.take();
                                queue_draft(&mut queue, &draft, &mut transcript);
                            }
                        }
                        // some terminals deliver ctrl+_ / ctrl+- as chars
                        (KeyCode::Char('z'), KeyModifiers::CONTROL)
                        | (KeyCode::Char('_'), KeyModifiers::CONTROL)
                        | (KeyCode::Char('-'), KeyModifiers::CONTROL) => {
                            input.undo();
                            slash_popup = update_suggestions(&input.text);
                        }
                        (KeyCode::Left, m)
                            if m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            input.word_left();
                        }
                        (KeyCode::Right, m)
                            if m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            input.word_right();
                        }
                        (KeyCode::Char('b'), KeyModifiers::ALT) => input.word_left(),
                        (KeyCode::Char('f'), KeyModifiers::ALT) => input.word_right(),
                        (KeyCode::Char('d'), KeyModifiers::ALT) => {
                            let del = input.delete_word_forward();
                            input.kill_push(del);
                            slash_popup = update_suggestions(&input.text);
                        }
                        (KeyCode::Delete, KeyModifiers::ALT) => {
                            let del = input.delete_word_forward();
                            input.kill_push(del);
                            slash_popup = update_suggestions(&input.text);
                        }
                        (KeyCode::Char('e'), KeyModifiers::ALT) if !busy => {
                            // draft → temp file → $EDITOR → read back
                            let stamp = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis())
                                .unwrap_or(0);
                            let path = std::env::temp_dir().join(format!(
                                "ka-edit-{}-{stamp}.txt",
                                std::process::id()
                            ));
                            let path_str = path.to_string_lossy().into_owned();
                            match std::fs::write(&path, &input.text) {
                                Err(e) => {
                                    transcript.push_separated(Line::Info(format!("edit failed: {e}")));
                                }
                                Ok(()) => {
                                    let editor = std::env::var("EDITOR")
                                        .ok()
                                        .filter(|e| !e.is_empty())
                                        .unwrap_or_else(|| "vi".to_string());
                                    match run_external(
                                        &editor,
                                        &[path_str.as_str()],
                                        terminal,
                                        capture_now,
                                    ) {
                                        Err(e) => transcript.push_separated(Line::Info(format!("editor failed: {e}"))),
                                        Ok(()) => {
                                            let read = std::fs::read_to_string(&path);
                                            let _ = std::fs::remove_file(&path);
                                            match read {
                                                Ok(edited) => {
                                                    input.text.clear();
                                                    input.cursor = 0;
                                                    input.insert_str(&edited);
                                                    slash_popup =
                                                        update_suggestions(&input.text);
                                                }
                                                Err(e) => transcript.push_separated(Line::Info(format!(
                                                    "edit failed: {e}"
                                                ))),
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        (KeyCode::Left, _) => input.left(),
                        (KeyCode::Right, _) => input.right(),
                        (KeyCode::Home, _) => input.home(),
                        (KeyCode::End, _) => input.end(),
                        (KeyCode::Backspace, _) => {
                            input.backspace();
                            slash_popup = update_suggestions(&input.text);
                        }
                        (KeyCode::Delete, KeyModifiers::NONE) => {
                            input.delete_forward();
                            slash_popup = update_suggestions(&input.text);
                        }
                        (KeyCode::Char(c), _) => {
                            input.insert(c);
                            slash_popup = update_suggestions(&input.text);
                        }
                        _ => {}
                    }
                } else if let Some(Ok(TermEvent::Paste(text))) = maybe_term {
                    // paste routes through the same overlay precedence as
                    // keystrokes; single-line fields strip whitespace (API
                    // keys and filters never contain it, clipboards often do)
                    if pending.is_some() {
                        // option dialogs have nothing to paste into
                    } else if let Some(open) = modal.as_mut() {
                        match open {
                            Modal::Key(prompt) => {
                                prompt.input.extend(text.chars().filter(|c| !c.is_whitespace()));
                            }
                            Modal::Prompts { .. } => {}
                            Modal::Memory { .. } => {}
                            Modal::Rewind { .. } => {}
                            Modal::Tree { .. } => {}
                            Modal::Features { .. } => {}
                            Modal::Session(picker) => {
                                picker.filter.extend(text.chars().filter(|c| !c.is_whitespace()));
                            }
                            Modal::Model(picker) => {
                                picker.filter.extend(text.chars().filter(|c| !c.is_whitespace()));
                                picker.selected = 0;
                            }
                            Modal::Provider(picker) => {
                                picker.filter.extend(text.chars().filter(|c| !c.is_whitespace()));
                                picker.selected = 0;
                            }
                            Modal::Settings(panel) => {
                                if let Some(edit) = panel.edit.as_mut() {
                                    edit.extend(text.chars().filter(|c| !c.is_whitespace()));
                                }
                            }
                            Modal::Help
                            | Modal::Spills { .. }
                            | Modal::Usage { .. }
                            | Modal::Context { .. }
                            | Modal::Tasks { .. }
                            | Modal::TaskDetail { .. }
                            | Modal::Debug { .. }
                            | Modal::Todos { .. }
                            | Modal::Skills { .. }
                            | Modal::Info { .. } => {}
                        }
                    } else if path_popup.is_some() {
                        path_popup = None;
                        input.insert_str(&text);
                        slash_popup = update_suggestions(&input.text);
                    } else if !input.searching() && text.split_whitespace().count() == 1 {
                        // bracketed paste carrying exactly one existing
                        // image path: attach it instead of inserting text
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            let as_path = std::path::PathBuf::from(trimmed);
                            if as_path.is_file() && sniff_image_type(&as_path).is_some() {
                                match image_part_from_path(&as_path) {
                                    Ok(part) => {
                                        let kb = part.data.len() * 3 / 4 / 1024;
                                        let name = as_path
                                            .file_name()
                                            .map(|n| n.to_string_lossy().into_owned())
                                            .unwrap_or_default();
                                        pending_image = Some(part);
                                        pop_toast(
                                            &mut toast,
                                            format!("[img {name} · {kb}KB] — press enter to send"),
                                        );
                                        slash_popup = None;
                                        continue;
                                    }
                                    Err(e) => {
                                        transcript.push_separated(Line::Info(e));
                                        slash_popup = None;
                                        continue;
                                    }
                                }
                            }
                        }
                        input.insert_str(&text);
                        slash_popup = update_suggestions(&input.text);
                    } else if input.searching() {
                        for c in text.chars().filter(|c| !c.is_whitespace()) {
                            input.search_push(c);
                        }
                    } else {
                        input.insert_str(&text);
                        slash_popup = update_suggestions(&input.text);
                    }
                } else if let Some(Ok(TermEvent::Mouse(mouse_evt))) = maybe_term {
                    // captured-mouse interactions: the wheel scrolls the
                    // chat at line granularity (page keys keep their
                    // page step); overlays keep focus. In native mode no
                    // mouse events arrive at all — drag/paste are the
                    // terminal's own.
                    //
                    // an open overlay or a native-mode interlude owns the
                    // pointer: any live rail drag ends here
                    if !capture_now
                        || modal.is_some()
                        || pending.is_some()
                        || slash_popup.is_some()
                        || path_popup.is_some()
                        || picker.is_some()
                    {
                        rail_drag = None;
                    }
                    //
                    // Left Down over an open overlay closes it instead of
                    // touching the transcript: a modal answers to the ✕
                    // chip or any click outside it (body clicks are dead —
                    // closing discards filters/edits exactly like Esc,
                    // nothing is applied), and a click outside the
                    // slash/path popup or the mode picker closes that
                    // popup (the picker never submits this way). The
                    // permission ask is deliberately unreachable here: an
                    // accidental click must never answer it.
                    let left_down = matches!(
                        mouse_evt.kind,
                        crossterm::event::MouseEventKind::Down(
                            crossterm::event::MouseButton::Left
                        )
                    );
                    // pointer motion (any-motion tracking, capture only):
                    // refresh the hover target; an unchanged target only
                    // skips the target bookkeeping — the loop still draws
                    if let crossterm::event::MouseEventKind::Moved = mouse_evt.kind {
                        if !capture_now {
                            continue;
                        }
                        let chip_popup = slash_popup.is_some() || path_popup.is_some();
                        let next = hover_target(
                            &modal_zone,
                            &popup_zone,
                            chip_popup,
                            &strip_zone,
                            &title_arrows,
                            &rail_zone,
                            &tx_content,
                            &transcript,
                            mouse_evt.column,
                            mouse_evt.row,
                        );
                        if next == hover {
                            continue;
                        }
                        hover = next;
                    } else if left_down && capture_now && pending.is_none() && modal.is_some() {
                        if let Some(rect) = modal_zone.get()
                            && modal_close_hit(rect, mouse_evt.column, mouse_evt.row)
                        {
                            modal = None;
                        }
                    } else if left_down
                        && capture_now
                        && pending.is_none()
                        && (slash_popup.is_some() || path_popup.is_some() || picker.is_some())
                    {
                        let closes = if let Some(rect) = popup_zone.get() {
                            if slash_popup.is_some() || path_popup.is_some() {
                                // these popups render the ✕ chip (shared
                                // modal_frame): chip or outside closes,
                                // exactly like a modal
                                modal_close_hit(rect, mouse_evt.column, mouse_evt.row)
                            } else {
                                // the mode picker is the input box itself
                                // — no chip drawn, so only an outside
                                // click closes, and it never submits
                                !rect.contains(ratatui::layout::Position {
                                    x: mouse_evt.column,
                                    y: mouse_evt.row,
                                })
                            }
                        } else {
                            false
                        };
                        if closes {
                            // the popups' Esc arm: discard, never submit
                            slash_popup = None;
                            path_popup = None;
                            picker = None;
                        }
                    } else if modal.is_none()
                        && pending.is_none()
                        && slash_popup.is_none()
                        && path_popup.is_none()
                        && capture_now
                    {
                        // ── scrollbar rail: its column is consumed
                        // before the generic click/drag-select arms —
                        // thumb drags scroll, track clicks page ──
                        let rail = rail_zone.get();
                        let rail_hit = rail.is_some_and(|(rect, ..)| {
                            rect.contains(ratatui::layout::Position {
                                x: mouse_evt.column,
                                y: mouse_evt.row,
                            })
                        });
                        match mouse_evt.kind {
                            crossterm::event::MouseEventKind::Down(
                                crossterm::event::MouseButton::Left,
                            ) if rail_hit => {
                                press = None; // never anchor a drag-select here
                                sel = None; // a release must not re-copy a stale span
                                if let Some((rect, total, visible, thumb_pos, thumb_len)) = rail {
                                    let row = (mouse_evt.row.saturating_sub(rect.y)) as usize;
                                    if (thumb_pos..thumb_pos + thumb_len).contains(&row) {
                                        rail_drag = Some(row - thumb_pos);
                                    } else {
                                        // track click: one page toward it
                                        if row < thumb_pos {
                                            page_up(&mut scroll, total, visible);
                                        } else {
                                            page_down(&mut scroll, total, visible);
                                        }
                                    }
                                }
                            }
                            crossterm::event::MouseEventKind::Drag(
                                crossterm::event::MouseButton::Left,
                            ) if rail_drag.is_some() => {
                                if let Some((rect, total, visible, _, thumb_len)) = rail {
                                    let grab = rail_drag.unwrap_or(0);
                                    let row = (mouse_evt.row.saturating_sub(rect.y)) as usize;
                                    let center = row.saturating_sub(grab);
                                    scroll = rail_drag_anchor(
                                        center,
                                        thumb_len,
                                        rect.height as usize,
                                        total,
                                        visible,
                                    );
                                }
                            }
                            crossterm::event::MouseEventKind::Up(
                                crossterm::event::MouseButton::Left,
                            ) if rail_drag.is_some() => {
                                rail_drag = None;
                            }
                            // a left press only anchors a pending
                            // click: the action waits for release, so
                            // the same gesture can resolve as a
                            // drag-select instead without toggling
                            // whatever sits under the anchor
                            crossterm::event::MouseEventKind::Down(
                                crossterm::event::MouseButton::Left,
                            ) if !rail_hit => {
                                sel = None;
                                press = Some((mouse_evt.column, mouse_evt.row));
                            }
                            // right click pastes the clipboard into the
                            // input
                            crossterm::event::MouseEventKind::Down(
                                crossterm::event::MouseButton::Right,
                            ) => {
                                let text = clipboard_text().await;
                                input.insert_str(&text);
                            }
                            // left drag: past the click tolerance the
                            // press becomes a row selection anchored at
                            // the press row; the head follows the
                            // pointer (clamped to the pane edges)
                            crossterm::event::MouseEventKind::Drag(
                                crossterm::event::MouseButton::Left,
                            ) => {
                                if let Some(p) = press
                                    && let Some((area, start)) = tx_content.get()
                                    && let Some(span) = drag_span(
                                        area,
                                        start,
                                        p,
                                        (mouse_evt.column, mouse_evt.row),
                                    )
                                {
                                    sel = Some(span);
                                }
                            }
                            // left release: a picked span copies to the
                            // clipboard (OSC 52) and stays highlighted
                            // until the next press; a press that never
                            // left its cell was a click and dispatches
                            // here instead
                            crossterm::event::MouseEventKind::Up(
                                crossterm::event::MouseButton::Left,
                            ) => {
                                if let Some(span) = sel {
                                    let picked = selection_text(&transcript, span);
                                    if !picked.is_empty() {
                                        let mut stdout = std::io::stdout().lock();
                                        let _ = stdout.write_all(b"\x1b]52;c;");
                                        let _ =
                                            stdout.write_all(b64encode(&picked).as_bytes());
                                        let _ = stdout.write_all(b"\x07");
                                        let _ = stdout.flush();
                                        let n = span.1.0 - span.0.0 + 1;
                                        pop_toast(
                                            &mut toast,
                                            format!(
                                                "✓ copied {n} row{} to the clipboard",
                                                if n == 1 { "" } else { "s" }
                                            ),
                                        );
                                    }
                                } else if let Some((col, row)) = press.take() {
                                    match click_target(
                                        &strip_zone,
                                        &title_arrows,
                                        &tx_content,
                                        &transcript,
                                        col,
                                        row,
                                    ) {
                                        Click::Tool { entry, call } => {
                                            transcript.toggle_tool_call(entry, call);
                                        }
                                        Click::Thought { entry } => {
                                            transcript.toggle_thought(entry);
                                        }
                                        Click::Summary { entry } => {
                                            transcript.toggle_summary(entry);
                                        }
                                        Click::JumpUp => {
                                            let rows = transcript.user_entry_rows();
                                            jump_to_user_message(
                                                &mut scroll,
                                                &rows,
                                                transcript.total_rows(),
                                                view_rows,
                                                true,
                                            );
                                        }
                                        Click::JumpDown => {
                                            let rows = transcript.user_entry_rows();
                                            jump_to_user_message(
                                                &mut scroll,
                                                &rows,
                                                transcript.total_rows(),
                                                view_rows,
                                                false,
                                            );
                                        }
                                        Click::Todos => {
                                            modal = Some(Modal::Todos {
                                                rows: todos_rows(&sidebar, 64),
                                            });
                                        }
                                        Click::Skills => {
                                            modal = Some(Modal::Skills {
                                                rows: inventory_rows(&sidebar, 64),
                                            });
                                        }
                                        Click::Info => {
                                            modal = Some(Modal::Info {
                                                rows: info_rows(&sidebar, &meters, 64),
                                            });
                                        }
                                        Click::None => {}
                                    }
                                }
                            }
                            crossterm::event::MouseEventKind::ScrollUp => {
                                line_up(&mut scroll, transcript.total_rows(), view_rows);
                            }
                            crossterm::event::MouseEventKind::ScrollDown => {
                                line_down(&mut scroll, transcript.total_rows(), view_rows);
                            }
                            _ => {}
                        }
                    }
                }
            }
            maybe_evt = events.recv() => {
                match maybe_evt {
                    None => { exit = Some(Exit::EngineEnded); }
                    Some(evt) => {
                        let replayed = matches!(evt, Event::Replay { .. });
                        if let Event::Replay { messages } = &evt {
                            // ↑/↓ recall the resumed session's earlier
                            // prompts: seed the draft history with them
                            input.seed_history(replay_user_prompts(messages));
                        }
                        // /tasks and /debug modals: a flagged roster event
                        // reroutes into the modal (skipping the transcript
                        // fallback below); TaskDetail always pages
                        if pending_modal.is_some() {
                            match &evt {
                            // mode/model acks are transient, not transcript rows
                            Event::ModeChanged { mode } => {
                                pop_toast(&mut toast, format!("mode · {}", mode_label(*mode)));
                            }
                            Event::ModelChanged { selector } => {
                                pop_toast(&mut toast, format!("model · {selector}"));
                            }
                                Event::Tasks { rows }
                                    if pending_modal == Some(PendingModal::Tasks) =>
                                {
                                    pending_modal = None;
                                    modal = Some(Modal::Tasks {
                                        entries: rows
                                            .iter()
                                            .map(|r| (task_id_of_row(r), r.clone()))
                                            .collect(),
                                        selected: 0,
                                    });
                                    continue;
                                }
                                Event::DebugRoster { rows }
                                    if pending_modal == Some(PendingModal::Debug) =>
                                {
                                    pending_modal = None;
                                    modal = Some(Modal::Debug {
                                        rows: rows.clone(),
                                        scroll: None,
                                    });
                                    continue;
                                }
                                _ => {}
                            }
                        }
                        match &evt {
                            Event::TaskDetail { id, text } => {
                                modal = Some(Modal::TaskDetail {
                                    id: *id,
                                    text: text.clone(),
                                    scroll: None,
                                });
                            }
                            Event::DebugRoster { rows } => {
                                // no pending modal: transcript fallback
                                transcript
                                    .push_separated(Line::Info("▸ debug sessions".into()));
                                for row in rows {
                                    transcript
                                        .push_separated(Line::Report(row.clone()));
                                }
                            }
                            _ => {}
                        }
                        if apply_event(
                            &evt,
                            &mut transcript,
                            &mut busy,
                            &mut busy_since,
                            &mut meters,
                            &mut pending,
                            &mut turn_produced,
                            &mut turn_usage,
                            &mut current_assistant,
                            &mut current_thought,
                            &mut current_tool,
                            &mut live_tool,
                            &mut last_user,
                            &mut last_error,
                            &mut spills,
                            &mut sidebar,
                        )
                        .is_some()
                        {
                            // Ctrl+O and click-to-expand target this call
                            last_tool_ref = transcript.last_tool_ref();
                        }
                        // the effort segment renders only for models
                        // that expose reasoning control; the model may
                        // have changed with this very event
                        let current = meters.model.clone();
                        meters.effort_supported = models
                            .iter()
                            .find(|m| m.id == current)
                            .or_else(|| models.iter().find(|m| current.starts_with(&m.id)))
                            .is_some_and(|m| !m.efforts.is_empty());
                        // a live title lands while the session picker is
                        // open: patch its row too (the picker otherwise
                        // reloads from the strand on every open, and the
                        // Title record is already persisted by then)
                        if let Event::Title { title } = &evt {
                            if !title.is_empty() {
                                if let Some(Modal::Session(picker)) = modal.as_mut() {
                                    if let Some(s) = picker
                                        .sessions
                                        .iter_mut()
                                        .find(|s| s.id == meters.session)
                                    {
                                        s.title = title.clone();
                                    }
                                }
                            }
                        }
                        if matches!(evt, Event::TurnFinished { .. }) {
                            turn_ended = true;
                            // one queued item per settle — and the queue
                            // drains in order: slash items run through
                            // the dispatcher (non-turn commands apply
                            // instantly and draining continues; a queued
                            // `/mode plan` arms the plan watcher like the
                            // submit arm), the first plain-text item
                            // becomes the next turn. Non-turn Idles
                            // (SetMode, …) are separate events and never
                            // reach this arm; an unanswered ask holds the
                            // queue until answered.
                            if pending.is_none() {
                                while let Some(next) = pop_queue_head(&mut queue) {
                                    if let Some(cmd) = slash_command(&next) {
                                        transcript.push_separated(Line::User(next));
                                        if let Some(note) = cmd.note {
                                            transcript.push_separated(
                                                if note.starts_with("usage:") {
                                                    Line::Warn(note)
                                                } else {
                                                    Line::Info(note)
                                                },
                                            );
                                        }
                                        if let Some(event) = cmd.event {
                                            let starts_turn =
                                                matches!(event, Command::Prompt { .. });
                                            if let Command::Prompt { text, .. } = &event {
                                                last_user = Some(text.clone());
                                            }
                                            if matches!(
                                                event,
                                                Command::SetMode {
                                                    mode: ka_protocol::Mode::Readonly
                                                }
                                            ) {
                                                plan_started =
                                                    Some(std::time::SystemTime::now());
                                            }
                                            if starts_turn {
                                                busy = true;
                                            }
                                            let _ = commands.send(event).await;
                                            if starts_turn {
                                                break;
                                            }
                                        }
                                        // note-only or instant command: keep
                                        // draining — nothing is running
                                    } else if next.starts_with('/') {
                                        // enqueue-time validation accepted this as a
                                        // command, but it no longer parses (custom
                                        // command deleted, trust revoked, bare mode) —
                                        // never replay it as a literal prompt
                                        transcript.push_separated(Line::Warn(format!(
                                            "{next}: no longer a command — skipped"
                                        )));
                                        continue;
                                    } else {
                                        transcript.push_separated(Line::User(next.clone()));
                                        last_user = Some(next.clone());
                                        busy = true;
                                        let _ = commands.send(Command::Prompt { text: next, schema: None, images: Vec::new(), allowed_tools: None, model: None, skills: Vec::new() }).await;
                                        break;
                                    }
                                }
                            }
                        }
                        if matches!(evt, Event::TurnFinished { .. })
                            && meters.mode == mode_label(ka_protocol::Mode::Readonly)
                            && plan_drafted(plan_started, &plan_file_path())
                        {
                            plan_started = None;
                            transcript.push_separated(Line::Info(format!(
                                "Plan drafted — review {}, then /approve to build",
                                plan_file_path().display()
                            )));
                        }
                        if let Event::ContextBreakdown { parts, window } = &evt {
                            modal = Some(Modal::Context {
                                rows: context_rows(parts, *window),
                            });
                        }
                        if replayed {
                            scroll = None;
                        }
                    }
                }
            }
            maybe_update = updates.recv() => {
                if let Some(notice) = maybe_update {
                    // one quiet transcript line; the status-bar badge
                    // persists for the session
                    meters.update = Some(notice.latest.clone());
                    transcript.push_separated(Line::Info(format!(
                        "update available: {} — run `ka update`",
                        notice.latest
                    )));
                }
            }
            // redraws while busy (spinner/clock) or while a toast is
            // alive; the arm also prunes the expired toast so ticking
            // actually stops once it fades
            _ = spin.tick(), if busy || toast.is_some() => {
                if toast.as_ref().is_some_and(|(_, at)| at.elapsed() >= TOAST_TTL) {
                    toast = None;
                }
            }
        }
    }
    let exit = exit.unwrap_or(Exit::Quit);
    let resume = resume_hint(
        &meters.session,
        meters.turns,
        busy,
        !transcript.entries().is_empty(),
    );
    Ok((exit, resume))
}

/// The after-exit resume hint: the command that brings this session
/// back, printed as plain text on the way out — the terminal's own
/// history and state stay untouched. Worth printing whenever the
/// session exists and something happened in it — a finished turn, a
/// turn still in flight when the user quit, or history replayed from
/// an earlier run: closing an older chat without a new turn still
/// deserves the way back.
fn resume_hint(session: &str, turns: u64, busy: bool, has_history: bool) -> Option<String> {
    if session.is_empty() || (turns == 0 && !busy && !has_history) {
        return None;
    }
    let tag = short_session(session).unwrap_or("?");
    Some(format!(
        "⟡ session saved — resume with: ka -c   (this session: ka --session {tag})"
    ))
}

/// Live in-flight tool call: rolling output preview (live region only)
/// plus the last emission, which becomes the compact finish row's note.
#[derive(Debug, Clone)]
struct LiveTool {
    id: String,
    /// Most recent output lines, oldest first; capped in render.
    preview: Vec<String>,
    /// Last CallOutput excerpt and its error flag.
    last: Option<(String, bool)>,
    /// Local start time: CallStarted→CallFinished becomes the row's
    /// duration.
    started: Instant,
}

/// Tool call header: `→ {tool}`, or `→ {tool} · {detail}` when the
/// engine supplied an argument summary (the CallStarted detail field).
fn tool_header(tool: &str, detail: &str) -> String {
    if detail.is_empty() {
        format!("→ {tool}")
    } else {
        format!("→ {tool} · {detail}")
    }
}

/// How long a toast stays on screen before it self-destructs.
const TOAST_TTL: Duration = Duration::from_secs(4);

/// Set (or replace) the transient action toast.
fn pop_toast(toast: &mut Option<(String, Instant)>, msg: impl Into<String>) {
    *toast = Some((msg.into(), Instant::now()));
}

/// A finished call's note: the first non-blank output line, capped at
/// 40 columns (empty when the call produced nothing readable).
fn tool_note(excerpt: &str) -> String {
    let first = excerpt.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    trunc_cols(first, 40)
}

#[allow(clippy::too_many_arguments)]
fn apply_event(
    evt: &Event,
    transcript: &mut Transcript,
    busy: &mut bool,
    busy_since: &mut Option<Instant>,
    meters: &mut Meters,
    pending: &mut Option<PendingAsk>,
    turn_produced: &mut bool,
    turn_usage: &mut Option<(u64, u64, u64)>,
    current_assistant: &mut String,
    current_thought: &mut String,
    current_tool: &mut String,
    live_tool: &mut Option<LiveTool>,
    last_user: &mut Option<String>,
    last_error: &mut Option<String>,
    // spill-file paths seen this session (for /spills); deduped, capped
    spills: &mut Vec<String>,
    // sidebar state: inventory + live todo list
    sidebar: &mut SidebarState,
) -> Option<ToolCall> {
    // the most recently finished call, surfaced for Ctrl+O / expand
    let mut finished: Option<ToolCall> = None;
    match evt {
        Event::TurnStarted { .. } => {
            *busy = true;
            *busy_since = Some(Instant::now());
            // the turn boundary masks the collapse of any streamed-open
            // thinking blocks still expanded from the previous turn
            transcript.collapse_streamed_all();
            *current_assistant = String::new();
            *current_thought = String::new();
            *current_tool = String::new();
            *live_tool = None;
            *turn_produced = false;
            *turn_usage = None;
            // a new turn invalidates a stale in-turn error buffer; the
            // retry target survives so /retry can resend the prompt
            // that produced the failure
            *last_error = None;
        }
        Event::Delta { kind } => match kind {
            ka_protocol::DeltaKind::Text(t) => {
                *turn_produced = true;
                current_assistant.push_str(t);
            }
            ka_protocol::DeltaKind::Thought(t) => {
                *turn_produced = true;
                current_thought.push_str(t);
            }
            ka_protocol::DeltaKind::Call { tool, id } => {
                *turn_produced = true;
                // chronology: text streamed before the call must render
                // above it, exactly like the turn-end flush
                flush_live_text(transcript, current_thought, current_assistant);
                *current_tool = tool_header(tool, "");
                *live_tool = Some(LiveTool {
                    id: id.clone(),
                    preview: Vec::new(),
                    last: None,
                    started: Instant::now(),
                });
            }
        },
        Event::CallStarted { tool, id, detail } => {
            *turn_produced = true;
            flush_live_text(transcript, current_thought, current_assistant);
            match live_tool.as_ref() {
                // the streaming header for this exact call is still the
                // live row: upgrade it in place with the argument detail
                Some(lt) if lt.id == *id && !current_tool.is_empty() => {
                    *current_tool = tool_header(tool, detail);
                }
                // cold start (no stream header) or a different call:
                // flush a never-finished header as a bare row, then
                // open the fresh live block
                _ => {
                    if !current_tool.is_empty() {
                        transcript.push_tool_call(ToolCall {
                            head: std::mem::take(current_tool),
                            ok: false,
                            note: String::new(),
                            excerpt: String::new(),
                            spill: None,
                            dur: None,
                            expanded: false,
                        });
                    }
                    *current_tool = tool_header(tool, detail);
                    *live_tool = Some(LiveTool {
                        id: id.clone(),
                        preview: Vec::new(),
                        last: None,
                        started: Instant::now(),
                    });
                }
            }
        }
        Event::CallOutput {
            id,
            excerpt,
            is_error,
            spill,
            ..
        } => {
            *turn_produced = true;
            if let Some(path) = spill {
                record_spill(spills, path);
            }
            match live_tool.as_mut() {
                // live block: refresh the rolling preview; the compact
                // note is built from `last` when the call finishes
                Some(lt) if lt.id == *id => {
                    observe_preview(&mut lt.preview, excerpt);
                    lt.last = Some((excerpt.clone(), *is_error));
                }
                // no matching block (replay paths, id drift): the note
                // is lost — the row still renders from its head
                _ => {}
            }
        }
        Event::CallFinished { ok, .. } => {
            // collapse the live block: the preview was transient; the
            // compact ` │ → tool … ✓` row is what gets cached
            if let Some(lt) = live_tool.take() {
                let (note, excerpt) = match lt.last {
                    Some((excerpt, _)) => (tool_note(&excerpt), excerpt),
                    None => (String::new(), String::new()),
                };
                let call = ToolCall {
                    head: std::mem::take(current_tool),
                    ok: *ok,
                    note,
                    excerpt,
                    spill: None,
                    dur: Some(lt.started.elapsed().as_secs_f64()),
                    expanded: false,
                };
                transcript.push_tool_call(call.clone());
                finished = Some(call);
            }
        }
        Event::Ask { id, questions } => {
            if let Some(q) = questions.first() {
                *pending = Some(PendingAsk {
                    id: id.clone(),
                    question: q.text.clone(),
                    options: q.options.clone(),
                    detail: q.detail.clone(),
                    selected: 0,
                });
            }
            fire_notifications("permission_ask", "ask");
        }
        Event::ShellOutput {
            command,
            output,
            note,
        } => {
            // `!` passthrough result: plain rows under the » command row
            let tail = note
                .as_deref()
                .map(|n| format!(" ({n})"))
                .unwrap_or_default();
            let body = if output.is_empty() {
                format!("(no output){tail}")
            } else {
                format!("{output}{tail}")
            };
            for line in body.lines().take(40) {
                transcript.push_separated(Line::Shell(line.to_string()));
            }
            let hidden = body.lines().count().saturating_sub(40);
            if hidden > 0 {
                transcript.push_separated(Line::Info(format!("… +{hidden} more")));
            }
            transcript.push_separated(Line::Info(format!(
                "» {command} — output joins your next prompt"
            )));
        }
        Event::Tasks { rows } => {
            // /tasks dashboard snapshot rendered as report rows
            transcript.push_separated(Line::Info("▸ background tasks".into()));
            for row in rows {
                transcript.push_separated(Line::Report(row.clone()));
            }
        }
        Event::TurnFinished { stop, usage } => {
            let elapsed = busy_since.map_or(0.0, |t| t.elapsed().as_secs_f64());
            flush_live_text(transcript, current_thought, current_assistant);
            // a call still running at turn end closes as a row too
            let mut finished_call = None;
            if let Some(lt) = live_tool.take() {
                let (note, excerpt) = match lt.last {
                    Some((excerpt, _)) => (tool_note(&excerpt), excerpt),
                    None => (String::new(), String::new()),
                };
                let call = ToolCall {
                    head: std::mem::take(current_tool),
                    ok: true,
                    note,
                    excerpt,
                    spill: None,
                    dur: Some(lt.started.elapsed().as_secs_f64()),
                    expanded: false,
                };
                transcript.push_tool_call(call.clone());
                finished_call = Some(call);
            } else {
                current_tool.clear();
            }
            *busy = false;
            *busy_since = None;
            meters.cost += usage.cost;
            let in_seen = usage.input + usage.cache_read + usage.cache_write;
            let cache_hit = if in_seen > 0 {
                Some(usage.cache_read as f32 / in_seen as f32)
            } else {
                None
            };
            meters.cache_hit = cache_hit;
            *turn_usage = Some((in_seen, usage.input, usage.output));
            meters.turns += 1;
            meters.tokens_in += in_seen;
            meters.tokens_out += usage.output;
            meters.cache_read += usage.cache_read;
            meters.elapsed += elapsed;
            let tail = usage_tail(usage, elapsed);
            let silent = matches!(stop, ka_protocol::Stop::Done)
                && usage.input + usage.output + usage.cache_read == 0
                && usage.cost == 0.0;
            // silent = replayed turns (resume): no notification for those
            if !silent {
                let stop_label = match stop {
                    ka_protocol::Stop::Done => "done",
                    ka_protocol::Stop::Aborted => "aborted",
                    ka_protocol::Stop::Length => "length",
                    ka_protocol::Stop::Error => "error",
                };
                fire_notifications("turn_finished", stop_label);
            }
            if !silent {
                let (glyph, tone, text, detail) = match stop {
                    ka_protocol::Stop::Done => ('✓', SummaryTone::Ok, format!("done{tail}"), None),
                    ka_protocol::Stop::Aborted => (
                        '◐',
                        SummaryTone::Warn,
                        format!("aborted · partial kept{tail}"),
                        None,
                    ),
                    ka_protocol::Stop::Length => (
                        '◑',
                        SummaryTone::Warn,
                        format!("stopped at output limit{tail}"),
                        None,
                    ),
                    ka_protocol::Stop::Error => match last_error.take() {
                        // the row keeps a 90-char preview; the full text
                        // stays one click away (▸ expands the detail)
                        Some(msg) if msg.chars().count() > 90 => {
                            let short: String =
                                msg.chars().take(89).chain(std::iter::once('…')).collect();
                            (
                                '✗',
                                SummaryTone::Err,
                                format!("failed · {short} · /retry{tail}"),
                                Some(msg),
                            )
                        }
                        Some(msg) => (
                            '✗',
                            SummaryTone::Err,
                            format!("failed · {msg} · /retry{tail}"),
                            None,
                        ),
                        None => (
                            '✗',
                            SummaryTone::Err,
                            format!("failed · /retry{tail}"),
                            None,
                        ),
                    },
                };
                transcript.push_separated(Line::Summary {
                    glyph,
                    tone,
                    text,
                    detail,
                    expanded: false,
                });
                // best-effort desktop notification (OSC 9); one write
                let label = match stop {
                    ka_protocol::Stop::Done => format!("done · {}", fmt_dur(elapsed)),
                    ka_protocol::Stop::Aborted => "aborted".to_string(),
                    ka_protocol::Stop::Length => format!("stopped · {}", fmt_dur(elapsed)),
                    ka_protocol::Stop::Error => "failed".to_string(),
                };
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(format!("\x1b]9;ka · {label}\x07").as_bytes());
                let _ = out.flush();
            }
            finished = finished_call;
        }
        Event::ModelChanged { selector } => meters.model = selector.clone(),
        Event::ContextMeter { used, window } => {
            meters.context = (*used, *window);
        }
        Event::EffortChanged { level } => {
            meters.effort = format!("{level:?}").to_lowercase();
        }
        Event::SessionInfo { id } => meters.session = id.clone(),
        Event::ModeChanged { mode } => {
            meters.mode = mode_label(*mode).to_string();
            meters.mode_icon = crate::icons::mode_icon(*mode).to_string();
        }
        Event::Error { message, .. } => {
            if *busy {
                // in-turn error: buffer it; the TurnFinished report row
                // surfaces it once the turn settles
                *last_error = Some(message.clone());
            } else {
                transcript.push_separated(Line::Err(message.clone()));
            }
        }
        Event::Replay { messages } => {
            // a replay is the full transcript of the active session:
            // rebuild from scratch (startup on a fresh Transcript, session
            // switch replaces the previous conversation)
            transcript.clear();
            meters.cost = 0.0;
            meters.context = (0, 0);
            current_assistant.clear();
            current_thought.clear();
            current_tool.clear();
            *live_tool = None;
            *last_user = None;
            *last_error = None;
            for m in messages {
                if m.digest || m.role == "digest" {
                    // faint compaction divider: history before this point
                    // lives in the summary, not the transcript
                    transcript.push_separated(Line::Info("⋯ digest ⋯".into()));
                    continue;
                }
                if m.role == "user" {
                    transcript.push_separated(Line::User(m.content.clone()));
                    continue;
                }
                // assistant: dim thought first, then one boxed card per
                // tool call (consecutive calls merge into one block),
                // then the text block when present
                if let Some(t) = m.thinking.clone().filter(|t| !t.trim().is_empty()) {
                    transcript.push_separated(Line::Thought(t));
                }
                for c in &m.calls {
                    transcript.push_tool_call(ToolCall {
                        head: tool_header(&c.tool, &c.detail),
                        ok: !c.is_error,
                        note: tool_note(c.result.as_deref().unwrap_or("")),
                        excerpt: c.result.clone().unwrap_or_default(),
                        spill: None,
                        dur: None,
                        expanded: false,
                    });
                }
                if !m.content.trim().is_empty() {
                    transcript.push_separated(Line::Assistant(m.content.clone()));
                }
            }
        }
        Event::Note { message } => transcript.push_separated(Line::Info(message.clone())),
        Event::Inventory {
            tools,
            mcp,
            agents,
            skills,
            prompts,
            disabled,
        } => {
            // the bootstrap card is transcript noise on re-emits (every
            // toggle change re-emits the inventory) — only the first one
            // introduces itself
            if sidebar.inventory.tools.is_empty() {
                let card = inventory_card(mcp, agents);
                if !card.is_empty() {
                    transcript.push_separated(Line::Info(card));
                }
            }
            // tools are the *visible* set; keep a superset so a hidden
            // tool can be re-enabled from the /features panel
            for t in tools {
                if !sidebar.known_tools.contains(t) {
                    sidebar.known_tools.push(t.clone());
                }
            }
            // the /skill: autocomplete follows the live inventory
            if let Ok(mut live) = SKILLS.write() {
                *live = skills.clone();
            }
            sidebar.inventory = Inventory {
                tools: tools.clone(),
                mcp: mcp.clone(),
                agents: agents.clone(),
                skills: skills.clone(),
                prompts: prompts.clone(),
                disabled: disabled.clone(),
            };
            meters.toggles = disabled.len();
        }
        Event::FeaturesChanged { disabled, sandbox } => {
            sidebar.inventory.disabled = disabled.clone();
            sidebar.sandbox = sandbox.clone();
            meters.toggles = disabled.len();
        }
        Event::Title { title } => {
            if !title.is_empty() {
                sidebar.title = Some(title.clone());
            }
        }
        Event::Todos { items } => sidebar.todos = items.clone(),
        // the engine sends Idle after every non-turn command; without
        // clearing here, the optimistic busy set on a session switch
        // leaks into the next prompt ("steering this turn" on turn one)
        Event::Idle => {
            *busy = false;
            *busy_since = None;
        }
        Event::DigestStarted => {
            transcript.push_separated(Line::Info("⋯ digesting context…".to_string()))
        }
        Event::DigestFinished { .. } => {}
        // the run loop opens the /context modal from this event
        Event::ContextBreakdown { .. } => {}
        // the run loop opens the /tasks pager and /debug overlay from
        // these events (transcript fallback included)
        Event::TaskDetail { .. } | Event::DebugRoster { .. } => {}
        // serve-side observer presence: the in-process writer TUI never
        // receives it (the server emits it to SSE subscribers only)
        Event::Presence { .. } => {}
    }
    finished
}

/// Compact bootstrap inventory card for [`Event::Inventory`]: one muted
/// transcript entry covering only what the sidebar does not own — the
/// mcp (`name ✓ n` / `name ✗`) and agents segments, one detail row each
/// under ~90 columns via `(+N)` overflow marks. With both empty no card
/// line lands at all (the transcript stays clean; the sidebar owns
/// tools and skills).
fn inventory_card(mcp: &[ka_protocol::McpSummary], agents: &[String]) -> String {
    const CAP: usize = 90;
    let mut rows = Vec::new();
    if !mcp.is_empty() {
        let items: Vec<String> = mcp
            .iter()
            .map(|s| {
                if s.ok {
                    format!("{} ✓ {}", s.name, s.tools)
                } else {
                    format!("{} ✗", s.name)
                }
            })
            .collect();
        rows.push(pack_row("mcp", &items, CAP));
    }
    if !agents.is_empty() {
        rows.push(pack_row("agents", agents, CAP));
    }
    rows.join("\n")
}

/// Split an inventory prompt row (`server/name (a, b)`) into its
/// coordinates and argument names.
fn parse_prompt_row(row: &str) -> (Option<(String, String)>, Vec<String>) {
    let (spec, args) = match row.find(" (") {
        Some(p) => (&row[..p], Some(&row[p + 2..])),
        None => (row, None),
    };
    let coords = spec
        .split_once('/')
        .map(|(server, name)| (server.to_string(), name.trim_end_matches(')').to_string()));
    let arg_names = args
        .map(|a| {
            a.trim_end_matches(')')
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    (coords, arg_names)
}

/// Parse `/prompt server/name k=v k2=v2` into a CallPrompt triple.
fn parse_prompt_invocation(
    rest: &str,
) -> Option<(String, String, std::collections::HashMap<String, String>)> {
    let mut parts = rest.split_whitespace();
    let spec = parts.next()?;
    let (server, name) = spec.split_once('/')?;
    let mut args = std::collections::HashMap::new();
    for pair in parts {
        let (k, v) = pair.split_once('=')?;
        args.insert(k.to_string(), v.to_string());
    }
    Some((server.to_string(), name.to_string(), args))
}

/// `label: a · b · c`, dropping tail items for a `(+N)` mark once the
/// row would pass `cap` columns (the first item always shows).
fn pack_row(label: &str, items: &[String], cap: usize) -> String {
    let mut row = format!("{label}: ");
    let mut shown = 0usize;
    for item in items {
        let candidate = if shown == 0 {
            format!("{row}{item}")
        } else {
            format!("{row} · {item}")
        };
        let remaining = items.len() - shown - 1;
        // room for the worst-case " (+NN)" suffix
        let suffix = if remaining > 0 { 6 } else { 0 };
        if shown > 0 && candidate.chars().count() + suffix > cap {
            break;
        }
        row = candidate;
        shown += 1;
    }
    let omitted = items.len() - shown;
    if omitted > 0 {
        row.push_str(&format!(" (+{omitted})"));
    }
    row
}

fn record_spill(spills: &mut Vec<String>, path: &str) {
    if spills.contains(&path.to_string()) {
        return;
    }
    spills.push(path.to_string());
    if spills.len() > 50 {
        spills.remove(0);
    }
}

/// Suspend the TUI (flush, leave raw mode and the alternate screen), run
/// `program` with inherited stdio, then restore raw mode + alternate
/// screen and force a full redraw. The child's exit status is ignored —
/// editors and pagers exit non-zero routinely. The mouse mode is
/// restored to whatever the session runs in (capture or native+1007) —
/// an unconditional capture re-arm would desync the flag and kill the
/// native wheel.
fn run_external(
    program: &str,
    args: &[&str],
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    mouse_captured: bool,
) -> std::io::Result<()> {
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::PopKeyboardEnhancementFlags,
        crossterm::event::DisableBracketedPaste,
        crossterm::event::DisableMouseCapture
    );
    // children must see a plain legacy keyboard: REPORT_ALL_KEYS makes
    // every key — including unmodified printables — arrive as CSI u,
    // garbling $EDITOR/pagers, and bracketed paste wraps their stdin
    let _ = std::io::stdout().write_all(b"\x1b[?1003l\x1b[?1007l");
    let _ = std::io::stdout().flush();
    let res = std::process::Command::new(program)
        .args(args)
        .status()
        .map(|_| ());
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::PushKeyboardEnhancementFlags(startup_keyboard_flags()),
        crossterm::event::EnableBracketedPaste
    );
    let _ = crossterm::terminal::enable_raw_mode();
    apply_mouse_mode(mouse_captured);
    let _ = terminal.clear();
    res
}
/// Slash-command autocomplete state.
#[derive(Debug, Clone)]
pub struct SlashPopup {
    /// (command, description) items filtered by the current prefix.
    pub items: Vec<(String, String)>,
    /// Selected item index.
    pub selected: usize,
}

/// All available slash commands: builtins + custom files.
/// Detect image bytes by magic prefix (png/jpeg/gif/webp).
fn sniff_image_bytes(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Detect an image file by magic bytes (png/jpeg/gif/webp).
fn sniff_image_type(path: &std::path::Path) -> Option<&'static str> {
    use std::io::Read;
    let mut head = [0u8; 12];
    let n = std::fs::File::open(path).ok()?.read(&mut head).ok()?;
    sniff_image_bytes(&head[..n])
}

/// Build an attachment part from raw image bytes: magic-byte sniff,
/// 5 MB cap, base64 payload.
fn image_part_from_bytes(data: &[u8]) -> Result<ka_protocol::ImagePart, String> {
    const MAX_BYTES: usize = 5 * 1024 * 1024;
    if data.len() > MAX_BYTES {
        return Err(format!(
            "clipboard image is {:.1} MB, over the 5 MB cap",
            data.len() as f64 / (1024.0 * 1024.0)
        ));
    }
    let media_type = sniff_image_bytes(data)
        .ok_or_else(|| "unsupported image type (png/jpg/webp/gif only)".to_string())?;
    Ok(ka_protocol::ImagePart {
        data: b64encode(data),
        media_type: media_type.to_string(),
    })
}

/// Load an image file into an attachment part: size precheck, then the
/// shared bytes path.
fn image_part_from_path(path: &std::path::Path) -> Result<ka_protocol::ImagePart, String> {
    const MAX_MB: u64 = 5;
    let meta = std::fs::metadata(path).map_err(|e| format!("image {}: {e}", path.display()))?;
    if meta.len() > MAX_MB * 1024 * 1024 {
        return Err(format!(
            "image {} is {:.1} MB, over the {MAX_MB} MB cap",
            path.display(),
            meta.len() as f64 / (1024.0 * 1024.0)
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("image {}: {e}", path.display()))?;
    image_part_from_bytes(&bytes).map_err(|e| format!("image {}: {e}", path.display()))
}

/// Attach `/image <path>`: validate, stage, and confirm in the
/// transcript. Returns `None` when the text is not an /image command.
fn handle_image_command(
    text: &str,
    pending_image: &mut Option<ka_protocol::ImagePart>,
) -> Option<Result<String, String>> {
    let rest = text.strip_prefix("/image ")?;
    let path = std::path::PathBuf::from(rest.trim());
    match image_part_from_path(&path) {
        Ok(part) => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let kb = part.data.len() * 3 / 4 / 1024;
            *pending_image = Some(part);
            Some(Ok(format!("[img {name} · {} · {kb}KB]", "attached")))
        }
        Err(e) => Some(Err(e)),
    }
}

/// True under WSL (Windows Subsystem for Linux).
fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/version")
        .map(|v| v.to_lowercase().contains("microsoft"))
        .unwrap_or(false)
}

/// Translate a Windows path (`C:\a\b`) to its WSL mount (`/mnt/c/a/b`).
fn win_to_wsl(path: &str) -> Option<std::path::PathBuf> {
    let path = path.trim().trim_matches('"');
    let (drive, rest) = path.split_once(':')?;
    let mut chars = drive.chars();
    let letter = chars.next()?;
    if chars.next().is_some() || !letter.is_ascii_alphabetic() {
        return None;
    }
    Some(std::path::PathBuf::from(format!(
        "/mnt/{}{}",
        letter.to_ascii_lowercase(),
        rest.replace('\\', "/")
    )))
}

/// One clipboard probe under the shared deadline: stdout bytes on
/// success, Err when the tool is missing, fails, or the budget ran out.
async fn clip_probe(
    program: &str,
    args: &[&str],
    deadline: tokio::time::Instant,
) -> Result<Vec<u8>, ()> {
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return Err(());
    }
    let out = tokio::time::timeout(remaining, async {
        tokio::process::Command::new(program)
            .args(args)
            .output()
            .await
    })
    .await
    .map_err(|_| ())?
    .map_err(|_| ())?;
    if out.status.success() && !out.stdout.is_empty() {
        Ok(out.stdout)
    } else {
        Err(())
    }
}

/// Save the Windows clipboard image to %TEMP%\ka-clip.png via
/// PowerShell and read it back across the /mnt/c boundary.
async fn clip_via_powershell(deadline: tokio::time::Instant) -> Result<Vec<u8>, ()> {
    const SCRIPT: &str = "$ErrorActionPreference='stop'; \
Add-Type -AssemblyName System.Windows.Forms; \
$i=[System.Windows.Forms.Clipboard]::GetImage(); \
if ($i) { $p=Join-Path $env:TEMP 'ka-clip.png'; \
$i.Save($p,[System.Drawing.Imaging.ImageFormat]::Png); Write-Output $p }";
    let out = clip_probe(
        "powershell.exe",
        &["-NoProfile", "-Command", SCRIPT],
        deadline,
    )
    .await?;
    // the printed path is ASCII; lossy decoding never panics
    let line = String::from_utf8_lossy(&out);
    let win_path = line.lines().next().ok_or(())?.trim();
    let path = win_to_wsl(win_path).ok_or(())?;
    std::fs::read(path).map_err(|_| ())
}

/// Read image bytes off the clipboard: wl-paste → xclip → WSL2
/// PowerShell bridge, all under one 4 s overall budget.
async fn clip_image_bytes() -> Result<Vec<u8>, String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(4);
    for (program, args) in [
        ("wl-paste", vec!["--type", "image/png"]),
        (
            "xclip",
            vec!["-selection", "clipboard", "-t", "image/png", "-o"],
        ),
    ] {
        if let Ok(bytes) = clip_probe(program, &args, deadline).await {
            return Ok(bytes);
        }
    }
    if is_wsl() {
        if let Ok(bytes) = clip_via_powershell(deadline).await {
            return Ok(bytes);
        }
    }
    Err("no image on clipboard (tried wl-paste, xclip, powershell)".to_string())
}

pub fn available_slash_commands() -> Vec<(String, String)> {
    let mut out = builtin_slash_commands();
    // custom files come after builtins, named like any slash command so
    // the popup prefix-filter and Tab-accept work the same way
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    // safe mode: custom commands are customizations — built-ins only
    let custom = if ka_engine::conventions::bare_mode() {
        Vec::new()
    } else {
        scan_custom_commands(&cwd)
    };
    for c in custom {
        let hint = if c.argument_hint.is_empty() {
            String::new()
        } else {
            format!(" ({})", c.argument_hint)
        };
        out.push((
            format!("/{}", c.name),
            format!("custom command{hint} — {desc}", desc = c.description),
        ));
    }
    out
}

fn builtin_slash_commands() -> Vec<(String, String)> {
    let mut cmds = vec![
        (
            "/model".to_string(),
            "pick a model (/model <vendor> filters)".to_string(),
        ),
        (
            "/mcp".to_string(),
            "refresh MCP tool lists (/mcp refresh)".to_string(),
        ),
        (
            "/tree".to_string(),
            "fork tree — branch from any input".to_string(),
        ),
        (
            "/memory".to_string(),
            "show loaded MEMORY.md tiers".to_string(),
        ),
        (
            "/tasks".to_string(),
            "background tasks, DAP sessions — ⏎ pages a task result".to_string(),
        ),
        (
            "/debug".to_string(),
            "live debug sessions (breakpoints + output)".to_string(),
        ),
        ("/prompt".to_string(), "run an MCP prompt".to_string()),
        (
            "/provider".to_string(),
            "connect a provider (api key)".to_string(),
        ),
        (
            "/mode".to_string(),
            "permission mode (ask|accept_edits|allow|readonly)".to_string(),
        ),
        (
            "/features".to_string(),
            "toggle features, tools, skills, MCP (on|off spec)".to_string(),
        ),
        (
            "/sandbox".to_string(),
            "switch the bash sandbox (off|fs)".to_string(),
        ),
        (
            "/skill".to_string(),
            "invoke a skill: /skill:<name> [args]".to_string(),
        ),
        (
            "/thinking".to_string(),
            "pick the reasoning level".to_string(),
        ),
        (
            "/plan".to_string(),
            "research the task, draft the plan file".to_string(),
        ),
        ("/build".to_string(), "implement the plan file".to_string()),
        (
            "/review".to_string(),
            "read-only review of current changes ([base])".to_string(),
        ),
        (
            "/approve".to_string(),
            "review the plan file, then build it".to_string(),
        ),
        (
            "/rewind".to_string(),
            "drop the last N exchanges".to_string(),
        ),
        (
            "/fork".to_string(),
            "fork this session into a copy [turns to drop]".to_string(),
        ),
        (
            "/checkpoint".to_string(),
            "snapshot the working tree (git)".to_string(),
        ),
        (
            "/restore".to_string(),
            "restore a checkpoint [id | list]".to_string(),
        ),
        ("/compact".to_string(), "digest the context now".to_string()),
        (
            "/name".to_string(),
            "name this session (ka --session <name>)".to_string(),
        ),
        (
            "/transfer".to_string(),
            "digest into a fresh handoff session".to_string(),
        ),
        (
            "/session".to_string(),
            "pick a session to resume".to_string(),
        ),
        ("/resume".to_string(), "alias for /session".to_string()),
        ("/new".to_string(), "start a fresh session".to_string()),
        (
            "/undo".to_string(),
            "restore the last edited/written file".to_string(),
        ),
        ("/retry".to_string(), "resend the last prompt".to_string()),
        (
            "/copy".to_string(),
            "copy the last reply (OSC52)".to_string(),
        ),
        ("/clip".to_string(), "attach a clipboard image".to_string()),
        ("/image".to_string(), "attach an image [path]".to_string()),
        ("/agents".to_string(), "list available agents".to_string()),
        (
            "/find".to_string(),
            "search the transcript: /find <text>, bare repeats".to_string(),
        ),
        (
            "/spills".to_string(),
            "browse spilled tool output in $PAGER".to_string(),
        ),
        (
            "/export".to_string(),
            "save this session to markdown [path]".to_string(),
        ),
        ("/help".to_string(), "commands and key bindings".to_string()),
        (
            "/settings".to_string(),
            "settings & provider status".to_string(),
        ),
        (
            "/usage".to_string(),
            "usage & cost: session + recent".to_string(),
        ),
        (
            "/context".to_string(),
            "context usage breakdown".to_string(),
        ),
        (
            "/key".to_string(),
            "set an api key for the current model".to_string(),
        ),
        ("/quit".to_string(), "exit".to_string()),
        (
            "/update".to_string(),
            "update ka, then resume this session (idle only)".to_string(),
        ),
    ];
    if let Ok(cwd) = std::env::current_dir() {
        let home = std::env::var("HOME").ok();
        // project scope anchors at the root project, not the launch dir
        let root = ka_engine::project_root(&cwd);
        let mut roots = vec![
            root.join(".ka/commands"),
            root.join(".agents/commands"),
            root.join(".claude/commands"),
        ];
        if let Some(h) = home {
            roots.push(std::path::PathBuf::from(h).join(".config/ka/commands"));
        }
        for root in roots {
            let Ok(entries) = std::fs::read_dir(&root) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "md") {
                    continue;
                }
                let name = format!(
                    "/{}",
                    path.file_stem().unwrap_or_default().to_string_lossy()
                );
                if !cmds.iter().any(|(n, _)| *n == name) {
                    let desc = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|body| {
                            body.lines().find(|l| !l.trim().is_empty()).map(|l| {
                                l.trim()
                                    .trim_start_matches(['#', '>'])
                                    .trim()
                                    .chars()
                                    .take(40)
                                    .collect::<String>()
                            })
                        })
                        .filter(|d| !d.is_empty())
                        .unwrap_or_else(|| "(custom)".to_string());
                    cmds.push((name, desc));
                }
            }
        }
    }
    cmds
}

/// Path-completion popup state (opened by Tab on a path-like or
/// @-mention token).
#[derive(Debug, Clone)]
pub struct PathPopup {
    /// Matching `(name, is_dir)` entries for the current directory.
    pub entries: Vec<(String, bool)>,
    /// Selected entry index.
    pub selected: usize,
    /// Char index where the completed token starts in the input.
    pub token_start: usize,
    /// The typed directory part incl. trailing `/` (e.g. `./src/`) for
    /// path completion, or the bare `@` sigil for file mentions; an
    /// accepted entry is inserted as `prefix + name`.
    pub prefix: String,
    /// False: filesystem completion against `list_matches`. True:
    /// @-mention mode over the recursive `walk_files` listing, where a
    /// file accept inserts a trailing space.
    pub mentions: bool,
}

/// Directories the @-mention walk never descends into (plus every
/// dotfile directory).
const WALK_SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", ".venv", "dist"];

/// Entry cap for the @-mention walk.
const WALK_CAP: usize = 500;

/// Recursive walk of `root` collecting `(relative_path, is_dir)` for
/// every non-dotfile, skipping [`WALK_SKIP_DIRS`] and dotfile
/// directories. Sorted for determinism; capped at `cap` entries.
pub fn walk_files(root: &std::path::Path, cap: usize) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    walk_into(root, "", &mut out);
    out.sort();
    out.truncate(cap);
    out
}

fn walk_into(dir: &std::path::Path, prefix: &str, out: &mut Vec<(String, bool)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            if WALK_SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            let rel = format!("{prefix}{name}");
            out.push((rel.clone(), true));
            walk_into(&dir.join(&name), &format!("{rel}/"), out);
        } else {
            out.push((format!("{prefix}{name}"), false));
        }
    }
}

/// The @-mention query carried by a completion token: Some(text after
/// the `@`) when the token starts with `@` and carries more than the
/// bare sigil.
pub fn mention_token(token: &str) -> Option<&str> {
    token.strip_prefix('@').filter(|q| !q.is_empty())
}

/// @-mention match rule: the text after the last `/` in the query is a
/// prefix of the entry path's last segment (case-sensitive; ordering is
/// the walk's deterministic sort).
pub fn mention_matches(entries: &[(String, bool)], query: &str) -> Vec<(String, bool)> {
    let base = query.rsplit('/').next().unwrap_or("");
    entries
        .iter()
        .filter(|(rel, _)| {
            rel.trim_end_matches('/')
                .rsplit('/')
                .next()
                .is_some_and(|seg| seg.starts_with(base))
        })
        .cloned()
        .collect()
}

/// The whitespace-delimited word at the caret (char index). None when
/// the caret sits between/inside whitespace or the text is empty.
pub fn path_token(text: &str, cursor: usize) -> Option<(usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let cursor = cursor.min(len);
    let ws = |c: char| matches!(c, ' ' | '\n' | '\t');
    if cursor < len && ws(chars[cursor]) {
        return None;
    }
    if cursor == len && (len == 0 || ws(chars[len - 1])) {
        return None;
    }
    let mut start = cursor;
    while start > 0 && !ws(chars[start - 1]) {
        start -= 1;
    }
    let mut end = cursor;
    while end < len && !ws(chars[end]) {
        end += 1;
    }
    let token: String = chars[start..end].iter().collect();
    if token.is_empty() {
        None
    } else {
        Some((start, token))
    }
}

/// Split a completion token at its last `/`: `(dir_part incl. '/', base, is_absolute)`.
pub fn split_path_token(token: &str) -> (String, String, bool) {
    match token.rfind('/') {
        Some(i) => (
            token[..=i].to_string(),
            token[i + 1..].to_string(),
            token.starts_with('/'),
        ),
        None => (String::new(), token.to_string(), false),
    }
}

/// The directory to list for `dir_part`: absolute as-is; `~/…` under
/// $HOME; relative joined to the cwd; empty = cwd. None when HOME is
/// needed but unset.
fn path_root(dir_part: &str) -> Option<std::path::PathBuf> {
    if dir_part.starts_with('/') {
        return Some(std::path::PathBuf::from(dir_part));
    }
    if dir_part == "~" {
        return std::env::var("HOME").ok().map(std::path::PathBuf::from);
    }
    if let Some(rest) = dir_part.strip_prefix("~/") {
        let home = std::env::var("HOME").ok()?;
        return Some(std::path::PathBuf::from(home).join(rest));
    }
    let cwd = std::env::current_dir().ok()?;
    Some(if dir_part.is_empty() {
        cwd
    } else {
        cwd.join(dir_part)
    })
}

/// Entries in `dir_part` whose names start with `base`. Dotfiles are
/// hidden unless `base` itself starts with `.`. Sorted; capped at 200.
pub fn list_matches(dir_part: &str, base: &str) -> Vec<(String, bool)> {
    let Some(root) = path_root(dir_part) else {
        return Vec::new();
    };
    let show_dots = base.starts_with('.');
    let mut out: Vec<(String, bool)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !show_dots && name.starts_with('.') {
            continue;
        }
        if !name.starts_with(base) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        out.push((name, is_dir));
        if out.len() >= 200 {
            break;
        }
    }
    out.sort();
    out
}

/// Replace the word at `[tok_start, tok_start + tok_len)` (chars) with
/// `insert`; returns the new text and the cursor after the insertion.
pub fn complete_token(
    text: &str,
    tok_start: usize,
    tok_len: usize,
    insert: &str,
) -> (String, usize) {
    let mut chars: Vec<char> = text.chars().collect();
    chars.splice(tok_start..(tok_start + tok_len), insert.chars());
    let out: String = chars.into_iter().collect();
    let cursor = tok_start + insert.chars().count();
    (out, cursor)
}

/// Recompute the suggestion popup from the raw input.
/// What Tab accepting a slash popup row types into the draft: a
/// trailing space for argument-bearing commands, the colon for the
/// `/skill` starter — completing `/sk` lands on `/skill:` with the
/// inventory names already in the popup, not on `/skill ` with the
/// family closed.
fn complete_slash(name: &str) -> String {
    if name == "/skill" {
        "/skill:".to_string()
    } else {
        format!("{name} ")
    }
}

pub fn update_suggestions(input: &str) -> Option<SlashPopup> {
    if !input.starts_with('/') || input.contains(' ') {
        return None;
    }
    let mut items = available_slash_commands();
    // /skill:<name> — explicit invocations of the live inventory's
    // skills (names only cross the protocol; discovery + trust stay
    // engine-side). The shared prefix filter matches the rest.
    if input.starts_with("/skill:") {
        if let Ok(skills) = SKILLS.read() {
            items.extend(
                skills
                    .iter()
                    .map(|name| (format!("/skill:{name}"), "invoke skill".to_string())),
            );
        }
    }
    let items: Vec<(String, String)> = items
        .into_iter()
        .filter(|(name, _)| name.starts_with(input))
        .collect();
    if items.is_empty() {
        None
    } else {
        Some(SlashPopup { items, selected: 0 })
    }
}

/// The user config path shown in the settings panel (mirrors
/// ka-engine's `config::user_config_path`).
fn ka_config_path() -> String {
    std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("ka/ka.toml")
        .display()
        .to_string()
}

/// A parsed slash command.
pub struct Slash {
    pub event: Option<Command>,
    pub quit: bool,
    pub followup: Option<String>,
    /// Modal to open instead of sending an event.
    pub modal: Option<ModalKind>,
    /// Local transcript note (no engine roundtrip).
    pub note: Option<String>,
}

/// Modal a slash command opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModalKind {
    /// `/mode` permission picker (borrows the input box).
    Mode,
    /// `/thinking` reasoning-level picker (borrows the input box).
    Thinking,
    /// Session picker.
    Session,
    /// Settings panel.
    Settings,
    /// Model picker; the carried vendor lock makes `/model <vendor>`
    /// open the list narrowed to that vendor (None = every configured
    /// provider).
    Model(Option<String>),
    /// Provider picker.
    Provider,
    /// API key prompt.
    Key,
    /// Spill-file viewer.
    Spills,
    /// MCP prompt picker.
    Prompts,
    /// Memory viewer.
    Memory,
    /// Usage dashboard.
    Usage,
    /// /tasks picker (rows arrive with `Event::Tasks`).
    Tasks,
    /// /debug overlay (rows arrive with `Event::DebugRoster`).
    Debug,
    /// Strand tree.
    Tree,
    /// Help overlay.
    Help,
}

/// Load a custom command body from `.ka/commands/<name>.md` (project) or
/// the user dir; `$ARGUMENTS` substituted with the rest of the line.
fn project_trusted_in(state_home: &std::path::Path, cwd: &std::path::Path) -> bool {
    // exact, canonicalized membership via the engine's helper: the old
    // raw-substring match missed differently-spelled launch dirs and
    // could false-positive on sibling path prefixes
    let file = state_home.join("ka/trust.json");
    ka_engine::trust::trusted_in(cwd, &ka_engine::trust::load_trust_at(&file))
}

/// One discovered custom slash command.
pub struct CustomCommand {
    /// Command name (without `/`).
    pub name: String,
    /// Frontmatter description (or first body line).
    pub description: String,
    /// Frontmatter argument hint.
    pub argument_hint: String,
}

/// Project command directories, scanned for the popup.
const PROJECT_COMMAND_DIRS: &[&str] = &[".ka/commands", ".agents/commands", ".claude/commands"];

/// Scan custom slash commands: project dirs (trust-gated like skills)
/// plus the user dir (ungated). Project wins on name collisions.
pub fn scan_custom_commands(cwd: &std::path::Path) -> Vec<CustomCommand> {
    let state_home = std::env::var("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| {
            std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state"))
        })
        .unwrap_or_else(|_| std::env::temp_dir());
    scan_custom_commands_in(cwd, &state_home)
}

pub fn scan_custom_commands_in(
    cwd: &std::path::Path,
    state_home: &std::path::Path,
) -> Vec<CustomCommand> {
    let mut out: Vec<CustomCommand> = Vec::new();
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    // project scope anchors at the root project, matching the engine's
    // trust store entries (one approval covers every launch dir)
    let root = ka_engine::project_root(cwd);
    if project_trusted_in(state_home, &root) {
        dirs.extend(PROJECT_COMMAND_DIRS.iter().map(|d| root.join(d)));
    }
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(std::path::PathBuf::from(home).join(".config/ka/commands"));
    }
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let (fm, _body) = parse_command_md(&text);
            let name = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if out.iter().any(|c: &CustomCommand| c.name == name) {
                continue;
            }
            out.push(CustomCommand {
                name,
                description: fm.description,
                argument_hint: fm.argument_hint,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Parsed frontmatter of a custom command markdown file.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CommandFrontmatter {
    pub description: String,
    pub argument_hint: String,
    /// `allowed-tools`: tool names this command's turn may use (empty =
    /// the session toolset). Comma/space separated.
    pub allowed_tools: Vec<String>,
    /// `model`: per-turn model selector override.
    pub model: Option<String>,
}

/// Split frontmatter (`description`, `argument-hint`,
/// `allowed-tools`, `model`) off a command markdown body. Unknown keys
/// are tolerated (agentskills.io convention). Public for contract
/// tests.
pub fn parse_command_md(text: &str) -> (CommandFrontmatter, String) {
    let mut fm = CommandFrontmatter::default();
    let mut body = text.trim().to_string();
    if let Some(rest) = text.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            for line in rest[..end].lines() {
                if let Some((key, value)) = line.split_once(':') {
                    match key.trim() {
                        "description" => fm.description = value.trim().to_string(),
                        "argument-hint" | "argument_hint" => {
                            fm.argument_hint = value.trim().to_string();
                        }
                        "allowed-tools" | "allowed_tools" => {
                            fm.allowed_tools = value
                                .trim()
                                .trim_matches('[')
                                .trim_matches(']')
                                .split([',', ' '])
                                .map(str::trim)
                                .filter(|t| !t.is_empty())
                                .map(str::to_string)
                                .collect();
                        }
                        "model" => {
                            fm.model = Some(value.trim().to_string()).filter(|m| !m.is_empty());
                        }
                        _ => {}
                    }
                }
            }
            body = rest[end + 4..]
                .trim_start_matches('\n')
                .trim_end()
                .to_string();
        }
    }
    if fm.description.is_empty() {
        fm.description = body
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("custom command")
            .chars()
            .take(60)
            .collect();
    }
    (fm, body)
}

fn custom_command(head: &str, rest: Option<&str>) -> Option<(CommandFrontmatter, String)> {
    // safe mode: custom commands are customizations — typed commands
    // must not load their bodies from disk (built-ins only)
    if ka_engine::conventions::bare_mode() {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    let state_home = std::env::var("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| {
            std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state"))
        })
        .unwrap_or_else(|_| std::env::temp_dir());
    custom_command_in(&cwd, &state_home, head, rest)
}

fn custom_command_in(
    cwd: &std::path::Path,
    state_home: &std::path::Path,
    head: &str,
    rest: Option<&str>,
) -> Option<(CommandFrontmatter, String)> {
    let name = head.strip_prefix('/')?;
    // project scope anchors at the root project, not the launch dir
    let root = ka_engine::project_root(cwd);
    let mut candidates = vec![
        root.join(format!(".ka/commands/{name}.md")),
        root.join(format!(".agents/commands/{name}.md")),
        root.join(format!(".claude/commands/{name}.md")),
    ];
    if let Ok(home) = std::env::var("HOME") {
        candidates
            .push(std::path::PathBuf::from(home).join(format!(".config/ka/commands/{name}.md")));
    }
    let home = std::env::var("HOME").unwrap_or_default();
    for path in candidates {
        // user-dir commands are ungated; project commands pay the trust
        // gate (like skills)
        let is_user_file = !home.is_empty() && path.starts_with(&home);
        if !is_user_file && !project_trusted_in(state_home, &root) {
            continue;
        }
        if let Ok(body) = std::fs::read_to_string(&path) {
            let args = rest.unwrap_or("");
            let (fm, clean) = parse_command_md(&body);
            return Some((fm, clean.replace("$ARGUMENTS", args)));
        }
    }
    None
}

/// `/skill:<name> [args]`: explicit skill invocation. The named
/// skill's SKILL.md body is injected into this turn's system prompt
/// engine-side (per-turn scope, like custom-command frontmatter); the
/// args become the prompt text. Validated against the discovered
/// inventory so a typo fails loudly instead of running a hollow turn —
/// the engine re-checks admit-time (fail closed there too).
///
/// Returns None for anything that is not a `/skill:` line (the normal
/// slash dispatch then handles it), Some(Err(msg)) for a refused
/// invocation (msg names the toggle or the miss).
pub fn skill_invocation(text: &str, inventory: &Inventory) -> Option<Result<Command, String>> {
    let rest = match text.strip_prefix("/skill:") {
        Some(rest) => rest,
        None if text == "/skill" || text.starts_with("/skill ") => {
            // bare `/skill` (or the space form): teach the syntax and
            // list what's on offer — same shape as bare /agents
            if inventory.skills.is_empty() {
                return Some(Err(format!(
                    "no skills discovered ({} skills lists what ka found)",
                    crate::icons::STRIP_SKILLS
                )));
            }
            return Some(Err(format!(
                "invoke one by name: /skill:<name> [args] — {}",
                inventory.skills.join(", ")
            )));
        }
        None => return None,
    };
    let (name, args) = match rest.split_once(' ') {
        Some((name, args)) => (name.trim(), args.trim()),
        None => (rest.trim(), ""),
    };
    if name.is_empty() {
        return Some(Err("skill name expected: /skill:<name>".to_string()));
    }
    // inventory skills = discovered AND enabled (emit_inventory filters
    // both); disabled names arrive in the shared spec grammar
    if !inventory.skills.iter().any(|s| s == name) {
        if inventory.disabled.iter().any(|d| d == "skills") {
            return Some(Err("skills are disabled — /features on skills".to_string()));
        }
        if inventory
            .disabled
            .iter()
            .any(|d| d == &format!("skill:{name}"))
        {
            return Some(Err(format!(
                "skill {name} is disabled — /features on skill:{name}"
            )));
        }
        return Some(Err(format!(
            "no such skill: {name} ({} skills lists what ka found)",
            crate::icons::STRIP_SKILLS
        )));
    }
    Some(Ok(Command::Prompt {
        text: if args.is_empty() {
            format!("Use the {name} skill.")
        } else {
            args.to_string()
        },
        schema: None,
        images: Vec::new(),
        allowed_tools: None,
        model: None,
        skills: vec![name.to_string()],
    }))
}

pub fn slash_command(text: &str) -> Option<Slash> {
    let mut parts = text.splitn(2, ' ');
    let head = parts.next()?.trim();
    let rest = parts.next().map(str::trim).filter(|s| !s.is_empty());
    // custom commands load from files; builtins win over files
    if !matches!(
        head,
        "/quit"
            | "/exit"
            | "/model"
            | "/provider"
            | "/mode"
            | "/thinking"
            | "/compact"
            | "/session"
            | "/resume"
            | "/new"
            | "/settings"
            | "/key"
            | "/export"
            | "/spills"
            | "/fork"
            | "/name"
            | "/transfer"
            | "/checkpoint"
            | "/restore"
            | "/mcp"
            | "/prompt"
            | "/tree"
            | "/memory"
            | "/tasks"
            | "/debug"
    ) {
        if let Some((fm, body)) = custom_command(head, rest) {
            return Some(Slash {
                note: None,
                event: Some(Command::Prompt {
                    text: body,
                    schema: None,
                    images: Vec::new(),
                    skills: Vec::new(),
                    allowed_tools: (!fm.allowed_tools.is_empty()).then_some(fm.allowed_tools),
                    model: fm.model,
                }),
                quit: false,
                followup: None,
                modal: None,
            });
        }
    }
    match head {
        "/quit" | "/exit" => Some(Slash {
            note: None,
            event: None,
            quit: true,
            followup: None,
            modal: None,
        }),
        // dispatched by the /update intercept before this table; a
        // real row keeps the queue refusal and completions honest
        "/update" => Some(Slash {
            note: None,
            event: None,
            quit: true,
            followup: None,
            modal: None,
        }),
        "/model" => match rest {
            // `vendor/model[@effort]` sets the model outright; a bare
            // `vendor` (no `/`) opens the picker locked to that vendor
            // instead — `/model opencode-go` lists opencode-go's models
            Some(selector) => {
                let base = selector.split('@').next().unwrap_or("");
                if base.contains('/') {
                    Some(Slash {
                        note: None,
                        event: Some(Command::SetModel {
                            selector: selector.to_string(),
                        }),
                        quit: false,
                        followup: None,
                        modal: None,
                    })
                } else {
                    Some(Slash {
                        note: None,
                        event: None,
                        quit: false,
                        followup: None,
                        modal: Some(ModalKind::Model(
                            (!base.is_empty()).then(|| base.to_string()),
                        )),
                    })
                }
            }
            None => Some(Slash {
                note: None,
                event: None,
                quit: false,
                followup: None,
                modal: Some(ModalKind::Model(None)),
            }),
        },
        "/provider" => Some(Slash {
            note: None,
            event: None,
            quit: false,
            followup: None,
            modal: Some(ModalKind::Provider),
        }),
        "/plan" => {
            let task = rest.map(str::to_string).unwrap_or_default();
            Some(Slash {
                note: None,
                event: Some(Command::SetMode {
                    mode: ka_protocol::Mode::Readonly,
                }),
                quit: false,
                followup: None,
                modal: None,
            })
            .map(|mut sl| {
                sl.followup = Some(format!(
                    "Plan this task. Research the codebase with read/glob/grep/pathfinder, \
then write a concrete numbered implementation plan to {}. Task: {task}",
                    plan_file_path().display()
                ));
                sl
            })
        }
        "/build" => Some(Slash {
            note: None,
            event: Some(Command::SetMode {
                mode: ka_protocol::Mode::Ask,
            }),
            quit: false,
            modal: None,
            followup: Some(build_followup()),
        }),
        "/review" => {
            // read-only review preset (codex /review shape): plan-mode
            // locking + a strict reviewer prompt; the report lands in
            // chat where /copy and /export can reach it
            let base = rest
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .unwrap_or("the repository's default branch — main or master, whichever exists; if neither, HEAD");
            Some(Slash {
                note: None,
                event: Some(Command::SetMode {
                    mode: ka_protocol::Mode::Readonly,
                }),
                quit: false,
                modal: None,
                followup: Some(format!(
                    "Review the current changes as a strict senior engineer — READ-ONLY, \\
do not modify any files. Compare the working tree against {base} (git diff plus \\
untracked files, via the bash tool; read files for full context). Produce: \\
(1) a one-line verdict (ship / fix first / blocked), (2) findings ordered \\
blocker > major > minor > nit, each as `file:line — issue — concrete fix`, \\
(3) what is missing (tests, docs, error handling)."
                )),
            })
        }
        "/approve" => Some(Slash {
            note: None,
            event: Some(Command::SetMode {
                mode: ka_protocol::Mode::Ask,
            }),
            quit: false,
            modal: None,
            followup: Some(build_followup()),
        }),
        "/rewind" => {
            // bare /rewind drops the last exchange; an unparsable count
            // is refused rather than silently defaulted (a wrong rewind
            // cut loses turns)
            let turns = match rest {
                None => Some(1),
                Some(r) => r.trim().parse::<u32>().ok(),
            };
            Some(match turns {
                Some(turns) => Slash {
                    note: None,
                    event: Some(Command::Rewind { turns }),
                    quit: false,
                    followup: None,
                    modal: None,
                },
                None => Slash {
                    note: Some("usage: /rewind [n]".to_string()),
                    event: None,
                    quit: false,
                    followup: None,
                    modal: None,
                },
            })
        }
        "/compact" => {
            let focus = rest.map(str::to_string);
            Some(Slash {
                note: None,
                event: Some(Command::Compact { focus }),
                quit: false,
                followup: None,
                modal: None,
            })
        }
        "/name" => {
            // a memorable alias for THIS session: `ka --session fox`
            // resolves on it (exact, then unique prefix)
            let clean = rest.map(str::trim).filter(|r| !r.is_empty());
            Some(match clean {
                Some(name) => Slash {
                    event: Some(Command::SetName {
                        name: name.to_string(),
                    }),
                    note: None,
                    quit: false,
                    followup: None,
                    modal: None,
                },
                None => Slash {
                    note: Some("usage: /name <word> — 1–64 chars, no spaces".to_string()),
                    event: None,
                    quit: false,
                    followup: None,
                    modal: None,
                },
            })
        }
        "/transfer" => {
            // digest now, then continue in a fresh handoff session that
            // carries only the summary
            let focus = rest.map(str::to_string);
            Some(Slash {
                event: Some(Command::TransferStrand { focus }),
                note: None,
                quit: false,
                followup: None,
                modal: None,
            })
        }
        "/session" | "/resume" => Some(Slash {
            note: None,
            event: None,
            quit: false,
            followup: None,
            modal: Some(ModalKind::Session),
        }),
        "/new" => Some(Slash {
            note: None,
            event: Some(Command::SwitchStrand {
                id: "new".to_string(),
            }),
            quit: false,
            followup: None,
            modal: None,
        }),
        "/undo" => Some(Slash {
            note: None,
            event: Some(Command::UndoFile),
            quit: false,
            followup: None,
            modal: None,
        }),
        "/fork" => {
            // bare /fork copies the session as-is; an unparsable count is
            // refused rather than defaulted — a wrong fork cut loses turns
            let turns = match rest {
                None => Some(0),
                Some(r) => r.trim().parse::<u32>().ok(),
            };
            Some(match turns {
                Some(turns) => Slash {
                    note: None,
                    event: Some(Command::ForkStrand { turns }),
                    quit: false,
                    followup: None,
                    modal: None,
                },
                None => Slash {
                    note: Some("usage: /fork [turns]".to_string()),
                    event: None,
                    quit: false,
                    followup: None,
                    modal: None,
                },
            })
        }
        "/checkpoint" => Some(Slash {
            note: None,
            event: Some(Command::Checkpoint),
            quit: false,
            followup: None,
            modal: None,
        }),
        "/restore" => Some(Slash {
            note: None,
            event: Some(Command::RestoreCheckpoint {
                id: rest
                    .map(str::to_string)
                    .unwrap_or_else(|| "list".to_string()),
            }),
            quit: false,
            followup: None,
            modal: None,
        }),
        "/export" => {
            // `/export [--html] [path]` — html renders a self-contained
            // offline page via the shared ka-strand renderer
            let rest = rest.unwrap_or_default().trim();
            let html = rest.starts_with("--html");
            let path = rest.trim_start_matches("--html").trim();
            Some(Slash {
                note: None,
                event: Some(Command::ExportMarkdown {
                    out: (!path.is_empty()).then(|| std::path::PathBuf::from(path)),
                    html,
                }),
                quit: false,
                followup: None,
                modal: None,
            })
        }
        "/spills" => Some(Slash {
            note: None,
            event: None,
            quit: false,
            followup: None,
            modal: Some(ModalKind::Spills),
        }),
        "/agents" => Some(Slash {
            event: None,
            quit: false,
            followup: None,
            modal: None,
            note: Some(format!(
                "agents:{}",
                AGENTS
                    .get()
                    .map(|a| {
                        a.iter()
                            .map(|x| format!("\n• {} — {}", x.0, x.1))
                            .collect::<String>()
                    })
                    .unwrap_or_default()
            )),
        }),
        "/help" => Some(Slash {
            note: None,
            event: None,
            quit: false,
            followup: None,
            modal: Some(ModalKind::Help),
        }),
        "/tasks" => Some(Slash {
            note: None,
            event: Some(Command::ListTasks),
            quit: false,
            followup: None,
            modal: None,
        }),
        "/debug" => Some(Slash {
            note: None,
            event: Some(Command::DebugRoster),
            quit: false,
            followup: None,
            modal: None,
        }),
        "/memory" => Some(Slash {
            note: None,
            event: None,
            quit: false,
            followup: None,
            modal: Some(ModalKind::Memory),
        }),
        "/tree" => Some(Slash {
            note: None,
            event: None,
            quit: false,
            followup: None,
            modal: Some(ModalKind::Tree),
        }),
        "/mcp" => match rest {
            Some("refresh") => Some(Slash {
                note: None,
                event: Some(Command::RefreshMcp),
                quit: false,
                followup: None,
                modal: None,
            }),
            _ => Some(Slash {
                note: Some("usage: /mcp refresh — re-list tools on every MCP server".into()),
                event: None,
                quit: false,
                followup: None,
                modal: None,
            }),
        },
        "/prompt" => {
            let parsed = rest.and_then(parse_prompt_invocation);
            match parsed {
                Some((server, name, args)) => Some(Slash {
                    note: None,
                    event: Some(Command::CallPrompt { server, name, args }),
                    quit: false,
                    followup: None,
                    modal: None,
                }),
                None => Some(Slash {
                    note: None,
                    event: None,
                    quit: false,
                    followup: None,
                    modal: Some(ModalKind::Prompts),
                }),
            }
        }
        "/settings" => Some(Slash {
            note: None,
            event: None,
            quit: false,
            followup: None,
            modal: Some(ModalKind::Settings),
        }),
        "/usage" => Some(Slash {
            note: None,
            event: None,
            quit: false,
            followup: None,
            modal: Some(ModalKind::Usage),
        }),
        "/context" => Some(Slash {
            note: None,
            event: None, // dispatched by the busy gate in the app loop
            quit: false,
            followup: None,
            modal: None,
        }),
        "/key" => Some(Slash {
            note: None,
            event: None,
            quit: false,
            followup: None,
            modal: Some(ModalKind::Key),
        }),
        "/mode" => match rest {
            None => Some(Slash {
                note: None,
                event: None,
                quit: false,
                followup: None,
                modal: Some(ModalKind::Mode),
            }),
            Some(name) => {
                let mode = match name {
                    "ask" | "guarded" | "needs-approval" | "needs_approval" => {
                        ka_protocol::Mode::Ask
                    }
                    "accept edits" | "accept-edits" | "accept_edits" => {
                        ka_protocol::Mode::AcceptEdits
                    }
                    "allow" | "free" | "full access" | "full-access" | "full_access" => {
                        ka_protocol::Mode::Allow
                    }
                    "readonly" | "read-only" | "read_only" | "plan" => ka_protocol::Mode::Readonly,
                    other => {
                        return Some(Slash {
                            note: Some(format!("unknown mode {other:?} — pick one below")),
                            event: None,
                            quit: false,
                            followup: None,
                            modal: Some(ModalKind::Mode),
                        });
                    }
                };
                Some(Slash {
                    note: None,
                    event: Some(Command::SetMode { mode }),
                    quit: false,
                    followup: None,
                    modal: None,
                })
            }
        },
        "/thinking" => match rest {
            None => Some(Slash {
                note: None,
                event: None,
                quit: false,
                followup: None,
                modal: Some(ModalKind::Thinking),
            }),
            // named level: session-scoped, like `/mode <name>`
            Some(name) => match parse_effort(name) {
                Some(level) => Some(Slash {
                    note: None,
                    event: Some(Command::SetEffort { level }),
                    quit: false,
                    followup: None,
                    modal: None,
                }),
                None => Some(Slash {
                    note: Some(format!("unknown level {name:?} — pick one below")),
                    event: None,
                    quit: false,
                    followup: None,
                    modal: Some(ModalKind::Thinking),
                }),
            },
        },
        _ => None,
    }
}

/// Everything the live region renders below the cached transcript rows:
/// the in-flight thought tail, the running tool block, streaming markdown.
#[derive(Debug, Clone)]
struct LiveBlock {
    thought: String,
    /// Running tool header (`→ tool · detail`); empty while no call is
    /// in flight. The band folds from this + the live preview at the
    /// RENDER width (prebuilt rows would survive a resize stale).
    tool_header: String,
    /// The in-flight call's rolling output preview.
    live_tool: Option<LiveTool>,
    md: Vec<ratatui::text::Line<'static>>,
}

/// Most recent preview lines shown under the running tool header.
const PREVIEW_WINDOW: usize = 3;

/// Flush in-progress thought and assistant text into the transcript, in
/// turn order. Used at call boundaries (chronology) and at turn end.
fn flush_live_text(transcript: &mut Transcript, thought: &mut String, assistant: &mut String) {
    if !thought.trim().is_empty() {
        transcript.push_thought_streamed(std::mem::take(thought));
    }
    if !assistant.trim().is_empty() {
        transcript.push_separated(Line::Assistant(std::mem::take(assistant)));
    }
}

/// Fold a preview excerpt into the rolling window: every line kept, at
/// most [`PREVIEW_WINDOW`] of the most recent survive.
fn observe_preview(window: &mut Vec<String>, excerpt: &str) {
    for line in excerpt.lines() {
        window.push(line.to_string());
    }
    if window.len() > PREVIEW_WINDOW {
        window.drain(..window.len() - PREVIEW_WINDOW);
    }
}

/// The running call, timeline style: the same header as a finished
/// call ([`timeline_head`]) led by a spinner glyph (`TOOL`) instead of
/// ✓/✗, the live elapsed time in the right cluster, then up to
/// [`PREVIEW_WINDOW`] preview rows threaded `│ ` (the last `╰ `), text
/// faint. Rebuilt every frame at `now`. With nothing to preview yet it
/// is the header alone, exactly like a settled call with no body.
fn tool_live_rows(
    header: &str,
    live: Option<&LiveTool>,
    width: usize,
    now: Instant,
) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::style::Style;
    use ratatui::text::Span;
    let Some(lt) = live else {
        return Vec::new();
    };
    let ms = now.saturating_duration_since(lt.started).as_millis();
    let status = Span::styled(
        spin_frame(ms).to_string(),
        Style::new().fg(crate::palette::TOOL),
    );
    let cluster = vec![Span::styled(
        fmt_dur(ms as f64 / 1000.0),
        Style::new().fg(crate::palette::FAINT),
    )];
    let start = lt.preview.len().saturating_sub(PREVIEW_WINDOW);
    let preview = &lt.preview[start..];
    let mut rows = vec![timeline_head(status, header, None, cluster, width)];
    let room = width.saturating_sub(2).max(1);
    for (i, line) in preview.iter().enumerate() {
        rows.push(thread_row(i + 1 == preview.len(), trunc_cols(line, room)));
    }
    rows
}

/// The one selection identity of the whole TUI: gold ink, bold, on the
/// lifted violet row (overlay rows add the `▌ ` ACCENT pointer and pad
/// to the width — see [`list_row`]).
fn selection_style() -> ratatui::style::Style {
    use ratatui::style::Modifier;
    ratatui::style::Style::new()
        .fg(crate::palette::SEL_FG)
        .bg(crate::palette::SEL_BG)
        .add_modifier(Modifier::BOLD)
}

/// Pad a row to `width` display columns (unicode-width aware) so an
/// inverse selection reads as a bar across the row, not a word.
fn pad_to_width(s: String, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let used = s.width();
    if used >= width {
        return s;
    }
    s + &" ".repeat(width - used)
}

/// Memory tier files for /memory: project MEMORY.md, then the
/// user-level one. Mirrors the engine's system-prompt fold.
/// Rows for the /memory modal: loaded MEMORY.md tiers.
pub fn memory_modal_rows(cwd: &std::path::Path) -> Vec<String> {
    let files = discover_memory_files(cwd);
    let mut rows = Vec::new();
    if files.is_empty() {
        rows.push("(no memory files)".to_string());
        rows.push("create MEMORY.md at the project root or ~/.config/ka/MEMORY.md".to_string());
    }
    for (path, content) in files {
        rows.push(format!("▸ {}", path.display()));
        for line in content.lines() {
            rows.push(line.to_string());
        }
    }
    rows
}

/// The staged-memory inbox file path. Anchors at the root project
/// (nearest `.git` ancestor, else cwd) — the same file the engine's
/// `remember` hand stages into, whatever directory the session runs in.
fn memory_inbox_path(cwd: &std::path::Path) -> std::path::PathBuf {
    ka_engine::project_root(cwd).join(".ka/memory/inbox.md")
}

/// The project MEMORY.md tier: root-anchored, mirroring the engine's
/// [`ka_engine::conventions`] discovery.
fn project_memory_path(cwd: &std::path::Path) -> std::path::PathBuf {
    ka_engine::project_root(cwd).join("MEMORY.md")
}

/// Staged memory proposals, oldest first.
pub fn read_memory_inbox(cwd: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(memory_inbox_path(cwd))
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Persist the inbox after a review action (removed when empty).
pub fn write_memory_inbox(cwd: &std::path::Path, inbox: &[String]) {
    let path = memory_inbox_path(cwd);
    if inbox.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    let mut body = inbox.join(
        "
",
    );
    body.push('\n');
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, body);
}

/// Append one staged note to a MEMORY.md tier (project or user).
/// Strips the `- [stamp] ` prefix before writing.
pub fn accept_memory_note(cwd: &std::path::Path, staged: &str, user: bool) -> std::io::Result<()> {
    let note = staged
        .split_once("] ")
        .map(|(_, note)| note)
        .unwrap_or(staged)
        .trim()
        .trim_start_matches("- ")
        .to_string();
    if note.is_empty() {
        return Ok(());
    }
    let target = if user {
        std::env::var("HOME")
            .map(|h| std::path::PathBuf::from(h).join(".config/ka/MEMORY.md"))
            .unwrap_or_else(|_| project_memory_path(cwd))
    } else {
        project_memory_path(cwd)
    };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut body = std::fs::read_to_string(&target).unwrap_or_default();
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(&note);
    body.push('\n');
    std::fs::write(&target, body)
}

fn discover_memory_files(cwd: &std::path::Path) -> Vec<(std::path::PathBuf, String)> {
    let mut out = Vec::new();
    let project = project_memory_path(cwd);
    if let Ok(content) = std::fs::read_to_string(&project) {
        if !content.trim().is_empty() {
            out.push((project, content));
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let user = std::path::PathBuf::from(home).join(".config/ka/MEMORY.md");
        if let Ok(content) = std::fs::read_to_string(&user) {
            if !content.trim().is_empty() {
                out.push((user, content));
            }
        }
    }
    out
}

/// Contextual key hints as spans — key in ACCENT, action FAINT, two
/// spaces between pairs — dropping pairs from the end until the row
/// fits `max` display columns (empty when not even the first fits).
fn hint_spans(pairs: &[(&str, &str)], max: usize) -> Vec<ratatui::text::Span<'static>> {
    use ratatui::text::Span;
    use unicode_width::UnicodeWidthStr;
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for (k, v) in pairs {
        let gap = if out.is_empty() { 0 } else { 2 };
        let w = gap + k.width() + 1 + v.width();
        if used + w > max {
            break;
        }
        if gap > 0 {
            out.push(Span::raw("  "));
        }
        out.push(Span::styled((*k).to_string(), crate::palette::ACCENT_STYLE));
        out.push(Span::styled(format!(" {v}"), crate::palette::FAINT));
        used += w;
    }
    out
}

/// Hints on the permission ask's bottom border.
const ASK_HINTS: &[(&str, &str)] = &[
    ("↑↓", "select"),
    ("1-9", "pick"),
    ("⏎", "confirm"),
    ("esc", "deny"),
];
/// Hints on the slash popup's bottom border.
const POPUP_HINTS: &[(&str, &str)] = &[("↑↓", "select"), ("tab", "complete"), ("esc", "close")];
/// Hints on the path popup's bottom border.
const PATH_HINTS: &[(&str, &str)] = &[("↑↓", "choose"), ("tab", "accept"), ("esc", "close")];

/// Hints on the composer's bottom border: the borrowed picker's keys,
/// the reverse search's, the busy turn's steer/queue/abort, or the
/// idle send/select/history story for whichever mouse mode is live.
fn composer_hints(
    picker: bool,
    rsearch: bool,
    busy: bool,
    mouse_captured: bool,
) -> &'static [(&'static str, &'static str)] {
    if picker {
        &[
            ("↑↓", "select"),
            ("1-4", "jump"),
            ("⏎", "apply"),
            ("esc", "close"),
        ]
    } else if rsearch {
        &[("^r", "next"), ("⏎", "accept"), ("esc", "cancel")]
    } else if busy {
        &[("⏎", "steer"), ("+ · alt+q", "queue"), ("esc", "abort")]
    } else if mouse_captured {
        &[
            ("⏎", "send"),
            ("drag", "select+copy"),
            ("↑↓ ^p/^n", "history"),
            ("/", "commands"),
        ]
    } else {
        &[
            ("⏎", "send"),
            ("drag", "select"),
            ("↑↓", "scroll"),
            ("^p/^n", "history"),
            ("/", "commands"),
        ]
    }
}

/// Hints on an open modal's bottom border.
fn modal_hints(open: &Modal) -> &'static [(&'static str, &'static str)] {
    match open {
        Modal::Session(_) => &[("type", "filter"), ("⏎", "switch"), ("esc", "close")],
        Modal::Model(_) => &[("type", "filter"), ("⏎", "select"), ("esc", "back")],
        Modal::Provider(_) => &[("type", "filter"), ("⏎", "connect"), ("esc", "close")],
        Modal::Settings(p) if p.edit.is_some() => {
            &[("type", "edit selector"), ("⏎", "apply"), ("esc", "cancel")]
        }
        Modal::Settings(_) => &[("⏎", "edit/cycle"), ("s", "save"), ("esc", "close")],
        Modal::Spills { .. } => &[("↑↓", "choose"), ("⏎", "open"), ("esc", "close")],
        Modal::Prompts { .. } => &[("↑↓", "choose"), ("⏎", "run / fill args"), ("esc", "close")],
        Modal::Tree { .. } => &[
            ("↑↓", "choose"),
            ("⏎", "attach / fork at prompt"),
            ("e", "fork & edit"),
            ("esc", "close"),
        ],
        Modal::Rewind { .. } => &[
            ("↑↓", "choose"),
            ("⏎", "rewind"),
            ("e", "edit"),
            ("esc", "close"),
        ],
        Modal::Memory { inbox, .. } if inbox.is_empty() => &[("esc", "close")],
        Modal::Memory { .. } => &[
            ("↑↓", "choose"),
            ("⏎", "project"),
            ("u", "user"),
            ("d", "discard"),
            ("esc", "close"),
        ],
        Modal::Usage { .. } | Modal::Todos { .. } | Modal::Skills { .. } | Modal::Info { .. } => {
            &[("any", "close")]
        }
        Modal::Features { .. } => &[
            ("↑↓", "choose"),
            ("⏎", "toggle"),
            ("s", "save as default"),
            ("esc", "close"),
        ],
        Modal::Context { .. } => &[("esc", "close")],
        Modal::Tasks { .. } => &[("↑↓", "choose"), ("⏎", "page result"), ("esc", "close")],
        Modal::TaskDetail { .. } | Modal::Debug { .. } => &[
            ("pgup/pgdn", "scroll"),
            ("home/end", "top/tail"),
            ("esc", "close"),
        ],
        Modal::Key(_) => &[("type", "value"), ("⏎", "save"), ("esc", "cancel")],
        Modal::Help => &[("⏎", "close")],
    }
}

/// `Buffer::set_line` guarded against an origin outside the buffer
/// (tiny terminals squeeze rects to nothing); the width clips at the
/// buffer edge.
fn put_line(buf: &mut ratatui::buffer::Buffer, x: u16, y: u16, line: &ratatui::text::Line<'_>) {
    if buf.area.contains(ratatui::layout::Position { x, y }) {
        buf.set_line(x, y, line, line.width() as u16);
    }
}

/// Paint contextual hints right-aligned on a border row: they end at
/// column `right` (exclusive), never start left of `left`, and carry
/// one cell of air each side. Pairs drop from the end to fit.
fn paint_hints(
    buf: &mut ratatui::buffer::Buffer,
    left: u16,
    right: u16,
    y: u16,
    pairs: &[(&str, &str)],
) {
    use ratatui::text::{Line as TuiLine, Span};
    let room = right.saturating_sub(left) as usize;
    let spans = hint_spans(pairs, room.saturating_sub(2));
    if spans.is_empty() {
        return;
    }
    let mut row = vec![Span::raw(" ")];
    row.extend(spans);
    row.push(Span::raw(" "));
    let line = TuiLine::from(row);
    put_line(buf, right.saturating_sub(line.width() as u16), y, &line);
}

/// The composer's bottom-border buttons in drawn order:
/// `(button, icon, label, key)`.
const STRIP_BUTTONS: [(StripButton, &str, &str, &str); 3] = [
    (
        StripButton::Todos,
        crate::icons::STRIP_TODOS,
        "todos",
        "alt+o",
    ),
    (
        StripButton::Skills,
        crate::icons::STRIP_SKILLS,
        "skills",
        "^t",
    ),
    (StripButton::Info, crate::icons::STRIP_INFO, "info", "alt+i"),
];

/// Draw the strip buttons ` {icon} {label} {key} ` (icon ACCENT, label
/// META, key FAINT) on a border row from column `x0`, one border cell
/// apart, never reaching `limit`. Returns the zones taken from the
/// exact drawn cells (a button that did not fit keeps an empty rect,
/// which no click can hit) and the first column past the last button's
/// trailing gap.
fn paint_strip(
    buf: &mut ratatui::buffer::Buffer,
    x0: u16,
    limit: u16,
    y: u16,
) -> (StripZones, u16) {
    use ratatui::text::{Line as TuiLine, Span};
    let mut zones = StripZones::default();
    let mut x = x0;
    for (button, icon, label, key) in STRIP_BUTTONS {
        let line = TuiLine::from(vec![
            Span::raw(" "),
            Span::styled(icon, crate::palette::ACCENT_STYLE),
            Span::raw(" "),
            Span::styled(label, crate::palette::META_STYLE),
            Span::raw(" "),
            Span::styled(key, crate::palette::FAINT),
            Span::raw(" "),
        ]);
        let w = line.width() as u16;
        if x.saturating_add(w) > limit {
            break;
        }
        put_line(buf, x, y, &line);
        let rect = ratatui::layout::Rect {
            x,
            y,
            width: w,
            height: 1,
        };
        match button {
            StripButton::Todos => zones.todos = rect,
            StripButton::Skills => zones.skills = rect,
            StripButton::Info => zones.info = rect,
        }
        x += w + 1;
    }
    (zones, x)
}

/// The permission mode the meters' word names (`icons::mode_word`
/// spelling); None for an empty or unknown word.
fn mode_of(meters: &Meters) -> Option<ka_protocol::Mode> {
    MODE_CHOICES
        .iter()
        .map(|(m, _, _)| *m)
        .find(|m| crate::icons::mode_word(*m) == meters.mode)
}

/// The mode pill ` {icon} {word} `: dark chip ink, bold, on the mode's
/// identity color (META for a word no mode claims). Empty while the
/// mode is still unknown.
fn mode_pill(meters: &Meters) -> Vec<ratatui::text::Span<'static>> {
    use ratatui::style::{Modifier, Style};
    if meters.mode.is_empty() {
        return Vec::new();
    }
    let mode = mode_of(meters);
    let icon = match mode {
        Some(m) => crate::icons::mode_icon(m),
        None => meters.mode_icon.as_str(),
    };
    let bg = mode.map_or(crate::palette::META, crate::palette::mode_color);
    let text = if icon.is_empty() {
        format!(" {} ", meters.mode)
    } else {
        format!(" {icon} {} ", meters.mode)
    };
    vec![ratatui::text::Span::styled(
        text,
        Style::new()
            .fg(crate::palette::CHIP_FG)
            .bg(bg)
            .add_modifier(Modifier::BOLD),
    )]
}

/// Effort segment: `✻ {glyph} {level}` when the level has a battery
/// glyph, `✻ {level}` otherwise (unknown levels hide the glyph).
fn effort_segment(level: &str) -> String {
    match crate::icons::effort_glyph(level) {
        Some(g) => format!("{} {g} {level}", crate::icons::THOUGHT),
        None => format!("{} {level}", crate::icons::THOUGHT),
    }
}

/// Status-line left: model (INK_DIM), effort (META), the feature-toggle
/// count and a pending update (WARN) — each fact only when known. The
/// mode pill closes the row on the right.
fn status_left(meters: &Meters) -> Vec<ratatui::text::Span<'static>> {
    use ratatui::text::Span;
    let mut segs: Vec<Span<'static>> = Vec::new();
    let mut push = |text: String, color: ratatui::style::Color| {
        let lead = if segs.is_empty() { "" } else { " " };
        segs.push(Span::styled(format!("{lead}{text}"), color));
    };
    if !meters.model.is_empty() {
        push(meters.model.clone(), crate::palette::INK_DIM);
    }
    if meters.effort_supported {
        // the engine always emits the level now; defensive: a stale
        // empty string reads as off
        let level = if meters.effort.is_empty() {
            "off"
        } else {
            meters.effort.as_str()
        };
        push(effort_segment(level), crate::palette::META);
    }
    if meters.toggles > 0 {
        // feature toggles active: a count keeps the status line honest
        // about what the model can no longer see
        push(format!("\u{2298}{}", meters.toggles), crate::palette::WARN);
    }
    if let Some(latest) = &meters.update {
        // a newer release is out (the startup check announced it once;
        // the badge persists)
        push(format!("\u{2934} {latest}"), crate::palette::WARN);
    }
    segs
}

/// Context-gauge cells on the status line.
const CTX_GAUGE_CELLS: usize = 10;

/// Status-line right: a ten-cell `━` context gauge (filled in OK, WARN
/// from 80%, ERR from 95%; the rest RULE) plus ` {pct}%` FAINT — or
/// `~N tok` when the window is unknown — then `  ${cost}` META, and the
/// mode pill at the very end. Cost always shows; the context facts only
/// when known.
fn status_right(meters: &Meters) -> Vec<ratatui::text::Span<'static>> {
    use ratatui::text::Span;
    let (used, window) = meters.context;
    let mut segs: Vec<Span<'static>> = Vec::new();
    if window > 0 {
        let frac = used as f64 / window as f64;
        let pct = (frac * 100.0) as u64;
        let filled = ((frac * CTX_GAUGE_CELLS as f64).round() as usize).min(CTX_GAUGE_CELLS);
        let color = if pct >= 95 {
            crate::palette::ERR
        } else if pct >= 80 {
            crate::palette::WARN
        } else {
            crate::palette::OK
        };
        if filled > 0 {
            segs.push(Span::styled("━".repeat(filled), color));
        }
        if filled < CTX_GAUGE_CELLS {
            segs.push(Span::styled(
                "━".repeat(CTX_GAUGE_CELLS - filled),
                crate::palette::RULE,
            ));
        }
        segs.push(Span::styled(format!(" {pct}%"), crate::palette::FAINT));
    } else if used > 0 {
        segs.push(Span::styled(
            format!("~{} tok", fmt_tok(used)),
            crate::palette::FAINT,
        ));
    }
    let lead = if segs.is_empty() { "" } else { "  " };
    segs.push(Span::styled(
        format!("{lead}${:.4}", meters.cost),
        crate::palette::META,
    ));
    // the mode pill closes the row: the rightmost cell is the state the
    // user acts from, and it never shifts when facts appear on the left
    let pill = mode_pill(meters);
    if !pill.is_empty() {
        segs.push(Span::raw(" "));
        segs.extend(pill);
    }
    segs
}

/// The welcome logo: "KA" in the ANSI Shadow face (one column per
/// glyph, every row the same width).
const WELCOME_LOGO: [&str; 6] = [
    "██╗  ██╗ █████╗ ",
    "██║ ██╔╝██╔══██╗",
    "█████╔╝ ███████║",
    "██╔═██╗ ██╔══██║",
    "██║  ██╗██║  ██║",
    "╚═╝  ╚═╝╚═╝  ╚═╝",
];

/// The welcome's starter tips: `(key, what it does)`.
const WELCOME_TIPS: [(&str, &str); 4] = [
    ("/help", "keys & commands"),
    ("/session", "past chats"),
    ("@", "attach files"),
    ("!", "run shell"),
];

/// The fresh-conversation welcome, centered in a `width`×`height`
/// transcript pane: the big brand-gradient "KA" logo, the tagline, the
/// mode pill + model, `{cwd} ⎇ {branch}`, then a two-column tips grid.
/// Panes under 44 cols or 16 rows get a one-line gradient `◆ ka`
/// instead of the logo. Rendered only while the transcript is empty
/// and no turn is in flight — the first exchange (or a replayed older
/// chat) replaces it.
fn welcome_rows(
    width: usize,
    height: usize,
    glyph: &str,
    meters: &Meters,
    sidebar: &SidebarState,
) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line as TuiLine, Span};
    use unicode_width::UnicodeWidthStr;
    let center = |spans: Vec<Span<'static>>| -> TuiLine<'static> {
        let w: usize = spans.iter().map(|s| s.content.width()).sum();
        let mut row = vec![Span::raw(" ".repeat(width.saturating_sub(w) / 2))];
        row.extend(spans);
        TuiLine::from(row)
    };
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let faint = Style::new().fg(crate::palette::FAINT);
    let glyph = if glyph.is_empty() {
        crate::icons::BRAND
    } else {
        glyph
    };
    let mut rows: Vec<TuiLine<'static>> = Vec::new();
    if width >= 44 && height >= 16 {
        for row in WELCOME_LOGO {
            rows.push(center(crate::fx::gradient(
                row,
                &crate::palette::BRAND,
                bold,
            )));
        }
    } else {
        rows.push(center(crate::fx::gradient(
            &format!("{glyph} ka"),
            &crate::palette::BRAND,
            bold,
        )));
    }
    rows.push(center(vec![Span::styled(
        "new conversation",
        faint.add_modifier(Modifier::ITALIC),
    )]));
    rows.push(TuiLine::default());
    // mode pill + model ride from the meters; unknown facts stay hidden
    let mut config = mode_pill(meters);
    if !meters.model.is_empty() {
        config.push(Span::styled(
            format!(" {}", meters.model),
            crate::palette::INK_DIM,
        ));
    }
    if !config.is_empty() {
        rows.push(center(config));
    }
    if !sidebar.cwd.is_empty() {
        let mut place = vec![Span::styled(sidebar.cwd.clone(), faint)];
        if let Some(branch) = &sidebar.branch {
            place.push(Span::styled(
                format!(" {} ", crate::icons::BRANCH),
                crate::palette::OK,
            ));
            place.push(Span::styled(branch.clone(), crate::palette::INK_DIM));
        }
        rows.push(center(place));
    }
    rows.push(TuiLine::default());
    let tip = |(key, desc): (&str, &str)| {
        vec![
            Span::styled(format!("{key:<9}"), crate::palette::ACCENT_STYLE),
            Span::styled(format!("{desc:<15}"), faint),
        ]
    };
    // two columns when they fit (2 × 24 cols + a 3-col gutter)
    if width >= 51 {
        for pair in WELCOME_TIPS.chunks_exact(2) {
            let mut row = tip(pair[0]);
            row.push(Span::raw("   "));
            row.extend(tip(pair[1]));
            rows.push(center(row));
        }
    } else {
        for t in WELCOME_TIPS {
            rows.push(center(tip(t)));
        }
    }
    let top = height.saturating_sub(rows.len()) / 2;
    let mut out = vec![TuiLine::default(); top];
    out.extend(rows);
    out
}

/// One selectable list row inside a `width`-col overlay body: the
/// selected row leads with the `▌ ` pointer (ACCENT on SEL_BG) and
/// wears [`selection_style`] padded to the width; unselected rows lead
/// two blank cols (columns stay aligned) in `style`. The text truncates
/// so a row never wraps.
fn list_row(
    text: String,
    selected: bool,
    width: usize,
    style: ratatui::style::Style,
) -> ratatui::text::Line<'static> {
    use ratatui::style::Style;
    use ratatui::text::{Line as TuiLine, Span};
    let room = width.saturating_sub(2);
    let text = trunc_cols(&text, room);
    if selected {
        TuiLine::from(vec![
            Span::styled(
                format!("{} ", crate::icons::RAIL),
                Style::new()
                    .fg(crate::palette::ACCENT)
                    .bg(crate::palette::SEL_BG),
            ),
            Span::styled(pad_to_width(text, room), selection_style()),
        ])
    } else {
        TuiLine::from(vec![Span::raw("  "), Span::styled(text, style)])
    }
}

/// The filter row atop a filterable picker: `❯ {filter}▌` in ACCENT;
/// an empty filter shows a FAINT italic `type to filter` instead.
fn filter_row(filter: &str) -> ratatui::text::Line<'static> {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line as TuiLine, Span};
    let lead = Span::styled(
        format!("{} ", crate::icons::PROMPT),
        crate::palette::ACCENT_STYLE,
    );
    if filter.is_empty() {
        TuiLine::from(vec![
            lead,
            Span::styled(
                "type to filter",
                Style::new()
                    .fg(crate::palette::FAINT)
                    .add_modifier(Modifier::ITALIC),
            ),
        ])
    } else {
        TuiLine::from(vec![
            lead,
            Span::styled(filter.to_string(), crate::palette::ACCENT_STYLE),
            Span::styled(crate::icons::RAIL, crate::palette::ACCENT_STYLE),
        ])
    }
}

/// Drop shadow under an overlay: the column right of `rect` and the
/// row below it (each offset one cell) take the SHADOW ground with
/// their truecolor glyphs dimmed toward it. Cells past the buffer are
/// skipped, so the shadow clips to the frame.
fn drop_shadow(buf: &mut ratatui::buffer::Buffer, rect: ratatui::layout::Rect) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let shade = |cell: &mut ratatui::buffer::Cell| {
        cell.set_bg(crate::palette::SHADOW);
        if let ratatui::style::Color::Rgb(..) = cell.fg {
            cell.set_fg(crate::fx::lerp(cell.fg, crate::palette::SHADOW, 0.6));
        }
    };
    let (right, bottom) = (rect.right(), rect.bottom());
    for y in rect.y.saturating_add(1)..=bottom {
        if let Some(cell) = buf.cell_mut((right, y)) {
            shade(cell);
        }
    }
    for x in rect.x.saturating_add(1)..right {
        if let Some(cell) = buf.cell_mut((x, bottom)) {
            shade(cell);
        }
    }
}

/// Dim the backdrop behind a modal or ask: every truecolor foreground
/// inside `area` blends 55% toward the canvas, so the overlay owns the
/// eye. Grounds stay put (no re-layout, no flicker).
fn dim_backdrop(buf: &mut ratatui::buffer::Buffer, area: ratatui::layout::Rect) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut((x, y))
                && let ratatui::style::Color::Rgb(..) = cell.fg
            {
                cell.set_fg(crate::fx::lerp(cell.fg, crate::palette::BG, 0.55));
            }
        }
    }
}

/// Composer placeholder for an empty idle draft.
const COMPOSER_PLACEHOLDER: &str = "Ask anything  ·  / commands  ·  @ files  ·  ! shell";
/// Composer placeholder for an empty draft while a turn runs.
const COMPOSER_PLACEHOLDER_BUSY: &str = "Steer the turn  ·  + to queue  ·  esc to abort";
/// Columns the composer's `❯ ` lead (and the continuation indent)
/// takes before the draft text.
const COMPOSER_LEAD: u16 = 2;
/// Busy comet: one column per this many ms across the top border.
const COMET_STEP_MS: u128 = 24;
/// Busy comet: half-width of the light band, in columns.
const COMET_RADIUS: f32 = 10.0;

#[allow(clippy::too_many_arguments)]
fn render(
    frame: &mut ratatui::Frame,
    transcript: &mut Transcript,
    scroll: Option<usize>,
    input: &str,
    cursor: usize,
    busy: bool,
    busy_since: Option<Instant>,
    now: Instant,
    queued: usize,
    ask: Option<&PendingAsk>,
    live: Option<&LiveBlock>,
    popup: Option<&SlashPopup>,
    path: Option<&PathPopup>,
    // live reverse-search query line; Some while Ctrl+R search is active
    rsearch: Option<&str>,
    modal: Option<&Modal>,
    picker: Option<&InputPicker>,
    meters: &Meters,
    sidebar: &SidebarState,
    fresh: bool,
    // mouse mode: the idle hints teach the selection/scroll story for
    // whichever mode is live; the ▲▼ arrows only exist under capture
    mouse_captured: bool,
    header_glyph: &str,
    strip_zone: &std::cell::Cell<Option<StripZones>>,
    // transient action feedback, auto-expiring; None most of the time
    toast: Option<&str>,
    title_arrows: &std::cell::Cell<Option<TitleArrows>>,
    // the modal overlay rect this frame drew (None without a modal):
    // the ✕ chip / click-outside close routing reads it
    modal_zone: &std::cell::Cell<Option<ratatui::layout::Rect>>,
    // the slash/path popup or mode-picker rect this frame drew (the
    // picker borrows the composer): click-outside close routing
    popup_zone: &std::cell::Cell<Option<ratatui::layout::Rect>>,
    // content rows + first visible row: click-to-collapse hit mapping
    tx_content: &std::cell::Cell<Option<(ratatui::layout::Rect, usize)>>,
    // the scrollbar rail geometry this frame drew (rect, total,
    // visible, thumb position, thumb length): the mouse router reads
    // it for thumb drags and track paging
    rail_zone: &std::cell::Cell<Option<RailZone>>,
    // the transcript width this frame adopts, published back to the
    // tick so the markdown cache keys on the real render width
    tx_width: &std::cell::Cell<u16>,
    // the drag-selected text span — (row, column) anchors in reading
    // order — rendered as a picked block highlight; None most of the
    // time
    sel: Option<((usize, u16), (usize, u16))>,
    // what the pointer currently hovers (capture mode only): the
    // hovered rows/chips paint the hover band / gold emphasis
    hover: Option<Hover>,
) {
    use ratatui::layout::Constraint::{Length, Min};
    use ratatui::layout::Rect;
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line as TuiLine, Span};
    use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
    use unicode_width::UnicodeWidthStr;
    // zones start empty each frame; the code below publishes the rects
    // it actually draws (nothing drawn → nothing to click)
    modal_zone.set(None);
    popup_zone.set(None);
    rail_zone.set(None);
    title_arrows.set(None);
    strip_zone.set(None);
    // ── canvas: paint the whole frame before any widget so the app sits
    // on the night ground whatever the terminal theme paints behind it
    let full = frame.area();
    frame.render_widget(Block::new().style(crate::palette::CANVAS), full);
    // ── layout: header bar · air · transcript · composer · air · status
    // line. The header and status line run the full frame width;
    // transcript and composer keep one column of air on each side.
    // Every width below derives from these chunks, never from the raw
    // frame area.
    let chunks = ratatui::layout::Layout::vertical([
        Length(1),
        Length(1),
        Min(0),
        Length(input_height(input_area_rows(
            picker,
            input,
            input_inner_w(full.width),
        ))),
        Length(1),
        Length(1),
    ])
    .split(full);
    let header = chunks[0];
    let tx_area = chunks[2].inner(ratatui::layout::Margin::new(1, 0));
    let composer = chunks[3].inner(ratatui::layout::Margin::new(1, 0));
    let status = chunks[5];
    let tx_inner = tx_area.width.saturating_sub(2);
    // single width authority: the frame's own layout decides the cache
    // width — never a tick-side terminal.size() guess
    tx_width.set(tx_inner);
    transcript.set_width(tx_inner);
    // modals center within the transcript pane, one col clear of its
    // edges: they never touch the composer or the status line
    let modal_area = tx_area.inner(ratatui::layout::Margin::new(1, 0));
    // an overlay owns keyboard focus (the composer border steps back)
    let overlay = modal.is_some() || popup.is_some() || path.is_some() || picker.is_some();

    // ── transcript: cached rows + live region under a scroll window ──
    let live_rows: Vec<TuiLine> = live
        .map(|lb| live_region_rows(lb, ask, busy_since, now, tx_inner as usize))
        .unwrap_or_default();
    let cached = transcript.total_rows();
    let total = cached + live_rows.len();
    // the pane is all content: no border or title row above it
    let visible = tx_area.height as usize;
    debug_assert_eq!(
        visible,
        visible_rows(full.height, composer.height),
        "viewport arithmetic must agree with the layout"
    );
    let (start, pinned) = window_range(total, visible, scroll);
    let end = (start + visible).min(total);
    let mut window: Vec<TuiLine> = Vec::with_capacity(end - start);
    for i in start..end {
        if i < cached {
            if let Some(row) = transcript.row(i) {
                window.push(row.clone());
            }
        } else if let Some(row) = live_rows.get(i - cached) {
            window.push(row.clone());
        }
    }
    // content rows fill the pane one column in from its left edge; the
    // right column is the scrollbar rail
    let content_rect = Rect {
        x: tx_area.x + 1,
        y: tx_area.y,
        width: tx_inner,
        height: tx_area.height,
    };
    // an empty, idle, fresh transcript opens on the welcome — the first
    // exchange (or a replayed older chat) replaces it
    if total == 0 && !busy && fresh {
        window = welcome_rows(
            tx_inner as usize,
            content_rect.height as usize,
            header_glyph,
            meters,
            sidebar,
        );
    }
    frame.render_widget(Paragraph::new(window), content_rect);
    // content rows + the first visible transcript row: click-to-collapse
    // maps screen rows back to entries through this
    tx_content.set(Some((content_rect, start)));

    // drag-selection highlight: patch the picked rows' cells so the
    // span reads as one block while it is being drawn and after
    if let Some(((r0, c0), (r1, c1))) = sel {
        let last = start + content_rect.height as usize;
        let lo = r0.max(start);
        let hi = r1.min(last.saturating_sub(1));
        if lo <= hi {
            let right = content_rect.width.saturating_sub(1);
            let buf = frame.buffer_mut();
            for trow in lo..=hi {
                let y = content_rect.y + (trow - start) as u16;
                // the drag's own rows paint just their picked columns;
                // rows in between paint edge to edge
                let x0 = if trow == r0 { c0.min(right) } else { 0 };
                let x1 = if trow == r1 { c1.min(right) } else { right };
                for x in x0..=x1 {
                    if let Some(cell) = buf.cell_mut((content_rect.x + x, y)) {
                        cell.set_bg(crate::palette::BG_SELECT);
                    }
                }
            }
        }
    }

    // ── header bar: brand › title (· scroll %) … cwd ⎇ branch ▲ ▼ ──
    // The ▲▼ user-message jump arrows ride the right end only in
    // captured-mouse mode (they are click targets — in native mode they
    // would be dead pixels; Ctrl+↑/↓ are the keyboard twin) and once
    // the user has sent a message. Narrow widths drop the place first,
    // then the arrows, then truncate the title.
    {
        let panel = Style::new().bg(crate::palette::BG_PANEL);
        let glyph = if header_glyph.is_empty() {
            crate::icons::BRAND
        } else {
            header_glyph
        };
        let mut left: Vec<Span<'static>> = crate::fx::gradient(
            &format!(" {glyph} ka "),
            &crate::palette::BRAND,
            panel.add_modifier(Modifier::BOLD),
        );
        left.push(Span::styled(" › ", crate::palette::FAINT));
        // how far through the history the viewport sits
        let scrolled = (!pinned).then(|| {
            let span = total.saturating_sub(visible).max(1);
            format!(" · {}%", (start * 100 / span).min(100))
        });
        let mut place: Vec<Span<'static>> = Vec::new();
        if !sidebar.cwd.is_empty() {
            place.push(Span::styled(sidebar.cwd.clone(), crate::palette::FAINT));
            if let Some(branch) = &sidebar.branch {
                place.push(Span::styled(
                    format!(" {} ", crate::icons::BRANCH),
                    crate::palette::OK,
                ));
                place.push(Span::styled(branch.clone(), crate::palette::INK_DIM));
            }
        }
        let cols =
            |spans: &[Span<'static>]| -> usize { spans.iter().map(|s| s.content.width()).sum() };
        let hw = header.width as usize;
        let fixed = cols(&left[..]) + scrolled.as_ref().map_or(0, |s| s.width());
        let title = sidebar.title.as_deref().filter(|t| !t.is_empty());
        let title_w = title.map_or("new session".width(), |t| t.width());
        let has_user = transcript
            .entries()
            .iter()
            .any(|l| matches!(l, Line::User(_)));
        let arrows_w = if has_user && mouse_captured { 5 } else { 0 };
        // the place carries its own trailing space of air
        let place_w = if place.is_empty() {
            0
        } else {
            cols(&place[..]) + 1
        };
        let show_place = place_w > 0 && fixed + title_w + 1 + place_w + arrows_w <= hw;
        let show_arrows = arrows_w > 0 && fixed + title_w.min(8) + 1 + arrows_w <= hw;
        let right_w =
            (if show_place { place_w } else { 0 }) + (if show_arrows { arrows_w } else { 0 });
        let room = hw.saturating_sub(fixed + right_w + 1);
        if room > 0 {
            left.push(match title {
                Some(t) => Span::styled(
                    trunc_cols(t, room),
                    Style::new()
                        .fg(crate::palette::FG)
                        .add_modifier(Modifier::BOLD),
                ),
                None => Span::styled(
                    trunc_cols("new session", room),
                    Style::new()
                        .fg(crate::palette::FAINT)
                        .add_modifier(Modifier::ITALIC),
                ),
            });
        }
        if let Some(s) = scrolled {
            left.push(Span::styled(s, crate::palette::FAINT));
        }
        frame.render_widget(
            Paragraph::new(TuiLine::from(left)).style(panel.fg(crate::palette::FG)),
            header,
        );
        let buf = frame.buffer_mut();
        let y = header.y;
        let mut x = header.right().saturating_sub(right_w as u16);
        if show_place {
            place.push(Span::raw(" "));
            put_line(buf, x, y, &TuiLine::from(place));
            x += place_w as u16;
        }
        if show_arrows && header.height > 0 {
            let arrow = crate::palette::ACCENT_STYLE;
            put_line(
                buf,
                x,
                y,
                &TuiLine::from(vec![
                    Span::raw(" "),
                    Span::styled("▲", arrow),
                    Span::raw(" "),
                    Span::styled("▼", arrow),
                    Span::raw(" "),
                ]),
            );
            // ' ▲' steps to the previous user message, ' ▼' to the next
            title_arrows.set(Some(TitleArrows {
                up: Rect {
                    x,
                    y,
                    width: 2,
                    height: 1,
                },
                down: Rect {
                    x: x + 2,
                    y,
                    width: 2,
                    height: 1,
                },
            }));
        }
    }

    // ── scrollbar rail: the pane's rightmost column, so no width math
    // ever learns about it ──
    if total > visible {
        let (thumb_pos, thumb_len) = rail_thumb(visible, total, visible, start);
        // span-level styling: Paragraph paints span styles, not line styles
        let rail: Vec<TuiLine> = (0..visible)
            .map(|i| {
                if (thumb_pos..thumb_pos + thumb_len).contains(&i) {
                    TuiLine::from(vec![Span::styled("┃", crate::palette::FAINT)])
                } else {
                    TuiLine::from(vec![Span::styled("│", crate::palette::BORDER_QUIET_STYLE)])
                }
            })
            .collect();
        let rect = Rect {
            x: tx_area.right().saturating_sub(1),
            y: tx_area.y,
            width: 1,
            height: visible as u16,
        };
        frame.render_widget(Paragraph::new(rail), rect);
        // capture mode owns the pointer: publish the geometry for
        // thumb drags and track paging (native mode stays display-only)
        if mouse_captured {
            rail_zone.set(Some((rect, total, visible, thumb_pos, thumb_len)));
        }
    }

    // ── jump pill: scrolled away from the tail, the bottom-right of the
    // pane says how much sits below and how to get back (display-only)
    if !pinned {
        let pill = format!(" ↓ {} below · esc ", total.saturating_sub(end));
        let w = pill.width() as u16;
        if content_rect.width > w + 1 && content_rect.height > 0 {
            put_line(
                frame.buffer_mut(),
                content_rect.right() - w - 1,
                content_rect.bottom() - 1,
                &TuiLine::from(Span::styled(
                    pill,
                    Style::new()
                        .fg(crate::palette::ACCENT)
                        .bg(crate::palette::BG_SURFACE),
                )),
            );
        }
    }

    // ── composer ─────────────────────────────────────────────────────
    // Border: WARN while an ask is pending; the resting BORDER while an
    // overlay owns focus; busy → a TOOL-tinted base the comet sweeps;
    // idle with a draft → BORDER_FOCUS; idle empty → BORDER. The
    // /mode and /thinking pickers borrow the box. The title carries
    // structure only (picker name, reverse search, queue count).
    let busy_base = crate::fx::lerp(crate::palette::BORDER, crate::palette::TOOL, 0.5);
    let border_fg = if ask.is_some() {
        crate::palette::WARN
    } else if overlay {
        crate::palette::BORDER
    } else if busy {
        busy_base
    } else if input.is_empty() {
        crate::palette::BORDER
    } else {
        crate::palette::BORDER_FOCUS
    };
    let title: Option<TuiLine<'static>> = match picker {
        Some(InputPicker::Mode(_)) => Some(padded_title("mode")),
        Some(InputPicker::Thinking(_)) => Some(padded_title("thinking")),
        None => match rsearch {
            Some(q) => Some(TuiLine::styled(
                format!(" search · {} ", q.strip_prefix("rsearch: ").unwrap_or(q)),
                crate::palette::ACCENT_STYLE,
            )),
            None => busy_input_title(queued).map(padded_title),
        },
    };
    // picker rows: borders + horizontal padding
    let picker_w = composer.width.saturating_sub(4) as usize;
    // the draft's cursor as (visible row, display col) past the lead
    let mut cursor_at: Option<(u16, u16)> = None;
    let body: Vec<TuiLine> = match picker {
        // one row per tier: the label column stays padded so the
        // descriptions align; unselected descriptions ride FAINT
        Some(InputPicker::Mode(pk)) => MODE_CHOICES
            .iter()
            .enumerate()
            .map(|(i, (mode, label, desc))| {
                let icon = crate::icons::mode_icon(*mode);
                if i == pk.selected {
                    list_row(
                        format!("{icon} {label:<15} {desc}"),
                        true,
                        picker_w,
                        Style::new(),
                    )
                } else {
                    TuiLine::from(vec![
                        Span::raw("  "),
                        Span::raw(format!("{icon} {label:<15} ")),
                        Span::styled(*desc, crate::palette::FAINT),
                    ])
                }
            })
            .collect(),
        // one row per level, the effort battery glyph leading
        Some(InputPicker::Thinking(tk)) => tk
            .choices
            .iter()
            .enumerate()
            .map(|(i, level)| {
                let label = effort_label(*level);
                let glyph = crate::icons::effort_glyph(label).unwrap_or(" ");
                list_row(
                    format!("{glyph} {label}"),
                    i == tk.selected,
                    picker_w,
                    Style::new(),
                )
            })
            .collect(),
        // textarea wrap: display rows fold at the draft width (the box
        // minus borders, padding, and the 2-col lead); the visible
        // window ends at the cursor's visual row and the terminal cursor
        // rides its offset inside that window
        None => {
            let draft_w = input_inner_w(full.width);
            let rows = wrap_rows(input, draft_w);
            let cur_row = visual_cursor_row(&rows, cursor);
            let vis = rows.len().min(6);
            let first = cur_row.saturating_sub(vis - 1).min(rows.len() - vis);
            let col: usize = rows[cur_row]
                .text
                .chars()
                .take(cursor.saturating_sub(rows[cur_row].start))
                .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
                .sum();
            cursor_at = Some(((cur_row - first) as u16, col as u16));
            let lead_style = Style::new().fg(if busy {
                crate::palette::FAINT
            } else {
                crate::palette::USER
            });
            let mut out: Vec<TuiLine> = rows[first..first + vis]
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let lead = if i == 0 {
                        Span::styled(format!("{} ", crate::icons::PROMPT), lead_style)
                    } else {
                        Span::raw("  ")
                    };
                    TuiLine::from(vec![lead, Span::raw(r.text.clone())])
                })
                .collect();
            if input.is_empty()
                && rsearch.is_none()
                && let Some(row) = out.first_mut()
            {
                let hint = if busy {
                    COMPOSER_PLACEHOLDER_BUSY
                } else {
                    COMPOSER_PLACEHOLDER
                };
                row.spans.push(Span::styled(
                    trunc_cols(hint, draft_w),
                    Style::new()
                        .fg(crate::palette::FAINT)
                        .add_modifier(Modifier::ITALIC),
                ));
            }
            out
        }
    };
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border_fg))
        .padding(ratatui::widgets::Padding::horizontal(1))
        .style(
            Style::new()
                .fg(crate::palette::FG)
                .bg(crate::palette::BG_PANEL),
        );
    if let Some(t) = title {
        block = block.title(t);
    }
    frame.render_widget(Paragraph::new(body).block(block), composer);
    // a picker borrows the composer: that chunk is its popup rect
    // (click-outside closes it without submitting)
    if picker.is_some() {
        popup_zone.set(Some(composer));
    }
    // the hardware cursor only rides the draft when it can actually
    // follow keystrokes (an open ask captures input into its dialog);
    // x = border + padding + the ❯ lead + the display column
    if ask.is_none()
        && let Some((row_off, col)) = cursor_at
    {
        frame.set_cursor_position((
            (composer.x + 2 + COMPOSER_LEAD + col).min(composer.right().saturating_sub(2)),
            composer.y + 1 + row_off.min(composer.height.saturating_sub(2)),
        ));
    }
    {
        let buf = frame.buffer_mut();
        // busy comet: a brand-colored light band sweeping the top border
        // (title text keeps its own color — only border glyphs light up)
        if busy && ask.is_none() && !overlay {
            let ms = busy_since.map_or(0, |t| now.saturating_duration_since(t).as_millis());
            let len = composer.width as usize;
            let span = len.saturating_sub(1).max(1) as f32;
            for i in 0..len {
                if let Some(cell) = buf.cell_mut((composer.x + i as u16, composer.y))
                    && matches!(cell.symbol(), "─" | "╭" | "╮")
                {
                    let hi = crate::fx::ramp(&crate::palette::BRAND, i as f32 / span);
                    let t = crate::fx::sweep(i, len, ms, COMET_STEP_MS, COMET_RADIUS);
                    cell.set_fg(crate::fx::lerp(busy_base, hi, t));
                }
            }
        }
        // bottom border: strip buttons left (capture mode clicks them;
        // native mode uses the printed shortcuts), contextual hints right
        // — unless a modal, the ask, or a popup holds focus, whose own
        // bottom border carries them
        if composer.height >= 2 && composer.width >= 4 {
            let y = composer.bottom() - 1;
            let limit = composer.right() - 2;
            let (zones, strip_end) = paint_strip(buf, composer.x + 2, limit, y);
            strip_zone.set(Some(zones));
            if ask.is_none() && modal.is_none() && popup.is_none() && path.is_none() {
                paint_hints(
                    buf,
                    strip_end,
                    limit,
                    y,
                    composer_hints(picker.is_some(), rsearch.is_some(), busy, mouse_captured),
                );
            }
        }
    }

    // ── status line: mode pill + model/effort/badges left, context
    // gauge + cost right ──
    {
        let left = status_left(meters);
        let right = status_right(meters);
        let lw: usize = left.iter().map(|s| s.content.width()).sum();
        let rw: usize = right.iter().map(|s| s.content.width()).sum();
        // one col of air at each end
        let pad = (status.width as usize).saturating_sub(lw + rw + 2).max(1);
        let mut bar: Vec<Span<'static>> = vec![Span::raw(" ")];
        bar.extend(left);
        bar.push(Span::raw(" ".repeat(pad)));
        bar.extend(right);
        // the pill closes with its own trailing space; only pad a row
        // that ends on the cost
        if meters.mode.is_empty() {
            bar.push(Span::raw(" "));
        }
        frame.render_widget(
            Paragraph::new(TuiLine::from(bar)).style(
                Style::new()
                    .fg(crate::palette::FG)
                    .bg(crate::palette::BG_PANEL),
            ),
            status,
        );
    }

    // modals and the ask own the eye: the transcript recedes
    if modal.is_some() || ask.is_some() {
        dim_backdrop(frame.buffer_mut(), chunks[2]);
    }

    // ── slash autocomplete popup (above the composer) ─────────────
    if let Some(popup) = popup {
        // borders + up to CAP items (hints ride the bottom border)
        const CAP: usize = 7;
        let rows = popup.items.len().min(CAP) as u16 + 2;
        let rect = Rect {
            x: composer.x,
            y: composer.y.saturating_sub(rows),
            width: composer.width.min(64),
            height: rows,
        };
        popup_zone.set(Some(rect));
        // wipe the covered cells: the paragraph paints only its text
        frame.render_widget(ratatui::widgets::Clear, rect);
        let inner_w = rect.width.saturating_sub(4) as usize; // borders + padding
        // lead 2 + name 14 + a space, the description takes the rest
        let desc_room = inner_w.saturating_sub(17);
        // window the list so the selection always stays visible: the
        // filter can match far more commands than the shown rows
        let offset = popup
            .selected
            .saturating_sub(CAP - 1)
            .min(popup.items.len().saturating_sub(CAP));
        let mut text = Vec::new();
        for (i, (name, desc)) in popup.items.iter().skip(offset).take(CAP).enumerate() {
            let desc = trunc_cols(desc, desc_room);
            if i + offset == popup.selected {
                text.push(TuiLine::from(vec![
                    Span::styled(
                        format!("{} ", crate::icons::RAIL),
                        Style::new()
                            .fg(crate::palette::ACCENT)
                            .bg(crate::palette::SEL_BG),
                    ),
                    Span::styled(format!("{name:<14} "), selection_style()),
                    Span::styled(
                        pad_to_width(desc, desc_room),
                        Style::new()
                            .fg(crate::palette::FAINT)
                            .bg(crate::palette::SEL_BG),
                    ),
                ]));
            } else {
                text.push(TuiLine::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{name:<14} "), crate::palette::ACCENT_STYLE),
                    Span::styled(desc, crate::palette::FAINT),
                ]));
            }
        }
        frame.render_widget(
            Paragraph::new(text)
                .block(modal_frame("commands").padding(ratatui::widgets::Padding::horizontal(1))),
            rect,
        );
        let buf = frame.buffer_mut();
        drop_shadow(buf, rect);
        if rect.height >= 2 {
            paint_hints(
                buf,
                rect.x + 2,
                rect.right().saturating_sub(2),
                rect.bottom() - 1,
                POPUP_HINTS,
            );
        }
    }

    // ── path completion popup (above the composer) ─────────────────
    if let Some(path) = path {
        const CAP: usize = 7;
        let rows = path.entries.len().min(CAP) as u16 + 2;
        let rect = Rect {
            x: composer.x,
            y: composer.y.saturating_sub(rows),
            width: composer.width.min(64),
            height: rows,
        };
        popup_zone.set(Some(rect));
        frame.render_widget(ratatui::widgets::Clear, rect);
        let inner_w = rect.width.saturating_sub(4) as usize; // borders + padding
        // window the list so the selection always stays visible
        let offset = path
            .selected
            .saturating_sub(CAP - 1)
            .min(path.entries.len().saturating_sub(CAP));
        let mut text = Vec::new();
        for (i, (name, is_dir)) in path.entries.iter().skip(offset).take(CAP).enumerate() {
            if i + offset == path.selected {
                let shown = if *is_dir {
                    format!("{name}/")
                } else {
                    name.clone()
                };
                text.push(list_row(shown, true, inner_w, Style::new()));
            } else if *is_dir {
                text.push(TuiLine::from(vec![
                    Span::raw("  "),
                    Span::raw(name.clone()),
                    Span::styled("/", crate::palette::META),
                ]));
            } else {
                text.push(TuiLine::from(vec![
                    Span::raw("  "),
                    Span::raw(name.clone()),
                ]));
            }
        }
        frame.render_widget(
            Paragraph::new(text).block(
                modal_frame(if path.mentions { "files" } else { "path" })
                    .padding(ratatui::widgets::Padding::horizontal(1)),
            ),
            rect,
        );
        let buf = frame.buffer_mut();
        drop_shadow(buf, rect);
        if rect.height >= 2 {
            paint_hints(
                buf,
                rect.x + 2,
                rect.right().saturating_sub(2),
                rect.bottom() - 1,
                PATH_HINTS,
            );
        }
    }

    // ── modals: each arm sizes its box and builds its body; the frame,
    // shadow, and bottom-border hints are shared ──
    if let Some(open) = modal {
        let (fw, fh) = (full.width, full.height);
        let meta = Style::new().fg(crate::palette::META);
        // borders + horizontal padding
        let inner = |w: u16| w.saturating_sub(4) as usize;
        // the window offset that keeps `selected` inside `cap` rows
        let window_of = |selected: usize, len: usize, cap: usize| {
            selected
                .saturating_sub(cap.saturating_sub(1))
                .min(len.saturating_sub(cap))
        };
        let (width, height, title, text): (u16, u16, String, Vec<TuiLine<'static>>) = match open {
            Modal::Session(picker) => {
                let rows = picker.rows();
                // borders(2) + vertical padding(2) + filter row(1)
                let height = (rows.len() as u16 + 5).min(22);
                let width = 68.min(fw);
                let cap = (height as usize).saturating_sub(5);
                let offset = window_of(picker.selected, rows.len(), cap);
                let mut text = vec![filter_row(&picker.filter)];
                for (i, (label, detail)) in rows.iter().skip(offset).take(cap).enumerate() {
                    text.push(list_row(
                        format!("{label}  —  {detail}"),
                        i + offset == picker.selected,
                        inner(width),
                        Style::new(),
                    ));
                }
                (width, height, modal_title("sessions"), text)
            }
            Modal::Key(prompt) => {
                let height = 10u16.min(fh.saturating_sub(2));
                let width = 68.min(fw);
                let mut text = vec![TuiLine::styled("api key", crate::palette::ACCENT_BOLD)];
                text.push(TuiLine::from(vec![
                    Span::styled("provider  ", meta),
                    Span::raw(prompt.provider.clone()),
                ]));
                text.push(TuiLine::from(vec![
                    Span::styled("key var   ", meta),
                    Span::raw(prompt.env_var.clone()),
                ]));
                if !prompt.doc_url.is_empty() {
                    // "get one   " (10 cols) + doc must fit the padded inner width
                    let doc: String = prompt
                        .doc_url
                        .chars()
                        .take(width.saturating_sub(14) as usize)
                        .collect();
                    text.push(TuiLine::from(vec![
                        Span::styled("get one   ", meta),
                        Span::styled(doc, crate::palette::TOOL),
                    ]));
                }
                text.push(TuiLine::default());
                let masked = "•".repeat(prompt.input.chars().count());
                text.push(TuiLine::from(vec![
                    Span::styled("value     ", meta),
                    Span::raw(masked),
                    Span::styled(crate::icons::RAIL, crate::palette::ACCENT_STYLE),
                ]));
                (width, height, modal_title("api key"), text)
            }
            Modal::Help => {
                let height = 34u16.min(fh.saturating_sub(2));
                let width = 80.min(fw);
                (
                    width,
                    height,
                    modal_title("help"),
                    help_modal_rows(inner(width)),
                )
            }
            Modal::Model(picker) => {
                let rows = picker.rows();
                // borders(2) + vertical padding(2) + filter row(1)
                let height = (rows.len() as u16 + 5).min(20);
                // 70 wide: the 66-col body fits a model row plus its lead
                let width = 70.min(fw);
                let cap = (height as usize).saturating_sub(5);
                let mut text = vec![filter_row(&picker.filter)];
                for (i, m) in rows.iter().enumerate().take(cap) {
                    let ctx = if m.context > 0 {
                        format!("{}k", m.context / 1000)
                    } else {
                        "?".to_string()
                    };
                    let key = if m.key_env.is_empty() {
                        String::new()
                    } else if m.key_set {
                        " ✓".to_string()
                    } else {
                        " ✗".to_string()
                    };
                    text.push(list_row(
                        model_row(m, &ctx, &key),
                        i == picker.selected,
                        inner(width),
                        Style::new(),
                    ));
                }
                if rows.is_empty() {
                    let note = if picker.filter.trim().is_empty() {
                        "no configured providers — /provider to connect".to_string()
                    } else {
                        format!("enter sets '{}' as a custom selector", picker.filter)
                    };
                    text.push(TuiLine::styled(note, crate::palette::WARN));
                }
                let title = picker
                    .vendor
                    .as_ref()
                    .map_or("model".to_string(), |v| format!("model · {v}"));
                (width, height, title, text)
            }
            Modal::Provider(picker) => {
                let rows = picker.rows();
                // borders(2) + vertical padding(2) + filter row(1)
                let height = (rows.len() as u16 + 5).min(20);
                let width = 68.min(fw);
                let cap = (height as usize).saturating_sub(5);
                let mut text = vec![filter_row(&picker.filter)];
                for (i, (p, detail)) in rows.iter().take(cap).enumerate() {
                    let name: String = p.name.chars().take(16).collect();
                    let detail: String = detail.chars().take(44).collect();
                    if i == picker.selected {
                        text.push(list_row(
                            format!("{name:<16} {detail}"),
                            true,
                            inner(width),
                            Style::new(),
                        ));
                    } else {
                        let key = if p.key_set || p.env_var.is_empty() {
                            crate::palette::OK
                        } else {
                            crate::palette::ERR
                        };
                        text.push(TuiLine::from(vec![
                            Span::raw("  "),
                            Span::raw(format!("{name:<16} ")),
                            Span::styled(detail, key),
                        ]));
                    }
                }
                if rows.is_empty() {
                    text.push(TuiLine::styled("  (no providers match)", meta));
                }
                (width, height, modal_title("providers"), text)
            }
            Modal::Settings(panel) => {
                // borders(2) + vertical padding(2) + the rows below
                let height = ((SettingsPanel::ROWS + panel.providers.len() + 7) as u16)
                    .min(fh.saturating_sub(2));
                let width = 80.min(fw);
                let inner_w = inner(width);
                // mode and effort render as their icon+word/battery forms
                let mode_str = format!(
                    "{} {}",
                    crate::icons::mode_icon(panel.mode),
                    mode_label(panel.mode)
                );
                let effort_str = panel
                    .effort
                    .as_ref()
                    .map(|e| effort_segment(&format!("{e:?}").to_lowercase()))
                    .unwrap_or_else(|| "(default)".to_string());
                // while editing, the row shows a cursor block in WARN so
                // the mode change is unmistakable (nothing else moves)
                let model_line = match &panel.edit {
                    Some(buf) => list_row(
                        format!("model ✎  {buf}▌"),
                        false,
                        inner_w,
                        Style::new()
                            .fg(crate::palette::WARN)
                            .add_modifier(Modifier::BOLD),
                    ),
                    None => list_row(
                        format!("model    {}", panel.model),
                        panel.selected == 0,
                        inner_w,
                        meta,
                    ),
                };
                let mut text = vec![
                    model_line,
                    list_row(
                        format!("mode     {mode_str}"),
                        panel.selected == 1,
                        inner_w,
                        meta,
                    ),
                    list_row(
                        format!("effort   {effort_str}"),
                        panel.selected == 2,
                        inner_w,
                        meta,
                    ),
                    TuiLine::styled(format!("  config: {}", panel.config_path), meta),
                    TuiLine::raw("  providers:"),
                ];
                let url_room = (width.saturating_sub(42)) as usize;
                // keyed providers first; cap the list so the panel stays
                // readable with a large catalog behind it
                let mut providers: Vec<&ProviderInfo> = panel.providers.iter().collect();
                providers.sort_by_key(|p| !p.key_set && !p.env_var.is_empty());
                const CAP: usize = 18;
                let (shown, rest) = providers.split_at(CAP.min(providers.len()));
                for p in shown {
                    let key = if p.env_var.is_empty() {
                        "(keyless)".to_string()
                    } else if p.key_set {
                        format!("{} ✓", p.env_var)
                    } else {
                        format!("{} ✗", p.env_var)
                    };
                    let url: String = p.base_url.chars().take(url_room).collect();
                    text.push(TuiLine::from(vec![
                        Span::styled(format!("    {:<12}", p.name), meta),
                        Span::styled(
                            key,
                            if p.key_set || p.env_var.is_empty() {
                                crate::palette::OK
                            } else {
                                crate::palette::ERR
                            },
                        ),
                        Span::styled(format!("  {url}"), crate::palette::FAINT),
                    ]));
                }
                if !rest.is_empty() {
                    text.push(TuiLine::styled(format!("    … +{} more", rest.len()), meta));
                }
                (width, height, modal_title("settings"), text)
            }
            Modal::Spills { items, selected } => {
                // borders(2) + vertical padding(2) + list rows
                let height = (items.len() as u16 + 5).clamp(7, 21);
                let width = 68.min(fw);
                let mut text = Vec::new();
                if items.is_empty() {
                    text.push(TuiLine::styled(
                        "(no spill files yet — full tool output lands here)",
                        meta,
                    ));
                }
                let cap = (height as usize).saturating_sub(5);
                let offset = window_of(*selected, items.len(), cap);
                for (i, path) in items.iter().skip(offset).take(cap).enumerate() {
                    text.push(list_row(
                        path.clone(),
                        i + offset == *selected,
                        inner(width),
                        Style::new(),
                    ));
                }
                (width, height, modal_title("spills"), text)
            }
            Modal::Prompts { items, selected } => {
                let height = (items.len() as u16 + 5).clamp(7, 21);
                let width = 68.min(fw);
                let mut text = Vec::new();
                if items.is_empty() {
                    text.push(TuiLine::styled("(no MCP prompts advertised)", meta));
                }
                let cap = (height as usize).saturating_sub(5);
                let offset = window_of(*selected, items.len(), cap);
                for (i, row) in items.iter().skip(offset).take(cap).enumerate() {
                    text.push(list_row(
                        row.clone(),
                        i + offset == *selected,
                        inner(width),
                        Style::new(),
                    ));
                }
                (width, height, modal_title("prompts"), text)
            }
            Modal::Tasks { entries, selected } => {
                let height = (entries.len() as u16 + 5).clamp(7, 21);
                let width = 90.min(fw);
                let mut text = Vec::new();
                if entries.is_empty() {
                    text.push(TuiLine::styled("(no background tasks or jobs)", meta));
                }
                let cap = (height as usize).saturating_sub(5);
                let offset = window_of(*selected, entries.len(), cap);
                for (i, (_, row)) in entries.iter().enumerate().skip(offset).take(cap) {
                    text.push(list_row(
                        row.clone(),
                        i == *selected,
                        inner(width),
                        Style::new(),
                    ));
                }
                let title = format!("{} tasks", crate::icons::tool_icon("tasks"));
                (width, height, title, text)
            }
            Modal::TaskDetail { id, text, scroll } => {
                let lines: Vec<&str> = text.lines().collect();
                // clamp to the transcript pane: a taller box would bleed
                // over the composer / status line
                let height = (lines.len() as u16 + 5).clamp(7, 30).min(modal_area.height);
                let width = 90.min(fw);
                let visible = (height as usize).saturating_sub(5);
                let (first, _) = window_range(lines.len(), visible, *scroll);
                let body = lines[first..(first + visible).min(lines.len())]
                    .iter()
                    .map(|line| TuiLine::raw((*line).to_string()))
                    .collect();
                let title = format!("{} task t-{id}", crate::icons::tool_icon("tasks"));
                (width, height, title, body)
            }
            Modal::Debug { rows, scroll } => {
                // clamp to the transcript pane: a taller box would bleed
                // over the composer / status line
                let height = (rows.len() as u16 + 5).clamp(7, 30).min(modal_area.height);
                let width = 90.min(fw);
                let visible = (height as usize).saturating_sub(5);
                let (first, _) = window_range(rows.len(), visible, *scroll);
                let body = rows[first..(first + visible).min(rows.len())]
                    .iter()
                    .map(|row| TuiLine::styled(row.clone(), meta))
                    .collect();
                let title = format!("{} debug sessions", crate::icons::tool_icon("debug"));
                (width, height, title, body)
            }
            Modal::Rewind { items, selected } => {
                let height = (items.len() as u16 + 5).clamp(6, 22);
                let width = 76.min(fw);
                let mut text = vec![TuiLine::styled(
                    "pick a message — ⏎/r rewind here · e edit & resend · esc close",
                    meta,
                )];
                let cap = (height as usize).saturating_sub(5);
                for (i, (turns, prompt)) in items.iter().enumerate().take(cap.max(1)) {
                    text.push(list_row(
                        format!("[−{turns}] {prompt}"),
                        i == *selected,
                        inner(width),
                        meta,
                    ));
                }
                (width, height, modal_title("rewind"), text)
            }
            Modal::Memory {
                rows,
                inbox,
                selected,
            } => {
                let extra = if inbox.is_empty() { 0 } else { inbox.len() + 2 };
                let height = (rows.len() as u16 + extra as u16 + 4).clamp(6, 24);
                let width = 80.min(fw);
                let mut text = Vec::new();
                let mut budget = (height as usize).saturating_sub(4);
                for row in rows.iter() {
                    if budget == 0 {
                        break;
                    }
                    budget -= 1;
                    if row.starts_with('▸') {
                        text.push(TuiLine::styled(row.clone(), crate::palette::ACCENT_BOLD));
                    } else {
                        text.push(TuiLine::raw(row.clone()));
                    }
                }
                if !inbox.is_empty() && budget >= 2 {
                    budget -= 2;
                    text.push(TuiLine::default());
                    text.push(TuiLine::styled(
                        "staged memories — ⏎ accept→MEMORY.md · u→user · d discard",
                        meta,
                    ));
                    for (i, line) in inbox.iter().enumerate() {
                        if budget == 0 {
                            break;
                        }
                        budget -= 1;
                        text.push(list_row(line.clone(), i == *selected, inner(width), meta));
                    }
                }
                (width, height, modal_title("memory"), text)
            }
            Modal::Features { entries, selected } => {
                let height = (entries.len() as u16 + 4).clamp(6, 24);
                let width = 76.min(fw);
                let cap = (height as usize).saturating_sub(4);
                // keep the selection inside the window
                let first = (*selected + 1).saturating_sub(cap);
                let mut text = Vec::new();
                for (i, row) in entries.iter().enumerate().skip(first).take(cap) {
                    if row.header {
                        text.push(TuiLine::styled(
                            row.label.clone(),
                            crate::palette::ACCENT_BOLD,
                        ));
                        continue;
                    }
                    let glyph = if row.on { "\u{25cf}" } else { "\u{25cb}" };
                    let shown = if row.spec == row.label || row.spec == "sandbox" {
                        format!("{glyph} {}", row.label)
                    } else {
                        format!("{glyph} {} ({})", row.label, row.spec)
                    };
                    text.push(list_row(shown, i == *selected, inner(width), Style::new()));
                }
                let more = entries.len().saturating_sub(first + cap);
                if more > 0 {
                    text.push(TuiLine::styled(format!("  … +{more} more"), meta));
                }
                (width, height, modal_title("features"), text)
            }
            Modal::Todos { rows } | Modal::Skills { rows } | Modal::Info { rows } => {
                let title = match open {
                    Modal::Todos { .. } => "todos",
                    Modal::Skills { .. } => "skills · agents · mcp",
                    _ => "info",
                };
                let height = (rows.len() as u16 + 4).clamp(6, 24);
                let width = 80.min(fw);
                let cap = (height as usize).saturating_sub(4);
                let mut text = Vec::new();
                for row in rows.iter().take(cap) {
                    let plain: String = row.spans.iter().map(|s| s.content.clone()).collect();
                    // section headers keep their accent; everything else
                    // renders as built
                    if plain == "skills" || plain == "agents" || plain == "mcp" {
                        text.push(TuiLine::styled(plain, crate::palette::ACCENT_BOLD));
                    } else {
                        text.push(TuiLine::from(
                            row.spans
                                .iter()
                                .map(|s| Span::styled(s.content.clone().into_owned(), s.style))
                                .collect::<Vec<_>>(),
                        ));
                    }
                }
                let more = rows.len().saturating_sub(cap);
                if more > 0 {
                    text.push(TuiLine::styled(format!("… +{more} more"), meta));
                }
                (width, height, modal_title(title), text)
            }
            Modal::Usage { rows } | Modal::Context { rows } => {
                let title = if matches!(open, Modal::Usage { .. }) {
                    "usage"
                } else {
                    "context"
                };
                let height = (rows.len() as u16 + 4).clamp(6, 24);
                let width = 80.min(fw);
                let cap = (height as usize).saturating_sub(4);
                let text = rows
                    .iter()
                    .take(cap)
                    .map(|row| {
                        if row.starts_with('▸') {
                            TuiLine::styled(row.clone(), crate::palette::ACCENT_BOLD)
                        } else {
                            TuiLine::raw(row.clone())
                        }
                    })
                    .collect();
                (width, height, modal_title(title), text)
            }
            Modal::Tree {
                items,
                actions,
                selected,
            } => {
                let height = (items.len() as u16 + 5)
                    .clamp(9, 34)
                    .min(fh.saturating_sub(2).max(9));
                let width = 110.min(fw);
                let mut text = Vec::new();
                if items.is_empty() {
                    text.push(TuiLine::styled("(no related sessions)", meta));
                }
                let cap = (height as usize).saturating_sub(5);
                // window the list so the selection always stays visible:
                // long trees scroll instead of hiding the selected row
                let offset = window_of(*selected, items.len(), cap);
                for (i, row) in items.iter().skip(offset).take(cap).enumerate() {
                    // list_row truncates at the ACTUAL inner width: a
                    // narrow terminal renders a smaller box than the
                    // build assumption, and wrapping must not come back
                    let style = if matches!(actions.get(i + offset), Some(TreeAction::Header)) {
                        meta
                    } else {
                        Style::new()
                    };
                    text.push(list_row(
                        row.clone(),
                        i + offset == *selected,
                        inner(width),
                        style,
                    ));
                }
                (width, height, modal_title("tree"), text)
            }
        };
        // the key prompt repaints the whole frame first: switching here
        // from the model picker must leave no stale pixels outside it
        if matches!(open, Modal::Key(_)) {
            frame.render_widget(Block::new().style(crate::palette::CANVAS), full);
        }
        let rect = centered(width, height, modal_area);
        modal_zone.set(Some(rect));
        // wipe the covered cells first: the paragraph only paints its
        // own text, and the transcript would bleed through
        frame.render_widget(ratatui::widgets::Clear, rect);
        frame.render_widget(
            Paragraph::new(text)
                .block(modal_frame(title))
                .wrap(Wrap { trim: false }),
            rect,
        );
        let buf = frame.buffer_mut();
        drop_shadow(buf, rect);
        if rect.height >= 2 {
            paint_hints(
                buf,
                rect.x + 2,
                rect.right().saturating_sub(2),
                rect.bottom() - 1,
                modal_hints(open),
            );
        }
    }

    // ── permission ask: a real dialog, layered above everything ────
    // the safety-critical surface: WARN frame and title, the question
    // in strong gold, budgeted diff detail, numbered options with the
    // selection pointer. No ✕ chip and no published zone — a click can
    // never answer it.
    if let Some(ask) = ask {
        let width = 80.min(full.width);
        let body = ask_modal_body(ask, width.saturating_sub(4) as usize);
        let height = (body.len() as u16 + 4).clamp(5, modal_area.height.max(5));
        let rect = centered(width, height, modal_area);
        frame.render_widget(ratatui::widgets::Clear, rect);
        let title = TuiLine::styled(
            format!(" {} permission ", crate::icons::BRAND),
            Style::new()
                .fg(crate::palette::WARN)
                .add_modifier(Modifier::BOLD),
        );
        frame.render_widget(
            Paragraph::new(body)
                .block(frame_block(title, crate::palette::WARN))
                .wrap(Wrap { trim: false }),
            rect,
        );
        let buf = frame.buffer_mut();
        drop_shadow(buf, rect);
        if rect.height >= 2 {
            paint_hints(
                buf,
                rect.x + 2,
                rect.right().saturating_sub(2),
                rect.bottom() - 1,
                ASK_HINTS,
            );
        }
    }

    // ── toast: transient feedback pill, bottom-center of the pane ──
    if let Some(msg) = toast {
        let room = (tx_area.width as usize).saturating_sub(4);
        if room >= 4 && tx_area.height > 0 {
            let body = trunc_cols(msg, room - 3);
            let pill = TuiLine::from(vec![
                Span::styled(
                    crate::icons::RAIL,
                    Style::new()
                        .fg(crate::palette::ACCENT)
                        .bg(crate::palette::BG_SURFACE),
                ),
                Span::styled(
                    format!(" {body} "),
                    Style::new()
                        .fg(crate::palette::STRONG)
                        .bg(crate::palette::BG_SURFACE),
                ),
            ]);
            let w = pill.width() as u16;
            put_line(
                frame.buffer_mut(),
                tx_area.x + tx_area.width.saturating_sub(w) / 2,
                tx_area.bottom() - 1,
                &pill,
            );
        }
    }

    // hover affordance (last, once every zone is published): the
    // hovered entry's rows take the hover lift edge to edge across the
    // content rect; chips/arrows/strip buttons/rail thumb glow gold.
    // The selection patch above runs first and keeps its band.
    if let Some(h) = hover {
        let buf = frame.buffer_mut();
        match h {
            Hover::Entry(entry) => {
                if let Some((first, len)) = transcript.entry_row_span(entry) {
                    for trow in first.max(start)..first + len {
                        if trow >= start + content_rect.height as usize {
                            break;
                        }
                        // a thinking block's canvas spacers (the blank
                        // rows before and after it) belong to the entry
                        // for click mapping, but lifting them would
                        // highlight air: only rows with content glow
                        let Some(row) = transcript.row(trow) else {
                            continue;
                        };
                        if row.spans.is_empty() {
                            continue;
                        }
                        let y = content_rect.y + (trow - start) as u16;
                        for x in content_rect.x..content_rect.right() {
                            if let Some(cell) = buf.cell_mut((x, y))
                                && cell.bg != crate::palette::BG_SELECT
                            {
                                cell.set_bg(crate::palette::BG_HOVER);
                            }
                        }
                    }
                }
            }
            Hover::Chip => {
                let zone = modal_zone.get().or_else(|| popup_zone.get());
                if let Some(rect) = zone {
                    for x in rect.x + rect.width.saturating_sub(3)..rect.x + rect.width {
                        if let Some(cell) = buf.cell_mut((x, rect.y)) {
                            cell.set_fg(crate::palette::STRONG);
                        }
                    }
                }
            }
            Hover::Arrow(up) => {
                if let Some(z) = title_arrows.get() {
                    let r = if up { z.up } else { z.down };
                    for x in r.x..r.x + r.width {
                        if let Some(cell) = buf.cell_mut((x, r.y)) {
                            cell.set_fg(crate::palette::STRONG);
                        }
                    }
                }
            }
            Hover::Strip(button) => {
                if let Some(z) = strip_zone.get() {
                    let r = match button {
                        StripButton::Todos => z.todos,
                        StripButton::Skills => z.skills,
                        StripButton::Info => z.info,
                    };
                    for x in r.x..r.x + r.width {
                        if let Some(cell) = buf.cell_mut((x, r.y)) {
                            cell.set_fg(crate::palette::STRONG);
                        }
                    }
                }
            }
            Hover::Rail => {
                if let Some((rect, _, _, thumb_pos, thumb_len)) = rail_zone.get() {
                    for i in thumb_pos..thumb_pos + thumb_len {
                        for x in rect.x..rect.x + rect.width {
                            if let Some(cell) = buf.cell_mut((x, rect.y + i as u16)) {
                                cell.set_fg(crate::palette::STRONG);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// User turn: a full-width `BG_USER` band — the `▌` rail (`ACCENT`) in
/// column 0, a space, then the text in bold `FG`, word-wrapped at W-2
/// with a two-column hanging indent (every row rides the rail), each
/// row padded to exactly W. No pad rows: the air around the band is
/// [`separate_before`]'s job. An empty message still draws one rail
/// row, so every user entry owns at least one row.
fn push_block(out: &mut Vec<ratatui::text::Line<'static>>, text: &str, width: u16) {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line as TuiLine, Span};
    use unicode_width::UnicodeWidthStr;
    let w = width as usize;
    let bg = crate::palette::BG_USER;
    let rail = Style::new().fg(crate::palette::USER).bg(bg);
    let body = Style::new()
        .fg(crate::palette::FG)
        .bg(bg)
        .add_modifier(Modifier::BOLD);
    let room = w.saturating_sub(2).max(1);
    let lines: Vec<&str> = if text.is_empty() {
        vec![""]
    } else {
        text.lines().collect()
    };
    for raw in lines {
        for row in wrap_rows(raw, room) {
            let seg = trunc_cols(&row.text, room);
            let pad = w.saturating_sub(2 + seg.width());
            let mut spans = vec![
                Span::styled(crate::icons::RAIL, rail),
                Span::styled(" ", body),
                Span::styled(seg, body),
            ];
            if pad > 0 {
                spans.push(Span::styled(" ".repeat(pad), body));
            }
            out.push(TuiLine::from(spans));
        }
    }
}

/// Glyph-gutter rows for ambient roles (info/warn/err/report, the ask
/// question): `{prefix}{text}` word-wrapped at the width, continuation
/// rows hanging under the text by the prefix's display width. One
/// line-level style tints glyph and text alike. No background, no
/// full-width padding, no built-in spacing — air between blocks is
/// [`separate_before`]'s job alone.
fn push_gutter(
    out: &mut Vec<ratatui::text::Line<'static>>,
    text: &str,
    width: u16,
    prefix: &str,
    style: ratatui::style::Style,
) {
    use ratatui::text::Line as TuiLine;
    use unicode_width::UnicodeWidthStr;
    let indent = " ".repeat(prefix.width());
    // max(1) guarantees forward progress even at degenerate widths
    // (0/1 columns) where the lead alone overflows
    let room = (width as usize).saturating_sub(prefix.width()).max(1);
    let mut first = true;
    for raw in text.lines() {
        for row in wrap_rows(raw, room) {
            let lead = if first { prefix } else { indent.as_str() };
            first = false;
            out.push(TuiLine::styled(format!("{lead}{}", row.text), style));
        }
    }
}

/// `/tree` modal rows: the current session's family as a real branch
/// tree — ancestors above, descendants below (children chronological,
/// drawn with `├─`/`└─` connectors), the current session marked `▸` —
/// with every fork edge labeled by the prompt it branched at. Below
/// the tree, a branch section lists `history` (the session's user
/// inputs, oldest first, annotated with the forks already growing
/// there). `records_of` loads a strand's records by id (the fork point
/// comes from the exact shared record prefix — fork copies keep the
/// parent's record ids). Each row truncates to `width` columns.
/// Returns (rows, actions) in the same order.
fn tree_modal_rows<F>(
    all: &[ka_strand::StrandSummary],
    current: Option<&str>,
    history: &[String],
    width: usize,
    records_of: F,
) -> (Vec<String>, Vec<TreeAction>)
where
    F: Fn(&str) -> Vec<ka_strand::Record>,
{
    let parent_of: std::collections::HashMap<&str, &str> = all
        .iter()
        .filter_map(|s| s.parent.as_deref().map(|p| (s.id.as_str(), p)))
        .collect();
    // ancestor chain of `current`, collected upwards then flipped so
    // the root leads (cycle-guarded against corrupt parent links)
    let mut chain: Vec<String> = Vec::new();
    let mut walk = current.map(|c| c.to_string());
    while let Some(id) = walk {
        if chain.contains(&id) {
            break;
        }
        chain.push(id.clone());
        walk = parent_of.get(id.as_str()).map(|p| p.to_string());
    }
    chain.reverse();
    // children of every family member (breadth-first from the chain),
    // sorted chronological so branches grow downward
    let mut children: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    // discovery starts empty: chain members must enter the map too,
    // or the depth-first walk below cannot reach them
    let mut seen: Vec<String> = Vec::new();
    let mut level = chain.clone();
    while !level.is_empty() {
        let mut next = Vec::new();
        for s in all {
            if let Some(p) = &s.parent {
                if level.contains(p) && !seen.contains(&s.id) {
                    seen.push(s.id.clone());
                    children.entry(p.clone()).or_default().push(s.id.clone());
                    next.push(s.id.clone());
                }
            }
        }
        level = next;
    }
    for kids in children.values_mut() {
        kids.sort_by_key(|id| {
            all.iter()
                .find(|s| &s.id == id)
                .map_or(String::new(), |s| s.ts.clone())
        });
    }
    // records cache: family edges read each strand at most once
    let mut cache: std::collections::HashMap<String, Vec<ka_strand::Record>> =
        std::collections::HashMap::new();
    let mut records = |id: &str| -> Vec<ka_strand::Record> {
        if let Some(r) = cache.get(id) {
            return r.clone();
        }
        let r = records_of(id);
        cache.insert(id.to_string(), r.clone());
        r
    };
    // how many forks already branch at each of the current session's
    // prompts — matches `history` entries by exact prompt text
    let mut branch_points: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    if let Some(cur) = current {
        if let Some(kids) = children.get(cur) {
            let cur_records = records(cur);
            for kid in kids {
                if let Some(prompt) = dropped_prompt(&cur_records, &records(kid)) {
                    if !prompt.is_empty() {
                        *branch_points.entry(prompt).or_insert(0) += 1;
                    }
                }
            }
        }
    }
    // depth-first from the family root, children chronological: the
    // classic tree reading order
    let mut ids: Vec<String> = Vec::new();
    if let Some(root) = chain.first() {
        let mut stack = vec![root.clone()];
        let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
        while let Some(id) = stack.pop() {
            if !visited.insert(id.clone()) {
                continue;
            }
            ids.push(id.clone());
            if let Some(kids) = children.get(&id) {
                for kid in kids.iter().rev() {
                    stack.push(kid.clone());
                }
            }
        }
    }
    let mut items = Vec::with_capacity(ids.len() + history.len() + 1);
    let mut actions = Vec::with_capacity(ids.len() + history.len() + 1);
    // fork-edge labels read every family strand in full; past this
    // family size the reads outweigh the labels, so edges go bare
    const LABEL_FAMILY_CAP: usize = 24;
    let label_edges = ids.len() <= LABEL_FAMILY_CAP;
    for id in &ids {
        let Some(s) = all.iter().find(|s| &s.id == id) else {
            continue;
        };
        let date = s.ts.get(..10).unwrap_or(&s.ts).to_string();
        let marker = if Some(id.as_str()) == current {
            "▸ "
        } else {
            ""
        };
        let fork = if label_edges {
            parent_of
                .get(id.as_str())
                .and_then(|p| fork_label(&records(p), &records(id), width))
                .map(|l| format!("  {l}"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let row = format!(
            "{}{marker}{} · {date} · {} msgs{fork}",
            tree_prefix(id, &parent_of, &children),
            s.title,
            s.messages
        );
        items.push(trunc_cols(&row, width));
        actions.push(TreeAction::Attach(id.clone()));
    }
    if !history.is_empty() {
        items.push("─ branch from a past input ─".to_string());
        actions.push(TreeAction::Header);
        // oldest first: a row's fork depth is its distance from the
        // newest input (1 = the latest)
        for (i, prompt) in history.iter().enumerate() {
            let forks = branch_points.get(prompt).copied().unwrap_or(0);
            let extra = if forks > 0 {
                format!("  ↳ {forks} fork{}", if forks == 1 { "" } else { "s" })
            } else {
                String::new()
            };
            items.push(trunc_cols(
                &format!("⑂ {}{extra}", first_line_of(prompt)),
                width,
            ));
            actions.push(TreeAction::Fork {
                turns: (history.len() - i) as u32,
                prompt: prompt.clone(),
            });
        }
    }
    (items, actions)
}

/// First non-empty line of a possibly multi-line prompt, so a row
/// never wraps.
fn first_line_of(s: &str) -> &str {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or("")
}

/// The `├─`/`└─` prefix for a family row: a vertical segment per
/// ancestor (blank when that ancestor closed its branch) plus the
/// node's own connector.
fn tree_prefix(
    id: &str,
    parent_of: &std::collections::HashMap<&str, &str>,
    children: &std::collections::HashMap<String, Vec<String>>,
) -> String {
    let is_last = |node: &str| -> bool {
        parent_of
            .get(node)
            .and_then(|p| children.get(*p))
            .is_none_or(|kids| kids.last().is_some_and(|k| k == node))
    };
    // path root → … → id
    let mut path = vec![id];
    while let Some(p) = parent_of.get(*path.last().unwrap_or(&id)) {
        if path.contains(p) {
            break;
        }
        path.push(p);
    }
    path.reverse();
    let mut prefix = String::new();
    for ancestor in path.iter().take(path.len().saturating_sub(1)) {
        // the family root has no parent: nothing runs above it
        if !parent_of.contains_key(*ancestor) {
            continue;
        }
        prefix.push_str(if is_last(ancestor) { "   " } else { "│  " });
    }
    if path.len() > 1 {
        prefix.push_str(if is_last(id) { "└─ " } else { "├─ " });
    }
    prefix
}

/// The prompt a child strand branched at: the first parent user
/// message after the longest shared record prefix. `Some("")` = full
/// copy (nothing dropped); None = parent unreadable. Headers key as a
/// constant — every strand's own Header differs by design.
fn dropped_prompt(parent: &[ka_strand::Record], child: &[ka_strand::Record]) -> Option<String> {
    if parent.is_empty() || child.is_empty() {
        return None;
    }
    let key = |r: &ka_strand::Record| match r.id() {
        Some(id) => format!("{id:?}"),
        None => "header".to_string(),
    };
    let mut k = 0usize;
    while k < parent.len() && k < child.len() && key(&parent[k]) == key(&child[k]) {
        k += 1;
    }
    if k >= parent.len() {
        return Some(String::new());
    }
    parent[k..].iter().find_map(|r| match r {
        ka_strand::Record::Message {
            role: ka_strand::Role::User,
            content,
            ..
        } => Some(content.clone()),
        _ => None,
    })
}

/// The row suffix labeling where a child forked off its parent.
fn fork_label(
    parent: &[ka_strand::Record],
    child: &[ka_strand::Record],
    width: usize,
) -> Option<String> {
    match dropped_prompt(parent, child) {
        None => None,
        Some(prompt) if prompt.is_empty() => {
            let msgs = |rs: &[ka_strand::Record]| {
                rs.iter()
                    .filter(|r| matches!(r, ka_strand::Record::Message { .. }))
                    .count()
            };
            Some(if msgs(child) > msgs(parent) {
                "⑂ continued".to_string()
            } else {
                "⑂ copy".to_string()
            })
        }
        Some(prompt) => {
            let first = first_line_of(&prompt);
            Some(trunc_cols(&format!("⑂ at \u{201c}{first}\u{201d}"), width))
        }
    }
}

/// Prefix a modal title with its icon from [`crate::icons::MODAL_ICONS`]
/// (prefix match, so compound titles like `skills · agents · mcp` ride
/// the same entry as their bare key). Titles with no table entry — the
/// vendor-derived model picker, the slash/path popups, the permission
/// ask — pass through unchanged.
fn modal_title(title: &str) -> String {
    match crate::icons::MODAL_ICONS
        .iter()
        .find(|(k, _)| title.starts_with(k))
    {
        Some((_, icon)) => format!("{icon} {title}"),
        None => title.to_string(),
    }
}

/// The one modal frame: rounded BORDER_FOCUS borders on the surface
/// with the ` {icon} {title} ` ACCENT bold title — every overlay speaks
/// this dialect, so a corner never changes shape between popups.
/// Carries the FAINT ✕ close chip at the right of the top border
/// (hover lights it GOLD_HI; [`surface_frame`] is the same frame
/// without it). The chip's cells match [`modal_close_hit`].
fn modal_frame(title: impl Into<String>) -> ratatui::widgets::Block<'static> {
    surface_frame(title).title(
        ratatui::text::Line::styled(
            format!(" {} ", crate::icons::CLOSE),
            ratatui::style::Style::new().fg(crate::palette::FAINT),
        )
        .right_aligned(),
    )
}

/// The modal frame dialect with no close chip.
fn surface_frame(title: impl Into<String>) -> ratatui::widgets::Block<'static> {
    frame_block(
        ratatui::text::Line::styled(format!(" {} ", title.into()), crate::palette::ACCENT_BOLD),
        crate::palette::BORDER_FOCUS,
    )
}

/// The overlay frame body every surface shares: rounded `border`
/// borders, `title` on the top border, one cell of padding all round,
/// prose ink on the BG_SURFACE ground. The permission ask builds its
/// WARN frame from this directly: a stray ✕ must never invite
/// dismissing (let alone denying) a permission decision.
fn frame_block(
    title: ratatui::text::Line<'static>,
    border: ratatui::style::Color,
) -> ratatui::widgets::Block<'static> {
    ratatui::widgets::Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .title(title)
        .border_style(ratatui::style::Style::new().fg(border))
        .padding(ratatui::widgets::Padding::new(1, 1, 1, 1))
        .style(
            ratatui::style::Style::new()
                .fg(crate::palette::FG)
                .bg(crate::palette::BG_SURFACE),
        )
}

/// Rows of the /help overlay, `width` cols wide: the full key table,
/// then commands grouped by what they touch. Section heads are ACCENT
/// bold followed by a RULE line to the width; rows are a key column
/// (GOLD_HI, as wide as the group's longest key plus two cols of air)
/// and the description (META).
fn help_modal_rows(width: usize) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::text::{Line as TuiLine, Span};
    use unicode_width::UnicodeWidthStr;
    let key_style = ratatui::style::Style::new().fg(crate::palette::STRONG);
    let dim = ratatui::style::Style::new().fg(crate::palette::META);
    let mut text = Vec::new();
    let section = |text: &mut Vec<TuiLine<'static>>, name: &str| {
        let rule = width.saturating_sub(name.width() + 1);
        text.push(TuiLine::from(vec![
            Span::styled(name.to_string(), crate::palette::ACCENT_BOLD),
            Span::styled(format!(" {}", "─".repeat(rule)), crate::palette::RULE_STYLE),
        ]));
    };
    fn rows(
        text: &mut Vec<TuiLine<'static>>,
        pairs: &[(&str, &str)],
        key_style: ratatui::style::Style,
    ) {
        let col = pairs.iter().map(|(k, _)| k.width()).max().unwrap_or(0) + 2;
        for (k, v) in pairs {
            let desc: String = v.chars().take(44).collect();
            text.push(TuiLine::from(vec![
                Span::styled(format!("{k}{}", " ".repeat(col - k.width())), key_style),
                Span::styled(desc, crate::palette::META_STYLE),
            ]));
        }
    }
    section(&mut text, "keys");
    let keys: &[(&str, &str)] = &[
        ("enter", "send · steer mid-turn"),
        ("⇧⏎ / ctrl+j", "newline in the draft"),
        ("+text · alt+q", "queue until the turn ends"),
        ("!cmd", "run a shell command directly"),
        ("esc", "close popups & modals · unpin scroll"),
        ("esc (busy)", "abort the running turn"),
        ("esc esc", "rewind menu (empty input)"),
        ("ctrl+c", "quit · clear draft · twice to exit"),
        ("↑ ↓", "prompt history · navigate pickers"),
        ("ctrl+p / ctrl+n", "prompt history"),
        ("ctrl+q", "recall the last queued item"),
        ("ctrl+r", "search the transcript"),
        ("ctrl+o", "expand/collapse the last tool call"),
        ("ctrl+m", "toggle mouse capture ⇄ native"),
        ("ctrl+t / alt+o / alt+i", "popups: skills · todos · info"),
        ("alt+t", "fold/unfold thinking"),
        ("alt+e", "edit the draft in $EDITOR"),
        ("ctrl+↑ / ctrl+↓", "jump between your messages"),
        ("pgup / pgdn", "scroll the transcript"),
        ("tab", "complete slash command or path"),
        ("1-9", "pick a row in pickers & asks"),
        ("ctrl+l", "clear screen"),
        ("ctrl+u/k/w · y · z", "kill to start/end/word · yank · undo"),
    ];
    rows(&mut text, keys, key_style);
    text.push(TuiLine::default());
    section(&mut text, "session");
    rows(
        &mut text,
        &[
            ("/session", "pick a session to resume"),
            ("/resume", "alias for /session"),
            ("/new", "start a fresh session"),
            ("/fork", "fork into a copy [turns to drop]"),
            ("/tree", "fork tree — branch from any input"),
            ("/rewind", "drop the last N exchanges"),
            ("/undo", "restore the last edited file"),
            ("/checkpoint", "snapshot the working tree (git)"),
            ("/restore", "restore a checkpoint [id | list]"),
            ("/export", "save this session to markdown [path]"),
            ("/compact", "digest the context now"),
            ("/retry", "resend the last prompt"),
        ],
        key_style,
    );
    text.push(TuiLine::default());
    section(&mut text, "model & providers");
    rows(
        &mut text,
        &[
            ("/model", "pick a model (/model <vendor> filters)"),
            ("/mode", "permission mode (ask|accept_edits|allow|readonly)"),
            ("/thinking", "pick the reasoning level"),
            (
                "/features",
                "toggle features/tools/skills/MCP (bare = panel)",
            ),
            ("/sandbox", "switch the bash sandbox (off|fs)"),
            ("/key", "set an api key for the current model"),
            ("/provider", "connect a provider (api key)"),
            ("/settings", "settings & provider status"),
        ],
        key_style,
    );
    text.push(TuiLine::default());
    section(&mut text, "tools & context");
    rows(
        &mut text,
        &[
            ("/tasks", "background tasks — ⏎ pages a result"),
            ("/debug", "live debug sessions"),
            ("/mcp", "refresh MCP tool lists (/mcp refresh)"),
            ("/prompt", "run an MCP prompt"),
            ("/spills", "browse spilled tool output"),
            ("/context", "context usage breakdown"),
            ("/usage", "usage & cost: session + recent"),
            ("/find", "search the transcript: /find <text>"),
            ("/copy", "copy the last reply (OSC52)"),
            ("/clip", "attach a clipboard image"),
            ("/image", "attach an image [path]"),
        ],
        key_style,
    );
    text.push(TuiLine::default());
    section(&mut text, "safety & plans");
    rows(
        &mut text,
        &[
            ("/plan", "research the task, draft the plan"),
            ("/build", "implement the plan file"),
            ("/review", "read-only review of changes [base]"),
            ("/approve", "review the plan file, then build"),
            ("/memory", "show loaded MEMORY.md tiers"),
        ],
        key_style,
    );
    text.push(TuiLine::default());
    section(&mut text, "meta");
    rows(
        &mut text,
        &[
            ("/help", "commands and key bindings"),
            ("/agents", "list available agents"),
            ("/skill", "invoke a skill: /skill:<name>"),
            ("/quit", "exit"),
            ("/update", "update ka, then resume here (idle)"),
        ],
        key_style,
    );
    text.push(TuiLine::default());
    text.push(TuiLine::from(Span::styled(
        "custom commands: .ka/commands/*.md (project, trust-gated) or \
~/.config/ka/commands/*.md; body supports $ARGUMENTS",
        dim,
    )));
    text
}

fn centered(width: u16, height: u16, area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    ratatui::layout::Rect {
        x,
        y,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

/// Does a Left Down at these terminal cells close the open modal? The
/// top-right 3 cells are the ✕ chip's hit zone (the chip renders right
/// against the right border); any cell outside the modal rect is
/// click-outside-close; body clicks keep the modal open.
fn modal_close_hit(rect: ratatui::layout::Rect, x: u16, y: u16) -> bool {
    let click = ratatui::layout::Position { x, y };
    let chip = ratatui::layout::Rect {
        x: rect.x + rect.width.saturating_sub(3),
        y: rect.y,
        width: 3,
        height: 1,
    };
    !rect.contains(click) || chip.contains(click)
}

/// What an undispatched left press (released without dragging) acts
/// on. Zones are mutually exclusive; the strip/arrows/transcript
/// order matches the old press-time handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Click {
    Tool { entry: usize, call: usize },
    Thought { entry: usize },
    Summary { entry: usize },
    JumpUp,
    JumpDown,
    Todos,
    Skills,
    Info,
    None,
}

/// Resolve a released left press into a click target. A press over
/// the transcript resolves through [`Transcript::row_ref_at`]; strip
/// buttons open their popups; ▲▼ jump between the user's messages;
/// anywhere else is inert.
fn click_target(
    strip: &std::cell::Cell<Option<StripZones>>,
    arrows: &std::cell::Cell<Option<TitleArrows>>,
    tx: &std::cell::Cell<Option<(ratatui::layout::Rect, usize)>>,
    transcript: &Transcript,
    col: u16,
    row: u16,
) -> Click {
    if let Some(button) = strip.get().and_then(|z| z.hit(col, row)) {
        return match button {
            StripButton::Todos => Click::Todos,
            StripButton::Skills => Click::Skills,
            StripButton::Info => Click::Info,
        };
    }
    if let Some(up) = arrows.get().and_then(|z| z.hit(col, row)) {
        return if up { Click::JumpUp } else { Click::JumpDown };
    }
    if let Some((area, start)) = tx.get()
        && area.contains(ratatui::layout::Position { x: col, y: row })
    {
        let row = start + (row - area.y) as usize;
        return match transcript.row_ref_at(row) {
            Some(RowRef::ToolCall(entry, call)) => Click::Tool { entry, call },
            Some(RowRef::Entry(entry)) => Click::Thought { entry },
            Some(RowRef::Summary(entry)) => Click::Summary { entry },
            None => Click::None,
        };
    }
    Click::None
}

/// What the pointer currently hovers — the visual affordance mirror of
/// [`Click`]. Only elements with an action highlight: cards, foldable
/// thinking bands, the strip buttons, the ▲▼ jump arrows, the rail
/// thumb, and a modal's ✕ chip. Plain prose rows are inert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hover {
    /// One transcript entry (a tool card or a foldable thinking band).
    Entry(usize),
    /// The ✕ chip of the open modal / chip-bearing popup.
    Chip,
    /// One of the ▲▼ jump arrows (`true` = ▲).
    Arrow(bool),
    /// One strip button on the composer's bottom border.
    Strip(StripButton),
    /// The scrollbar thumb.
    Rail,
}

/// Resolve the pointer position to a hover target (capture mode only —
/// native terminals never forward motion events). While a modal is
/// open only its ✕ chip answers; the input-box pickers are
/// keyboard-driven, so nothing inside them highlights.
#[allow(clippy::too_many_arguments)]
fn hover_target(
    modal_zone: &std::cell::Cell<Option<ratatui::layout::Rect>>,
    popup_zone: &std::cell::Cell<Option<ratatui::layout::Rect>>,
    chip_bearing_popup: bool,
    strip: &std::cell::Cell<Option<StripZones>>,
    arrows: &std::cell::Cell<Option<TitleArrows>>,
    rail: &std::cell::Cell<Option<RailZone>>,
    tx: &std::cell::Cell<Option<(ratatui::layout::Rect, usize)>>,
    transcript: &Transcript,
    col: u16,
    row: u16,
) -> Option<Hover> {
    let pos = ratatui::layout::Position { x: col, y: row };
    if let Some(rect) = modal_zone.get() {
        // the ✕ chip's hit zone mirrors modal_close_hit
        let chip = ratatui::layout::Rect {
            x: rect.x + rect.width.saturating_sub(3),
            y: rect.y,
            width: 3,
            height: 1,
        };
        return chip.contains(pos).then_some(Hover::Chip);
    }
    if let Some(rect) = popup_zone.get() {
        if chip_bearing_popup {
            let chip = ratatui::layout::Rect {
                x: rect.x + rect.width.saturating_sub(3),
                y: rect.y,
                width: 3,
                height: 1,
            };
            return chip.contains(pos).then_some(Hover::Chip);
        }
        return None;
    }
    if let Some((rect, _, _, thumb_pos, thumb_len)) = rail.get()
        && rect.contains(pos)
    {
        let r = (row - rect.y) as usize;
        return (thumb_pos..thumb_pos + thumb_len)
            .contains(&r)
            .then_some(Hover::Rail);
    }
    match click_target(strip, arrows, tx, transcript, col, row) {
        Click::Tool { entry, .. } => Some(Hover::Entry(entry)),
        Click::Thought { entry } => transcript.is_foldable(entry).then_some(Hover::Entry(entry)),
        Click::Summary { entry } => transcript
            .summary_foldable(entry)
            .then_some(Hover::Entry(entry)),
        Click::JumpUp => Some(Hover::Arrow(true)),
        Click::JumpDown => Some(Hover::Arrow(false)),
        Click::Todos => Some(Hover::Strip(StripButton::Todos)),
        Click::Skills => Some(Hover::Strip(StripButton::Skills)),
        Click::Info => Some(Hover::Strip(StripButton::Info)),
        Click::None => None,
    }
}

/// The text span a left drag picks, or `None` while the pointer is
/// still within click tolerance of the press (≤1 cell in both axes —
/// such a release is a click, not a drag). Anchors are `(row, column)`
/// in transcript space, columns relative to the content area's left
/// edge and clamped into it, so drags past an edge keep extending to
/// that edge. The pair comes back in reading order, so a vertical drag
/// degrades to whole rows exactly as a row selection always did.
fn drag_span(
    area: ratatui::layout::Rect,
    start: usize,
    press: (u16, u16),
    head: (u16, u16),
) -> Option<((usize, u16), (usize, u16))> {
    if head.0.abs_diff(press.0) < 2 && head.1.abs_diff(press.1) < 2 {
        return None;
    }
    if !area.contains(ratatui::layout::Position {
        x: press.0,
        y: press.1,
    }) {
        return None;
    }
    let right = area.x + area.width.saturating_sub(1);
    let col = |x: u16| x.clamp(area.x, right) - area.x;
    let bottom = area.y + area.height.saturating_sub(1);
    let anchor = (start + (press.1 - area.y) as usize, col(press.0));
    let head = (
        start + (head.1.clamp(area.y, bottom) - area.y) as usize,
        col(head.0),
    );
    Some(if anchor <= head {
        (anchor, head)
    } else {
        (head, anchor)
    })
}

/// The clipboard text of a drag selection: every picked row's visible
/// text sliced to the selected display columns, right-trimmed, the
/// rows joined by newlines.
fn selection_text(t: &Transcript, sel: ((usize, u16), (usize, u16))) -> String {
    let ((r0, c0), (r1, c1)) = sel;
    let mut out = String::new();
    for row in r0..=r1 {
        if row > r0 {
            out.push('\n');
        }
        if let Some(text) = t.row_text(row) {
            let from = if row == r0 { c0 as usize } else { 0 };
            let to = if row == r1 { c1 as usize } else { usize::MAX };
            out.push_str(slice_cols(&text, from, to).trim_end());
        }
    }
    out.trim_end().to_string()
}

/// The slice of a rendered row covering display columns `from..=to`
/// (clamped to what the row has; a wide glyph is kept whole when its
/// left column is in range, and combining marks follow their base
/// glyph's decision).
fn slice_cols(text: &str, from: usize, to: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::new();
    let mut w = 0usize;
    let mut keep = false;
    for ch in text.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if cw == 0 {
            if keep {
                out.push(ch);
            }
            continue;
        }
        keep = w >= from && w <= to;
        if keep {
            out.push(ch);
        }
        w += cw;
    }
    out
}

/// A block title with one column of air on each side, so titles never
/// kiss the border glyphs or the frame edge.
fn padded_title(title: impl Into<String>) -> ratatui::text::Line<'static> {
    ratatui::text::Line::from(format!(" {} ", title.into())).style(crate::palette::META)
}

/// Jump the viewport anchor to the previous (up) or next (down) user
/// message relative to the window top. Up with nothing above is a
/// no-op; down past the last user message lands on the live tail
/// (scroll = None) unless the view is already pinned there.
fn jump_to_user_message(
    scroll: &mut Option<usize>,
    user_rows: &[usize],
    total: usize,
    visible: usize,
    up: bool,
) {
    if user_rows.is_empty() || visible == 0 {
        return;
    }
    let (start, pinned) = window_range(total, visible, *scroll);
    if up {
        if let Some(&r) = user_rows.iter().rev().find(|&&r| r < start) {
            *scroll = Some(r);
        }
    } else if let Some(&r) = user_rows.iter().find(|&&r| r > start) {
        *scroll = Some(r);
    } else if !pinned {
        *scroll = None;
    }
}

/// Read the system clipboard as text: wayland → X11 → WSL2/Windows.
async fn clipboard_text() -> String {
    // Every probe is capped (a hanging xclip against an unresponsive
    // selection owner must not freeze the event loop).
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
    let run = |program: &str, args: &[&str]| {
        tokio::time::timeout(
            TIMEOUT,
            tokio::process::Command::new(program).args(args).output(),
        )
    };
    // WSL2 first: the Windows clipboard lives behind powershell.exe.
    if let Ok(Ok(out)) = run(
        "powershell.exe",
        &["-NoProfile", "-Command", "Get-Clipboard"],
    )
    .await
    {
        if out.status.success() && !out.stdout.is_empty() {
            return String::from_utf8_lossy(&out.stdout)
                .trim_end_matches(['\r', '\n'])
                .to_string();
        }
    }
    for (program, args) in [
        ("wl-paste", vec!["--no-newline"]),
        ("xclip", vec!["-selection", "clipboard", "-o"]),
    ] {
        if let Ok(Ok(out)) = run(program, &args).await {
            if out.status.success() && !out.stdout.is_empty() {
                return String::from_utf8_lossy(&out.stdout)
                    .trim_end_matches(['\r', '\n'])
                    .to_string();
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn model(id: &str, context: u32) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            wire: "openai_chat".to_string(),
            context,
            key_env: "X_API_KEY".to_string(),
            key_set: false,
            doc_url: String::new(),
            price_in: 0.0,
            price_out: 0.0,
            priced: false,
            plan: false,
            efforts: Vec::new(),
        }
    }

    #[test]
    fn queue_refusal_classifies_queuable_drafts() {
        // plain text always queues
        assert_eq!(queue_refusal("keep going"), None);
        assert_eq!(queue_refusal("see /mode in a sentence"), None);
        // argumented commands carry events: they queue
        assert_eq!(queue_refusal("/mode free"), None);
        assert_eq!(queue_refusal("/thinking high"), None);
        // picker-openers, quit, and unknown commands refuse with a reason
        let bare = queue_refusal("/model").unwrap_or_default();
        assert!(bare.contains("picker"), "{bare}");
        let quit = queue_refusal("/quit").unwrap_or_default();
        assert!(quit.contains("immediately"), "{quit}");
        let unknown = queue_refusal("/definitely-not-a-command").unwrap_or_default();
        assert!(unknown.contains("unknown"), "{unknown}");
    }

    #[test]
    fn queue_draft_refuses_and_never_enqueues_disallowed_slash() {
        let mut queue = Vec::new();
        let mut transcript = Transcript::default();
        queue_draft(&mut queue, "/quit", &mut transcript);
        assert!(queue.is_empty(), "refused drafts must not enqueue");
        queue_draft(&mut queue, "/mode free", &mut transcript);
        assert_eq!(queue, vec!["/mode free".to_string()]);
        queue_draft(&mut queue, "then summarize", &mut transcript);
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn safe_mode_blocks_custom_command_execution() {
        // isolated here: ka-term's test binary is its own process, so
        // flipping the bare-mode atomic cannot disturb ka-engine's tests
        let dir = std::env::temp_dir().join(format!("ka-tui-safemode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".ka/commands")).unwrap();
        std::fs::write(dir.join(".ka/commands/ship.md"), "ship body $ARGUMENTS\n").unwrap();
        let state_home = dir.join("state");
        // trusted so the normal path would load the body
        std::fs::create_dir_all(&state_home).unwrap();
        let trust_file = state_home.join("ka/trust.json");
        std::fs::create_dir_all(trust_file.parent().unwrap()).unwrap();
        ka_engine::trust::save_trust_at(&trust_file, std::slice::from_ref(&dir));
        // sanity: without safe mode the command resolves (explicit
        // paths — no process-global cwd mutation in a parallel test
        // binary)
        let body = custom_command_in(&dir, &state_home, "/ship", Some("v1"));
        assert!(body.is_some(), "trusted project loads the command body");
        // safe mode: execution path refuses even though the popup gate
        // (available_slash_commands) also hides it
        ka_engine::conventions::set_bare_mode(true);
        assert!(
            custom_command("/ship", Some("v1")).is_none(),
            "safe mode must not load custom command bodies"
        );
        assert!(
            !available_slash_commands()
                .iter()
                .any(|(name, _)| name == "cmd:ship"),
            "safe mode must not advertise custom commands"
        );
        ka_engine::conventions::set_bare_mode(false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_picker_filters_picks_and_falls_back_to_custom() {
        // keyless locals: configured by definition
        let keyless = |id: &str, context: u32| {
            let mut m = model(id, context);
            m.key_env = String::new();
            m
        };
        let picker = ModelPicker {
            models: vec![
                keyless("ollama/qwen3-32b", 131_072),
                keyless("openai/gpt-5.1", 400_000),
                keyless("anthropic/claude-sonnet-5", 200_000),
            ],
            vendor: None,
            selected: 0,
            filter: String::new(),
        };
        assert_eq!(picker.rows().len(), 3);
        assert_eq!(picker.pick().as_deref(), Some("ollama/qwen3-32b"));

        let mut filtered = picker.clone();
        filtered.filter = "gpt".to_string();
        assert_eq!(filtered.rows().len(), 1);
        assert_eq!(filtered.pick().as_deref(), Some("openai/gpt-5.1"));

        // no match: Enter sets the filter itself as a custom selector
        let mut custom = picker.clone();
        custom.filter = "groq/llama-3.3-70b".to_string();
        assert!(custom.rows().is_empty());
        assert_eq!(
            custom.pick().as_deref(),
            Some("groq/llama-3.3-70b"),
            "unmatched filter becomes the selector"
        );

        // no match and no filter: nothing to apply
        let mut empty = picker.clone();
        empty.filter = "   ".to_string();
        assert!(empty.rows().is_empty());
        assert_eq!(empty.pick(), None);

        // selection follows the filtered view
        let mut second = picker.clone();
        second.filter = "claude".to_string();
        second.selected = 0;
        assert_eq!(second.pick().as_deref(), Some("anthropic/claude-sonnet-5"));

        // a keyed model without its key drops out of the list; typing
        // its selector stays the reachability path
        let mut mixed = picker.clone();
        mixed.models[1].key_env = "OPENAI_API_KEY".to_string();
        let ids: Vec<&str> = mixed.rows().iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["ollama/qwen3-32b", "anthropic/claude-sonnet-5"],
            "unconfigured providers are hidden"
        );
        mixed.filter = "openai/gpt-5.1".to_string();
        assert!(mixed.rows().is_empty());
        assert_eq!(
            mixed.pick().as_deref(),
            Some("openai/gpt-5.1"),
            "hidden models stay reachable as custom selectors"
        );
    }

    #[test]
    fn model_picker_lists_configured_providers_only() {
        let mut models = vec![
            model("zai/glm-5.3", 200_000),               // keyed, no key → hidden
            model("ollama/qwen3-32b", 131_072),          // keyless → listed
            model("openai/gpt-5.1", 400_000),            // keyed + key set → listed
            model("anthropic/claude-sonnet-5", 200_000), // keyed, no key
        ];
        models[0].key_env = "ZAI_API_KEY".to_string();
        models[1].key_env = String::new();
        models[2].key_set = true;
        let picker = ModelPicker {
            models,
            vendor: None,
            selected: 0,
            filter: String::new(),
        };
        let ids: Vec<&str> = picker.rows().iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["ollama/qwen3-32b", "openai/gpt-5.1"],
            "keyed-unset vendors drop; keyless and keyed-set stay (stable order)"
        );
        // a hidden vendor's model still applies through the custom
        // selector fallback
        let mut f = picker.clone();
        f.filter = "zai/glm-5.3".to_string();
        assert!(f.rows().is_empty());
        assert_eq!(f.pick().as_deref(), Some("zai/glm-5.3"));

        // the vendor lock composes: an unconfigured vendor's locked
        // list is empty and Enter has nothing to apply
        let mut locked = picker.clone();
        locked.vendor = Some("zai".to_string());
        assert!(locked.rows().is_empty());
        assert_eq!(locked.pick(), None);
    }

    fn provider(name: &str, env_var: &str, key_set: bool) -> ProviderInfo {
        ProviderInfo {
            name: name.to_string(),
            env_var: env_var.to_string(),
            base_url: String::new(),
            key_set,
        }
    }

    #[test]
    fn provider_picker_counts_filters_and_picks() {
        let models = vec![
            model("anthropic/claude-sonnet-5", 200_000),
            model("anthropic/claude-opus-4", 200_000),
            model("ollama/qwen3-32b", 131_072),
        ];
        let mut picker = ProviderPicker::new(
            vec![
                provider("anthropic", "ANTHROPIC_API_KEY", false),
                provider("ollama", "", false),
            ],
            &models,
        );
        let rows = picker.rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1, "ANTHROPIC_API_KEY ✗ · 2 models");
        assert_eq!(rows[1].1, "keyless · local · 1 model");
        assert_eq!(picker.pick().unwrap().name, "anthropic");

        // the filter matches the env var, not just the name
        picker.filter = "ANTHROPIC_API_KEY".to_string();
        assert_eq!(picker.rows().len(), 1);
        assert_eq!(picker.pick().unwrap().name, "anthropic");

        picker.filter = "OLLAMA".to_string();
        assert_eq!(picker.rows().len(), 1);
        assert_eq!(picker.pick().unwrap().name, "ollama");

        // no match: nothing to connect
        picker.filter = "midscene".to_string();
        assert!(picker.rows().is_empty());
        assert!(picker.pick().is_none());
    }

    #[test]
    fn model_picker_vendor_lock_narrows_rows() {
        let mut models = vec![
            model("anthropic/claude-sonnet-5", 200_000),
            model("anthropic/claude-opus-4", 200_000),
            model("ollama/qwen3-32b", 131_072),
        ];
        // anthropic keyed-in, ollama keyless: every row configured
        models[0].key_set = true;
        models[1].key_set = true;
        models[2].key_env = String::new();
        let mut picker = ModelPicker {
            models,
            vendor: Some("anthropic".to_string()),
            selected: 0,
            filter: String::new(),
        };
        assert_eq!(picker.rows().len(), 2);
        assert_eq!(picker.pick().as_deref(), Some("anthropic/claude-sonnet-5"));

        // the filter still applies within the vendor
        picker.filter = "opus".to_string();
        assert_eq!(picker.rows().len(), 1);
        assert_eq!(picker.pick().as_deref(), Some("anthropic/claude-opus-4"));

        // an unmatched filter still falls back to a custom selector
        picker.filter = "anthropic/claude-haiku".to_string();
        assert!(picker.rows().is_empty());
        assert_eq!(picker.pick().as_deref(), Some("anthropic/claude-haiku"));

        // no lock: every model shows (the plain picker's behavior)
        let mut open = picker.clone();
        open.vendor = None;
        open.filter = String::new();
        assert_eq!(open.rows().len(), 3);
    }

    #[test]
    fn mark_key_set_flips_only_matching_entries() {
        let mut models = vec![model("openai/gpt-5.1", 400_000)];
        models[0].key_env = "OPENAI_API_KEY".to_string();
        let mut providers = vec![
            provider("openai", "OPENAI_API_KEY", false),
            provider("zai", "ZAI_API_KEY", false),
        ];
        mark_key_set(&mut models, &mut providers, "OPENAI_API_KEY");
        assert!(models[0].key_set);
        assert!(providers[0].key_set);
        assert!(!providers[1].key_set, "other providers stay as they were");
    }

    /// The picker's row budget: 68 wide minus borders, padding, and the
    /// 2-col selection pointer.
    const MODEL_ROW_BUDGET: usize = 62;

    fn row_w(row: &str) -> usize {
        unicode_width::UnicodeWidthStr::width(row)
    }

    #[test]
    fn model_rows_stay_single_line() {
        let long = model(
            "ollama/some-extremely-long-model-name:with-tag-and-more",
            1_000_000,
        );
        let row = model_row(&long, "1000k", " ✗");
        assert!(row_w(&row) <= MODEL_ROW_BUDGET, "row too wide: {row}");
        let short = model("ollama/qwen3.5:9b", 262_144);
        let row = model_row(&short, "262k", "");
        assert!(row.contains("qwen3.5:9b"));
        assert!(row.contains("openai"), "wire abbreviated: {row}");
    }

    #[test]
    fn model_rows_show_reasoning_battery() {
        // a reasoning model: the ✻ battery rides last, one glyph per
        // level, and the columns stay aligned with plain rows
        let mut m = model("anthropic/claude-opus-4.6", 200_000);
        m.efforts = ["low", "medium", "high"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let row = model_row(&m, "200k", "");
        assert!(row.ends_with(" ✻◔◑◕"), "{row}");
        let plain = model("ollama/qwen3.5:9b", 262_144);
        let plain_row = model_row(&plain, "262k", "");
        assert!(!plain_row.contains('✻'), "{plain_row}");
        assert_eq!(
            row.find("200k").map(|i| row[..i].chars().count()),
            plain_row
                .find("262k")
                .map(|i| plain_row[..i].chars().count()),
            "ctx column aligned: {row} | {plain_row}"
        );
        // worst case: four levels, long wire, real prices — still fits
        let mut big = model("anthropic/claude-sonnet-5-opus-max", 200_000);
        big.wire = "anthropic_messages".into();
        big.priced = true;
        big.price_in = 12.5;
        big.price_out = 100.0;
        big.efforts = ["low", "medium", "high", "max"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let row = model_row(&big, "200k", " ✗");
        assert!(
            row_w(&row) <= MODEL_ROW_BUDGET,
            "four-level row too wide: {row}"
        );
        assert!(row.ends_with(" ✻◔◑◕●"), "{row}");
        // a level without a battery glyph keeps its name
        let mut odd = model("x/y", 8_000);
        odd.efforts = vec!["low".into(), "xhigh".into()];
        let row = model_row(&odd, "8k", "");
        assert!(row.ends_with(" ✻◔ xhigh"), "{row}");
        assert!(row_w(&row) <= MODEL_ROW_BUDGET, "{row}");
    }

    #[test]
    fn model_slash_opens_picker_or_sets_directly() {
        assert!(matches!(
            slash_command("/model"),
            Some(Slash {
                note: None,
                modal: Some(ModalKind::Model(None)),
                event: None,
                ..
            })
        ));
        assert!(matches!(
            slash_command("/model groq/llama-3.3-70b"),
            Some(Slash {
                note: None,
                event: Some(Command::SetModel { selector }),
                ..
            }) if selector == "groq/llama-3.3-70b"
        ));
        // a bare vendor opens the picker locked to it instead of
        // poisoning the selector with a vendor-less string
        assert!(matches!(
            slash_command("/model opencode-go"),
            Some(Slash {
                note: None,
                modal: Some(ModalKind::Model(v)),
                event: None,
                ..
            }) if v.as_deref() == Some("opencode-go")
        ));
        // the effort suffix does not turn a vendor into a selector
        assert!(matches!(
            slash_command("/model opencode-go@high"),
            Some(Slash {
                modal: Some(ModalKind::Model(v)),
                ..
            }) if v.as_deref() == Some("opencode-go")
        ));
        // a selector with an effort suffix still sets directly
        assert!(matches!(
            slash_command("/model zai/glm-5.3@high"),
            Some(Slash {
                event: Some(Command::SetModel { selector }),
                ..
            }) if selector == "zai/glm-5.3@high"
        ));
        assert!(matches!(
            slash_command("/provider"),
            Some(Slash {
                note: None,
                modal: Some(ModalKind::Provider),
                event: None,
                ..
            })
        ));
    }

    #[test]
    fn thinking_choices_parses_and_dedupes() {
        let words = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // off leads, unparseable entries skip, duplicate off dedupes
        let got = thinking_choices(&words(&["low", "bogus", "high", "off"]));
        assert_eq!(
            got,
            vec![
                ka_protocol::Effort::Off,
                ka_protocol::Effort::Low,
                ka_protocol::Effort::High
            ]
        );
        // empty efforts: just off
        assert_eq!(thinking_choices(&[]), vec![ka_protocol::Effort::Off]);
        // the picker preselects the current level (or off)
        let efforts = words(&["low", "medium", "high"]);
        let tp = ThinkingPicker::for_effort(Some(ka_protocol::Effort::High), &efforts);
        assert_eq!(tp.pick(), ka_protocol::Effort::High);
        let tp = ThinkingPicker::for_effort(None, &efforts);
        assert_eq!(tp.pick(), ka_protocol::Effort::Off);
    }

    #[test]
    fn slash_thinking_direct_and_unknown() {
        // bare: opens the picker
        assert!(matches!(
            slash_command("/thinking"),
            Some(Slash {
                note: None,
                modal: Some(ModalKind::Thinking),
                event: None,
                ..
            })
        ));
        // named: session-scoped SetEffort
        for (word, want) in [
            ("off", ka_protocol::Effort::Off),
            ("low", ka_protocol::Effort::Low),
            ("medium", ka_protocol::Effort::Medium),
            ("high", ka_protocol::Effort::High),
            ("max", ka_protocol::Effort::Max),
        ] {
            let s = slash_command(&format!("/thinking {word}")).unwrap();
            assert!(
                matches!(s.event, Some(Command::SetEffort { level }) if level == want),
                "/thinking {word}"
            );
        }
        // unknown level: a note plus the picker
        let s = slash_command("/thinking turbo").unwrap();
        assert!(s.note.as_deref().is_some_and(|n| n.contains("turbo")));
        assert!(matches!(s.modal, Some(ModalKind::Thinking)));
        assert!(s.event.is_none());
    }

    #[test]
    fn word_moves_cross_whitespace_runs() {
        // "foo bar  baz"
        let mut b = InputBuffer {
            text: "foo bar  baz".into(),
            cursor: 0,
            ..Default::default()
        };
        b.word_left();
        assert_eq!(b.cursor, 0, "at start: no-op");
        b.word_right();
        assert_eq!(b.cursor, 3, "end of first word");
        b.word_right();
        assert_eq!(b.cursor, 7, "end of second word");
        b.word_right();
        assert_eq!(b.cursor, 12, "end of last word = text end");
        b.word_right();
        assert_eq!(b.cursor, 12, "clamped at end");

        // cursor inside a word: word_left to its start, word_right to its end
        let mut mid = InputBuffer {
            text: "foo bar  baz".into(),
            cursor: 10,
            ..Default::default()
        };
        mid.word_left();
        assert_eq!(mid.cursor, 9, "start of 'baz'");
        mid.word_left();
        assert_eq!(mid.cursor, 4, "start of 'bar'");
        mid.cursor = 5;
        mid.word_right();
        assert_eq!(mid.cursor, 7, "end of 'bar'");
        mid.cursor = 8;
        mid.word_left();
        assert_eq!(mid.cursor, 4, "double space skipped back to 'bar'");
        mid.cursor = 8;
        mid.word_right();
        assert_eq!(mid.cursor, 12, "double space skipped forward past 'baz'");
    }

    #[test]
    fn delete_word_backward_kills_only_the_word_prefix() {
        let mut b = InputBuffer {
            text: "foo bar".into(),
            cursor: 5,
            ..Default::default()
        }; // inside 'bar'
        assert_eq!(b.delete_word_backward().as_deref(), Some("b"));
        assert_eq!(b.text, "foo ar");
        assert_eq!(b.cursor, 4);
        b.cursor = 0;
        assert_eq!(b.delete_word_backward(), None, "at text start: no-op");
        assert_eq!(b.text, "foo ar");
        b.cursor = 6;
        assert_eq!(b.delete_word_backward().as_deref(), Some("ar"));
        assert_eq!(b.text, "foo ");
        assert_eq!(b.cursor, 4);
    }

    #[test]
    fn delete_word_forward_kills_to_next_word_start() {
        let mut b = InputBuffer {
            text: "foo bar  baz".into(),
            cursor: 4,
            ..Default::default()
        }; // start of 'bar'
        assert_eq!(b.delete_word_forward().as_deref(), Some("bar"));
        assert_eq!(b.text, "foo   baz");
        assert_eq!(b.cursor, 4, "cursor unmoved by forward delete");
        assert_eq!(b.delete_word_forward().as_deref(), Some("  baz"));
        assert_eq!(b.text, "foo ");
        assert_eq!(b.cursor, 4);
        assert_eq!(b.delete_word_forward(), None, "at end: no-op");
        assert_eq!(b.text, "foo ");
    }

    #[test]
    fn line_kills_respect_the_current_line_only() {
        // "line one\ntwo three\nfour"
        let mut b = InputBuffer {
            text: "line one\ntwo three\nfour".into(),
            cursor: 15,
            ..Default::default()
        }; // inside 'three'
        assert_eq!(b.delete_to_line_start().as_deref(), Some("two th"));
        assert_eq!(b.text, "line one\nree\nfour");
        assert_eq!(b.cursor, 9, "cursor at line start");
        assert_eq!(b.delete_to_line_start(), None, "already at line start");
        assert_eq!(b.delete_to_line_end().as_deref(), Some("ree"));
        assert_eq!(
            b.text, "line one\n\nfour",
            "line end kill stops at, never eats, \\n"
        );

        let mut tail = InputBuffer {
            text: "aa\nbb".into(),
            cursor: 5,
            ..Default::default()
        }; // end of text
        assert_eq!(tail.delete_to_line_end(), None, "at text end: no-op");
        assert_eq!(tail.text, "aa\nbb");
        tail.cursor = 2; // at the newline: end of line one
        assert_eq!(
            tail.delete_to_line_end(),
            None,
            "at line end before \n: no-op"
        );
    }

    #[test]
    fn line_kill_at_start_deletes_whole_line_content() {
        let mut b = InputBuffer {
            text: "hello".into(),
            cursor: 5,
            ..Default::default()
        };
        assert_eq!(b.delete_to_line_start().as_deref(), Some("hello"));
        assert!(b.text.is_empty());
        assert_eq!(b.cursor, 0);
        b.undo();
        assert_eq!(b.text, "hello");
        assert_eq!(b.cursor, 5);

        let mut fwd = InputBuffer {
            text: "hello world".into(),
            cursor: 0,
            ..Default::default()
        };
        assert_eq!(fwd.delete_to_line_end().as_deref(), Some("hello world"));
        assert!(fwd.text.is_empty());
        fwd.undo();
        assert_eq!(fwd.text, "hello world");
    }

    #[test]
    fn kill_replace_and_yank_roundtrip_multiline() {
        let mut b = InputBuffer::default();
        b.insert_str("aa\nbb\ncc");
        b.cursor = 3; // start of 'bb'
        let del = b.delete_to_line_end();
        b.kill_push(del); // kill "bb"
        assert_eq!(b.text, "aa\n\ncc");
        assert_eq!(b.kill.clone().as_deref(), Some("bb"));
        b.yank();
        assert_eq!(b.text, "aa\nbb\ncc", "yank restores multiline text");
        assert_eq!(b.cursor, 5, "cursor after the yanked text");

        // kills REPLACE the slot (no readline append)
        // kills REPLACE the slot (no readline append)
        let mut r = InputBuffer::default();
        r.insert_str("one two");
        r.cursor = 4;
        let del = r.delete_word_forward();
        r.kill_push(del); // kills "two"
        assert_eq!(r.text, "one ");
        assert_eq!(r.kill.as_deref(), Some("two"));
        r.cursor = 3;
        let del = r.delete_word_backward();
        r.kill_push(del); // kills "one"
        assert_eq!(r.text, " ");
        assert_eq!(r.kill.as_deref(), Some("one"), "new kill replaces the old");
        // empty kill never clobbers
        r.cursor = 0;
        let del = r.delete_to_line_start();
        r.kill_push(del); // already at line start
        assert_eq!(r.kill.as_deref(), Some("one"), "no-op kill keeps the slot");

        let mut empty = InputBuffer::default();
        empty.yank(); // kill = None: silent no-op
        assert!(empty.text.is_empty());
    }

    #[test]
    fn clear_draft_is_undoable_and_yankable() {
        let mut b = InputBuffer::default();
        b.insert_str("long draft");
        b.clear_draft();
        assert!(b.text.is_empty(), "draft cleared");
        b.undo();
        assert_eq!(b.text, "long draft", "ctrl+z restores the cleared draft");
        assert_eq!(b.cursor, 10);
        // the cleared text also landed in the kill ring (ctrl+y yanks)
        b.clear_draft();
        b.yank();
        assert_eq!(b.text, "long draft", "ctrl+y yanks the killed draft");
    }

    #[test]
    fn undo_restores_pre_op_snapshots_across_ops() {
        let mut b = InputBuffer::default();
        b.insert('a');
        b.insert('b');
        b.newline();
        b.insert_str("cd");
        assert_eq!(b.text, "ab\ncd");
        b.undo();
        assert_eq!(b.text, "ab\n", "undo insert_str");
        b.undo();
        assert_eq!(b.text, "ab", "undo newline");
        b.undo();
        assert_eq!(b.text, "a", "undo insert b");
        b.undo();
        assert_eq!(b.text, "", "undo insert a");
        b.undo();
        assert_eq!(b.text, "", "empty undo stack: no-op");

        // backspace + delete_forward
        let mut c = InputBuffer::default();
        c.insert_str("abc");
        c.backspace();
        assert_eq!(c.text, "ab");
        c.undo();
        assert_eq!(c.text, "abc");
        c.cursor = 1;
        c.delete_forward();
        assert_eq!(c.text, "ac");
        c.undo();
        assert_eq!(c.text, "abc");

        // word kill + line kill
        let mut d = InputBuffer::default();
        d.insert_str("foo bar baz");
        d.cursor = 7;
        assert!(d.delete_word_backward().is_some());
        assert_eq!(d.text, "foo  baz");
        d.undo();
        assert_eq!(d.text, "foo bar baz", "undo word kill");
        assert_eq!(d.cursor, 7, "undo restores the cursor too");
        let del = d.delete_to_line_start();
        d.kill_push(del);
        d.undo();
        assert_eq!(d.text, "foo bar baz");
        d.cursor = 10;
        let del = d.delete_to_line_end();
        d.kill_push(del); // kills the trailing 'z'
        assert_eq!(d.text, "foo bar ba");
        d.undo();
        assert_eq!(d.text, "foo bar baz");
    }

    #[test]
    fn shift_enter_breaks_the_line_not_sends() {
        // Shift+Enter only arrives as a distinct key when the startup
        // kitty flags include REPORT_ALL_KEYS_AS_ESCAPE_CODES — without
        // it Enter stays legacy \r and the draft sends. Pin the flag
        // contract (the key arm feeding input.newline() is the same one
        // Ctrl+J exercises).
        use crossterm::event::KeyboardEnhancementFlags as F;
        let flags = startup_keyboard_flags();
        assert!(
            flags.contains(F::REPORT_ALL_KEYS_AS_ESCAPE_CODES),
            "Enter must be promoted to CSI-u for Shift+Enter to report its modifier"
        );
        assert!(flags.contains(F::DISAMBIGUATE_ESCAPE_CODES));
        assert!(flags.contains(F::REPORT_EVENT_TYPES));
        assert!(
            flags.contains(F::REPORT_ALTERNATE_KEYS),
            "the shifted codepoint must ride the alternate-key field, or a \
             layout's shifted symbols type as their un-shifted neighbours"
        );

        // the CSI-u event the arm at the top of the key match receives:
        // (KeyCode::Enter, KeyModifiers::SHIFT) => input.newline()
        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::SHIFT,
        );
        assert_eq!(key.code, crossterm::event::KeyCode::Enter);
        assert!(
            key.modifiers
                .contains(crossterm::event::KeyModifiers::SHIFT)
        );
        let mut b = InputBuffer::default();
        b.insert_str("one");
        b.newline();
        b.insert_str("two");
        assert_eq!(b.text, "one\ntwo", "shift+enter inserts a newline");
    }

    #[test]
    fn undo_stack_is_bounded_at_100() {
        let mut b = InputBuffer::default();
        for _ in 0..120 {
            b.insert('a');
        }
        assert_eq!(b.text, "a".repeat(120));
        for _ in 0..100 {
            b.undo();
        }
        assert_eq!(b.text, "a".repeat(20), "oldest 20 snapshots were dropped");
        b.undo();
        assert_eq!(b.text, "a".repeat(20), "stack exhausted");
    }

    #[test]
    fn unicode_word_ops_and_undo_keep_char_indices() {
        // "héllo 世界": h é l l o ' ' 世 界 = 8 chars (13 bytes != chars)
        let mut b = InputBuffer::default();
        b.insert_str("héllo 世界");
        assert_eq!(b.cursor, 8);
        b.word_left();
        assert_eq!(b.cursor, 6, "back to the start of 世界 (char index)");
        b.insert('!'); // multibyte prefix, insertion at char 6
        assert_eq!(b.text, "héllo !世界");
        assert_eq!(b.cursor, 7);
        b.undo();
        assert_eq!(b.text, "héllo 世界");
        assert_eq!(b.cursor, 6, "undo restores the multi-byte cursor position");

        let mut c = InputBuffer::default();
        c.insert_str("héllo 世界");
        assert_eq!(c.cursor, 8);
        assert_eq!(c.delete_word_backward().as_deref(), Some("世界"));
        assert_eq!(c.text, "héllo ");
        c.undo();
        assert_eq!(c.text, "héllo 世界");
        assert_eq!(c.cursor, 8, "undo restores the end cursor");

        let mut d = InputBuffer::default();
        d.insert_str("世界");
        d.backspace();
        assert_eq!(d.text, "世");
        d.undo();
        assert_eq!(d.text, "世界");

        // word_right over multibyte
        let mut e = InputBuffer {
            text: "héllo 世界".into(),
            cursor: 0,
            ..Default::default()
        };
        e.word_right();
        assert_eq!(e.cursor, 5, "end of 'héllo' (char 5, bytes differ)");
        e.word_right();
        assert_eq!(e.cursor, 8, "end of text");
        assert_eq!(e.char_to_byte(8), e.text.len(), "cursor at byte end");
        // delete_word_forward across the multibyte boundary
        let mut f = InputBuffer {
            text: "héllo 世界".into(),
            cursor: 6,
            ..Default::default()
        };
        assert_eq!(f.delete_word_forward().as_deref(), Some("世界"));
        assert_eq!(f.text, "héllo ");
    }

    #[test]
    fn transcript_renders_once_per_entry_and_resizes() {
        let mut t = Transcript::default();
        t.set_width(40);
        t.push(Line::User("hello".into()));
        t.push(Line::Info("note".into()));
        t.push_tool_call(ToolCall {
            head: "→ read".into(),
            ..no_note_call()
        });
        // push_tool_call lands via push_separated: one air row + the row
        assert_eq!(t.render_passes(), 4, "one pass per pushed entry");
        t.set_width(40); // same width: no rebuild
        assert_eq!(t.render_passes(), 4, "same width must not re-render");
        t.set_width(72); // resize: one pass per entry again
        assert_eq!(t.render_passes(), 8, "resize rebuilds the cache");
        assert!(t.total_rows() > 0);
        assert!(t.row(0).is_some());
        assert!(t.row(t.total_rows()).is_none());
        t.clear();
        assert_eq!(t.total_rows(), 0);
    }

    #[test]
    fn window_range_pins_and_clamps() {
        // everything fits: always pinned from 0
        assert_eq!(window_range(10, 20, None), (0, true));
        assert_eq!(window_range(10, 20, Some(5)), (0, true));
        // pinned tail
        assert_eq!(window_range(100, 20, None), (80, true));
        // anchored mid-history
        assert_eq!(window_range(100, 20, Some(30)), (30, false));
        // anchor past the tail re-pins
        assert_eq!(window_range(100, 20, Some(95)), (80, true));
        // zero visible
        assert_eq!(window_range(100, 0, Some(10)), (0, true));
    }
    #[test]
    fn user_entry_rows_tracks_user_message_positions() {
        let mut t = Transcript::default();
        t.set_width(60);
        t.push(Line::User("hello".into()));
        t.push(Line::Assistant("world".into()));
        t.push(Line::User("again".into()));
        let rows = t.user_entry_rows();
        assert_eq!(rows.len(), 2, "two user entries");
        assert!(rows[0] == 0, "first entry is a user message");
        assert!(
            rows[1] > rows[0] && rows[1] < t.total_rows(),
            "second user entry starts after the first, before the tail"
        );
        // every reported row really is the start of a user band
        for &r in &rows {
            let row = t.row(r).unwrap();
            let bg = row.spans.first().and_then(|s| s.style.bg);
            assert_eq!(bg, Some(crate::palette::BG_USER), "user band at row {r}");
        }
        // resizing keeps the mapping stable
        t.set_width(20);
        assert_eq!(t.user_entry_rows(), rows);
    }
    #[test]
    fn rail_thumb_sizes_and_positions_proportionally() {
        // no rail when everything fits or the track is empty
        assert_eq!(rail_thumb(10, 10, 10, 0), (0, 0));
        assert_eq!(rail_thumb(0, 100, 0, 0), (0, 0));
        // the thumb never collapses to zero cells
        assert_eq!(rail_thumb(10, 1000, 10, 0), (0, 1));
        // half the content visible: half-size thumb, tail-pinned bottom
        assert_eq!(rail_thumb(10, 20, 10, 10), (5, 5));
        // mid-scroll anchor rides proportionally
        assert_eq!(rail_thumb(10, 100, 20, 40), (4, 2));
        // an out-of-range anchor clamps inside the track
        assert_eq!(rail_thumb(10, 20, 10, 999), (5, 5));
    }

    #[test]
    fn rail_drag_anchor_roundtrip() {
        // total 100, visible 10, track 10, thumb 1: dragging the thumb
        // center to the top row anchors at row 0; the bottom row
        // re-pins the tail (None)
        assert_eq!(rail_drag_anchor(0, 1, 10, 100, 10), Some(0));
        assert_eq!(rail_drag_anchor(9, 1, 10, 100, 10), None);
        // mid-track maps proportionally: 4 * 90 / 9 = 40
        assert_eq!(rail_drag_anchor(4, 1, 10, 100, 10), Some(40));
        // a fat thumb: half the track, top → Some(0), bottom → None
        assert_eq!(rail_drag_anchor(2, 5, 10, 20, 10), Some(0));
        assert_eq!(rail_drag_anchor(7, 5, 10, 20, 10), None);
        // degenerate track (thumb fills it): the tail stays pinned
        assert_eq!(rail_drag_anchor(3, 10, 10, 20, 10), None);
    }
    /// One frame and the zones `render` published for it.
    struct PlainFrame {
        terminal: ratatui::Terminal<ratatui::backend::TestBackend>,
        strip: Option<StripZones>,
        arrows: Option<TitleArrows>,
        tx: Option<(ratatui::layout::Rect, usize)>,
    }

    /// Render one idle, captured-mouse frame at `w`×`h` over `t` with
    /// the draft `input` (cursor at its end), an optional modal, and an
    /// optional hover; hand back the terminal plus published zones.
    fn plain_frame(
        t: &mut Transcript,
        (w, h): (u16, u16),
        input: &str,
        sidebar: &SidebarState,
        modal: Option<&Modal>,
        hover: Option<Hover>,
    ) -> PlainFrame {
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;
        let mut terminal = ratatui::Terminal::new(TestBackend::new(w, h)).unwrap();
        let strip = std::cell::Cell::new(None::<StripZones>);
        let arrows = std::cell::Cell::new(None::<TitleArrows>);
        let tx = std::cell::Cell::new(None::<(Rect, usize)>);
        terminal
            .draw(|f| {
                super::render(
                    f,
                    t,
                    None,
                    input,
                    input.chars().count(),
                    false,
                    None,
                    Instant::now(),
                    0,
                    None,
                    None,
                    None,
                    None,
                    None,
                    modal,
                    None,
                    &Meters::default(),
                    sidebar,
                    false,
                    true,
                    "◆",
                    &strip,
                    None,
                    &arrows,
                    &std::cell::Cell::new(None::<Rect>),
                    &std::cell::Cell::new(None::<Rect>),
                    &tx,
                    &std::cell::Cell::new(None::<(Rect, usize, usize, usize, usize)>),
                    &std::cell::Cell::new(0u16),
                    None,
                    hover,
                );
            })
            .unwrap();
        PlainFrame {
            terminal,
            strip: strip.get(),
            arrows: arrows.get(),
            tx: tx.get(),
        }
    }

    /// One buffer row's symbols, concatenated.
    fn buf_row(buf: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    #[test]
    fn visible_rows_carves_out_chrome() {
        // header bar + its row of air + composer + a row of air + status
        assert_eq!(visible_rows(24, 3), 17);
        assert_eq!(visible_rows(5, 3), 0, "never underflows");
        assert_eq!(visible_rows(0, 0), 0);
        // growth of the composer eats the viewport one row at a time
        assert_eq!(visible_rows(24, 8), 12);
    }

    #[test]
    fn frame_layout_header_air_transcript_composer_status() {
        let sidebar = SidebarState {
            cwd: "/tmp/ka".to_string(),
            branch: Some("main".to_string()),
            ..Default::default()
        };
        let f = plain_frame(
            &mut Transcript::default(),
            (120, 40),
            "",
            &sidebar,
            None,
            None,
        );
        let buf = f.terminal.backend().buffer();
        // row 0: the header bar spans the full width on the panel ground,
        // brand left, cwd ⎇ branch right
        for x in 0..120u16 {
            assert_eq!(buf[(x, 0)].bg, crate::palette::BG_PANEL, "header col {x}");
        }
        let header = buf_row(buf, 0);
        assert!(header.contains("◆ ka"), "{header}");
        assert!(header.contains("new session"), "{header}");
        let flat = header.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains(&format!("/tmp/ka {} main", crate::icons::BRANCH)),
            "{flat}"
        );
        // row 1: a row of canvas air
        for x in 0..120u16 {
            assert_eq!(buf[(x, 1)].symbol(), " ", "row 1 col {x} must be blank");
            assert_eq!(buf[(x, 1)].bg, crate::palette::BG, "row 1 rides the canvas");
        }
        // the transcript content starts right under it: no border row
        let (area, start) = f.tx.expect("content rect published");
        assert_eq!((area.y, start), (2, 0));
        assert_eq!(area.height as usize, visible_rows(40, 3));
        assert_eq!(area.width, transcript_width(120));
        // the composer closes with rounded corners (rows 35..=37)
        assert_eq!(buf[(1, 35)].symbol(), "╭");
        assert_eq!(buf[(118, 35)].symbol(), "╮");
        assert_eq!(buf[(1, 37)].symbol(), "╰");
        assert_eq!(buf[(118, 37)].symbol(), "╯");
        // a row of canvas air separates the composer from the status line
        assert!(buf_row(buf, 38).trim().is_empty());
        assert_eq!(buf[(0, 38)].bg, crate::palette::BG);
        // the status line owns the last row, full width, cost on the right
        assert!(buf_row(buf, 39).contains("$0.0000"));
        assert_eq!(buf[(0, 39)].bg, crate::palette::BG_PANEL);
        assert_eq!(buf[(119, 39)].bg, crate::palette::BG_PANEL);
    }

    #[test]
    fn thinking_tail_keeps_its_spacer_above_the_composer() {
        // 80x24: header y0, air y1, transcript y2..=18, composer
        // y19..=21, air y22, status y23. A thinking block closes on its
        // own canvas spacer row: the last transcript row stays blank and
        // the band's text ends one row higher.
        let draw = |last_thought: bool| -> (String, String, String) {
            let mut t = Transcript::default();
            for i in 0..20 {
                t.push(Line::Info(format!("filler row {i}")));
            }
            if last_thought {
                t.push(Line::Thought("ponder\ndeeply".into()));
            } else {
                t.push(Line::Info("tail info".into()));
            }
            let f = plain_frame(&mut t, (80, 24), "", &SidebarState::default(), None, None);
            let buf = f.terminal.backend().buffer();
            (buf_row(buf, 18), buf_row(buf, 17), buf_row(buf, 19))
        };
        let (last, above, composer) = draw(true);
        // the rightmost column is the scrollbar rail, not content
        let content = |row: &str| row.trim_end_matches(['│', '┃', ' ']).to_string();
        assert!(
            content(&last).is_empty(),
            "spacer row stays blank: {last:?}"
        );
        assert!(above.contains("deeply"), "band tail rides above: {above:?}");
        assert!(composer.contains('╭'), "composer opens at y19");
        let (last, _, composer) = draw(false);
        assert!(last.contains("tail info"), "no spacer otherwise: {last:?}");
        assert!(composer.contains('╭'));
    }

    #[test]
    fn hover_entry_lifts_every_content_cell_of_its_rows() {
        // same transcript, hovered vs not: every content-rect cell of the
        // entry's rows takes the hover lift; nothing else changes ground
        let build = || {
            let mut t = Transcript::default();
            for i in 0..14 {
                t.push(Line::Info(format!("filler row {i}")));
            }
            t.push(Line::ToolBlock(vec![ToolCall {
                head: "bash · ls".into(),
                note: "note".into(),
                ..no_note_call()
            }]));
            t
        };
        let mut plain_t = build();
        let entry = plain_t.entries().len() - 1;
        let plain = plain_frame(
            &mut plain_t,
            (80, 24),
            "",
            &SidebarState::default(),
            None,
            None,
        );
        let mut hot_t = build();
        let hot = plain_frame(
            &mut hot_t,
            (80, 24),
            "",
            &SidebarState::default(),
            None,
            Some(Hover::Entry(entry)),
        );
        let (area, start) = hot.tx.expect("content rect published");
        let (first, len) = hot_t.entry_row_span(entry).expect("entry rows");
        let rows: Vec<u16> = (first..first + len)
            .filter(|r| *r >= start && *r < start + area.height as usize)
            .map(|r| area.y + (r - start) as u16)
            .collect();
        assert!(!rows.is_empty(), "the card is on screen");
        let (pb, hb) = (
            plain.terminal.backend().buffer(),
            hot.terminal.backend().buffer(),
        );
        for y in 0..24u16 {
            for x in 0..80u16 {
                if rows.contains(&y) && x >= area.x && x < area.right() {
                    assert_eq!(hb[(x, y)].bg, crate::palette::BG_HOVER, "({x},{y}) lifts");
                } else {
                    assert_eq!(hb[(x, y)].bg, pb[(x, y)].bg, "({x},{y}) keeps its ground");
                }
            }
        }
    }

    #[test]
    fn hover_skips_a_thinking_blocks_canvas_spacers() {
        // a thinking block owns the blank rows before and after it (its
        // click target), but the hover lift must light only the head and
        // body rows — never the air around them
        let build = || {
            let mut t = Transcript::default();
            for i in 0..12 {
                t.push(Line::Info(format!("filler row {i}")));
            }
            t.push(Line::Thought("one\ntwo\nthree\nfour".into()));
            t
        };
        let mut plain_t = build();
        let entry = plain_t.entries().len() - 1;
        let plain = plain_frame(
            &mut plain_t,
            (80, 24),
            "",
            &SidebarState::default(),
            None,
            None,
        );
        let mut hot_t = build();
        let hot = plain_frame(
            &mut hot_t,
            (80, 24),
            "",
            &SidebarState::default(),
            None,
            Some(Hover::Entry(entry)),
        );
        let (area, start) = hot.tx.expect("content rect published");
        let (first, len) = hot_t.entry_row_span(entry).expect("entry rows");
        let (pb, hb) = (
            plain.terminal.backend().buffer(),
            hot.terminal.backend().buffer(),
        );
        let mut lifted = 0;
        let mut grounded = 0;
        for r in first..first + len {
            let blank = hot_t.row(r).is_none_or(|row| row.spans.is_empty());
            let y = area.y + (r.saturating_sub(start)) as u16;
            if r < start || r >= start + area.height as usize || y >= area.bottom() {
                continue;
            }
            for x in area.x..area.right() {
                if blank {
                    assert_eq!(
                        hb[(x, y)].bg,
                        pb[(x, y)].bg,
                        "spacer row ({x},{y}) keeps its ground"
                    );
                } else {
                    assert_eq!(hb[(x, y)].bg, crate::palette::BG_HOVER, "({x},{y}) lifts");
                }
            }
            if blank {
                grounded += 1;
            } else {
                lifted += 1;
            }
        }
        assert!(lifted >= 2, "head + body rows lift: {lifted}");
        assert!(
            grounded >= 2,
            "leading + trailing spacers stay put: {grounded}"
        );
    }

    #[test]
    fn composer_cursor_accounts_for_the_prompt_lead() {
        // 80x24, one draft row: composer y19..=21, x1..=78; the text
        // starts after border + padding + the `❯ ` lead
        let mut f = plain_frame(
            &mut Transcript::default(),
            (80, 24),
            "hello",
            &SidebarState::default(),
            None,
            None,
        );
        let buf = f.terminal.backend().buffer().clone();
        assert_eq!(buf[(3, 20)].symbol(), crate::icons::PROMPT);
        assert_eq!(buf[(5, 20)].symbol(), "h");
        let pos = f.terminal.get_cursor_position().unwrap();
        assert_eq!((pos.x, pos.y), (5 + 5, 20), "cursor sits after `hello`");
        // continuation rows lead two blank cols, so text columns align
        let mut f = plain_frame(
            &mut Transcript::default(),
            (80, 24),
            "a\nbc",
            &SidebarState::default(),
            None,
            None,
        );
        let buf = f.terminal.backend().buffer().clone();
        // two rows grow the composer: y18..=21, content on y19 and y20
        assert_eq!(buf[(3, 19)].symbol(), crate::icons::PROMPT);
        assert_eq!(buf[(5, 19)].symbol(), "a");
        assert_eq!(buf[(3, 20)].symbol(), " ");
        assert_eq!(buf[(5, 20)].symbol(), "b");
        let pos = f.terminal.get_cursor_position().unwrap();
        assert_eq!((pos.x, pos.y), (5 + 2, 20));
        // the wrap width is the box minus borders, padding, and lead
        assert_eq!(input_inner_w(80), 72);
        // an empty draft shows the placeholder after the lead
        let f = plain_frame(
            &mut Transcript::default(),
            (120, 40),
            "",
            &SidebarState::default(),
            None,
            None,
        );
        let row = buf_row(f.terminal.backend().buffer(), 36);
        assert!(row.contains("Ask anything"), "{row}");
    }

    #[test]
    fn strip_zones_land_on_the_drawn_buttons() {
        let f = plain_frame(
            &mut Transcript::default(),
            (120, 40),
            "",
            &SidebarState::default(),
            None,
            None,
        );
        let zones = f.strip.expect("strip zones published");
        let buf = f.terminal.backend().buffer();
        let cells = |r: ratatui::layout::Rect| -> String {
            (r.x..r.right()).map(|x| buf[(x, r.y)].symbol()).collect()
        };
        for (rect, button, icon, label, key) in [
            (
                zones.todos,
                StripButton::Todos,
                crate::icons::STRIP_TODOS,
                "todos",
                "alt+o",
            ),
            (
                zones.skills,
                StripButton::Skills,
                crate::icons::STRIP_SKILLS,
                "skills",
                "^t",
            ),
            (
                zones.info,
                StripButton::Info,
                crate::icons::STRIP_INFO,
                "info",
                "alt+i",
            ),
        ] {
            // the composer's bottom border row (35..=37 at 120x40)
            assert_eq!(rect.y, 37, "{label}");
            assert_eq!(cells(rect), format!(" {icon} {label} {key} "));
            // icon ACCENT, label META, key FAINT
            assert_eq!(buf[(rect.x + 1, 37)].fg, crate::palette::ACCENT);
            assert_eq!(buf[(rect.x + 3, 37)].fg, crate::palette::META);
            assert_eq!(buf[(rect.right() - 2, 37)].fg, crate::palette::FAINT);
            // every drawn cell answers for its button
            for x in rect.x..rect.right() {
                assert_eq!(zones.hit(x, 37), Some(button));
            }
        }
        // the buttons keep the border's corner and lead-in intact
        assert_eq!(buf[(1, 37)].symbol(), "╰");
        assert_eq!(buf[(2, 37)].symbol(), "─");
        assert_eq!(zones.hit(2, 37), None);
    }

    #[test]
    fn hints_ride_the_focused_surface_bottom_border() {
        // idle: the composer's bottom border carries the send hints
        let f = plain_frame(
            &mut Transcript::default(),
            (120, 40),
            "",
            &SidebarState::default(),
            None,
            None,
        );
        assert!(buf_row(f.terminal.backend().buffer(), 37).contains("⏎ send"));
        // a modal takes them onto its own bottom border
        let (terminal, modal_zone, _) =
            draw_overlay(None, None, Some(&Modal::Todos { rows: vec![] }), None);
        let rect = modal_zone.get().expect("modal rect published");
        let buf = terminal.backend().buffer();
        let bottom: String = (rect.x..rect.right())
            .map(|x| buf[(x, rect.bottom() - 1)].symbol())
            .collect();
        assert!(bottom.contains("any close"), "{bottom}");
        assert!(
            !buf_row(buf, 37).contains("send"),
            "composer cedes its hints"
        );
        assert!(buf_row(buf, 37).contains("todos"), "strip buttons stay");
        // the ask carries its own keys
        let ask = PendingAsk {
            id: AskId("a1".into()),
            question: "run it?".into(),
            options: vec!["allow".into(), "deny".into()],
            selected: 0,
            detail: None,
        };
        let (terminal, _, _) = draw_overlay(Some(&ask), None, None, None);
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(text.contains("1-9 pick"), "ask hints drawn");
    }

    #[test]
    fn hint_spans_drop_pairs_from_the_end_to_fit() {
        let pairs = [("⏎", "send"), ("esc", "close")];
        let text = |spans: &[ratatui::text::Span<'static>]| -> String {
            spans.iter().map(|s| s.content.as_ref()).collect()
        };
        let all = hint_spans(&pairs, 80);
        assert_eq!(text(&all), "⏎ send  esc close");
        assert_eq!(all[0].style.fg, Some(crate::palette::ACCENT));
        assert_eq!(all[1].style.fg, Some(crate::palette::FAINT));
        assert_eq!(text(&hint_spans(&pairs, 10)), "⏎ send");
        assert!(hint_spans(&pairs, 3).is_empty());
    }

    #[test]
    fn overlay_shadow_hugs_the_frame_and_clips_to_the_buffer() {
        let (terminal, modal_zone, _) =
            draw_overlay(None, None, Some(&Modal::Todos { rows: vec![] }), None);
        let rect = modal_zone.get().expect("modal rect published");
        let buf = terminal.backend().buffer();
        let shadow = crate::palette::SHADOW;
        // one column right and one row down, offset by one cell
        assert_eq!(buf[(rect.right(), rect.y + 1)].bg, shadow);
        assert_eq!(buf[(rect.right(), rect.bottom())].bg, shadow);
        assert_eq!(buf[(rect.x + 1, rect.bottom())].bg, shadow);
        assert_ne!(buf[(rect.right(), rect.y)].bg, shadow);
        assert_ne!(buf[(rect.x, rect.bottom())].bg, shadow);
        // nothing inside the frame is shaded
        assert_ne!(buf[(rect.x + 2, rect.y + 2)].bg, shadow);
        // an overlay flush with the buffer edge clips instead of panicking
        let area = ratatui::layout::Rect::new(0, 0, 10, 5);
        let mut small = ratatui::buffer::Buffer::empty(area);
        drop_shadow(&mut small, ratatui::layout::Rect::new(5, 2, 5, 3));
        assert!(small.content.iter().all(|c| c.bg != shadow));
    }

    #[test]
    fn modals_dim_the_transcript_backdrop() {
        let draw = |modal: Option<&Modal>| -> ratatui::style::Color {
            let mut t = Transcript::default();
            t.push(Line::Info("backdrop line".into()));
            let f = plain_frame(&mut t, (120, 40), "", &SidebarState::default(), modal, None);
            let buf = f.terminal.backend().buffer();
            let x = (0..120u16)
                .find(|&x| buf[(x, 2)].symbol() == "b")
                .expect("the info row is drawn");
            buf[(x, 2)].fg
        };
        let plain = draw(None);
        let dimmed = draw(Some(&Modal::Todos { rows: vec![] }));
        assert_eq!(dimmed, crate::fx::lerp(plain, crate::palette::BG, 0.55));
        assert_ne!(dimmed, plain);
    }

    #[test]
    fn welcome_shows_the_logo_or_falls_back() {
        let text = |rows: &[ratatui::text::Line<'static>]| -> String {
            rows.iter()
                .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
                .collect()
        };
        let meters = Meters {
            model: "mock-5".into(),
            mode: "readonly".into(),
            ..Default::default()
        };
        let sidebar = SidebarState {
            cwd: "~/src/ka".into(),
            branch: Some("main".into()),
            ..Default::default()
        };
        let big = text(&welcome_rows(80, 30, "◆", &meters, &sidebar));
        assert!(big.contains("██╗"), "the big logo: {big}");
        assert!(big.contains("new conversation"));
        assert!(big.contains("readonly") && big.contains("mock-5"));
        assert!(big.contains(&format!("~/src/ka {} main", crate::icons::BRANCH)));
        for (key, desc) in WELCOME_TIPS {
            assert!(big.contains(key) && big.contains(desc), "tip {key}");
        }
        // narrow or short panes fall back to the one-line mark
        for (w, h) in [(43, 30), (80, 15)] {
            let small = text(&welcome_rows(w, h, "◆", &meters, &sidebar));
            assert!(!small.contains('█'), "{w}x{h}: no logo");
            assert!(small.contains("◆ ka"), "{w}x{h}: {small}");
        }
    }

    #[test]
    fn paging_roundtrip_repins_at_tail() {
        let mut scroll = None;
        page_up(&mut scroll, 100, 20);
        assert_eq!(scroll, Some(80 - 18), "pinned PgUp lands a page up");
        page_up(&mut scroll, 100, 20);
        assert_eq!(scroll, Some(80 - 36));
        page_down(&mut scroll, 100, 20);
        assert_eq!(scroll, Some(80 - 18));
        page_down(&mut scroll, 100, 20);
        assert_eq!(scroll, None, "reaching the tail re-pins");
        // small transcript: PgUp is a no-op
        let mut small = None;
        page_up(&mut small, 5, 20);
        assert_eq!(small, None);
    }

    #[test]
    fn short_session_takes_tail() {
        assert_eq!(short_session("s19a4f2e1b0-3f9c2a81d4b7"), Some("3f9c2a81"));
        assert_eq!(short_session("no-tail"), Some("tail"));
        assert_eq!(short_session(""), None);
    }

    #[test]
    fn resume_hint_needs_a_session_and_activity() {
        let hint = resume_hint("s19a4f2e1b0-3f9c2a81d4b7", 1, false, false).unwrap();
        assert!(hint.contains("ka -c"), "{hint}");
        assert!(hint.contains("ka --session 3f9c2a81"), "{hint}");
        // text only: the terminal's history is never touched
        assert!(!hint.contains("shell-integration"), "{hint}");
        // quitting mid-turn (turns == 0, still busy) still resumes
        assert!(resume_hint("s19a4f2e1b0-3f9c2a81d4b7", 0, true, false).is_some());
        // an older chat replayed at startup, closed with no new turn
        assert!(resume_hint("s19a4f2e1b0-3f9c2a81d4b7", 0, false, true).is_some());
        // a fresh session with no turns, idle: not worth resuming
        assert!(resume_hint("s19a4f2e1b0-3f9c2a81d4b7", 0, false, false).is_none());
        // no session id (engine died at bootstrap): nothing to print
        assert!(resume_hint("", 3, false, true).is_none());
    }

    #[test]
    fn slash_update_is_an_idle_quit_variant() {
        let cmd = slash_command("/update").unwrap();
        assert!(cmd.quit, "update exits the loop");
        assert!(cmd.event.is_none() && cmd.modal.is_none());
        // while busy it must not ride the queue either
        let refusal = queue_refusal("/update").unwrap_or_default();
        assert!(refusal.contains("immediately"), "{refusal}");
    }

    #[test]
    fn session_picker_marks_the_current_session() {
        let picker = SessionPicker {
            current: Some("s1-bbbb22221111".to_string()),
            sessions: vec![
                ka_strand::StrandSummary {
                    path: std::path::PathBuf::from("/tmp/a.jsonl"),
                    id: "s1-aaaa11112222".to_string(),
                    ts: "2026-01-02T00:00:00Z".to_string(),
                    title: "older session".to_string(),
                    messages: 2,
                    cost: 0.0,
                    tokens: 0,
                    parent: None,
                    name: None,
                },
                ka_strand::StrandSummary {
                    path: std::path::PathBuf::from("/tmp/b.jsonl"),
                    id: "s1-bbbb22221111".to_string(),
                    ts: "2026-01-01T00:00:00Z".to_string(),
                    title: "active session".to_string(),
                    messages: 5,
                    cost: 0.0,
                    tokens: 0,
                    parent: None,
                    name: None,
                },
            ],
            selected: 0,
            filter: String::new(),
        };
        let rows = picker.rows();
        assert!(rows[1].0.contains("older session") && !rows[1].0.contains("current"));
        assert!(rows[2].0.contains("active session") && rows[2].0.contains("· current"));
    }

    #[test]
    fn session_picker_filters_and_picks() {
        let picker = SessionPicker {
            current: None,
            sessions: vec![ka_strand::StrandSummary {
                path: std::path::PathBuf::from("/tmp/a.jsonl"),
                id: "s1-aaaa11112222".to_string(),
                ts: "2026-01-01T00:00:00Z".to_string(),
                title: "fix the parser".to_string(),
                messages: 4,
                cost: 0.0,
                tokens: 0,
                parent: None,
                name: None,
            }],
            selected: 1,
            filter: "parser".to_string(),
        };
        let rows = picker.rows();
        assert_eq!(rows.len(), 2, "new-session row + the match");
        assert!(rows[1].0.contains("fix the parser"));
        assert_eq!(picker.pick().as_deref(), Some("s1-aaaa11112222"));

        let mut miss = picker.clone();
        miss.filter = "nomatch".to_string();
        assert_eq!(miss.rows().len(), 1);
        assert_eq!(miss.pick(), None, "only the new-session row remains");

        let mut fresh = picker.clone();
        fresh.selected = 0;
        assert_eq!(fresh.pick(), None, "row 0 = new session");
    }

    #[test]
    fn settings_save_skips_canned_model_placeholder() {
        // mirrored logic from the 's' handler: the guard is one expression
        let panel_model = "(canned)";
        let model = (panel_model != "(canned)").then(|| panel_model.to_string());
        assert_eq!(model, None);
        let model = (panel_model == "x/y").then(|| panel_model.to_string());
        assert_eq!(model, None);
        let real = "groq/llama-3.3-70b";
        let model = (real != "(canned)").then(|| real.to_string());
        assert_eq!(model.as_deref(), Some("groq/llama-3.3-70b"));
    }

    #[test]
    fn settings_effort_cycles_all_variants() {
        use ka_protocol::Effort;
        let mut panel = SettingsPanel {
            model: "x/y".to_string(),
            mode: ka_protocol::Mode::Ask,
            effort: None,
            selected: 0,
            edit: None,
            providers: vec![],
            config_path: String::new(),
        };
        let seq = [
            Some(Effort::Low),
            Some(Effort::Medium),
            Some(Effort::High),
            Some(Effort::Max),
            None,
        ];
        for expected in seq {
            panel.cycle_effort();
            assert_eq!(panel.effort, expected);
        }
    }

    #[test]
    fn slash_commands_include_session_and_settings() {
        let names: Vec<String> = available_slash_commands()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        for want in [
            "/session",
            "/resume",
            "/new",
            "/settings",
            "/export",
            "/retry",
            "/copy",
            "/usage",
            // dispatchable all along, now cataloged too
            "/image",
            "/agents",
        ] {
            assert!(names.contains(&want.to_string()), "missing {want}");
        }
        // customs are named like any slash command, so the popup
        // prefix-filter matches and Tab inserts a usable command
        assert!(
            names.iter().all(|n| n.starts_with('/')),
            "every catalog entry is a /command: {names:?}"
        );
        // popup filter serves /he → /help (prefix match over the names)
        let popup = update_suggestions("/he").expect("popup for /he");
        assert!(popup.items.iter().any(|(n, _)| n == "/help"), "{popup:?}");
    }

    #[test]
    fn rewind_bad_arg_warns_instead_of_defaulting() {
        // unparsable count: usage warning, no command sent
        let cmd = slash_command("/rewind abc").unwrap();
        assert_eq!(cmd.note.as_deref(), Some("usage: /rewind [n]"));
        assert!(cmd.event.is_none());
        // bare /rewind keeps its documented default of one exchange
        let cmd = slash_command("/rewind").unwrap();
        assert!(matches!(cmd.event, Some(Command::Rewind { turns: 1 })));
        // an explicit count passes through
        let cmd = slash_command("/rewind 3").unwrap();
        assert!(matches!(cmd.event, Some(Command::Rewind { turns: 3 })));
    }

    #[test]
    fn help_lists_all_keys_and_groups() {
        let text: String = help_modal_rows(76)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        // the previously undocumented keys are taught
        for key in [
            "ctrl+r",
            "ctrl+p / ctrl+n",
            "ctrl+q",
            "ctrl+o",
            "ctrl+m",
            "ctrl+t / alt+o / alt+i",
            "alt+t",
            "alt+e",
            "+text · alt+q",
            "ctrl+↑ / ctrl+↓",
            "esc esc",
        ] {
            assert!(text.contains(key), "help lacks {key}");
        }
        // command groups in order
        for group in [
            "keys",
            "session",
            "model & providers",
            "tools & context",
            "safety & plans",
            "meta",
        ] {
            // a section head is the group name followed by its rule
            assert!(
                text.contains(&format!("{group} ─")),
                "help lacks group {group}"
            );
        }
        // within a group every description starts at one column, and no
        // key (however long) touches its description
        let rows = help_modal_rows(76);
        let group: Vec<&ratatui::text::Line> = rows
            .iter()
            .skip(1)
            .take_while(|l| l.spans.len() == 2 && !l.spans[1].content.starts_with(" ─"))
            .collect();
        assert!(group.len() > 10, "keys group rows: {}", group.len());
        let col = unicode_width::UnicodeWidthStr::width(group[0].spans[0].content.as_ref());
        for l in &group {
            let key = l.spans[0].content.as_ref();
            assert_eq!(unicode_width::UnicodeWidthStr::width(key), col, "{l:?}");
            assert!(key.ends_with("  "), "key collides with its text: {l:?}");
        }

        assert!(matches!(
            slash_command("/new"),
            Some(Slash {
                note: None,
                event: Some(Command::SwitchStrand { id }),
                ..
            }) if id == "new"
        ));
        assert!(matches!(
            slash_command("/settings"),
            Some(Slash {
                note: None,
                modal: Some(ModalKind::Settings),
                ..
            })
        ));
        assert!(matches!(
            slash_command("/resume"),
            Some(Slash {
                note: None,
                modal: Some(ModalKind::Session),
                ..
            })
        ));
    }

    #[test]
    fn usage_rows_render_session_totals_and_recent() {
        let meters = Meters {
            turns: 3,
            tokens_in: 1500,
            tokens_out: 300,
            cache_read: 900,
            cost: 0.0123,
            elapsed: 95.0,
            ..Default::default()
        };
        let sessions = vec![
            ka_strand::StrandSummary {
                path: std::path::PathBuf::from("/tmp/a.jsonl"),
                id: "s1-aaaa1111".into(),
                ts: "2026-09-01T10:00:00Z".into(),
                title: "fix the parser".into(),
                messages: 12,
                cost: 0.5,
                parent: None,
                name: None,
                tokens: 1234,
            },
            ka_strand::StrandSummary {
                path: std::path::PathBuf::from("/tmp/b.jsonl"),
                id: "s1-bbbb2222".into(),
                ts: "2026-09-02T10:00:00Z".into(),
                title: "tidy docs".into(),
                messages: 4,
                cost: 0.003,
                parent: None,
                name: None,
                tokens: 0,
            },
        ];
        let rows = usage_rows(&meters, &sessions);
        let joined = rows.join("\n");
        assert_eq!(rows[0], "▸ session", "{joined}");
        assert!(joined.contains("3 turns"), "{joined}");
        assert!(joined.contains("1.5k in / 300 out"), "{joined}");
        assert!(joined.contains("cache 900"), "{joined}");
        assert!(joined.contains("hit 60%"), "{joined}");
        assert!(joined.contains("$0.0123"), "{joined}");
        assert!(joined.contains("1:35 busy"), "{joined}");
        // recent sessions: title, age, tokens, cost
        assert!(joined.contains("fix the parser"), "{joined}");
        assert!(joined.contains("1.2k tok · $0.50"), "{joined}");
        // total row sums the listed sessions
        assert!(
            joined.contains("total · 2 sessions · 1.2k tok · $0.50"),
            "{joined}"
        );
        // empty history degrades to a placeholder, not a bare section
        let empty = usage_rows(&Meters::default(), &[]);
        assert!(empty.join("\n").contains("(no recorded sessions)"));
    }

    #[test]
    fn approve_matches_build_commands() {
        let build = slash_command("/build").unwrap();
        let approve = slash_command("/approve").unwrap();
        let guard = |sl: &Slash| {
            matches!(
                sl.event,
                Some(Command::SetMode {
                    mode: ka_protocol::Mode::Ask,
                })
            )
        };
        assert!(guard(&build) && guard(&approve), "both flip to Guarded");
        assert_eq!(build.followup, approve.followup, "shared followup");
        assert!(
            approve
                .followup
                .as_deref()
                .is_some_and(|f| f.contains(".ka/plans/plan.md")),
            "{:?}",
            approve.followup
        );
    }

    #[test]
    fn plan_drafted_needs_a_file_fresh_after_start() {
        let dir = std::env::temp_dir().join(format!("ka-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let plan = dir.join("plan.md");
        let started = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        // no file yet → not drafted
        assert!(!plan_drafted(Some(started), &plan));
        std::fs::write(&plan, "# plan\n").unwrap();
        // file written after start → drafted
        assert!(plan_drafted(Some(started), &plan));
        // start in the future (file older) → not drafted
        assert!(!plan_drafted(
            Some(std::time::SystemTime::now() + std::time::Duration::from_secs(60)),
            &plan
        ));
        // never started planning → not drafted
        assert!(!plan_drafted(None, &plan));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn context_rows_renders_bars_and_footer() {
        let parts = vec![
            ka_protocol::ContextPart {
                name: "system".into(),
                tokens: 800,
            },
            ka_protocol::ContextPart {
                name: "user".into(),
                tokens: 200,
            },
        ];
        let rows = context_rows(&parts, 10_000);
        let joined = rows.join("\n");
        assert!(joined.contains("system"), "{joined}");
        assert!(joined.contains("█"), "{joined}");
        assert!(joined.contains("200 (20%)"), "{joined}");
        assert!(
            joined.contains("used 1k of 10k (10%) · free 9k"),
            "{joined}"
        );
        let rows = context_rows(&parts, 0);
        assert!(rows.join("\n").contains("free 0"), "{rows:?}");
    }

    #[test]
    fn input_buffer_edits_and_history() {
        let mut b = InputBuffer::default();
        for c in "héllo".chars() {
            b.insert(c);
        }
        assert_eq!(b.text, "héllo");
        b.left();
        b.backspace(); // cursor was between 'l' and 'o': removes the second 'l'
        assert_eq!(b.text, "hélo");
        assert_eq!(b.cursor, 3);
        b.end();
        b.insert('!');
        assert_eq!(b.text, "hélo!");
        let taken = b.take();
        assert_eq!(taken, "hélo!");
        assert!(b.text.is_empty());

        b.insert('x');
        let _ = b.take();
        b.history_prev();
        assert_eq!(b.text, "x");
        b.history_prev();
        assert_eq!(b.text, "hélo!");
        b.history_next();
        b.history_next();
        assert!(b.text.is_empty());
    }
    #[test]
    fn newline_and_backspace_roundtrip() {
        let mut b = InputBuffer::default();
        for c in "ab".chars() {
            b.insert(c);
        }
        b.newline();
        for c in "cd".chars() {
            b.insert(c);
        }
        assert_eq!(b.text, "ab\ncd");
        assert_eq!(b.rows(), vec!["ab", "cd"]);
        b.backspace();
        b.backspace();
        b.backspace();
        assert_eq!(b.text, "ab", "backspaces remove c, d, then the newline");
    }

    #[test]
    fn cursor_row_col_tracks_newlines() {
        let mut b = InputBuffer::default();
        for c in "ab\ncd\né".chars() {
            b.insert(c);
        }
        assert_eq!(b.cursor_row_col(), (2, 1));
        b.left();
        b.left();
        assert_eq!(b.cursor_row_col(), (1, 2));
        b.home();
        assert_eq!(b.cursor_row_col(), (0, 0));
    }

    #[test]
    fn insert_str_normalizes_crlf() {
        let mut b = InputBuffer::default();
        b.insert_str("pasted\r\nmulti\rline");
        assert_eq!(b.text, "pasted\nmulti\nline");
        assert_eq!(b.cursor_row_col(), (2, 4));
        b.insert('!');
        assert_eq!(b.text, "pasted\nmulti\nline!");
    }

    #[test]
    fn multiline_history_bypass() {
        let mut b = InputBuffer::default();
        for c in "old".chars() {
            b.insert(c);
        }
        b.take(); // history: ["old"]
        for c in "one\ntwo".chars() {
            b.insert(c);
        }
        assert_eq!(b.cursor_row_col(), (1, 3));
        // Up on multiline navigates rows, never history
        assert!(b.move_up());
        assert_eq!(b.cursor_row_col(), (0, 3), "column clamped to row end");
        assert_eq!(b.text, "one\ntwo", "history never overwrites the draft");
        assert!(b.move_down());
        assert_eq!(b.cursor_row_col(), (1, 3));
        // single-line text stays history territory
        let mut c = InputBuffer::default();
        for ch in "single".chars() {
            c.insert(ch);
        }
        assert!(!c.move_up());
    }

    #[test]
    fn spin_frame_cycles_braille_set() {
        let frames = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";
        assert_eq!(spin_frame(0), '⠋');
        assert_eq!(spin_frame(79), '⠋', "same frame within one tick");
        assert_eq!(spin_frame(80), '⠙');
        // wraps after the full set
        assert_eq!(spin_frame(80 * frames.chars().count() as u128), '⠋');
        for ms in (0..3000).step_by(40) {
            assert!(frames.contains(spin_frame(ms)));
        }
    }
    #[test]
    fn working_row_spins_then_waits_for_approval() {
        let t0 = Instant::now();
        let now = t0 + Duration::from_millis(1500);
        // streaming: a spinner glyph leads, the shimmering label, then
        // elapsed seconds and the interrupt hint trail
        let row = working_row(None, Some(t0), now);
        let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.ends_with("Working · 1.5s · esc to interrupt"),
            "{text}"
        );
        assert!(
            "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(text.chars().next().unwrap()),
            "spinner glyph leads: {text}"
        );
        // permission ask up: one static line, no spinner, no shimmer
        let ask = PendingAsk {
            id: ka_protocol::AskId("a".into()),
            question: "allow?".into(),
            options: vec!["yes".into()],
            detail: None,
            selected: 0,
        };
        let row = working_row(Some(&ask), Some(t0), now);
        let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "◆ waiting for approval · 1.5s");
        assert_eq!(row.spans.len(), 1, "static: {row:?}");
        // a missing clock degrades to zero elapsed, never panics
        let row = working_row(None, None, now);
        let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.ends_with("Working · 0.0s · esc to interrupt"),
            "{text}"
        );
    }

    #[test]
    fn live_region_lands_where_the_flush_will() {
        let text_of = |l: &ratatui::text::Line| -> String {
            l.spans.iter().map(|s| s.content.to_string()).collect()
        };
        let t0 = Instant::now();
        let now = t0 + Duration::from_millis(900);
        let block = |thought: &str, md: &str| LiveBlock {
            thought: thought.into(),
            tool_header: String::new(),
            live_tool: None,
            md: if md.is_empty() {
                Vec::new()
            } else {
                crate::markdown::render(md, assistant_md_width(40))
            },
        };
        let no_double_air = |rows: &[ratatui::text::Line<'static>]| {
            for pair in rows.windows(2) {
                assert!(
                    !(pair[0].spans.is_empty() && pair[1].spans.is_empty()),
                    "air never doubles: {rows:?}"
                );
            }
        };
        // nothing streamed yet: the working row between air rows
        let rows = live_region_rows(&block("", ""), None, Some(t0), now, 40);
        assert_eq!(rows.len(), 3);
        assert!(rows[0].spans.is_empty() && rows[2].spans.is_empty());
        assert!(text_of(&rows[1]).contains("Working"));

        // thinking: row-for-row the band the flush caches
        let rows = live_region_rows(&block("a\nb\nc\nd", ""), None, Some(t0), now, 40);
        let cached = thought_rows_tail("a\nb\nc\nd", 40, None);
        let live: Vec<String> = rows[..cached.len()].iter().map(text_of).collect();
        let flushed: Vec<String> = cached.iter().map(text_of).collect();
        assert_eq!(live, flushed, "hand-off never jumps");
        assert_eq!(rows.len(), cached.len() + 2, "band + working + air");
        no_double_air(&rows);

        // streaming text: the cursor rides the last row, and the row is
        // the cached assistant row at the same width and indent
        let rows = live_region_rows(&block("", "hello there"), None, Some(t0), now, 40);
        let settled = render_line(&Line::Assistant("hello there".into()), 40, false);
        assert!(rows[0].spans.is_empty(), "the separator's air leads");
        assert_eq!(text_of(&rows[1]), format!("{}▌", text_of(&settled[0])));
        assert!(text_of(&rows[1]).starts_with("  hello there"));
        no_double_air(&rows);

        // thinking then text: the band's trailing spacer is the air
        let rows = live_region_rows(&block("why", "because"), None, Some(t0), now, 40);
        no_double_air(&rows);
        assert!(rows.iter().any(|r| text_of(r).starts_with("  because")));
    }

    #[test]
    fn live_stale_gates_reparse() {
        let t0 = Instant::now();
        assert!(!live_stale(t0, t0), "fresh cache is reused");
        assert!(!live_stale(t0, t0 + Duration::from_millis(79)));
        assert!(live_stale(t0, t0 + Duration::from_millis(80)));
        assert!(live_stale(t0, t0 + Duration::from_secs(5)));
    }

    #[test]
    fn activity_reports_blocked_over_working_and_coalesces() {
        use AgentActivity::{Blocked, Idle, Working};
        assert_eq!(activity_of(false, false), Idle);
        assert_eq!(activity_of(true, false), Working);
        assert_eq!(activity_of(true, true), Blocked, "an ask outranks the turn");
        assert_eq!(activity_of(false, true), Blocked);
        // latest-wins: a slow consumer never replays a stale state
        let sink = ActivitySink::new();
        sink.publish(AgentReport::Report(ActivityReport {
            activity: Working,
            session: "s1".into(),
        }));
        sink.publish(AgentReport::Report(ActivityReport {
            activity: Idle,
            session: "s1".into(),
        }));
        assert_eq!(
            sink.wait(),
            Some(AgentReport::Report(ActivityReport {
                activity: Idle,
                session: "s1".into()
            }))
        );
        sink.publish(AgentReport::Release);
        assert_eq!(sink.wait(), Some(AgentReport::Release));
    }

    #[test]
    fn status_line_splits_meters_and_always_shows_cost() {
        let m = Meters {
            model: "ollama/qwen3.5:9b".into(),
            mode: "allow".into(),
            mode_icon: crate::icons::mode_icon(ka_protocol::Mode::Allow).into(),
            context: (10_000, 100_000),
            cost: 0.0123,
            ..Default::default()
        };
        let text = |spans: &[ratatui::text::Span<'static>]| {
            spans.iter().map(|s| s.content.as_ref()).collect::<String>()
        };
        // left: the model in INK_DIM (the mode pill lives at the far
        // right now)
        let left = status_left(&m);
        assert_eq!(text(&left), "ollama/qwen3.5:9b");
        assert_eq!(left[0].style.fg, Some(crate::palette::INK_DIM));
        // right: 10% of the window fills one of ten gauge cells, then the
        // percentage, the cost, and the mode pill last (dark chip ink on
        // the mode's color)
        let right = status_right(&m);
        assert_eq!(
            text(&right),
            format!(
                "━━━━━━━━━━ 10%  $0.0123  {} allow ",
                crate::icons::mode_icon(ka_protocol::Mode::Allow)
            )
        );
        assert_eq!(right[0].content, "━");
        assert_eq!(right[0].style.fg, Some(crate::palette::OK));
        assert_eq!(right[1].style.fg, Some(crate::palette::RULE));
        let pill = right.last().expect("pill closes the row");
        assert_eq!(
            pill.style.bg,
            Some(crate::palette::mode_color(ka_protocol::Mode::Allow))
        );
        assert_eq!(pill.style.fg, Some(crate::palette::CHIP_FG));
        assert!(pill.content.ends_with("allow "), "{pill:?}");
        // effort rides its battery segment when the model supports
        // reasoning; unsupported models hide it; an unknown level keeps
        // the word and drops the glyph
        let high = Meters {
            effort: "high".into(),
            effort_supported: true,
            cost: 0.5,
            ..Default::default()
        };
        assert_eq!(
            text(&status_left(&high)),
            format!("{} ◕ high", crate::icons::THOUGHT)
        );
        assert_eq!(text(&status_right(&high)), "$0.5000");
        let odd = Meters {
            effort: "turbo".into(),
            effort_supported: true,
            ..Default::default()
        };
        assert_eq!(
            text(&status_left(&odd)),
            format!("{} turbo", crate::icons::THOUGHT)
        );
        // a supported model with no level yet reads as off
        let unset = Meters {
            effort_supported: true,
            ..Default::default()
        };
        assert_eq!(
            text(&status_left(&unset)),
            format!("{} ○ off", crate::icons::THOUGHT)
        );
        // unsupported: no segment even with a stale level string
        let unsupported = Meters {
            effort: "high".into(),
            ..Default::default()
        };
        assert!(status_left(&unsupported).is_empty());
        // fresh session: unknown facts collapse to the cost alone
        assert!(status_left(&Meters::default()).is_empty());
        assert_eq!(text(&status_right(&Meters::default())), "$0.0000");
        // an unknown window still reports what was used
        let estimate = Meters {
            context: (1_500, 0),
            ..Default::default()
        };
        assert!(text(&status_right(&estimate)).contains(" tok  $0.0000"));
    }
    #[test]
    fn ctx_gauge_cells_and_thresholds() {
        // (filled cells, their color); the gauge is always ten cells
        let gauge = |used, window| {
            let m = Meters {
                context: (used, window),
                ..Default::default()
            };
            let spans = status_right(&m);
            let cells: usize = spans.iter().map(|s| s.content.matches('━').count()).sum();
            assert_eq!(cells, CTX_GAUGE_CELLS);
            spans
                .iter()
                .find(|s| s.content.contains('━') && s.style.fg != Some(crate::palette::RULE))
                .map_or((0, None), |s| (s.content.chars().count(), s.style.fg))
        };
        // 10% per cell, thresholds at 80% (WARN) and 95% (ERR)
        assert_eq!(gauge(20_000, 100_000).0, 2);
        assert_eq!(gauge(50_000, 100_000).0, 5);
        assert_eq!(gauge(79_000, 100_000).1, Some(crate::palette::OK));
        assert_eq!(gauge(80_000, 100_000).1, Some(crate::palette::WARN));
        assert_eq!(gauge(94_000, 100_000).1, Some(crate::palette::WARN));
        assert_eq!(gauge(95_000, 100_000).1, Some(crate::palette::ERR));
        assert_eq!(gauge(100_000, 100_000).0, 10);
        // rounding at cell edges: 5% = half a cell rounds to one; less
        // stays an empty track
        assert_eq!(gauge(5_000, 100_000).0, 1);
        assert_eq!(gauge(4_000, 100_000), (0, None));
    }

    #[test]
    fn user_band_spans_full_width() {
        use super::push_block;
        use ratatui::text::Line as TuiLine;

        fn row_width(line: &TuiLine) -> usize {
            line.spans.iter().map(|s| s.content.chars().count()).sum()
        }
        let text =
            |l: &TuiLine| -> String { l.spans.iter().map(|s| s.content.to_string()).collect() };

        // user band: padded to exactly the width, no pad rows
        let mut out: Vec<TuiLine> = Vec::new();
        push_block(&mut out, "short text", 40);
        assert_eq!(out.len(), 1, "text rows only: air is the separator's job");
        assert_eq!(row_width(&out[0]), 40, "band must span exactly the width");
        // wrapped rows: every row rides the rail, spans the width, and
        // breaks between words (hanging indent 2)
        let mut out2: Vec<TuiLine> = Vec::new();
        push_block(&mut out2, &"word ".repeat(30), 40);
        assert!(out2.len() > 2, "{out2:?}");
        for (i, line) in out2.iter().enumerate() {
            assert_eq!(
                row_width(line),
                40,
                "row {i} of wrapped band must span the width"
            );
            assert!(
                text(line).starts_with("▌ word"),
                "row {i}: {:?}",
                text(line)
            );
            assert!(
                text(line).trim_end().ends_with("word"),
                "word-wrapped: {:?}",
                text(line)
            );
        }
        // an empty message still owns one rail row
        let mut out3: Vec<TuiLine> = Vec::new();
        push_block(&mut out3, "", 40);
        assert_eq!(out3.len(), 1);
    }

    #[test]
    fn gutter_rows_wrap_without_padding() {
        use super::push_gutter;
        use ratatui::text::Line as TuiLine;
        use unicode_width::UnicodeWidthStr;

        fn row_width(line: &TuiLine) -> usize {
            line.spans.iter().map(|s| s.content.chars().count()).sum()
        }
        let text =
            |l: &TuiLine| -> String { l.spans.iter().map(|s| s.content.to_string()).collect() };

        let mut out: Vec<TuiLine> = Vec::new();
        push_gutter(
            &mut out,
            "short text",
            40,
            "• ",
            crate::palette::META.into(),
        );
        assert_eq!(out.len(), 1, "text rows only: air is the separator's job");
        assert!(row_width(&out[0]) <= 40, "gutter rows never pad to width");

        // an unbreakable token hard-wraps at the width; continuation
        // rows hang under the text by the prefix's display width
        let prefix = format!("{} ", crate::icons::THOUGHT);
        let indent = " ".repeat(prefix.width());
        let mut out2: Vec<TuiLine> = Vec::new();
        push_gutter(
            &mut out2,
            &"x".repeat(100),
            40,
            &prefix,
            crate::palette::THOUGHT,
        );
        assert!(out2.len() > 2);
        for (i, line) in out2.iter().enumerate() {
            let text = text(line);
            assert!(text.width() <= 40, "row {i} overflows the width");
            if i > 0 {
                assert!(
                    text.starts_with(&format!("{indent}x")),
                    "continuation indents by the prefix width: {text:?}"
                );
            } else {
                assert!(text.starts_with(&prefix));
            }
        }
        // prose breaks between words
        let mut out3: Vec<TuiLine> = Vec::new();
        push_gutter(
            &mut out3,
            "alpha beta gamma",
            14,
            "• ",
            crate::palette::META.into(),
        );
        let rows: Vec<String> = out3.iter().map(text).collect();
        assert_eq!(rows, vec!["• alpha beta", "  gamma"]);

        // degenerate width still terminates
        let mut out4: Vec<TuiLine> = Vec::new();
        push_gutter(
            &mut out4,
            "abc",
            0,
            "✗ ",
            ratatui::style::Style::new().fg(crate::palette::ERR),
        );
        assert!(!out4.is_empty());
    }

    #[test]
    fn assistant_entry_sits_on_the_canvas_indented() {
        // no slab: assistant rows carry no background and no padding
        // rows; markdown folds at W-2 behind a two-column canvas indent
        let row_text = |l: &ratatui::text::Line<'static>| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        let out = super::render_line(&Line::Assistant("**hi** there".into()), 40, false);
        assert_eq!(out.len(), 1, "content only: {out:?}");
        assert_eq!(row_text(&out[0]), "  hi there");
        assert!(
            out[0].spans.iter().all(|s| s.style.bg.is_none()),
            "canvas, not a slab: {out:?}"
        );
        // long prose wraps inside the indent: every row fits W
        use unicode_width::UnicodeWidthStr;
        let long = super::render_line(&Line::Assistant("word ".repeat(40)), 40, false);
        assert!(long.len() > 1);
        for row in &long {
            let t = row_text(row);
            assert!(t.starts_with("  "), "{t:?}");
            assert!(t.width() <= 40, "{t:?}");
        }
    }

    #[test]
    fn multiline_band_rails_every_row() {
        use super::push_block;
        use ratatui::text::Line as TuiLine;

        let mut out: Vec<TuiLine> = Vec::new();
        push_block(&mut out, "alpha\nbeta", 40);
        let text = |l: &TuiLine| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        assert_eq!(out.len(), 2, "one row per source line, no pad rows");
        assert!(text(&out[0]).starts_with("▌ alpha"));
        assert!(
            text(&out[1]).starts_with("▌ beta"),
            "every row rides the rail"
        );
        for row in &out {
            assert!(
                row.spans
                    .iter()
                    .all(|s| s.style.bg == Some(crate::palette::BG_USER)),
                "the band is BG_USER edge to edge: {row:?}"
            );
            assert_eq!(row.spans[0].style.fg, Some(crate::palette::USER));
        }
    }

    #[test]
    fn multiline_gutter_prefixes_only_first_row() {
        use super::push_gutter;
        use ratatui::text::Line as TuiLine;

        let mut out: Vec<TuiLine> = Vec::new();
        push_gutter(&mut out, "one\ntwo", 40, "⚙ ", crate::palette::META.into());
        let text = |l: &TuiLine| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        assert!(text(&out[0]).starts_with("⚙ one"));
        assert!(
            text(&out[1]).starts_with("  two"),
            "second source line indents"
        );
    }
    #[test]
    fn custom_command_loads_and_substitutes() {
        let dir = std::env::temp_dir().join(format!("ka-cmd-{}", std::process::id()));
        let state = std::env::temp_dir().join(format!("ka-cmd-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state);
        let cmds = dir.join(".ka/commands");
        std::fs::create_dir_all(&cmds).unwrap();
        std::fs::write(cmds.join("review.md"), "Review this diff: $ARGUMENTS").unwrap();
        // untrusted project: the command must NOT dispatch
        assert!(
            custom_command_in(&dir, &state, "/review", Some("src/main.rs")).is_none(),
            "untrusted project commands stay gated"
        );
        // trust the project: dispatch substitutes $ARGUMENTS
        std::fs::create_dir_all(state.join("ka")).unwrap();
        ka_engine::trust::save_trust_at(&state.join("ka/trust.json"), std::slice::from_ref(&dir));
        let (fm, body) = custom_command_in(&dir, &state, "/review", Some("src/main.rs")).unwrap();
        assert_eq!(body, "Review this diff: src/main.rs");
        assert!(fm.allowed_tools.is_empty() && fm.model.is_none());

        // frontmatter is stripped from the body and surfaces in the scan
        std::fs::write(
            cmds.join("ship.md"),
            "---\ndescription: ship it\nargument-hint: branch\n---\nShip {branch}: $ARGUMENTS",
        )
        .unwrap();
        let scanned = scan_custom_commands_in(&dir, &state);
        let ship = scanned.iter().find(|c| c.name == "ship").unwrap();
        assert_eq!(ship.description, "ship it");
        assert_eq!(ship.argument_hint, "branch");
        let (fm, body) = custom_command_in(&dir, &state, "/ship", Some("main")).unwrap();
        assert_eq!(body, "Ship {branch}: main");
        assert!(fm.description == "ship it" && fm.model.is_none());

        // Phase 9.7: allowed-tools + model frontmatter parse through
        std::fs::write(
            cmds.join("audit.md"),
            "---\ndescription: audit\nallowed-tools: read, grep glob\nmodel: zai/glm-5.3@low\n---\nAudit $ARGUMENTS",
        )
        .unwrap();
        let (fm, body) = custom_command_in(&dir, &state, "/audit", Some("src")).unwrap();
        assert_eq!(fm.allowed_tools, vec!["read", "grep", "glob"]);
        assert_eq!(fm.model.as_deref(), Some("zai/glm-5.3@low"));
        assert_eq!(body, "Audit src");
        let (fm, _) = parse_command_md("---\nallowed-tools: [read, write]\n---\nbody");
        assert_eq!(
            fm.allowed_tools,
            vec!["read", "write"],
            "bracketed lists parse too"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn slash_commands_parse() {
        assert!(slash_command("/quit").unwrap().quit);
        let m = slash_command("/model openai/gpt-5.1").unwrap();
        assert!(!m.quit);
        assert!(matches!(m.event, Some(Command::SetModel { .. })));
        let md = slash_command("/mode allow").unwrap();
        assert!(matches!(
            md.event,
            Some(Command::SetMode {
                mode: ka_protocol::Mode::Allow
            })
        ));
        // the friendly words parse to the same tiers
        for (word, want) in [
            ("ask", ka_protocol::Mode::Ask),
            ("accept_edits", ka_protocol::Mode::AcceptEdits),
            ("allow", ka_protocol::Mode::Allow),
            ("readonly", ka_protocol::Mode::Readonly),
        ] {
            let s = slash_command(&format!("/mode {word}")).unwrap();
            assert!(
                matches!(s.event, Some(Command::SetMode { mode }) if mode == want),
                "/mode {word}"
            );
        }
        // every legacy slug keeps parsing
        for (slug, want) in [
            ("guarded", ka_protocol::Mode::Ask),
            ("free", ka_protocol::Mode::Allow),
            ("plan", ka_protocol::Mode::Readonly),
            ("needs-approval", ka_protocol::Mode::Ask),
            ("needs_approval", ka_protocol::Mode::Ask),
            ("accept edits", ka_protocol::Mode::AcceptEdits),
            ("accept-edits", ka_protocol::Mode::AcceptEdits),
            ("full-access", ka_protocol::Mode::Allow),
            ("full_access", ka_protocol::Mode::Allow),
        ] {
            let s = slash_command(&format!("/mode {slug}")).unwrap();
            assert!(
                matches!(s.event, Some(Command::SetMode { mode }) if mode == want),
                "/mode {slug}"
            );
        }
        // footer labels round-trip through the parser (settings bootstrap)
        for mode in [
            ka_protocol::Mode::Ask,
            ka_protocol::Mode::AcceptEdits,
            ka_protocol::Mode::Allow,
            ka_protocol::Mode::Readonly,
        ] {
            assert_eq!(mode_from_label(mode_label(mode)), mode, "{mode:?}");
        }
        assert!(slash_command("plain text").is_none());
        assert!(
            matches!(
                slash_command("/model"),
                Some(Slash {
                    note: None,
                    modal: Some(ModalKind::Model(None)),
                    ..
                })
            ),
            "bare /model opens the picker"
        );
        let with_path = slash_command("/export o.md").unwrap();
        assert!(matches!(
            &with_path.event,
            Some(Command::ExportMarkdown {
                out: Some(p),
                html: false
            }) if p == &std::path::PathBuf::from("o.md")
        ));
        assert!(matches!(
            &slash_command("/export").unwrap().event,
            Some(Command::ExportMarkdown {
                out: None,
                html: false
            })
        ));
        // --html flips the renderer and still accepts a path
        assert!(matches!(
            &slash_command("/export --html page.html").unwrap().event,
            Some(Command::ExportMarkdown {
                out: Some(p),
                html: true
            }) if p == &std::path::PathBuf::from("page.html")
        ));
    }

    #[test]
    fn slash_tasks_and_debug_parse() {
        assert!(matches!(
            slash_command("/tasks").unwrap().event,
            Some(Command::ListTasks)
        ));
        assert!(matches!(
            slash_command("/debug").unwrap().event,
            Some(Command::DebugRoster)
        ));
        // both appear in the help listing
        let names: Vec<String> = builtin_slash_commands()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(
            names.iter().filter(|n| n.as_str() == "/tasks").count(),
            1,
            "one /tasks entry: {names:?}"
        );
        assert!(names.contains(&"/debug".to_string()), "{names:?}");
    }

    #[test]
    fn pager_visible_matches_render_clamp() {
        // short pane: the window shrinks with the pane, so the top of a
        // long result stays reachable (render: height = min(n+5, 30,
        // pane) and visible = height - 5)
        assert_eq!(pager_visible(24), 19);
        assert_eq!(pager_visible(10), 5);
        // degenerate pane: at least one row
        assert_eq!(pager_visible(4), 1);
        // tall pane: capped at the box's own cap
        assert_eq!(pager_visible(80), 25);
    }

    #[test]
    fn task_id_of_row_parses_roster_shapes() {
        assert_eq!(task_id_of_row("t-3  running  12s  coder — audit"), Some(3));
        assert_eq!(task_id_of_row("t-42  done    1m03s  x"), Some(42));
        assert_eq!(task_id_of_row("  t-7  running"), Some(7), "leading space");
        assert_eq!(task_id_of_row("t-x  running"), None, "non-numeric id");
        assert_eq!(
            task_id_of_row("job-1`  exit 0  cargo test"),
            None,
            "job rows carry no task id"
        );
        assert_eq!(
            task_id_of_row("dap d1  live     breakpoints in 2 file(s)"),
            None,
            "dap rows carry no task id"
        );
        assert_eq!(task_id_of_row("no background tasks or jobs"), None);
    }

    #[test]
    fn apply_event_collects_tool_and_text_lines() {
        let mut lines = Transcript::default();
        let mut busy = true;
        let mut busy_since = None;
        let mut meters = Meters::default();
        let mut pending = None;
        let mut turn_produced = false;
        let mut usage = None;
        let mut a = String::new();
        let mut t = String::new();
        let mut tool = String::new();
        let mut live_tool: Option<LiveTool> = None;
        let mut last_user: Option<String> = None;
        let mut last_error: Option<String> = None;
        let mut spills: Vec<String> = Vec::new();
        let mut sidebar = SidebarState::default();

        let _ = apply_event(
            &Event::Delta {
                kind: ka_protocol::DeltaKind::Text("hi ".into()),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::Delta {
                kind: ka_protocol::DeltaKind::Text("there".into()),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::CallStarted {
                tool: "read".into(),
                id: "c1".into(),
                detail: "main.rs".into(),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::CallOutput {
                tool: "read".into(),
                id: "c1".into(),
                excerpt: "1\tfile body".into(),
                is_error: false,
                spill: None,
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::TurnFinished {
                stop: ka_protocol::Stop::Done,
                usage: ka_protocol::Usage {
                    input: 10,
                    output: 2,
                    ..Default::default()
                },
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );

        assert!(!busy);
        let texts: Vec<&str> = lines
            .entries()
            .iter()
            .filter_map(|l| match l {
                Line::Assistant(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert!(texts.contains(&"hi there"), "{texts:?}");
        // the tool call lands as a block with a boxed card: head from
        // the CallStarted detail, verdict from CallFinished
        let calls: Vec<&ToolCall> = lines
            .entries()
            .iter()
            .filter_map(|l| match l {
                Line::ToolBlock(v) => v.first(),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].head.contains("read"), "{calls:?}");
        assert!(calls[0].ok, "{calls:?}");
    }

    #[test]
    fn call_boundary_flushes_text_before_the_tool_row() {
        let mut lines = Transcript::default();
        let mut busy = false;
        let mut busy_since = None;
        let mut meters = Meters::default();
        let mut pending = None;
        let mut turn_produced = false;
        let mut usage = None;
        let mut a = String::new();
        let mut t = String::new();
        let mut tool = String::new();
        let mut live_tool: Option<LiveTool> = None;
        let mut last_user: Option<String> = None;
        let mut last_error: Option<String> = None;
        let mut spills: Vec<String> = Vec::new();
        let mut sidebar = SidebarState::default();

        let _ = apply_event(
            &Event::Delta {
                kind: ka_protocol::DeltaKind::Thought("pondering".into()),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::Delta {
                kind: ka_protocol::DeltaKind::Text("before ".into()),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::Delta {
                kind: ka_protocol::DeltaKind::Call {
                    tool: "bash".into(),
                    id: "c1".into(),
                },
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        // mid-call: text must already be flushed above the tool block, and
        // partial outputs must refresh the preview, not the compact row
        assert!(
            a.is_empty() && t.is_empty(),
            "buffers taken at the boundary"
        );
        let _ = apply_event(
            &Event::CallOutput {
                tool: "bash".into(),
                id: "c1".into(),
                excerpt: "line-1\nline-2".into(),
                is_error: false,
                spill: None,
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::CallOutput {
                tool: "bash".into(),
                id: "c1".into(),
                excerpt: "line-3".into(),
                is_error: false,
                spill: None,
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let lt = live_tool.as_ref().unwrap();
        assert_eq!(lt.preview, vec!["line-1", "line-2", "line-3"]);
        assert_eq!(tool, "→ bash", "partials must not append the note");
        let _ = apply_event(
            &Event::CallFinished {
                tool: "bash".into(),
                id: "c1".into(),
                ok: true,
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        assert!(live_tool.is_none(), "block collapses on finish");
        let _ = apply_event(
            &Event::Delta {
                kind: ka_protocol::DeltaKind::Text("after".into()),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::CallStarted {
                tool: "bash".into(),
                id: "c2".into(),
                detail: String::new(),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::CallOutput {
                tool: "bash".into(),
                id: "c2".into(),
                excerpt: "boom".into(),
                is_error: true,
                spill: None,
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::CallFinished {
                tool: "bash".into(),
                id: "c2".into(),
                ok: false,
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::TurnFinished {
                stop: ka_protocol::Stop::Done,
                usage: ka_protocol::Usage::default(),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );

        let entries = lines.entries();
        let shapes: Vec<String> = entries
            .iter()
            .map(|l| match l {
                Line::Thought(s) => format!("T:{s}"),
                Line::Assistant(s) => format!("A:{s}"),
                Line::ToolBlock(v) => format!(
                    "T:{}",
                    v.iter()
                        .map(|c| c.head.as_str())
                        .collect::<Vec<_>>()
                        .join("|")
                ),
                Line::Report(s) => format!("R:{s}"),
                Line::User(s) => format!("U:{s}"),
                _ => "?".to_string(),
            })
            .collect();
        assert_eq!(
            shapes,
            vec![
                "T:pondering",
                "A:before ",
                "R:",
                "T:→ bash",
                "R:",
                "A:after",
                "R:",
                "T:→ bash",
            ],
            "final order must interleave text and tools: {shapes:?}"
        );
        // verdict + note live on the call, not the head: the first call
        // closed ok, the second failed
        let blocks: Vec<&Vec<ToolCall>> = lines
            .entries()
            .iter()
            .filter_map(|l| match l {
                Line::ToolBlock(v) => Some(v),
                _ => None,
            })
            .collect();
        assert_eq!(blocks.len(), 2, "text between calls splits the blocks");
        assert!(blocks[0][0].ok);
        assert_eq!(blocks[0][0].note, "line-3");
        assert!(!blocks[1][0].ok);
        assert_eq!(blocks[1][0].note, "boom");
    }

    #[test]
    fn flush_live_text_orders_thought_then_assistant() {
        let mut transcript = Transcript::default();
        let mut thought = "why\n".to_string();
        let mut assistant = String::new();
        flush_live_text(&mut transcript, &mut thought, &mut assistant);
        assert!(thought.is_empty() && assistant.is_empty());
        assert_eq!(transcript.entries().len(), 1, "empty text flushes nothing");
        assistant.push_str("answer");
        thought.push_str("more");
        flush_live_text(&mut transcript, &mut thought, &mut assistant);
        let shapes: Vec<String> = transcript
            .entries()
            .iter()
            .map(|l| match l {
                Line::Thought(s) => format!("T:{s}"),
                Line::Assistant(s) => format!("A:{s}"),
                Line::Report(s) => format!("R:{s}"),
                _ => "?".to_string(),
            })
            .collect();
        // thinking bands ship their own spacer rows: no Report
        // separator opens between the thought and the assistant card
        assert_eq!(shapes, vec!["T:why\n", "T:more", "A:answer"]);
    }

    #[test]
    fn preview_window_rolls_and_stays_transient() {
        let mut window: Vec<String> = Vec::new();
        for i in 1..=5 {
            observe_preview(&mut window, &format!("line-{i}"));
        }
        assert_eq!(window, vec!["line-3", "line-4", "line-5"], "rolling 3");
        // the running call renders its timeline header + at most the 3
        // newest faint rows on the thread, rebuilt every frame
        let t0 = Instant::now();
        let now = t0 + Duration::from_millis(2500);
        let lt = LiveTool {
            id: "c1".into(),
            preview: window.clone(),
            last: Some(("line-5".into(), false)),
            started: t0,
        };
        let rows = tool_live_rows("→ bash", Some(&lt), 40, now);
        assert_eq!(rows.len(), 1 + PREVIEW_WINDOW);
        use unicode_width::UnicodeWidthStr;
        let texts: Vec<String> = rows
            .iter()
            .map(|r| {
                r.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect();
        // header: spinner instead of a verdict, icon + tool, the live
        // elapsed right-aligned at the pane edge
        assert!(
            "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(texts[0].chars().next().unwrap()),
            "{texts:?}"
        );
        assert!(texts[0][3..].starts_with(" $ bash"), "{texts:?}");
        assert!(texts[0].ends_with("2.5s"), "{texts:?}");
        assert_eq!(texts[0].width(), 40, "cluster rides the right edge");
        assert_eq!(texts[1], "│ line-3");
        assert_eq!(texts[2], "│ line-4");
        assert_eq!(texts[3], "╰ line-5", "the last row turns the thread");
        for row in &rows {
            assert!(
                row.spans.iter().all(|s| s.style.bg.is_none()),
                "no box, no fill: {row:?}"
            );
        }
        assert_eq!(tool_live_rows("→ bash", None, 40, now).len(), 0);

        // nothing previewed yet: the header stands alone
        let bare = LiveTool {
            id: "c1".into(),
            preview: Vec::new(),
            last: None,
            started: t0,
        };
        let rows = tool_live_rows("→ web_fetch · https://example.com", Some(&bare), 40, now);
        assert_eq!(rows.len(), 1, "one line while there is nothing to show");
        let text: String = rows[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("web_fetch  https://example.com"), "{text:?}");
    }

    #[test]
    fn turn_error_is_buffered_and_surfaced_on_the_report_row() {
        let mut lines = Transcript::default();
        let mut busy = false;
        let mut busy_since = None;
        let mut meters = Meters::default();
        let mut pending = None;
        let mut turn_produced = false;
        let mut usage = None;
        let mut a = String::new();
        let mut t = String::new();
        let mut tool = String::new();
        let mut live_tool: Option<LiveTool> = None;
        let mut last_user: Option<String> = None;
        let mut last_error: Option<String> = None;
        let mut spills: Vec<String> = Vec::new();
        let mut sidebar = SidebarState::default();

        let _ = apply_event(
            &Event::TurnStarted {
                context: ka_protocol::ContextMeter {
                    used: 100,
                    window: 200,
                },
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::Delta {
                kind: ka_protocol::DeltaKind::Text("hi".into()),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::Error {
                class: ka_protocol::ErrorClass::RateLimit,
                retryable: true,
                message: "429 over 5h".into(),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        assert!(
            lines.entries().is_empty(),
            "busy in-turn error must not push a standalone row"
        );
        let _ = apply_event(
            &Event::TurnFinished {
                stop: ka_protocol::Stop::Error,
                usage: ka_protocol::Usage {
                    input: 100,
                    output: 5,
                    cost: 0.0002,
                    ..Default::default()
                },
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let entries = lines.entries();
        assert_eq!(
            entries.len(),
            3,
            "assistant, air, one bold failure summary — no second meta row: {entries:?}"
        );
        assert!(matches!(entries[0], Line::Assistant(_)));
        // air opens between the card and the failure summary (family change)
        assert_eq!(&entries[1], &Line::Report(String::new()));
        let Line::Summary {
            glyph, tone, text, ..
        } = &entries[2]
        else {
            panic!("third entry must be a failure Summary: {entries:?}");
        };
        assert_eq!(*glyph, '✗');
        assert_eq!(*tone, SummaryTone::Err);
        for want in ["failed", "429", "/retry", "in", "out", "$0.0002"] {
            assert!(text.contains(want), "summary {text:?} lacks {want}");
        }
        assert!(
            last_error.is_none(),
            "buffered error consumed by the report"
        );
    }

    #[test]
    fn done_with_zero_usage_pushes_no_report_row() {
        let mut lines = Transcript::default();
        let mut busy = false;
        let mut busy_since = None;
        let mut meters = Meters::default();
        let mut pending = None;
        let mut turn_produced = false;
        let mut usage = None;
        let mut a = String::new();
        let mut t = String::new();
        let mut tool = String::new();
        let mut live_tool: Option<LiveTool> = None;
        let mut last_user: Option<String> = None;
        let mut last_error: Option<String> = None;
        let mut spills: Vec<String> = Vec::new();
        let mut sidebar = SidebarState::default();

        let _ = apply_event(
            &Event::TurnStarted {
                context: ka_protocol::ContextMeter::default(),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::TurnFinished {
                stop: ka_protocol::Stop::Done,
                usage: ka_protocol::Usage::default(),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        assert!(
            lines.entries().is_empty(),
            "canned/no-model turns must stay silent"
        );
    }

    #[test]
    fn fmt_dur_and_fmt_tok_hit_the_boundaries() {
        assert_eq!(fmt_dur(0.0), "0.0s");
        assert_eq!(fmt_dur(59.94), "59.9s");
        assert_eq!(fmt_dur(60.0), "1:00");
        assert_eq!(fmt_dur(754.0), "12:34");
        assert_eq!(fmt_tok(0), "0");
        assert_eq!(fmt_tok(999), "999");
        assert_eq!(fmt_tok(1000), "1k");
        assert_eq!(fmt_tok(1234), "1.2k");
        assert_eq!(fmt_tok(100_000), "100k");
        let u = ka_protocol::Usage {
            input: 1000,
            output: 5,
            cost: 0.0002,
            ..Default::default()
        };
        assert_eq!(usage_tail(&u, 3.25), " · 3.2s · 1k in · 5 out · $0.0002");
        let zero = ka_protocol::Usage::default();
        assert_eq!(usage_tail(&zero, 0.0), " · 0.0s");
        let cached = ka_protocol::Usage {
            cache_read: 2000,
            ..Default::default()
        };
        assert!(usage_tail(&cached, 1.0).contains("2k cache"));
    }

    #[test]
    fn b64encode_matches_the_standard_vectors() {
        assert_eq!(b64encode(""), "");
        assert_eq!(b64encode("hello"), "aGVsbG8=");
        assert_eq!(
            b64encode("any carnal pleasure."),
            "YW55IGNhcm5hbCBwbGVhc3VyZS4="
        );
        assert_eq!(b64encode("héllo"), "aMOpbGxv", "multibyte input");
    }

    #[test]
    fn session_picker_row_zero_is_always_new() {
        let empty = SessionPicker {
            sessions: vec![],
            selected: 0,
            filter: String::new(),
            current: None,
        };
        assert_eq!(empty.rows()[0].0, "(new session)");
        let s = ka_strand::StrandSummary {
            path: std::path::PathBuf::from("/tmp/x.jsonl"),
            id: "s1-test".into(),
            ts: "2026-09-02T10:00:00Z".into(),
            title: "hello".into(),
            messages: 2,
            cost: 0.0,
            tokens: 0,
            parent: None,
            name: None,
        };
        let picker = SessionPicker {
            sessions: vec![s],
            selected: 0,
            filter: String::new(),
            current: None,
        };
        let rows = picker.rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "(new session)");
        assert_eq!(rows[1].0, "#test  hello");
        assert!(rows[1].1.ends_with("msgs · 2h ago") || rows[1].1.contains("msgs · "));
        assert_eq!(picker.pick(), None, "row 0 switches to a fresh session");
    }

    #[test]
    fn path_token_finds_the_word_at_the_caret() {
        assert_eq!(path_token("", 0), None);
        assert_eq!(path_token("abc", 0).unwrap(), (0, "abc".to_string()));
        assert_eq!(path_token("abc", 3).unwrap(), (0, "abc".to_string()));
        assert_eq!(path_token("abc def", 4).unwrap(), (4, "def".to_string()));
        assert_eq!(path_token("abc def", 6).unwrap(), (4, "def".to_string()));
        assert_eq!(
            path_token("./src/m", 7).unwrap(),
            (0, "./src/m".to_string())
        );
        assert_eq!(path_token("x ./a y", 4).unwrap(), (2, "./a".to_string()));
        // caret between whitespace: no word
        assert_eq!(path_token("a  b", 2), None);
        assert_eq!(path_token("a ", 2), None);
        assert_eq!(path_token("a  b", 0), Some((0, "a".to_string())));
        // unicode keeps char indices
        assert_eq!(path_token("é 世界 x", 3).unwrap(), (2, "世界".to_string()));
        // newline/tab are whitespace too
        assert_eq!(path_token("a\tb", 2).unwrap(), (2, "b".to_string()));
        assert_eq!(path_token("a\nb", 1), None);
    }

    #[test]
    fn split_path_token_separates_dir_and_base() {
        let (d, b, a) = split_path_token("./src/main.rs");
        assert_eq!(d, "./src/");
        assert_eq!(b, "main.rs");
        assert!(!a);
        let (d, b, a) = split_path_token("~/notes.txt");
        assert_eq!(d, "~/");
        assert_eq!(b, "notes.txt");
        assert!(!a);
        let (d, b, a) = split_path_token("/etc/hosts");
        assert_eq!(d, "/etc/");
        assert_eq!(b, "hosts");
        assert!(a);
        let (d, b, a) = split_path_token("bareword");
        assert_eq!(d, "");
        assert_eq!(b, "bareword");
        assert!(!a);
        let (d, b, _) = split_path_token("dir/");
        assert_eq!(d, "dir/");
        assert_eq!(b, "");
    }

    #[test]
    fn complete_token_replaces_only_the_word() {
        let (t, c) = complete_token("run ./s now", 4, 3, "./src/");
        assert_eq!(t, "run ./src/ now");
        assert_eq!(c, 10);
        let (t, c) = complete_token("./s", 0, 3, "./src/main.rs");
        assert_eq!(t, "./src/main.rs");
        assert_eq!(c, 13);
        // multibyte prefix keeps char arithmetic straight
        let (t, c) = complete_token("é ./s", 2, 3, "./x.md");
        assert_eq!(t, "é ./x.md");
        assert_eq!(c, 8);
    }

    #[test]
    fn list_matches_filters_prefixes_and_hides_dotfiles() {
        let dir = std::env::temp_dir().join(format!("ka-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("alpha.rs"), "x").unwrap();
        std::fs::write(dir.join("beta.md"), "x").unwrap();
        std::fs::write(dir.join(".hidden"), "x").unwrap();
        let root = dir.to_string_lossy().to_string() + "/";
        let all = list_matches(&root, "");
        assert!(all.contains(&("src".to_string(), true)), "{all:?}");
        assert!(all.contains(&("alpha.rs".to_string(), false)), "{all:?}");
        assert!(
            all.iter().all(|(n, _)| !n.starts_with('.')),
            "dotfiles hidden: {all:?}"
        );
        let a = list_matches(&root, "a");
        assert_eq!(a, vec![("alpha.rs".to_string(), false)]);
        let dot = list_matches(&root, ".h");
        assert_eq!(
            dot,
            vec![(".hidden".to_string(), false)],
            "dotfiles when base dots"
        );
        let sub = list_matches(&(root.clone() + "src/"), "m");
        assert!(sub.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn rel_age_at_uses_the_hinnant_civil_days() {
        let now = 1_788_350_400; // 2026-09-02T12:00:00Z
        assert_eq!(rel_age_at("2026-09-02T11:59:30Z", now), "just now");
        assert_eq!(
            rel_age_at("2026-09-02T12:00:10Z", now),
            "just now",
            "future clamps"
        );
        assert_eq!(rel_age_at("2026-09-02T11:00:01Z", now), "59m ago");
        assert_eq!(rel_age_at("2026-09-02T11:00:00Z", now), "1h ago");
        assert_eq!(rel_age_at("2026-09-01T11:00:00Z", now), "1d ago");
        assert_eq!(rel_age_at("2026-07-30T12:00:00Z", now), "4w ago", "34 days");
        // fractional seconds and offsets parse like RFC3339
        assert_eq!(rel_age_at("2026-09-02T11:59:30.250Z", now), "just now");
        assert_eq!(rel_age_at("garbage", now), "garbage");
        assert_eq!(
            rel_age_at("2026-09-02", now),
            "2026-09-02",
            "missing time fields"
        );
        // epoch-aligned sanity: the civil formula matches the calendar
        assert_eq!(rel_age_at("1970-01-01T00:00:00Z", 0), "just now");
    }
    #[test]
    fn reverse_search_cycles_matches_newest_first() {
        let mut b = InputBuffer::default();
        for t in ["alpha one", "beta", "alpha two"] {
            b.insert_str(t);
            b.take();
        } // history oldest→newest: alpha one, beta, alpha two
        b.search_start();
        for c in "alpha".chars() {
            b.search_push(c);
        }
        assert_eq!(
            b.text, "alpha two",
            "first match is the newest entry containing the query"
        );
        assert_eq!(b.cursor, 9, "cursor lands at the match end");
        b.search_next();
        assert_eq!(b.text, "alpha one", "ctrl+r steps to the older match");
        b.search_next();
        assert_eq!(b.text, "alpha one", "exhausted history stays on the match");
        b.search_accept();
        assert!(!b.searching());
        assert_eq!(b.text, "alpha one", "accept keeps the match loaded");

        // a growing query restarts from the newest entry
        let mut c = InputBuffer::default();
        for t in ["alpha one", "alpha two"] {
            c.insert_str(t);
            c.take();
        }
        c.search_start();
        c.search_push('a');
        c.search_push('l');
        c.search_push('p');
        assert_eq!(c.text, "alpha two");
        assert_eq!(c.search_title().as_deref(), Some("rsearch: `alp`"));
    }

    #[test]
    fn reverse_search_cancel_restores_the_draft() {
        let mut b = InputBuffer::default();
        b.insert_str("keep");
        b.take();
        b.insert_str("draft in progress");
        b.cursor = 5;
        b.search_start();
        b.search_push('k');
        assert_eq!(b.text, "keep", "a live match replaces the draft view");
        b.search_cancel();
        assert_eq!(b.text, "draft in progress", "esc restores the draft");
        assert_eq!(b.cursor, 5);
        assert!(!b.searching());
    }

    #[test]
    fn reverse_search_empty_query_is_a_no_op() {
        let mut b = InputBuffer::default();
        b.insert_str("older");
        b.take();
        b.insert_str("live draft");
        b.search_start();
        assert_eq!(b.search_title().as_deref(), Some("rsearch: ``"));
        b.search_next();
        b.search_accept();
        assert_eq!(b.text, "live draft", "empty query never touches the buffer");

        // backspacing the query away re-matches from the newest entry
        let mut c = InputBuffer::default();
        c.insert_str("hello world");
        c.take();
        c.insert_str("draft");
        c.search_start();
        c.search_push('x');
        assert!(c.search_title().unwrap().contains("(no match)"));
        assert_eq!(c.text, "draft", "a missed query leaves the buffer alone");
        c.search_backspace();
        c.search_push('h');
        assert_eq!(c.text, "hello world");
        c.search_backspace();
        assert_eq!(c.search_title().as_deref(), Some("rsearch: ``"));
    }
    #[test]
    fn mention_token_requires_more_than_the_sigil() {
        assert_eq!(mention_token("@src/ma"), Some("src/ma"));
        assert_eq!(mention_token("@"), None, "bare @ never opens the popup");
        assert_eq!(mention_token("src/ma"), None, "missing sigil");
        assert_eq!(mention_token(""), None);
        // the word at the caret carries the sigil
        assert_eq!(
            path_token("run @src/ma", 11).unwrap(),
            (4, "@src/ma".to_string())
        );
    }

    #[test]
    fn walk_files_skips_noise_dirs_and_sorts() {
        let dir = std::env::temp_dir().join(format!("ka-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        std::fs::create_dir_all(dir.join(".git/objects")).unwrap();
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::write(dir.join("README.md"), "x").unwrap();
        std::fs::write(dir.join("src/main.rs"), "x").unwrap();
        std::fs::write(dir.join("src/deep/util.rs"), "x").unwrap();
        std::fs::write(dir.join(".hidden"), "x").unwrap();
        std::fs::write(dir.join("target/out.bin"), "x").unwrap();
        let got = walk_files(&dir, WALK_CAP);
        assert_eq!(
            got,
            vec![
                ("README.md".to_string(), false),
                ("src".to_string(), true),
                ("src/deep".to_string(), true),
                ("src/deep/util.rs".to_string(), false),
                ("src/main.rs".to_string(), false),
            ],
            "dotfiles, dot-dirs and skip-dirs are gone; sorted, no slashes on dirs"
        );
        // the cap truncates the sorted listing
        assert_eq!(walk_files(&dir, 2).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mention_matches_use_the_last_segment_prefix() {
        let entries = vec![
            ("src/main.rs".to_string(), false),
            ("lib/main.rs".to_string(), false),
            ("README.md".to_string(), false),
            ("src".to_string(), true),
        ];
        assert_eq!(
            mention_matches(&entries, "src/ma"),
            vec![
                ("src/main.rs".to_string(), false),
                ("lib/main.rs".to_string(), false),
            ],
            "only the text after the last / is prefix-matched"
        );
        assert_eq!(
            mention_matches(&entries, "src"),
            vec![("src".to_string(), true)],
            "segment match, not a path-prefix match"
        );
        assert!(mention_matches(&entries, "zzz").is_empty());
    }

    #[test]
    fn mention_accept_insert_math() {
        // Tab on `@src/ma` with src/main.rs selected: token replaced by
        // the mention plus a trailing space
        let (t, c) = complete_token("run @src/ma", 4, 7, "@src/main.rs ");
        assert_eq!(t, "run @src/main.rs ");
        assert_eq!(c, 17);
        // a directory accept descends: token becomes the new prefix
        let (t, c) = complete_token("@src/", 0, 5, "@src/deep/");
        assert_eq!(t, "@src/deep/");
        assert_eq!(c, 10);
    }
    #[test]
    fn transcript_find_finds_from_the_anchor_row() {
        let mut t = Transcript::default();
        t.set_width(40);
        t.push(Line::User("hello world".into()));
        t.push(Line::Assistant("quick brown".into()));
        t.push(Line::Info("HELLO again".into()));
        // case-insensitive scan from the top
        let (first_i, first_off) = t.find_from(0, "HeLLo").unwrap();
        assert_eq!(first_i, 0, "first match from the top");
        assert_eq!(first_off, 0);
        assert!(t.find_from(0, "BROWN").is_some());
        // resume past the first hit: the note is the next match
        let (second_i, second_off) = t.find_from(first_off + 1, "hello").unwrap();
        assert_eq!(second_i, 2, "non-matching entries are skipped");
        assert!(
            second_off > first_off,
            "offsets come from the rendered cache"
        );
        // the anchor row itself is included…
        assert_eq!(t.find_from(second_off, "hello").unwrap().0, 2);
        // …but strictly past it is exhausted
        assert!(t.find_from(second_off + 1, "hello").is_none());
        // an anchor inside a multi-row entry skips that entry
        let mid = first_off + 1;
        assert_eq!(t.find_from(mid, "hello").unwrap().0, 2);
        assert!(t.find_from(0, "").is_none());
    }

    #[test]
    fn find_never_matches_blank_separator_rows() {
        let mut t = Transcript::default();
        t.set_width(40);
        t.push_separated(Line::User("alpha".into()));
        t.push_separated(Line::Assistant("needle here".into()));
        t.push_separated(Line::Info("beta".into()));
        // every anchor row must land on a content entry — the Report("")
        // separators hold no text and can never be hits
        let mut hits: Vec<usize> = Vec::new();
        for row in 0..t.total_rows() {
            if let Some((i, _)) = t.find_from(row, "needle") {
                hits.push(i);
            }
        }
        hits.dedup();
        assert_eq!(hits, vec![2], "only the assistant entry matches");
    }

    #[test]
    fn record_spill_dedupes_and_caps_at_50() {
        let mut spills: Vec<String> = Vec::new();
        record_spill(&mut spills, "/tmp/a");
        record_spill(&mut spills, "/tmp/b");
        record_spill(&mut spills, "/tmp/a");
        assert_eq!(spills, vec!["/tmp/a".to_string(), "/tmp/b".to_string()]);
        for i in 0..60 {
            record_spill(&mut spills, &format!("/tmp/x{i}"));
        }
        assert_eq!(spills.len(), 50, "capped at 50");
        assert_eq!(spills.last().unwrap(), "/tmp/x59", "newest last");
        assert!(
            !spills.contains(&"/tmp/a".to_string()),
            "oldest entries dropped"
        );
    }
    #[test]
    fn slash_commands_fork_checkpoint_restore() {
        // bare /fork copies the session as-is
        let fork = slash_command("/fork").unwrap();
        assert!(matches!(fork.event, Some(Command::ForkStrand { turns: 0 })));
        let fork3 = slash_command("/fork 3").unwrap();
        assert!(matches!(
            fork3.event,
            Some(Command::ForkStrand { turns: 3 })
        ));
        // an unparsable count is refused, not defaulted
        let bad = slash_command("/fork two").unwrap();
        assert!(bad.event.is_none(), "nothing sent on a bad count");
        assert_eq!(bad.note.as_deref(), Some("usage: /fork [turns]"));

        let cp = slash_command("/checkpoint").unwrap();
        assert!(matches!(cp.event, Some(Command::Checkpoint)));
        let restore = slash_command("/restore").unwrap();
        assert!(
            matches!(restore.event, Some(Command::RestoreCheckpoint { ref id }) if id == "list"),
            "bare /restore lists"
        );
        let restore_id = slash_command("/restore abc-123").unwrap();
        assert!(matches!(
            restore_id.event,
            Some(Command::RestoreCheckpoint { ref id }) if id == "abc-123"
        ));

        let names: Vec<String> = available_slash_commands()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        for want in ["/fork", "/checkpoint", "/restore"] {
            assert!(names.contains(&want.to_string()), "missing {want}");
        }
    }

    #[test]
    fn session_picker_rows_show_cost_and_tokens() {
        let rich = SessionPicker {
            current: None,
            sessions: vec![ka_strand::StrandSummary {
                path: std::path::PathBuf::from("/tmp/r.jsonl"),
                id: "s1-cccc3333".into(),
                ts: "2026-09-02T10:00:00Z".into(),
                title: "priced".into(),
                messages: 7,
                cost: 1.234,
                tokens: 12_345,
                parent: None,
                name: None,
            }],
            selected: 0,
            filter: String::new(),
        };
        let (label, detail) = &rich.rows()[1];
        assert!(detail.contains("7 msgs"), "{detail}");
        assert!(detail.contains("$1.23"), "{detail}");
        assert!(detail.contains("12.3k tok"), "{detail}");
        assert!(!label.contains('$'), "label line stays clean: {label}");

        // sub-cent cost and zero tokens add nothing (no dangling separators)
        let free = SessionPicker {
            current: None,
            sessions: vec![ka_strand::StrandSummary {
                path: std::path::PathBuf::from("/tmp/f.jsonl"),
                id: "s1-dddd4444".into(),
                ts: "2026-09-02T10:00:00Z".into(),
                title: "no-cost chat".into(),
                messages: 2,
                cost: 0.004,
                tokens: 0,
                parent: None,
                name: None,
            }],
            selected: 0,
            filter: String::new(),
        };
        let detail = &free.rows()[1].1;
        assert!(detail.contains("2 msgs · "), "{detail}");
        assert!(!detail.contains('$'), "sub-cent stays hidden: {detail}");
        assert!(!detail.contains("tok"), "{detail}");
    }

    /// Feed one event through `apply_event` with fresh scratch state.
    fn feed(lines: &mut Transcript, evt: &Event) {
        let mut busy = false;
        let mut busy_since = None;
        let mut meters = Meters::default();
        let mut pending = None;
        let mut turn_produced = false;
        let mut usage = None;
        let mut a = String::new();
        let mut t = String::new();
        let mut tool = String::new();
        let mut live_tool: Option<LiveTool> = None;
        let mut last_user: Option<String> = None;
        let mut last_error: Option<String> = None;
        let mut spills: Vec<String> = Vec::new();
        let mut sidebar = SidebarState::default();
        let _ = apply_event(
            evt,
            lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
    }

    #[test]
    fn inventory_without_mcp_or_agents_lands_no_card_line() {
        let mut lines = Transcript::default();
        lines.set_width(90);
        feed(
            &mut lines,
            &Event::Inventory {
                tools: ["read", "edit", "bash"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                mcp: Vec::new(),
                agents: Vec::new(),
                skills: vec!["rust-docs".into()],
                prompts: Vec::new(),
                disabled: Vec::new(),
            },
        );
        // tools and skills live in the sidebar now: nothing in the chat
        assert!(lines.entries().is_empty(), "{:?}", lines.entries());
        assert_eq!(lines.total_rows(), 0);
    }

    #[test]
    fn feature_panel_rows_reflect_the_disabled_set() {
        let sidebar = SidebarState {
            known_tools: vec!["read".into(), "bash".into()],
            sandbox: Some("fs".into()),
            inventory: Inventory {
                mcp: vec![ka_protocol::McpSummary {
                    name: "github".into(),
                    ok: true,
                    tools: 4,
                }],
                skills: vec!["pdf".into()],
                agents: vec!["reviewer".into()],
                disabled: vec!["agents".into(), "mcp:github".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let rows = feature_rows(&sidebar);
        let on = |rows: &[FeatureRow], spec: &str| {
            rows.iter()
                .find(|r| r.spec == spec)
                .map(|r| r.on)
                .unwrap_or_else(|| panic!("row {spec} missing"))
        };
        assert!(!on(&rows, "agents"), "disabled spec reads off");
        assert!(on(&rows, "skills"));
        assert!(!on(&rows, "mcp:github"));
        assert!(on(&rows, "tool:bash"), "tools section covers base hands");
        assert!(on(&rows, "sandbox"), "fs override reads on");
        assert!(
            rows.first().is_some_and(|r| r.header),
            "panel starts with a section header"
        );
        // the status badge counts the disabled set
        let meters = Meters {
            toggles: sidebar.inventory.disabled.len(),
            ..Default::default()
        };
        let right = status_left(&meters);
        let joined: String = right
            .iter()
            .map(|s| s.content.clone())
            .collect::<Vec<_>>()
            .join("");
        assert!(joined.contains("\u{2298}2"), "badge shows ⊘2: {joined}");
    }

    #[test]
    fn inventory_marks_failed_mcp_and_omits_empty_segments() {
        let mut lines = Transcript::default();
        feed(
            &mut lines,
            &Event::Inventory {
                tools: vec!["read".into()],
                mcp: vec![
                    ka_protocol::McpSummary {
                        name: "fetch".into(),
                        ok: true,
                        tools: 3,
                    },
                    ka_protocol::McpSummary {
                        name: "jira".into(),
                        ok: false,
                        tools: 0,
                    },
                ],
                agents: Vec::new(),
                skills: Vec::new(),
                prompts: Vec::new(),
                disabled: Vec::new(),
            },
        );
        let entries = lines.entries();
        let [Line::Info(text)] = entries else {
            panic!("one Info card expected, got {entries:?}")
        };
        let rows: Vec<&str> = text.split('\n').collect();
        assert_eq!(rows, ["mcp: fetch ✓ 3 · jira ✗"]);
    }

    #[test]
    fn inventory_agents_only_row_with_overflow_mark() {
        let mut lines = Transcript::default();
        let agents: Vec<String> = (0..6)
            .map(|i| format!("agent-with-a-long-name-{i}"))
            .collect();
        feed(
            &mut lines,
            &Event::Inventory {
                tools: vec!["read".into()],
                mcp: Vec::new(),
                agents,
                skills: vec!["rust-docs".into()],
                prompts: Vec::new(),
                disabled: Vec::new(),
            },
        );
        let entries = lines.entries();
        let [Line::Info(text)] = entries else {
            panic!("one Info card expected, got {entries:?}")
        };
        assert!(
            text.starts_with("agents: agent-with-a-long-name-0"),
            "agents-only card, no headline: {text}"
        );
        assert!(text.contains("(+4)"), "overflow mark present: {text}");
        assert!(!text.contains("tools"), "no tools segment: {text}");
        assert!(!text.contains("skills"), "no skills segment: {text}");
        for row in text.split('\n') {
            assert!(row.chars().count() <= 90, "row fits ~90 cols: {row}");
        }
    }

    #[test]
    fn delete_forward_removes_char_at_cursor_and_is_noop_at_end() {
        let mut c = InputBuffer::default();
        c.insert_str("héllo");
        c.end();
        c.delete_forward();
        assert_eq!(c.text, "héllo", "no-op at the end of the text");
        c.home();
        c.right(); // cursor sits on the multibyte 'é'
        c.delete_forward();
        assert_eq!(c.text, "hllo", "the whole char is removed");
        assert_eq!(c.cursor, 1, "cursor stays put");
    }

    #[test]
    fn separate_before_puts_air_between_families_only() {
        let mut t = Transcript::default();
        separate_before(&mut t, Family::User);
        assert!(t.entries().is_empty(), "fresh transcript: no air");
        t.push(Line::User("first".into()));
        separate_before(&mut t, Family::User);
        assert_eq!(t.entries().len(), 1, "same family: no air");
        separate_before(&mut t, Family::Meta);
        assert_eq!(
            t.entries().last(),
            Some(&Line::Report(String::new())),
            "different family: exactly one blank"
        );
        separate_before(&mut t, Family::Assistant);
        assert_eq!(
            t.entries()
                .iter()
                .filter(|l| **l == Line::Report(String::new()))
                .count(),
            1,
            "blank tail: never doubled"
        );
    }

    #[test]
    fn tool_run_stays_tight_then_reopens_air() {
        let mut t = Transcript::default();
        t.push_tool_call(ToolCall {
            head: "→ read · lib.rs".into(),
            ok: true,
            ..no_note_call()
        });
        t.push_tool_call(ToolCall {
            head: "→ bash · cargo build".into(),
            ok: true,
            note: "ok".into(),
            ..no_note_call()
        });
        t.push_separated(Line::Assistant("answer".into()));
        // a turn ends on meta; the next user card reopens the air
        t.push_separated(Line::Summary {
            glyph: '✓',
            tone: SummaryTone::Ok,
            text: "done · 0.0s".into(),
            detail: None,
            expanded: false,
        });
        t.push_separated(Line::Report("task row".into()));
        t.push_separated(Line::User("again".into()));
        let shapes: Vec<String> = t
            .entries()
            .iter()
            .map(|l| match l {
                Line::User(s) => format!("U:{s}"),
                Line::Assistant(s) => format!("A:{s}"),
                Line::ToolBlock(v) => format!(
                    "T:{}",
                    v.iter()
                        .map(|c| c.head.as_str())
                        .collect::<Vec<_>>()
                        .join("|")
                ),
                Line::Summary { text, .. } => format!("S:{text}"),
                Line::Report(s) => format!("R:{s}"),
                _ => "?".to_string(),
            })
            .collect();
        assert_eq!(
            shapes,
            [
                "T:→ read · lib.rs|→ bash · cargo build",
                "R:",
                "A:answer",
                "R:",
                "S:done · 0.0s",
                "R:task row",
                "R:",
                "U:again",
            ],
            "consecutive calls merge tight, meta runs stay tight, families get air"
        );
        // nothing is stranded: the blank is exactly BETWEEN the blocks
        assert_ne!(
            t.entries().last(),
            Some(&Line::Report(String::new())),
            "no trailing blank after the final user row"
        );
    }

    /// A [`ToolCall`] with no output, no spill, no duration — the
    /// replay/shape-test baseline.
    fn no_note_call() -> ToolCall {
        ToolCall {
            head: String::new(),
            ok: true,
            note: String::new(),
            excerpt: String::new(),
            spill: None,
            dur: None,
            expanded: false,
        }
    }

    #[test]
    fn call_started_upgrades_stream_header_by_id() {
        let mut lines = Transcript::default();
        let mut busy = true;
        let mut busy_since = None;
        let mut meters = Meters::default();
        let mut pending = None;
        let mut turn_produced = false;
        let mut usage = None;
        let mut a = String::new();
        let mut t = String::new();
        let mut tool = String::new();
        let mut live_tool: Option<LiveTool> = None;
        let mut last_user: Option<String> = None;
        let mut last_error: Option<String> = None;
        let mut spills: Vec<String> = Vec::new();
        let mut sidebar = SidebarState::default();
        let mut feed = |lines: &mut Transcript, evt: &Event| -> (String, Option<Line>) {
            let _ = apply_event(
                evt,
                lines,
                &mut busy,
                &mut busy_since,
                &mut meters,
                &mut pending,
                &mut turn_produced,
                &mut usage,
                &mut a,
                &mut t,
                &mut tool,
                &mut live_tool,
                &mut last_user,
                &mut last_error,
                &mut spills,
                &mut sidebar,
            );
            (tool.clone(), lines.entries().last().cloned())
        };
        let (head, _last) = feed(
            &mut lines,
            &Event::Delta {
                kind: ka_protocol::DeltaKind::Call {
                    tool: "bash".into(),
                    id: "c1".into(),
                },
            },
        );
        assert_eq!(head, "→ bash", "stream header starts plain");
        let (head, last) = feed(
            &mut lines,
            &Event::CallStarted {
                tool: "bash".into(),
                id: "c1".into(),
                detail: "cargo build".into(),
            },
        );
        assert_eq!(head, "→ bash · cargo build", "same id upgrades in place");
        assert!(last.is_none(), "upgrade must not close the row");
        let (head, last) = feed(
            &mut lines,
            &Event::CallStarted {
                tool: "read".into(),
                id: "c2".into(),
                detail: "lib.rs".into(),
            },
        );
        // the never-finished bash header closes as a bare block row
        let got = last.expect("new call closes the old row");
        let Line::ToolBlock(v) = got else {
            panic!("expected a ToolBlock, got {got:?}");
        };
        assert_eq!(v[0].head, "→ bash · cargo build");
        assert!(!v[0].ok, "a call closed without CallFinished reads failed");
        assert_eq!(head, "→ read · lib.rs");
    }

    #[test]
    fn info_warn_err_render_three_tiers() {
        let info = super::render_line(&Line::Info("steering this turn".into()), 40, false);
        let warn = super::render_line(&Line::Warn("turn running".into()), 40, false);
        let err = super::render_line(&Line::Err("429 too many requests".into()), 40, false);
        let text = |rows: &Vec<ratatui::text::Line<'static>>| {
            rows[0]
                .spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        // gutter rows carry their style on the line (push_gutter), so
        // Paragraph paints prefix and text alike
        let fg = |rows: &Vec<ratatui::text::Line<'static>>| rows[0].style.fg;
        // info: `• ` muted
        assert_eq!(text(&info), "• steering this turn");
        assert_eq!(fg(&info), Some(crate::palette::META));
        // warn: `▲ ` salmon
        assert_eq!(text(&warn), "▲ turn running");
        assert_eq!(fg(&warn), Some(crate::palette::WARN));
        // err: `✗ ` bold — the loudest system row
        assert_eq!(text(&err), "✗ 429 too many requests");
        assert_eq!(fg(&err), Some(crate::palette::ERR));
        assert!(
            err[0]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
    }

    #[test]
    fn shell_rows_render_plain_fg() {
        let rows = super::render_line(&Line::Shell("hello world".into()), 40, false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].spans[0].content.as_ref(), "hello world");
        assert_eq!(rows[0].spans[0].style.fg, Some(crate::palette::FG));
    }

    #[test]
    fn summary_rows_render_tone_glyph_faint_text_and_rule() {
        use unicode_width::UnicodeWidthStr;
        let row = |glyph, tone| {
            super::render_line(
                &Line::Summary {
                    glyph,
                    tone,
                    text: "done · 2.0s".into(),
                    detail: None,
                    expanded: false,
                },
                40,
                false,
            )
        };
        for (glyph, tone, fg) in [
            ('✓', SummaryTone::Ok, crate::palette::OK),
            ('◐', SummaryTone::Warn, crate::palette::WARN),
        ] {
            let rows = row(glyph, tone);
            let first = &rows[0].spans[0];
            assert_eq!(first.content.as_ref(), format!("{glyph} "));
            assert_eq!(first.style.fg, Some(fg));
            assert_eq!(rows[0].spans[1].style.fg, Some(crate::palette::FAINT));
            // a rule closes the turn, filling the row to the width
            let text: String = rows[0].spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                text.starts_with(&format!("{glyph} done · 2.0s ─")),
                "{text}"
            );
            assert!(text.ends_with('─'), "{text}");
            assert_eq!(text.width(), 40, "{text}");
        }
        // the error tone is bold so failures outweigh every other row
        let rows = row('✗', SummaryTone::Err);
        assert_eq!(rows[0].spans[0].style.fg, Some(crate::palette::ERR));
        assert!(
            rows[0].spans[0]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
    }

    #[test]
    fn summary_chevron_tracks_detail_fold() {
        let call = |expanded| {
            super::render_line(
                &Line::Summary {
                    glyph: '✗',
                    tone: SummaryTone::Err,
                    text: "failed · HTTP 400: nope · /retry".into(),
                    detail: Some("the full untruncated error".into()),
                    expanded,
                },
                40,
                false,
            )
        };
        let text_of = |l: &ratatui::text::Line| -> String {
            l.spans.iter().map(|s| s.content.as_ref()).collect()
        };
        let collapsed = call(false);
        assert_eq!(collapsed.len(), 1, "collapsed shows the verdict only");
        let head = text_of(&collapsed[0]);
        assert!(
            head.contains("/retry ▸ ─"),
            "marker trails the verdict: {head}"
        );
        // a verdict longer than the row truncates its text, never the
        // affordance
        let long = super::render_line(
            &Line::Summary {
                glyph: '✗',
                tone: SummaryTone::Err,
                text: "failed · ".repeat(10),
                detail: Some("all of it".into()),
                expanded: false,
            },
            40,
            false,
        );
        let head = text_of(&long[0]);
        use unicode_width::UnicodeWidthStr;
        assert!(head.width() <= 40, "{head}");
        assert!(head.contains("… ▸"), "marker must survive clipping: {head}");
        let open = call(true);
        assert!(open.len() > 1, "expanded reveals wrapped detail");
        assert!(text_of(&open[0]).contains("▾"), "open marker");
        let body: String = open[1..].iter().map(text_of).collect();
        assert!(body.contains("the full untruncated error"), "{body}");
        // no hidden detail → no chevron (no dead indicator)
        let plain = super::render_line(
            &Line::Summary {
                glyph: '✓',
                tone: SummaryTone::Ok,
                text: "done · 0.0s".into(),
                detail: None,
                expanded: false,
            },
            40,
            false,
        );
        let s = text_of(&plain[0]);
        assert!(!s.contains('▸') && !s.contains('▾'), "{s}");
    }

    #[test]
    fn summary_fold_expands_full_detail_and_maps_clicks() {
        let mut t = Transcript::default();
        t.set_width(60);
        t.push(Line::Summary {
            glyph: '✗',
            tone: SummaryTone::Err,
            text: "failed · HTTP 400: {\"type\":\"err… · /retry · 0.9s".into(),
            detail: Some(format!("HTTP 400: {}", "the gateway said no ".repeat(8))),
            expanded: false,
        });
        assert_eq!(t.rendered_rows_of(0), 1, "collapsed keeps one row");
        assert_eq!(t.row_ref_at(0), Some(RowRef::Summary(0)));
        assert!(t.summary_foldable(0));
        t.toggle_summary(0);
        assert!(
            matches!(t.lines.first(), Some(Line::Summary { expanded: true, .. })),
            "toggle must flip the fold"
        );
        let rows = t.rendered_rows_of(0);
        assert!(rows > 1, "wrapped detail adds rows: {rows}");
        for r in 0..rows {
            assert_eq!(t.row_ref_at(r), Some(RowRef::Summary(0)), "row {r}");
        }
        let body: String = (1..rows).filter_map(|r| t.row_text(r)).collect();
        assert!(body.contains("the gateway said no"), "{body}");
        t.toggle_summary(0);
        assert_eq!(t.rendered_rows_of(0), 1, "collapse restores one row");
        // nothing hidden → no fold, no click target, toggle no-ops
        t.push(Line::Summary {
            glyph: '✓',
            tone: SummaryTone::Ok,
            text: "done · 0.0s".into(),
            detail: None,
            expanded: false,
        });
        assert!(!t.summary_foldable(1));
        assert_eq!(t.row_ref_at(1), Some(RowRef::Entry(1)));
        t.toggle_summary(1);
        assert_eq!(t.rendered_rows_of(1), 1);
    }

    #[test]
    fn long_turn_error_keeps_full_text_for_the_fold() {
        let mut lines = Transcript::default();
        let mut busy = false;
        let mut busy_since = None;
        let mut meters = Meters::default();
        let mut pending = None;
        let mut turn_produced = false;
        let mut usage = None;
        let mut a = String::new();
        let mut t = String::new();
        let mut tool = String::new();
        let mut live_tool: Option<LiveTool> = None;
        let mut last_user: Option<String> = None;
        let mut last_error: Option<String> = None;
        let mut spills: Vec<String> = Vec::new();
        let mut sidebar = SidebarState::default();

        let _ = apply_event(
            &Event::TurnStarted {
                context: ka_protocol::ContextMeter {
                    used: 100,
                    window: 200,
                },
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let full = format!("HTTP 400: {}", "the gateway refused ".repeat(16));
        let _ = apply_event(
            &Event::Error {
                class: ka_protocol::ErrorClass::Protocol,
                retryable: false,
                message: full.clone(),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::TurnFinished {
                stop: ka_protocol::Stop::Error,
                usage: ka_protocol::Usage {
                    input: 6,
                    output: 0,
                    cost: 0.0,
                    ..Default::default()
                },
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let entries = lines.entries();
        let Some(Line::Summary {
            text,
            detail,
            expanded,
            ..
        }) = entries.last()
        else {
            panic!("last entry must be the failure summary: {entries:?}");
        };
        assert!(text.contains('…'), "preview stays truncated: {text}");
        assert!(!*expanded, "rows start collapsed");
        assert_eq!(
            detail.as_deref(),
            Some(full.as_str()),
            "the untruncated error stays reachable through the fold"
        );
    }

    #[test]
    fn row_refs_map_toolblock_rows() {
        let mut t = Transcript::default();
        t.set_width(60);
        t.push(Line::Thought("deep\nthoughts".into()));
        t.push_tool_call(ToolCall {
            head: "→ a".into(),
            ..no_note_call()
        });
        t.push_tool_call(ToolCall {
            head: "→ b".into(),
            excerpt: "line one\nline two".into(),
            ..no_note_call()
        });
        t.toggle_tool_call(1, 1);
        // the thought owns its spacer rows (blank + head + body +
        // blank); the call block starts right after it
        let air = t.rendered_rows_of(0);
        assert_eq!(air, 5, "blank + head + 2 + blank");
        assert_eq!(t.row_ref_at(0), Some(RowRef::Entry(0)));
        assert_eq!(t.row_ref_at(air - 1), Some(RowRef::Entry(0)));
        // no separator between the families: the thought's trailing
        // spacer row is the last thought row; the block starts right at it
        assert_eq!(t.row_ref_at(air), Some(RowRef::ToolCall(1, 0)));
        // a no-note call is its header alone; the next header follows
        // directly (calls stack with no rule between them)
        assert_eq!(t.row_ref_at(air + 1), Some(RowRef::ToolCall(1, 1)));

        // the second call is expanded: header + 2 output rows, so the
        // block is 4 rows (1 + 3) — exactly the height authority's sum
        assert_eq!(t.rendered_rows_of(1), 4);
        let Some(Line::ToolBlock(calls)) = t.entries().get(1) else {
            panic!("entry 1 is the tool block");
        };
        let heights: usize = calls.iter().map(|c| tool_card_height(c, 60)).sum();
        assert_eq!(
            heights,
            t.rendered_rows_of(1),
            "height authority = rendered rows"
        );
        for off in [2usize, 3] {
            assert_eq!(
                t.row_ref_at(air + off),
                Some(RowRef::ToolCall(1, 1)),
                "row {off} of the expanded call"
            );
        }
        assert_eq!(t.row_ref_at(air + 4), None, "past the end");
        assert_eq!(
            t.tool_call(1, 1).map(|c| c.head.as_str()),
            Some("→ b"),
            "the click source resolves the call"
        );
    }

    #[test]
    fn tool_call_expands_full_content_inline() {
        let call = ToolCall {
            head: "→ bash · cargo build".into(),
            ok: true,
            note: "first line".into(),
            excerpt: "first line\n\nthird line".into(),
            spill: None,
            dur: Some(1.25),
            expanded: false,
        };
        use unicode_width::UnicodeWidthStr;
        let text_of = |l: &ratatui::text::Line| -> String {
            l.spans.iter().map(|s| s.content.to_string()).collect()
        };
        // collapsed: the header, then the note turning the thread
        let out = super::render_line(&Line::ToolBlock(vec![call.clone()]), 40, false);
        assert_eq!(out.len(), 2, "collapsed call = header + note");
        assert!(
            text_of(&out[0]).starts_with("✓ $ bash  cargo build ▸"),
            "{out:?}"
        );
        assert!(text_of(&out[0]).ends_with("1.2s"), "{out:?}");
        assert_eq!(text_of(&out[0]).width(), 40, "cluster rides the right edge");
        assert_eq!(text_of(&out[1]), "╰ first line");
        // expanded: the note row stays (now mid-thread), every excerpt
        // line follows on the panel (blank line included)
        let expanded = ToolCall {
            expanded: true,
            ..call
        };
        let out = super::render_line(&Line::ToolBlock(vec![expanded]), 40, false);
        assert_eq!(out.len(), 5, "header + note + 3 output: {out:?}");
        let texts: Vec<String> = out.iter().map(text_of).collect();
        assert!(texts[0].starts_with("✓ $ bash  cargo build ▾"), "{texts:?}");
        assert_eq!(texts[1], "│ first line");
        assert!(texts[2].starts_with("│  first line"), "{texts:?}");
        assert_eq!(out[2].spans[3].style.fg, Some(crate::palette::INK_DIM));
        assert!(texts[3].starts_with("│ "), "{texts:?}");
        assert!(texts[4].starts_with("╰  third line"), "{texts:?}");
        for (row, text) in out[2..].iter().zip(&texts[2..]) {
            assert_eq!(text.width(), 40, "the panel runs to the edge: {text:?}");
            assert!(
                row.spans[2..]
                    .iter()
                    .all(|s| s.style.bg == Some(crate::palette::BG_TOOL)),
                "ok output rides BG_TOOL from column 2: {row:?}"
            );
            assert_eq!(
                row.spans[0].style.bg, None,
                "the thread stays on the canvas"
            );
        }
        // long lines hard-wrap and continue on the thread
        let long = ToolCall {
            excerpt: "x".repeat(100),
            expanded: true,
            ..no_note_call()
        };
        let out = super::render_line(&Line::ToolBlock(vec![long]), 20, false);
        assert!(out.len() > 4, "wrapped into continuation rows: {out:?}");
        for row in &out[1..out.len() - 1] {
            assert_eq!(row.spans[0].content.as_ref(), "│", "railed: {row:?}");
            assert_eq!(text_of(row).width(), 20);
        }
        assert_eq!(out[out.len() - 1].spans[0].content.as_ref(), "╰");
    }

    #[test]
    fn expanded_call_points_at_the_spill_file() {
        let call = ToolCall {
            head: "→ bash".into(),
            excerpt: "tail of the run".into(),
            spill: Some("/tmp/spill-1".into()),
            expanded: true,
            ..no_note_call()
        };
        let out = super::render_line(&Line::ToolBlock(vec![call]), 60, false);
        // the spill pointer is the last body row, turning the thread
        let spill = out.last().unwrap();
        let text: String = spill.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(
            text.contains("spill file") && text.contains("/spills"),
            "{text:?}"
        );
        // the trailer rides the muted tier, not the content tier
        assert_eq!(spill.spans[3].style.fg, Some(crate::palette::META));
        assert_eq!(spill.spans[0].content.as_ref(), "╰");
    }

    #[test]
    fn toggling_expands_and_collapses_in_place() {
        let mut t = Transcript::default();
        t.set_width(40);
        t.push_tool_call(ToolCall {
            head: "→ bash".into(),
            excerpt: "line one\nline two".into(),
            expanded: false,
            ..no_note_call()
        });
        let (entry, call) = t.last_tool_ref().expect("ref for the fresh block");
        assert_eq!(
            t.rendered_rows_of(entry),
            1,
            "collapsed: the header alone (no note to show)"
        );
        // expand: header + two output rows, the flag lives on the call
        t.toggle_tool_call(entry, call);
        assert!(t.tool_call(entry, call).unwrap().expanded, "flag flipped");
        assert_eq!(t.rendered_rows_of(entry), 3, "header + 2 output rows");
        let row_text = |r: usize| -> String {
            t.row(r)
                .unwrap()
                .spans
                .iter()
                .map(|s| s.content.to_string())
                .collect()
        };
        // the note-less call opens straight onto its output
        assert!(row_text(1).starts_with("│  line one"), "{:?}", row_text(1));
        assert!(row_text(2).starts_with("╰  line two"), "{:?}", row_text(2));
        // collapse again: back to the one-line shape
        t.toggle_tool_call(entry, call);
        assert!(!t.tool_call(entry, call).unwrap().expanded);
        assert_eq!(t.rendered_rows_of(entry), 1);
        // stale indices (rewound past the block) no-op instead of panicking
        t.toggle_tool_call(99, 0);
        assert_eq!(t.rendered_rows_of(entry), 1);
        // a call with nothing to reveal never expands
        t.push_tool_call(ToolCall {
            head: "→ read".into(),
            ..no_note_call()
        });
        let (e2, c2) = t.last_tool_ref().expect("ref for the quiet card");
        let before = t.rendered_rows_of(e2);
        t.toggle_tool_call(e2, c2);
        assert!(
            !t.tool_call(e2, c2).unwrap().expanded,
            "quiet card stays put"
        );
        assert_eq!(t.rendered_rows_of(e2), before, "no re-render, no growth");
        // the fold flag survives a width rebuild (it rides the entry data)
        t.toggle_tool_call(entry, call);
        t.set_width(72);
        assert!(
            t.tool_call(entry, call).unwrap().expanded,
            "flag survives resize"
        );
    }

    #[test]
    fn pad_to_width_uses_display_columns() {
        assert_eq!(pad_to_width("ab".into(), 5), "ab   ");
        assert_eq!(pad_to_width("世界".into(), 5), "世界 ");
        assert_eq!(pad_to_width("toolong".into(), 3), "toolong");
    }

    #[test]
    fn queue_head_auto_sends_fifo_and_drains() {
        let mut queue = vec!["first".to_string(), "second".to_string()];
        assert_eq!(pop_queue_head(&mut queue).as_deref(), Some("first"));
        assert_eq!(pop_queue_head(&mut queue).as_deref(), Some("second"));
        assert_eq!(pop_queue_head(&mut queue), None, "empty queue: no send");
    }

    #[test]
    fn queue_draft_pushes_and_acks_shared_by_prefix_and_alt_q() {
        // the `+` prefix and alt+q share queue_draft: trimmed push +
        // one ack line carrying the new depth; blanks never queue
        let mut queue = Vec::new();
        let mut transcript = Transcript::default();
        queue_draft(&mut queue, "  run the tests  ", &mut transcript);
        assert_eq!(queue, vec!["run the tests".to_string()]);
        queue_draft(&mut queue, "then commit", &mut transcript);
        assert_eq!(queue.len(), 2);
        let acks: Vec<&String> = transcript
            .lines
            .iter()
            .filter_map(|l| match l {
                Line::Info(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(
            acks,
            vec![
                &"⏳ queued · 1 waiting for this turn to end".to_string(),
                &"⏳ queued · 2 waiting for this turn to end".to_string(),
            ]
        );
        queue_draft(&mut queue, "   ", &mut transcript);
        assert_eq!(queue.len(), 2, "blank drafts never queue");
    }

    #[test]
    fn tab_completes_skill_starter_onto_its_colon() {
        // argument-bearing commands gain a space; the /skill starter
        // lands on its colon so the names popup is the next frame
        assert_eq!(complete_slash("/model"), "/model ");
        assert_eq!(complete_slash("/skill"), "/skill:");
        // the completed text must open the names popup immediately
        *SKILLS.write().unwrap() = vec!["pdf".to_string()];
        let popup = update_suggestions(&complete_slash("/skill")).expect("names appear");
        let names: Vec<&str> = popup.items.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["/skill:pdf"], "{names:?}");
        *SKILLS.write().unwrap() = Vec::new();
    }

    #[test]
    fn status_bar_badges_a_pending_update() {
        // the badge is the persistent half of the startup check's
        // verdict (the transcript line is the transient half)
        let mut meters = Meters {
            update: Some("v0.2.10".into()),
            ..Default::default()
        };
        let text: String = status_left(&meters)
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(text.contains("\u{2934} v0.2.10"), "badge renders: {text}");
        meters.update = None;
        let text: String = status_left(&meters)
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            !text.contains("\u{2934}"),
            "no badge when up to date: {text}"
        );
    }

    #[test]
    fn busy_title_carries_only_the_queue_hint() {
        // action hints ride the composer's bottom border; the title
        // only appears for a waiting queue
        assert_eq!(busy_input_title(0), None);
        assert_eq!(busy_input_title(2).as_deref(), Some("2 queued"));
    }

    #[test]
    fn input_area_rows_picker_and_draft_only() {
        assert_eq!(input_area_rows(None, "long\ndraft\ntext", 40), 3);
        assert_eq!(input_area_rows(None, "one line", 40), 1);
        // the draft arm folds long lines: 200 cols at width 40 → 5 rows
        assert_eq!(input_area_rows(None, &"x".repeat(200), 40), 5);
        // the picker borrows the box for its four tier rows
        assert_eq!(
            input_area_rows(
                Some(&InputPicker::Mode(ModePicker::for_mode(
                    ka_protocol::Mode::Allow
                ))),
                "x",
                40
            ),
            4
        );
    }

    #[test]
    fn ask_renders_numbered_modal_options() {
        let ask = PendingAsk {
            id: AskId("a".into()),
            question: "allow write to modify files?".into(),
            options: vec!["allow".into(), "deny".into()],
            detail: Some("+a\n".into()),
            selected: 0,
        };
        let rows = ask_modal_body(&ask, 40);
        let text = |l: &ratatui::text::Line<'static>| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        // bold gold question, then the budgeted detail, then options
        assert_eq!(text(&rows[0]), "allow write to modify files?");
        assert_eq!(rows[0].spans[0].style.fg, Some(crate::palette::STRONG));
        assert_eq!(text(&rows[1]), "+a");
        // the selected option leads with the ▌ pointer, its FAINT key
        // cap, then the label in the selection style padded to the width
        assert_eq!(text(&rows[2]), pad_to_width("▌ 1 allow".to_string(), 40));
        assert_eq!(rows[2].spans[1].style.fg, Some(crate::palette::FAINT));
        assert_eq!(rows[2].spans[2].style, selection_style());
        // unselected rows lead two blank cols so the caps align
        assert_eq!(text(&rows[3]), "  2 deny");
        assert_ne!(rows[3].spans[2].style, selection_style());
        // the pointer follows the selection
        let ask = PendingAsk { selected: 1, ..ask };
        let rows = ask_modal_body(&ask, 40);
        assert_eq!(rows[3].spans[2].style, selection_style());
        assert_ne!(rows[2].spans[2].style, selection_style());
    }

    #[test]
    fn ask_detail_rows_colorize_and_clamp() {
        let rows = ask_detail_rows(
            "--- a/f.rs\n+++ b/f.rs\n@@ -1,3 +1,3 @@\n context\n-removed\n+added\n",
            8,
        );
        let fg = |l: &ratatui::text::Line<'static>| l.spans[0].style.fg;
        assert_eq!(rows[0].spans[0].content.as_ref(), "--- a/f.rs");
        assert_eq!(fg(&rows[0]), Some(crate::palette::FAINT));
        assert_eq!(fg(&rows[1]), Some(crate::palette::FAINT));
        assert_eq!(fg(&rows[2]), Some(crate::palette::META));
        assert_eq!(fg(&rows[3]), None, "context stays default");
        assert_eq!(fg(&rows[4]), Some(crate::palette::ERR));
        assert_eq!(fg(&rows[5]), Some(crate::palette::OK));
        // clamp keeps the budget and notes the remainder
        let clamped = ask_detail_rows("ctx\n+added\n-removed\n", 2);
        assert_eq!(clamped.len(), 3);
        assert!(
            clamped[2].spans[0].content.contains("1 more"),
            "{:?}",
            clamped[2].spans[0].content
        );
        // the budget keeps trailer headroom: 2 allowed rows means the
        // third detail row becomes the `… +N more` trailer
        assert_eq!(clamped.len(), 3, "trailer rides in the budget");
    }

    #[test]
    fn wrap_rows_breaks_at_spaces_hard_tokens_and_newlines() {
        // space-preferred breaks
        let rows = wrap_rows("alpha beta gamma", 6);
        let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["alpha", "beta", "gamma"]);
        assert_eq!(rows[0].start, 0);
        assert_eq!(rows[1].start, 6, "continuation starts after the space");

        // hard break: a token with no space still wraps
        let rows = wrap_rows("abcdefghijkl", 5);
        let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["abcde", "fghij", "kl"]);

        // wide chars count as two display columns
        let rows = wrap_rows("世界 world", 5);
        assert_eq!(rows[0].text, "世界", "{}", rows[0].text);
        assert_eq!(rows[1].text, "world");

        // explicit newlines always break; empty lines stay rows
        let rows = wrap_rows("a\n\nb", 10);
        let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["a", "", "b"]);
        assert_eq!(rows[2].start, 3);
    }

    #[test]
    fn visual_cursor_row_maps_wrapped_positions() {
        let rows = wrap_rows("alpha beta", 6);
        assert_eq!(visual_cursor_row(&rows, 0), 0);
        assert_eq!(visual_cursor_row(&rows, 5), 0, "end of 'alpha'");
        assert_eq!(visual_cursor_row(&rows, 6), 1, "cursor after the space");
        assert_eq!(visual_cursor_row(&rows, 10), 1);
    }
    // ── sidebar ──────────────────────────────────────────────────
    use unicode_width::UnicodeWidthStr;

    fn meters_sample() -> Meters {
        Meters {
            model: "mockco/mock".into(),
            mode: "allow".into(),
            effort: String::new(),
            session: "s19a4f2e1b0-3f9c2a81d4b7".into(),
            context: (12_000, 200_000),
            cost: 0.0123,
            cache_hit: None,
            ..Default::default()
        }
    }

    fn plain_text(rows: &[ratatui::text::Line<'static>]) -> Vec<String> {
        rows.iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn info_rows_reports_session_facts() {
        let sidebar = SidebarState {
            cwd: "\u{2026}/projects/ka".into(),
            branch: Some("main".into()),
            ..Default::default()
        };
        let rows = info_rows(&sidebar, &meters_sample(), 60);
        let text = plain_text(&rows);
        assert!(
            text.contains(&format!(
                "\u{2026}/projects/ka {} main",
                crate::icons::BRANCH
            )),
            "{text:?}"
        );
        assert!(text.contains(&"session #3f9c2a81".to_string()), "{text:?}");
        assert!(text.contains(&"model mockco/mock".to_string()), "{text:?}");
        assert!(text.contains(&"cost $0.0123".to_string()), "{text:?}");
        assert!(
            text.iter().any(|t| t.starts_with("ctx 12000/200000")),
            "{text:?}"
        );
    }

    #[test]
    fn todos_rows_mark_done_and_next() {
        let sidebar = SidebarState {
            todos: vec![
                ka_protocol::TodoItem {
                    text: "survey".into(),
                    state: ka_protocol::TodoState::Done,
                    confidence: None,
                },
                ka_protocol::TodoItem {
                    text: "implement".into(),
                    state: ka_protocol::TodoState::Pending,
                    confidence: None,
                },
                ka_protocol::TodoItem {
                    text: "ship".into(),
                    state: ka_protocol::TodoState::Pending,
                    confidence: None,
                },
            ],
            ..Default::default()
        };
        let rows = todos_rows(&sidebar, 60);
        let text = plain_text(&rows);
        let done = text
            .iter()
            .find(|t| t.contains("survey"))
            .expect("done row");
        assert!(done.starts_with("\u{2713} "), "{done}");
        let pending = text
            .iter()
            .find(|t| t.contains("implement"))
            .expect("pending row");
        // the first pending item ("next") rides the filled accent bullet
        assert!(pending.starts_with("\u{25cf} "), "{pending}");
        let later = text
            .iter()
            .find(|t| t.contains("ship"))
            .expect("later pending row");
        // later pending items stay quiet: hollow bullet, no accent
        assert!(later.starts_with("\u{25cb} "), "{later}");
        // done rows carry the crossed-out modifier, the first pending
        // row is accented bold ('next'), later pendings are not
        let done_idx = rows
            .iter()
            .position(|l| l.spans.iter().any(|s| s.content.contains("survey")))
            .unwrap();
        assert!(rows[done_idx].spans.iter().any(|s| {
            s.style
                .add_modifier
                .contains(ratatui::style::Modifier::CROSSED_OUT)
        }));
        let pend_idx = rows
            .iter()
            .position(|l| l.spans.iter().any(|s| s.content.contains("implement")))
            .unwrap();
        assert!(rows[pend_idx].spans.iter().any(|s| {
            s.style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
                && s.style.fg == Some(crate::palette::ACCENT)
        }));
        let later_idx = rows
            .iter()
            .position(|l| l.spans.iter().any(|s| s.content.contains("ship")))
            .unwrap();
        assert!(!rows[later_idx].spans.iter().any(|s| {
            s.style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        }));
        // empty state explains itself instead of rendering nothing
        let empty = todos_rows(&SidebarState::default(), 60);
        assert!(plain_text(&empty)[0].contains("no live todo list"));
    }

    #[test]
    fn inventory_rows_list_skills_agents_and_mcp_health() {
        let sidebar = SidebarState {
            inventory: Inventory {
                mcp: vec![
                    ka_protocol::McpSummary {
                        name: "demo".into(),
                        ok: true,
                        tools: 2,
                    },
                    ka_protocol::McpSummary {
                        name: "jira".into(),
                        ok: false,
                        tools: 0,
                    },
                ],
                skills: vec!["rust-docs".into()],
                agents: vec!["scout".into()],
                prompts: Vec::new(),
                ..Default::default()
            },
            ..Default::default()
        };
        let text = plain_text(&inventory_rows(&sidebar, 60));
        // each section opens with its glyph header
        assert!(
            text.contains(&format!("{} skills", crate::icons::STRIP_SKILLS)),
            "{text:?}"
        );
        assert!(
            text.contains(&format!("{} agents", crate::icons::AGENTS)),
            "{text:?}"
        );
        assert!(
            text.contains(&format!("{} mcp", crate::icons::tool_icon("mcp."))),
            "{text:?}"
        );
        assert!(text.contains(&"rust-docs".to_string()), "{text:?}");
        assert!(text.contains(&"scout".to_string()), "{text:?}");
        assert!(text.iter().any(|t| t == "demo \u{2713} 2"), "{text:?}");
        assert!(text.iter().any(|t| t == "jira \u{2717}"), "{text:?}");
        // nothing discovered: the headers alone still summon the hint row
        let empty = plain_text(&inventory_rows(&SidebarState::default(), 60));
        assert!(
            empty[0].contains("nothing discovered"),
            "empty inventory explains itself: {empty:?}"
        );
    }

    #[test]
    fn popup_rows_truncate_multibyte_to_width() {
        let sidebar = SidebarState {
            cwd: "\u{2026}/\u{4e16}\u{754c}/\u{4e16}\u{754c}".into(),
            branch: Some("main".into()),
            todos: vec![ka_protocol::TodoItem {
                text: "\u{4e16}\u{754c}\u{4e16}\u{754c}\u{4e16}\u{754c}".into(),
                state: ka_protocol::TodoState::Pending,
                confidence: None,
            }],
            ..Default::default()
        };
        for row in info_rows(&sidebar, &Meters::default(), 10) {
            let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(text.width() <= 10, "{text}");
        }
        for row in todos_rows(&sidebar, 10) {
            let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(text.width() <= 10, "{text}");
        }
    }

    #[test]
    fn transcript_width_is_full_width_minus_chrome() {
        // margin cols on each side + the paragraph's side padding; no
        // sidebar column anywhere in the layout anymore
        assert_eq!(transcript_width(120), 116);
        assert_eq!(transcript_width(100), 96);
        assert_eq!(transcript_width(60), 56, "never underflows");
    }

    #[test]
    fn trunc_cols_is_width_aware() {
        assert_eq!(trunc_cols("short", 24), "short");
        let cut = trunc_cols("世界世界世界", 7);
        assert!(cut.width() <= 7, "{cut}");
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn apply_event_updates_sidebar_inventory_and_todos() {
        let mut lines = Transcript::default();
        let mut busy = false;
        let mut busy_since = None;
        let mut meters = Meters::default();
        let mut pending = None;
        let mut turn_produced = false;
        let mut usage = None;
        let mut a = String::new();
        let mut t = String::new();
        let mut tool = String::new();
        let mut live_tool: Option<LiveTool> = None;
        let mut last_user: Option<String> = None;
        let mut last_error: Option<String> = None;
        let mut spills: Vec<String> = Vec::new();
        let mut sidebar = SidebarState::default();

        let _ = apply_event(
            &Event::Inventory {
                tools: vec!["read".into()],
                mcp: vec![ka_protocol::McpSummary {
                    name: "demo".into(),
                    ok: true,
                    tools: 2,
                }],
                agents: vec!["coder".into()],
                skills: vec!["rust-docs".into()],
                prompts: Vec::new(),
                disabled: Vec::new(),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        assert_eq!(sidebar.inventory.tools, vec!["read".to_string()]);
        assert_eq!(sidebar.inventory.skills, vec!["rust-docs".to_string()]);

        let _ = apply_event(
            &Event::Todos {
                items: vec![ka_protocol::TodoItem {
                    text: "dig".into(),
                    state: ka_protocol::TodoState::Done,
                    confidence: None,
                }],
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        assert_eq!(sidebar.todos.len(), 1);
        assert_eq!(sidebar.todos[0].state, ka_protocol::TodoState::Done);
        // whole-list replacement: a follow-up event supersedes
        let _ = apply_event(
            &Event::Todos { items: Vec::new() },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        assert!(sidebar.todos.is_empty());
    }
    #[test]
    fn apply_event_title_sets_sidebar_session_title() {
        let mut lines = Transcript::default();
        let mut busy = false;
        let mut busy_since = None;
        let mut meters = Meters::default();
        let mut pending = None;
        let mut turn_produced = false;
        let mut usage = None;
        let mut a = String::new();
        let mut t = String::new();
        let mut tool = String::new();
        let mut live_tool: Option<LiveTool> = None;
        let mut last_user: Option<String> = None;
        let mut last_error: Option<String> = None;
        let mut spills: Vec<String> = Vec::new();
        let mut sidebar = SidebarState::default();
        let mut feed = |evt: &Event, sidebar: &mut SidebarState| {
            let _ = apply_event(
                evt,
                &mut lines,
                &mut busy,
                &mut busy_since,
                &mut meters,
                &mut pending,
                &mut turn_produced,
                &mut usage,
                &mut a,
                &mut t,
                &mut tool,
                &mut live_tool,
                &mut last_user,
                &mut last_error,
                &mut spills,
                sidebar,
            );
        };

        feed(
            &Event::Title {
                title: "Fix the parser".into(),
            },
            &mut sidebar,
        );
        assert_eq!(sidebar.title.as_deref(), Some("Fix the parser"));
        // empty titles (additive default) never blank a real one
        feed(
            &Event::Title {
                title: String::new(),
            },
            &mut sidebar,
        );
        assert_eq!(sidebar.title.as_deref(), Some("Fix the parser"));

        assert_eq!(sidebar.title.as_deref(), Some("Fix the parser"));
    }

    #[test]
    fn thinking_blocks_collapse_to_band_and_expand() {
        use unicode_width::UnicodeWidthStr;
        let thought = Line::Thought("l1\nl2\nl3\nl4\nl5".into());
        let text_of = |l: &ratatui::text::Line| -> String {
            l.spans.iter().map(|s| s.content.to_string()).collect()
        };

        // collapsed (default): spacer + head + at most three body rows
        // + the "more" row while anything is hidden + spacer
        let collapsed = super::render_line(&thought, 60, false);
        assert_eq!(
            collapsed.len(),
            7,
            "blank + head + 3 of 5 lines + more + blank: {collapsed:?}"
        );
        assert!(collapsed[0].spans.is_empty() && collapsed[6].spans.is_empty());
        let head = text_of(&collapsed[1]);
        assert!(
            head.starts_with(&format!("{} Thinking ▸", crate::icons::THOUGHT)),
            "{head}"
        );
        assert!(head.ends_with("5 lines"), "right-aligned count: {head}");
        assert_eq!(head.width(), 60, "{head}");
        let all: String = collapsed.iter().map(text_of).collect();
        assert!(all.contains("l1") && all.contains("l3"), "{all}");
        assert!(
            !all.contains("l4") && !all.contains("l5"),
            "max 3 rows visible: {all}"
        );
        assert_eq!(text_of(&collapsed[2]), "│ l1");
        assert_eq!(text_of(&collapsed[5]), "│ ⋯ +2 more");
        assert!(!all.contains('▾'), "collapsed: no open cue: {all}");

        // open: head + every line, and the more row and count are gone
        let open = super::render_line(&thought, 60, true);
        assert_eq!(open.len(), 8, "blank + head + 5 lines + blank: {open:?}");
        let text: String = open.iter().map(text_of).collect();
        assert!(text.contains("l1") && text.contains("l5"), "{text}");
        assert!(text_of(&open[1]).ends_with('▾'), "open fold marker: {text}");
        assert!(!text.contains("more"), "{text}");
        assert!(!text.contains("lines"), "no count when open: {text}");

        // single-line thoughts: head without chevron or count + the
        // one body row — nothing to fold is visible at a glance
        let one = super::render_line(&Line::Thought("just one".into()), 60, false);
        assert_eq!(one.len(), 4, "blank + head + the line + blank");
        assert_eq!(
            text_of(&one[1]),
            format!("{} Thinking", crate::icons::THOUGHT),
            "no fold cue, no count"
        );
        assert_eq!(text_of(&one[2]), "│ just one");

        // the toggle rebuilds the cache: row counts follow the flag
        let mut t = Transcript::default();
        t.set_width(60);
        t.push(thought);
        assert_eq!(t.total_rows(), 7, "collapsed band + more row + spacers");
        t.set_thoughts_open(true);
        assert_eq!(t.total_rows(), 8, "expanded after the toggle");
        t.set_thoughts_open(false);
        assert_eq!(t.total_rows(), 7, "collapsed again");
    }

    #[test]
    fn thought_block_rails_its_body_on_the_canvas() {
        // one ✻ per block: the glyph rides the head only; body rows are
        // `│ ` rails (RULE) + THOUGHT prose, and nothing carries a fill
        let open = super::render_line(&Line::Thought("l1\nl2\nl3\nl4\nl5".into()), 60, true);
        let icon_rows = open
            .iter()
            .filter(|l| {
                l.spans
                    .iter()
                    .any(|s| s.content.contains(crate::icons::THOUGHT))
            })
            .count();
        assert_eq!(icon_rows, 1, "exactly one ✻ row (the head)");
        assert_eq!(open[1].spans[0].style.fg, Some(crate::palette::THINK));
        for l in &open[2..open.len() - 1] {
            assert_eq!(l.spans[0].content.as_ref(), "│ ");
            assert_eq!(l.spans[0].style.fg, Some(crate::palette::RULE));
            assert_eq!(l.spans[1].style, crate::palette::THOUGHT);
        }
        for l in &open {
            assert!(
                l.spans.iter().all(|s| s.style.bg.is_none()),
                "no tint: {l:?}"
            );
        }
        // long lines wrap at W-2 under the rail
        let long = super::render_line(&Line::Thought("word ".repeat(30)), 40, true);
        assert!(long.len() > 4, "{long:?}");
        for l in &long[2..long.len() - 1] {
            assert_eq!(l.spans[0].content.as_ref(), "│ ");
            assert!(l.width() <= 40, "{l:?}");
        }
    }

    /// A call that produced nothing readable is its header alone: no
    /// thread, no empty interior.
    #[test]
    fn empty_output_calls_render_one_header_row() {
        let text_of = |l: &ratatui::text::Line| -> String {
            l.spans.iter().map(|s| s.content.to_string()).collect()
        };
        let out = super::render_line(&Line::ToolBlock(vec![no_note_call()]), 40, false);
        assert_eq!(out.len(), 1, "the header alone: {out:?}");
        assert!(text_of(&out[0]).starts_with('✓'), "{out:?}");
        // the shape in the wild: a failed call that produced nothing
        // (blocked/refused)
        let refused = ToolCall {
            ok: false,
            ..no_note_call()
        };
        let out = super::render_line(&Line::ToolBlock(vec![refused]), 44, false);
        assert_eq!(out.len(), 1, "failed, nothing to say: {out:?}");
        assert!(
            text_of(&out[0]).starts_with('✗'),
            "status leads the row: {out:?}"
        );
        // the height authority (click mapping rides it) agrees
        assert_eq!(super::tool_card_height(&no_note_call(), 40), 1);
        // expanded output still opens the body, note or not
        let with_output = ToolCall {
            excerpt: "one\ntwo".into(),
            expanded: true,
            ..no_note_call()
        };
        assert_eq!(
            super::tool_card_height(&with_output, 40),
            3,
            "header + two output rows"
        );
        assert_eq!(
            super::render_line(&Line::ToolBlock(vec![with_output]), 40, false).len(),
            3,
            "rendered rows match the height authority"
        );
    }

    #[test]
    fn tool_header_styles_and_state_panel() {
        use unicode_width::UnicodeWidthStr;
        let call = ToolCall {
            head: "→ bash · cargo build".into(),
            ok: true,
            note: "done".into(),
            excerpt: "done".into(),
            spill: None,
            dur: Some(1.25),
            expanded: false,
        };
        let out = super::render_line(&Line::ToolBlock(vec![call]), 40, false);
        assert_eq!(out.len(), 2, "collapsed call: header + note");
        let text_of = |l: &ratatui::text::Line| -> String {
            l.spans.iter().map(|s| s.content.to_string()).collect()
        };
        let top = &out[0];
        let text = text_of(top);
        // status, icon + tool, detail, chevron left; duration right
        assert!(text.starts_with("✓ $ bash  cargo build ▸"), "{text:?}");
        assert!(text.ends_with("1.2s"), "{text:?}");
        assert_eq!(text.width(), 40, "the cluster rides the right edge");
        assert_eq!(top.spans[0].style.fg, Some(crate::palette::OK));
        assert_eq!(top.spans[2].style.fg, Some(crate::palette::TOOL));
        assert_eq!(top.spans[4].content.as_ref(), "bash");
        assert!(
            top.spans[4]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        assert_eq!(top.spans[6].style.fg, Some(crate::palette::INK_DIM));
        let dur = top.spans.last().unwrap();
        assert_eq!(dur.content.as_ref(), "1.2s");
        assert_eq!(dur.style.fg, Some(crate::palette::FAINT));
        assert!(
            top.spans.iter().all(|s| s.style.bg.is_none()),
            "no header tint: {top:?}"
        );
        assert_eq!(text_of(&out[1]), "╰ done");
        // a failed call flips the status to the red ✗ and its output
        // panel to the error tint
        let fail = ToolCall {
            ok: false,
            excerpt: "boom".into(),
            expanded: true,
            ..no_note_call()
        };
        let out = super::render_line(&Line::ToolBlock(vec![fail]), 20, false);
        assert_eq!(out[0].spans[0].content.as_ref(), "✗");
        assert_eq!(out[0].spans[0].style.fg, Some(crate::palette::ERR));
        assert!(
            out[1].spans[2..]
                .iter()
                .all(|s| s.style.bg == Some(crate::palette::BG_ERR_TINT)),
            "failed output rides the error tint: {:?}",
            out[1]
        );
        // sub-tenth durations and replayed calls omit the duration
        let quiet = ToolCall {
            head: "→ x".into(),
            dur: Some(0.04),
            ..no_note_call()
        };
        let out = super::render_line(&Line::ToolBlock(vec![quiet]), 20, false);
        let text = text_of(&out[0]);
        assert!(!text.contains("0.0s"), "{text:?}");
        // a long detail truncates; the chevron survives at the edge
        let long = ToolCall {
            head: format!("→ read · {}", "src/".repeat(20)),
            excerpt: "x".into(),
            ..no_note_call()
        };
        let out = super::render_line(&Line::ToolBlock(vec![long]), 30, false);
        let text = text_of(&out[0]);
        assert!(text.width() <= 30, "{text:?}");
        assert!(text.ends_with("… ▸"), "{text:?}");
        assert_eq!(text.width(), 30, "{text:?}");
    }

    #[test]
    fn bash_exit_code_parses_the_engine_bash_hand_format() {
        // ka-engine hands/bash.rs leads failed output with `(exit N)`
        // before the echoed command — this pin is byte-exact with that
        let content = format!("(exit {})\n$ {}\n", 1, "false");
        assert_eq!(super::bash_exit_code(&content), Some(1));
        assert_eq!(super::bash_exit_code("(exit 130)\n$ sleep 9"), Some(130));
        // the other engine shapes never parse as a code
        assert_eq!(super::bash_exit_code("(no exit status)\n$ x"), None);
        assert_eq!(super::bash_exit_code("$ false\n"), None);
        assert_eq!(super::bash_exit_code("boom"), None);
        assert_eq!(super::bash_exit_code("(exit )"), None);
        assert_eq!(super::bash_exit_code("(exit x)"), None);
        assert_eq!(super::bash_exit_code(""), None);
    }

    #[test]
    fn bash_failure_card_surfaces_the_exit_code() {
        let fail = ToolCall {
            head: "→ bash · false".into(),
            ok: false,
            note: "(exit 1)".into(),
            ..no_note_call()
        };
        let out = super::render_line(&Line::ToolBlock(vec![fail.clone()]), 44, false);
        let text: String = out[0].spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.ends_with("⏎ exit 1"), "{text:?}");
        assert!(!text.contains("0.0s"), "replay-fast call: no duration");
        // the exit note carries the error tier
        let exit_span = out[0].spans.last().unwrap();
        assert_eq!(exit_span.style.fg, Some(crate::palette::ERR));
        // with a measured duration the cluster reads `⏎ exit N  {dur}`
        let timed = ToolCall {
            dur: Some(2.0),
            ..fail
        };
        let out = super::render_line(&Line::ToolBlock(vec![timed]), 44, false);
        let text: String = out[0].spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.ends_with("⏎ exit 1  2.0s"), "{text:?}");
        // a successful call never shows an exit note, even when its
        // note happens to carry one
        let ok = ToolCall {
            head: "→ bash · echo".into(),
            ok: true,
            note: "(exit 1)".into(),
            ..no_note_call()
        };
        let out = super::render_line(&Line::ToolBlock(vec![ok]), 44, false);
        let text: String = out[0].spans.iter().map(|s| s.content.to_string()).collect();
        assert!(!text.contains("exit"), "{text:?}");
    }

    #[test]
    fn consecutive_calls_stack_without_rules() {
        let a = ToolCall {
            head: "→ read · lib.rs".into(),
            ..no_note_call()
        };
        let b = ToolCall {
            head: "→ bash · cargo build".into(),
            note: "ok".into(),
            ..no_note_call()
        };
        let out = super::render_line(&Line::ToolBlock(vec![a, b]), 40, false);
        assert_eq!(out.len(), 3, "1 (no note) + header + note, nothing between");
        let text_of = |l: &ratatui::text::Line| -> String {
            l.spans.iter().map(|s| s.content.to_string()).collect()
        };
        assert!(text_of(&out[0]).starts_with("✓ ≡ read  lib.rs"), "{out:?}");
        assert!(
            text_of(&out[1]).starts_with("✓ $ bash  cargo build"),
            "call two follows directly: {out:?}"
        );
        assert_eq!(text_of(&out[2]), "╰ ok");
    }

    /// Every overlay speaks one dialect: rounded corners. Opens each
    /// modal on a test backend and checks its top-left corner glyph.
    #[test]
    fn all_modals_rounded() {
        use ratatui::backend::TestBackend;
        let mk = |modal: Modal| -> ratatui::Terminal<TestBackend> {
            let mut terminal = ratatui::Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal
                .draw(|f| {
                    let mut t = Transcript::default();
                    super::render(
                        f,
                        &mut t,
                        None,
                        "",
                        0,
                        false,
                        None,
                        Instant::now(),
                        0,
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some(&modal),
                        None,
                        &Meters::default(),
                        &SidebarState::default(),
                        false,
                        true,
                        "◆",
                        &std::cell::Cell::new(None::<StripZones>),
                        None,
                        &std::cell::Cell::new(None::<TitleArrows>),
                        &std::cell::Cell::new(None::<ratatui::layout::Rect>),
                        &std::cell::Cell::new(None::<ratatui::layout::Rect>),
                        &std::cell::Cell::new(None::<(ratatui::layout::Rect, usize)>),
                        &std::cell::Cell::new(
                            None::<(ratatui::layout::Rect, usize, usize, usize, usize)>,
                        ),
                        &std::cell::Cell::new(0u16),
                        None,
                        None,
                    )
                })
                .unwrap();
            terminal
        };
        let modals = vec![
            Modal::Session(SessionPicker {
                sessions: vec![],
                selected: 0,
                filter: String::new(),
                current: None,
            }),
            Modal::Help,
            Modal::Model(ModelPicker {
                models: vec![],
                vendor: None,
                selected: 0,
                filter: String::new(),
            }),
            Modal::Provider(ProviderPicker {
                providers: vec![],
                counts: vec![],
                selected: 0,
                filter: String::new(),
            }),
            Modal::Key(KeyPrompt {
                provider: "mockco".into(),
                env_var: "MOCK_KEY".into(),
                doc_url: String::new(),
                drill: None,
                input: String::new(),
                pending_model: None,
            }),
            Modal::Spills {
                items: vec![],
                selected: 0,
            },
            Modal::Prompts {
                items: vec![],
                selected: 0,
            },
            Modal::Rewind {
                items: vec![(1, "hello".into())],
                selected: 0,
            },
            Modal::Memory {
                rows: vec![],
                inbox: vec![],
                selected: 0,
            },
            Modal::Usage { rows: vec![] },
            Modal::Context { rows: vec![] },
            Modal::Tree {
                items: vec![],
                actions: vec![],
                selected: 0,
            },
            Modal::Tasks {
                entries: vec![],
                selected: 0,
            },
            Modal::TaskDetail {
                id: 1,
                text: "x".into(),
                scroll: None,
            },
            Modal::Debug {
                rows: vec![],
                scroll: None,
            },
            Modal::Todos { rows: vec![] },
            Modal::Skills { rows: vec![] },
            Modal::Info { rows: vec![] },
        ];
        // every pinned title: monochrome icon prefix + literal (model
        // picker is vendor-derived and stays unprefixed; the task views
        // wear their tool glyph)
        let tasks = crate::icons::tool_icon("tasks");
        let titles = vec![
            modal_title("sessions"),
            modal_title("help"),
            "model".to_string(),
            modal_title("providers"),
            modal_title("api key"),
            modal_title("spills"),
            modal_title("prompts"),
            modal_title("rewind"),
            modal_title("memory"),
            modal_title("usage"),
            modal_title("context"),
            modal_title("tree"),
            format!("{tasks} tasks"),
            format!("{tasks} task t-1"),
            format!("{} debug sessions", crate::icons::tool_icon("debug")),
            modal_title("todos"),
            modal_title("skills · agents · mcp"),
            modal_title("info"),
        ];
        assert_eq!(modals.len(), titles.len(), "title pin per modal");
        for (m, want) in modals.into_iter().zip(titles) {
            let terminal = mk(m.clone());
            let buf = terminal.backend().buffer();
            // find the modal's top border row: a `╭` somewhere on the frame
            let has_rounded_top_left = (0..buf.area.width).any(|x| buf[(x, 0)].symbol() == "╭")
                || (1..buf.area.height.saturating_sub(8))
                    .any(|y| (0..buf.area.width).any(|x| buf[(x, y)].symbol() == "╭"));
            assert!(has_rounded_top_left, "{m:?} must draw a rounded corner");
            // the title rides the top border; wide glyphs leave a gap
            // cell in the buffer, so compare whitespace-collapsed text
            let text: String = buf.content.iter().map(|c| c.symbol().to_string()).collect();
            let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(
                flat.contains(want.as_str()),
                "{m:?} title must read `{want}`: {flat}"
            );
            // every modal frame carries the ✕ close chip (the
            // permission ask, which shares no frame, is not in this list)
            assert!(
                flat.contains(crate::icons::CLOSE),
                "{m:?} must render the ✕ close chip: {flat}"
            );
        }
    }

    #[test]
    fn header_bar_shows_brand_title_and_place() {
        let mut sidebar = SidebarState {
            cwd: "~/src/ka".into(),
            branch: Some("main".into()),
            ..Default::default()
        };
        let header = |sidebar: &SidebarState, w: u16| -> String {
            let f = plain_frame(&mut Transcript::default(), (w, 24), "", sidebar, None, None);
            buf_row(f.terminal.backend().buffer(), 0)
        };
        let row = header(&sidebar, 120);
        assert!(row.contains("◆ ka"), "brand glyph + name: {row}");
        assert!(row.contains("new session"), "untitled placeholder: {row}");
        sidebar.title = Some("refactor the login flow".into());
        let row = header(&sidebar, 120);
        assert!(row.contains("› refactor the login flow"), "{row}");
        assert!(row.contains("main"), "the place rides the right: {row}");
        // narrow: the place drops before the title truncates
        let row = header(&sidebar, 40);
        assert!(!row.contains("main"), "{row}");
        assert!(row.contains("refactor the login flow"), "{row}");
    }

    #[test]
    fn toast_renders_and_expires() {
        use ratatui::backend::TestBackend;
        let draw = |toast: Option<&str>| -> (String, usize) {
            let mut terminal = ratatui::Terminal::new(TestBackend::new(120, 40)).unwrap();
            let mut rows_with = 0;
            terminal
                .draw(|f| {
                    let mut t = Transcript::default();
                    super::render(
                        f,
                        &mut t,
                        None,
                        "",
                        0,
                        false,
                        None,
                        Instant::now(),
                        0,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        &Meters::default(),
                        &SidebarState::default(),
                        false,
                        true,
                        "◆",
                        &std::cell::Cell::new(None::<StripZones>),
                        toast,
                        &std::cell::Cell::new(None::<TitleArrows>),
                        &std::cell::Cell::new(None::<ratatui::layout::Rect>),
                        &std::cell::Cell::new(None::<ratatui::layout::Rect>),
                        &std::cell::Cell::new(None::<(ratatui::layout::Rect, usize)>),
                        &std::cell::Cell::new(
                            None::<(ratatui::layout::Rect, usize, usize, usize, usize)>,
                        ),
                        &std::cell::Cell::new(0u16),
                        None,
                        None,
                    )
                })
                .unwrap();
            let buf = terminal.backend().buffer();
            let text: String = buf.content.iter().map(|c| c.symbol().to_string()).collect();
            let _ = &mut rows_with;
            (text, rows_with)
        };
        let (with_toast, _) = draw(Some("✓ copied last reply"));
        assert!(
            with_toast.contains("copied last reply"),
            "toast visible while fresh"
        );
        let (without, _) = draw(None);
        assert!(
            !without.contains("copied last reply"),
            "no toast row without a toast"
        );
        // the run loop only passes the message while younger than the TTL
        let mut toast = Some(("hi".to_string(), Instant::now()));
        assert!(
            toast
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() < TOAST_TTL)
        );
        toast = Some((
            "hi".to_string(),
            Instant::now() - TOAST_TTL - Duration::from_secs(1),
        ));
        assert!(
            toast
                .as_ref()
                .is_none_or(|(_, at)| at.elapsed() >= TOAST_TTL)
        );
    }

    #[test]
    fn mouse_toggle_toasts() {
        // the toggle announces itself with the persisted mode names
        assert!(mouse_mode_text(true).contains("capture"));
        assert!(mouse_mode_text(false).contains("native"));
    }

    #[test]
    fn slash_popup_selection_stays_visible() {
        use ratatui::backend::TestBackend;
        let items: Vec<(String, String)> = (0..30)
            .map(|i| (format!("/cmd{i:02}"), format!("desc {i}")))
            .collect();
        let draw = |selected: usize| -> String {
            let popup = SlashPopup {
                items: items.clone(),
                selected,
            };
            let mut terminal = ratatui::Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal
                .draw(|f| {
                    let mut t = Transcript::default();
                    super::render(
                        f,
                        &mut t,
                        None,
                        "/c",
                        2,
                        false,
                        None,
                        Instant::now(),
                        0,
                        None,
                        None,
                        Some(&popup),
                        None,
                        None,
                        None,
                        None,
                        &Meters::default(),
                        &SidebarState::default(),
                        false,
                        true,
                        "◆",
                        &std::cell::Cell::new(None::<StripZones>),
                        None,
                        &std::cell::Cell::new(None::<TitleArrows>),
                        &std::cell::Cell::new(None::<ratatui::layout::Rect>),
                        &std::cell::Cell::new(None::<ratatui::layout::Rect>),
                        &std::cell::Cell::new(None::<(ratatui::layout::Rect, usize)>),
                        &std::cell::Cell::new(
                            None::<(ratatui::layout::Rect, usize, usize, usize, usize)>,
                        ),
                        &std::cell::Cell::new(0u16),
                        None,
                        None,
                    )
                })
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol().to_string())
                .collect()
        };
        // selection 0: /cmd00 visible
        assert!(draw(0).contains("/cmd00"));
        // deep in the list: the selected row is on screen, the top of the
        // list is scrolled away — the old take(7) render lost the selection
        let deep = draw(24);
        assert!(deep.contains("/cmd24"), "selected row must be visible");
        assert!(!deep.contains("/cmd00"), "list is windowed");
        // bottom of the list
        assert!(draw(29).contains("/cmd29"));
    }

    #[test]
    fn empty_idle_transcript_shows_the_welcome() {
        use ratatui::backend::TestBackend;
        let frame_text = |transcript: &mut Transcript, busy: bool, fresh: bool| -> String {
            let mut terminal = ratatui::Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal
                .draw(|f| {
                    super::render(
                        f,
                        transcript,
                        None,
                        "",
                        0,
                        busy,
                        None,
                        Instant::now(),
                        0,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        &Meters::default(),
                        &SidebarState::default(),
                        fresh,
                        true,
                        "◆",
                        &std::cell::Cell::new(None::<StripZones>),
                        None,
                        &std::cell::Cell::new(None::<TitleArrows>),
                        &std::cell::Cell::new(None::<ratatui::layout::Rect>),
                        &std::cell::Cell::new(None::<ratatui::layout::Rect>),
                        &std::cell::Cell::new(None::<(ratatui::layout::Rect, usize)>),
                        &std::cell::Cell::new(
                            None::<(ratatui::layout::Rect, usize, usize, usize, usize)>,
                        ),
                        &std::cell::Cell::new(0u16),
                        None,
                        None,
                    )
                })
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol().to_string())
                .collect()
        };
        let frame = frame_text(&mut Transcript::default(), false, true);
        assert!(frame.contains("new conversation"), "welcome on fresh chat");
        assert!(frame.contains("██╗"), "the big logo at 120x40");
        assert!(frame.contains("keys & commands"), "tips grid present");
        // busy, carrying entries, or an older chat still replaying: the
        // welcome steps aside
        assert!(
            !frame_text(&mut Transcript::default(), true, true).contains("new conversation"),
            "busy turn: no welcome"
        );
        assert!(
            !frame_text(&mut Transcript::default(), false, false).contains("new conversation"),
            "older chat before its replay lands: no welcome"
        );
        let mut t = Transcript::default();
        t.set_width(80);
        t.push(Line::User("hello".into()));
        assert!(
            !frame_text(&mut t, false, true).contains("new conversation"),
            "carrying entries: no welcome"
        );
    }

    #[test]
    fn idle_hints_teach_the_active_mouse_mode() {
        use ratatui::backend::TestBackend;
        let draw = |captured: bool| -> String {
            // seed the place so the header bar renders `cwd ⎇ branch`
            let sidebar = SidebarState {
                cwd: "/tmp/ka".to_string(),
                branch: Some("main".to_string()),
                ..Default::default()
            };
            let mut terminal = ratatui::Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal
                .draw(|f| {
                    super::render(
                        f,
                        &mut Transcript::default(),
                        None,
                        "",
                        0,
                        false,
                        None,
                        Instant::now(),
                        0,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        &Meters::default(),
                        &sidebar,
                        false,
                        captured,
                        "◆",
                        &std::cell::Cell::new(None::<StripZones>),
                        None,
                        &std::cell::Cell::new(None::<TitleArrows>),
                        &std::cell::Cell::new(None::<ratatui::layout::Rect>),
                        &std::cell::Cell::new(None::<ratatui::layout::Rect>),
                        &std::cell::Cell::new(None::<(ratatui::layout::Rect, usize)>),
                        &std::cell::Cell::new(
                            None::<(ratatui::layout::Rect, usize, usize, usize, usize)>,
                        ),
                        &std::cell::Cell::new(0u16),
                        None,
                        None,
                    )
                })
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol().to_string())
                .collect()
        };
        let captured = draw(true);
        assert!(
            !captured.contains("⇧drag"),
            "the shift-drag chip is gone from the idle hints: {:?}",
            captured
                .chars()
                .filter(|c| *c != ' ')
                .take(400)
                .collect::<String>()
        );
        assert!(
            captured.contains("^p/^n"),
            "captured mode still teaches prompt history: {captured}"
        );
        let native = draw(false);
        assert!(
            native.contains("drag") && native.contains("↑↓ scroll"),
            "native mode teaches plain drag + scroll: {native}"
        );
        assert!(
            !native.contains("⇧drag"),
            "no shift bypass advertised without capture"
        );
        // the composer's strip buttons render in both modes; `cwd ⎇
        // branch` rides the header bar's right side. Compare
        // whitespace-collapsed text.
        let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            native.contains("todos alt+o")
                && native.contains("skills ^t")
                && native.contains("info alt+i"),
            "strip buttons visible in native mode: {native}"
        );
        assert!(
            flat(&native).contains(&format!("/tmp/ka {} main", crate::icons::BRANCH)),
            "header carries cwd ⎇ branch: {native}"
        );
        assert!(
            draw(true).contains("todos alt+o"),
            "strip visible in capture too"
        );
    }

    #[test]
    fn jump_to_user_message_steps_between_user_rows() {
        // user messages start at rows 5, 40, 100; total 200, visible 20
        let rows = [5usize, 40, 100];
        let mut scroll: Option<usize> = None;
        // pinned at the tail (start 180): up finds the newest message
        jump_to_user_message(&mut scroll, &rows, 200, 20, true);
        assert_eq!(scroll, Some(100));
        // down from there: nothing below the pinned tail stays pinned
        scroll = None;
        jump_to_user_message(&mut scroll, &rows, 200, 20, false);
        assert_eq!(scroll, None);
        // anchored mid-history: up = previous, down = next
        scroll = Some(50);
        jump_to_user_message(&mut scroll, &rows, 200, 20, true);
        assert_eq!(scroll, Some(40));
        scroll = Some(50);
        jump_to_user_message(&mut scroll, &rows, 200, 20, false);
        assert_eq!(scroll, Some(100));
        // exactly on a message start: strict comparison steps past it
        scroll = Some(40);
        jump_to_user_message(&mut scroll, &rows, 200, 20, false);
        assert_eq!(scroll, Some(100));
        // above the first message: up is a no-op
        scroll = Some(0);
        jump_to_user_message(&mut scroll, &rows, 200, 20, true);
        assert_eq!(scroll, Some(0));
        // past the last message, not pinned: down lands on the live tail
        scroll = Some(120);
        jump_to_user_message(&mut scroll, &rows, 200, 20, false);
        assert_eq!(scroll, None);
        // no user rows / zero visible: never moves
        scroll = Some(7);
        jump_to_user_message(&mut scroll, &[], 200, 20, true);
        assert_eq!(scroll, Some(7));
        scroll = Some(7);
        jump_to_user_message(&mut scroll, &rows, 200, 0, false);
        assert_eq!(scroll, Some(7));
    }

    #[test]
    fn wheel_steps_scroll_and_repin() {
        let mut scroll = None;
        line_up(&mut scroll, 100, 20);
        assert_eq!(scroll, Some(77), "three rows up from the pinned tail");
        line_down(&mut scroll, 100, 20);
        assert_eq!(scroll, None, "77+3 reaches the tail anchor: re-pins");
        scroll = Some(40);
        line_down(&mut scroll, 100, 20);
        assert_eq!(scroll, Some(43));
        line_down(&mut scroll, 10, 20);
        assert_eq!(scroll, None, "everything visible: always pinned");
    }

    #[test]
    fn strip_zone_hit_testing() {
        let zone = StripZones {
            todos: ratatui::layout::Rect {
                x: 1,
                y: 38,
                width: 11,
                height: 1,
            },
            skills: ratatui::layout::Rect {
                x: 15,
                y: 38,
                width: 9,
                height: 1,
            },
            info: ratatui::layout::Rect {
                x: 27,
                y: 38,
                width: 11,
                height: 1,
            },
        };
        assert_eq!(zone.hit(1, 38), Some(StripButton::Todos));
        assert_eq!(zone.hit(11, 38), Some(StripButton::Todos));
        assert_eq!(zone.hit(15, 38), Some(StripButton::Skills));
        assert_eq!(zone.hit(27, 38), Some(StripButton::Info));
        assert_eq!(zone.hit(14, 38), None, "gap between buttons");
        assert_eq!(zone.hit(15, 37), None, "row above the strip");
    }

    #[test]
    fn title_arrows_hit_testing() {
        let zone = TitleArrows {
            up: ratatui::layout::Rect {
                x: 88,
                y: 1,
                width: 2,
                height: 1,
            },
            down: ratatui::layout::Rect {
                x: 90,
                y: 1,
                width: 2,
                height: 1,
            },
        };
        assert_eq!(zone.hit(88, 1), Some(true), "up arrow cell");
        assert_eq!(zone.hit(89, 1), Some(true), "up zone is two cells wide");
        assert_eq!(zone.hit(90, 1), Some(false), "down arrow cell");
        assert_eq!(zone.hit(91, 1), Some(false), "down zone is two cells wide");
        assert_eq!(zone.hit(92, 1), None, "past the arrows");
        assert_eq!(zone.hit(88, 2), None, "row below the title");
    }

    #[test]
    fn title_arrows_render_and_record_click_zones() {
        let mut t = Transcript::default();
        t.push(Line::User("hello".into()));
        let f = plain_frame(&mut t, (120, 40), "", &SidebarState::default(), None, None);
        let buf = f.terminal.backend().buffer();
        let arrows = f.arrows.expect("click zones recorded");
        // the header bar's right end: each zone covers ` ▲` / ` ▼`
        assert_eq!((arrows.up.y, arrows.down.y), (0, 0));
        assert_eq!(buf[(arrows.up.x + 1, 0)].symbol(), "▲");
        assert_eq!(buf[(arrows.down.x + 1, 0)].symbol(), "▼");
        assert_eq!(buf[(arrows.up.x + 1, 0)].fg, crate::palette::ACCENT);
        assert!(arrows.down.right() < 120, "a col of air closes the bar");
        assert_eq!(arrows.hit(arrows.up.x + 1, 0), Some(true));
        assert_eq!(arrows.hit(arrows.down.x + 1, 0), Some(false));
        // no user message yet: no arrows, no zones
        let f = plain_frame(
            &mut Transcript::default(),
            (120, 40),
            "",
            &SidebarState::default(),
            None,
            None,
        );
        assert!(f.arrows.is_none());
        assert!(!buf_row(f.terminal.backend().buffer(), 0).contains('▲'));
    }

    #[test]
    fn live_region_stays_inside_the_transcript_across_resizes() {
        use ratatui::backend::TestBackend;
        // a cached tool row plus a live block streaming wide chars and a
        // 200-col token, while the terminal width drags: the live region
        // folds at each frame's own width and no transcript surface
        // bleeds into the header, composer, or status line
        let mut t = Transcript::default();
        t.push_tool_call(ToolCall {
            head: "→ bash · cargo build".into(),
            ..no_note_call()
        });
        let live = LiveBlock {
            thought: "thinking".into(),
            tool_header: "→ bash · cargo build".into(),
            live_tool: Some(LiveTool {
                id: "c1".into(),
                preview: vec![
                    format!("{} {}", "世界".repeat(30), "x".repeat(200)),
                    "second line".to_string(),
                ],
                last: None,
                started: Instant::now(),
            }),
            md: crate::markdown::render("# heading\ntext", 60),
        };
        let surfaces = [
            crate::palette::BG_TOOL,
            crate::palette::BG_CODE,
            crate::palette::BG_USER,
            crate::palette::BG_ERR_TINT,
        ];
        for width in [99u16, 100, 101] {
            let mut terminal = ratatui::Terminal::new(TestBackend::new(width, 24)).unwrap();
            let tx_width = std::cell::Cell::new(0u16);
            terminal
                .draw(|f| {
                    super::render(
                        f,
                        &mut t,
                        None,
                        "",
                        0,
                        true,
                        Some(Instant::now()),
                        Instant::now(),
                        0,
                        None,
                        Some(&live),
                        None,
                        None,
                        None,
                        None,
                        None,
                        &Meters::default(),
                        &SidebarState::default(),
                        false,
                        true,
                        "◆",
                        &std::cell::Cell::new(None::<StripZones>),
                        None,
                        &std::cell::Cell::new(None::<TitleArrows>),
                        &std::cell::Cell::new(None::<ratatui::layout::Rect>),
                        &std::cell::Cell::new(None::<ratatui::layout::Rect>),
                        &std::cell::Cell::new(None::<(ratatui::layout::Rect, usize)>),
                        &std::cell::Cell::new(
                            None::<(ratatui::layout::Rect, usize, usize, usize, usize)>,
                        ),
                        &tx_width,
                        None,
                        None,
                    )
                })
                .unwrap();
            assert_eq!(tx_width.get(), transcript_width(width), "w{width}");
            let buf = terminal.backend().buffer();
            let area = buf.area;
            // header (row 0) + composer (3 rows) + status line (last row)
            let chrome = std::iter::once(0).chain((area.height - 4)..area.height);
            for y in chrome {
                for x in 0..area.width {
                    let bg = buf[(x, y)].bg;
                    assert!(!surfaces.contains(&bg), "surface bleed at {x},{y} w{width}");
                }
            }
        }
    }

    /// Render one frame with the given overlays and hand back the
    /// terminal plus the zone cells `render` published — the same
    /// read-back pattern as `title_arrows_render_and_record_click_zones`.
    fn draw_overlay(
        ask: Option<&PendingAsk>,
        popup: Option<&SlashPopup>,
        modal: Option<&Modal>,
        picker: Option<&InputPicker>,
    ) -> (
        ratatui::Terminal<ratatui::backend::TestBackend>,
        std::cell::Cell<Option<ratatui::layout::Rect>>,
        std::cell::Cell<Option<ratatui::layout::Rect>>,
    ) {
        use ratatui::backend::TestBackend;
        let mut terminal = ratatui::Terminal::new(TestBackend::new(120, 40)).unwrap();
        let modal_zone: std::cell::Cell<Option<ratatui::layout::Rect>> = std::cell::Cell::new(None);
        let popup_zone: std::cell::Cell<Option<ratatui::layout::Rect>> = std::cell::Cell::new(None);
        let mut t = Transcript::default();
        terminal
            .draw(|f| {
                super::render(
                    f,
                    &mut t,
                    None,
                    "",
                    0,
                    false,
                    None,
                    Instant::now(),
                    0,
                    ask,
                    None,
                    popup,
                    None,
                    None,
                    modal,
                    picker,
                    &Meters::default(),
                    &SidebarState::default(),
                    false,
                    true,
                    "◆",
                    &std::cell::Cell::new(None::<StripZones>),
                    None,
                    &std::cell::Cell::new(None::<TitleArrows>),
                    &modal_zone,
                    &popup_zone,
                    &std::cell::Cell::new(None::<(ratatui::layout::Rect, usize)>),
                    &std::cell::Cell::new(
                        None::<(ratatui::layout::Rect, usize, usize, usize, usize)>,
                    ),
                    &std::cell::Cell::new(0u16),
                    None,
                    None,
                )
            })
            .unwrap();
        (terminal, modal_zone, popup_zone)
    }

    /// A released left press resolves through the zones in
    /// strip → arrows → transcript order; dead cells resolve to
    /// nothing. Transcript rows map through `row_ref_at` (an entry
    /// click becomes a thought toggle, which no-ops for non-thought
    /// entries exactly like the old press-time handler).
    #[test]
    fn click_target_maps_strip_arrows_and_transcript_rows() {
        let strip = std::cell::Cell::new(Some(StripZones {
            todos: ratatui::layout::Rect {
                x: 2,
                y: 38,
                width: 10,
                height: 1,
            },
            skills: ratatui::layout::Rect {
                x: 14,
                y: 38,
                width: 10,
                height: 1,
            },
            info: ratatui::layout::Rect {
                x: 26,
                y: 38,
                width: 10,
                height: 1,
            },
        }));
        let arrows = std::cell::Cell::new(Some(TitleArrows {
            up: ratatui::layout::Rect {
                x: 114,
                y: 0,
                width: 1,
                height: 1,
            },
            down: ratatui::layout::Rect {
                x: 116,
                y: 0,
                width: 1,
                height: 1,
            },
        }));
        let tx = std::cell::Cell::new(Some((
            ratatui::layout::Rect {
                x: 1,
                y: 2,
                width: 100,
                height: 10,
            },
            0,
        )));
        let mut t = Transcript::default();
        t.set_width(60);
        t.push(Line::User("hello there".into()));
        t.push(Line::Assistant("general kenobi".into()));

        assert_eq!(click_target(&strip, &arrows, &tx, &t, 4, 38), Click::Todos);
        assert_eq!(
            click_target(&strip, &arrows, &tx, &t, 16, 38),
            Click::Skills
        );
        assert_eq!(click_target(&strip, &arrows, &tx, &t, 28, 38), Click::Info);
        assert_eq!(
            click_target(&strip, &arrows, &tx, &t, 114, 0),
            Click::JumpUp
        );
        assert_eq!(
            click_target(&strip, &arrows, &tx, &t, 116, 0),
            Click::JumpDown
        );
        // a transcript row resolves to its entry (a user row: a
        // thought-toggle that no-ops, matching the old handler)
        assert_eq!(
            click_target(&strip, &arrows, &tx, &t, 5, 2),
            Click::Thought { entry: 0 }
        );
        // outside every zone: inert
        assert_eq!(click_target(&strip, &arrows, &tx, &t, 60, 20), Click::None);
    }

    #[test]
    fn hover_target_tracks_clickable_elements_only() {
        let strip = std::cell::Cell::new(Some(StripZones {
            todos: ratatui::layout::Rect {
                x: 2,
                y: 38,
                width: 10,
                height: 1,
            },
            skills: ratatui::layout::Rect {
                x: 14,
                y: 38,
                width: 10,
                height: 1,
            },
            info: ratatui::layout::Rect {
                x: 26,
                y: 38,
                width: 10,
                height: 1,
            },
        }));
        let arrows = std::cell::Cell::new(Some(TitleArrows {
            up: ratatui::layout::Rect {
                x: 114,
                y: 0,
                width: 1,
                height: 1,
            },
            down: ratatui::layout::Rect {
                x: 116,
                y: 0,
                width: 1,
                height: 1,
            },
        }));
        let rail = std::cell::Cell::new(Some((
            ratatui::layout::Rect {
                x: 110,
                y: 2,
                width: 1,
                height: 10,
            },
            100usize,
            10usize,
            6usize,
            1usize,
        )));
        let tx = std::cell::Cell::new(Some((
            ratatui::layout::Rect {
                x: 1,
                y: 2,
                width: 100,
                height: 30,
            },
            0usize,
        )));
        let modal_zone = std::cell::Cell::new(None::<ratatui::layout::Rect>);
        let popup_zone = std::cell::Cell::new(None::<ratatui::layout::Rect>);
        let mut t = Transcript::default();
        t.set_width(60);
        t.push(Line::Thought("a\nb\nc\nd".into())); // foldable band
        t.push(Line::Thought("solo".into())); // single-liner: inert
        t.push(Line::Assistant("plain prose".into())); // inert
        let card = ToolCall {
            head: "bash · ls".into(),
            ok: true,
            note: "note".into(),
            excerpt: String::new(),
            spill: None,
            dur: None,
            expanded: false,
        };
        t.push(Line::ToolBlock(vec![card]));

        let ht = |col, row| {
            hover_target(
                &modal_zone,
                &popup_zone,
                false,
                &strip,
                &arrows,
                &rail,
                &tx,
                &t,
                col,
                row,
            )
        };
        // foldable thinking rows highlight; single-liners and prose do not
        assert_eq!(ht(5, 2), Some(Hover::Entry(0)));
        assert_eq!(
            ht(5, 2 + t.rendered_rows_of(0) as u16),
            None,
            "single-line thought: inert"
        );
        assert_eq!(
            ht(
                5,
                2 + (t.rendered_rows_of(0) + t.rendered_rows_of(1)) as u16
            ),
            None,
            "prose: inert"
        );
        // a tool card row highlights its entry
        let card_row = t.rendered_rows_of(0) + t.rendered_rows_of(1) + t.rendered_rows_of(2);
        assert_eq!(ht(5, 2 + card_row as u16), Some(Hover::Entry(3)));
        // chrome: strip, arrows, rail thumb (track rows stay inert)
        assert_eq!(ht(4, 38), Some(Hover::Strip(StripButton::Todos)));
        assert_eq!(ht(114, 0), Some(Hover::Arrow(true)));
        assert_eq!(ht(116, 0), Some(Hover::Arrow(false)));
        assert_eq!(ht(110, 8), Some(Hover::Rail));
        assert_eq!(ht(110, 3), None);
        // inert space
        // past every entry: inert space
        assert_eq!(ht(60, 24), None);

        // an open modal owns the pointer: only its ✕ chip highlights
        modal_zone.set(Some(ratatui::layout::Rect {
            x: 30,
            y: 10,
            width: 40,
            height: 10,
        }));
        assert_eq!(ht(68, 10), Some(Hover::Chip));
        assert_eq!(ht(50, 14), None);
        modal_zone.set(None);

        // a chip-bearing popup highlights its chip; the input-box
        // picker (no chip) never highlights
        popup_zone.set(Some(ratatui::layout::Rect {
            x: 30,
            y: 10,
            width: 40,
            height: 10,
        }));
        assert_eq!(
            hover_target(
                &modal_zone,
                &popup_zone,
                true,
                &strip,
                &arrows,
                &rail,
                &tx,
                &t,
                68,
                10
            ),
            Some(Hover::Chip)
        );
        assert_eq!(
            hover_target(
                &modal_zone,
                &popup_zone,
                true,
                &strip,
                &arrows,
                &rail,
                &tx,
                &t,
                50,
                14
            ),
            None
        );
    }

    /// A drag only becomes a selection past the click tolerance; the
    /// anchor is the press cell, the head clamps to the pane edges, the
    /// pair comes back in reading order, and columns are relative to
    /// the content area, clamped into it.
    #[test]
    fn drag_span_needs_motion_clamps_and_orders() {
        let area = ratatui::layout::Rect {
            x: 1,
            y: 3,
            width: 50,
            height: 5,
        };
        // unmoved / one-cell jitter: still a click
        assert_eq!(drag_span(area, 0, (10, 5), (10, 5)), None);
        assert_eq!(drag_span(area, 0, (10, 5), (11, 5)), None);
        // two cells down from screen row 5 (start row 2): rows 2..=4,
        // columns measured from the area's left edge (x = 1)
        assert_eq!(
            drag_span(area, 0, (10, 5), (12, 7)),
            Some(((2, 9), (4, 11)))
        );
        // upward drag orders the pair around the anchor (reading order)
        assert_eq!(
            drag_span(area, 0, (10, 7), (12, 5)),
            Some(((2, 11), (4, 9)))
        );
        // same row dragged left: the columns order, the row is shared
        assert_eq!(
            drag_span(area, 0, (30, 5), (10, 5)),
            Some(((2, 9), (2, 29)))
        );
        // a press outside the transcript never selects
        assert_eq!(drag_span(area, 0, (0, 0), (30, 6)), None);
        // dragging above/below clamps the head row; past either side
        // clamps its column
        assert_eq!(drag_span(area, 7, (10, 5), (10, 0)), Some(((7, 9), (9, 9))));
        assert_eq!(
            drag_span(area, 7, (10, 5), (40, 30)),
            Some(((9, 9), (11, 39)))
        );
        assert_eq!(
            drag_span(area, 0, (10, 5), (500, 5)),
            Some(((2, 9), (2, 49)))
        );
        assert_eq!(drag_span(area, 0, (10, 5), (0, 5)), Some(((2, 0), (2, 9))));
    }

    /// Column slicing is display-width aware: wide glyphs count their
    /// full width and stay whole, combining marks ride their base.
    #[test]
    fn slice_cols_counts_display_width() {
        assert_eq!(super::slice_cols("hello world", 0, 4), "hello");
        assert_eq!(super::slice_cols("hello world", 6, 10), "world");
        assert_eq!(super::slice_cols("hello", 3, 99), "lo");
        assert_eq!(super::slice_cols("hello", 0, usize::MAX), "hello");
        // 世 and 界 are two columns each
        assert_eq!(super::slice_cols("世界!", 0, 1), "世");
        assert_eq!(super::slice_cols("世界!", 2, 3), "界");
        // e + combining acute: the mark follows its base's decision
        assert_eq!(super::slice_cols("e\u{301}x", 0, 0), "e\u{301}");
        assert_eq!(super::slice_cols("e\u{301}x", 1, 1), "x");
    }

    /// A selection copies the picked columns of every row it spans,
    /// joining them with newlines and dropping trailing blanks.
    #[test]
    fn selection_text_slices_rows_and_trims() {
        let mut t = Transcript::default();
        t.set_width(40);
        t.push(Line::Shell("alpha beta".into()));
        t.push(Line::Shell("gamma delta".into()));
        // full rows: the whole text, no padding
        assert_eq!(
            super::selection_text(&t, ((0, 0), (1, 39))),
            "alpha beta\ngamma delta"
        );
        // a column range slices every row it spans
        assert_eq!(super::selection_text(&t, ((0, 6), (1, 4))), "beta\ngamma");
        // a range past the end of a single row is empty
        assert_eq!(super::selection_text(&t, ((0, 11), (0, 30))), "");
    }

    /// `row_text` returns the rendered row's spans concatenated — the
    /// exact text a selection copies.
    #[test]
    fn row_text_joins_rendered_span_content() {
        let mut t = Transcript::default();
        t.set_width(60);
        t.push(Line::User("remember this prompt".into()));
        // entries lead with air rows: find the content row
        let hit = (0..t.total_rows()).any(|r| {
            t.row_text(r)
                .is_some_and(|s| s.contains("remember this prompt"))
        });
        assert!(hit, "the user row's text is copyable");
        assert_eq!(t.row_text(999), None, "past the end is None");
    }

    /// While a selection is active its rows paint with the tool-band
    /// background; with no selection nothing is patched.
    #[test]
    fn selection_highlights_picked_rows_while_active() {
        use ratatui::backend::TestBackend;
        let draw =
            |sel: Option<((usize, u16), (usize, u16))>, x: u16| -> Vec<ratatui::style::Color> {
                let mut t = Transcript::default();
                t.set_width(80);
                t.push(Line::User("first".into()));
                t.push(Line::Assistant("second".into()));
                let mut terminal = ratatui::Terminal::new(TestBackend::new(100, 30)).unwrap();
                terminal
                    .draw(|f| {
                        super::render(
                            f,
                            &mut t,
                            None,
                            "",
                            0,
                            false,
                            None,
                            Instant::now(),
                            0,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            &Meters::default(),
                            &SidebarState::default(),
                            false,
                            true,
                            "◆",
                            &std::cell::Cell::new(None::<StripZones>),
                            None,
                            &std::cell::Cell::new(None::<TitleArrows>),
                            &std::cell::Cell::new(None::<ratatui::layout::Rect>),
                            &std::cell::Cell::new(None::<ratatui::layout::Rect>),
                            &std::cell::Cell::new(None::<(ratatui::layout::Rect, usize)>),
                            &std::cell::Cell::new(
                                None::<(ratatui::layout::Rect, usize, usize, usize, usize)>,
                            ),
                            &std::cell::Cell::new(0u16),
                            sel,
                            None,
                        )
                    })
                    .unwrap();
                // content starts at y=2 (header bar 0, air 1), x=2;
                // sample one cell per content row
                (2..11)
                    .map(|y| terminal.backend().buffer()[(x, y)].bg)
                    .collect()
            };
        let plain = draw(None, 5);
        assert!(
            plain.iter().all(|bg| *bg != crate::palette::BG_SELECT),
            "no selection, no highlight: {plain:?}"
        );
        let picked = draw(Some(((0, 0), (1, 95))), 5);
        assert_eq!(picked[0], crate::palette::BG_SELECT, "row 0 highlighted");
        assert_eq!(picked[1], crate::palette::BG_SELECT, "row 1 highlighted");
        assert_ne!(
            picked[2],
            crate::palette::BG_SELECT,
            "row 2 is outside the span"
        );
        // a partial row highlights only its picked columns
        assert_ne!(
            draw(Some(((0, 10), (0, 20))), 5)[0],
            crate::palette::BG_SELECT,
            "left of the selection stays plain"
        );
        assert_eq!(
            draw(Some(((0, 10), (0, 20))), 15)[0],
            crate::palette::BG_SELECT,
            "inside the selection is picked"
        );
        // only the drag's own rows are cut: a middle row paints edge to
        // edge, so a column left of the anchor is still picked there
        assert_eq!(
            draw(Some(((0, 10), (2, 20))), 5)[1],
            crate::palette::BG_SELECT,
            "middle rows paint the full width"
        );
    }

    /// Expandable calls carry the thinking-block fold vocabulary in
    /// their header, trailing the detail: `$ bash  cmd ▸` collapsed,
    /// `$ bash  cmd ▾` expanded. A call with nothing to reveal shows
    /// no chevron at all — no dead indicator.
    #[test]
    fn tool_card_chevron_tracks_expand_state() {
        let text_of = |l: &ratatui::text::Line| -> String {
            l.spans.iter().map(|s| s.content.to_string()).collect()
        };
        let expandable = ToolCall {
            head: "→ bash · cargo build".into(),
            ok: true,
            note: "done".into(),
            excerpt: "done".into(),
            spill: None,
            dur: None,
            expanded: false,
        };
        let collapsed = super::render_line(&Line::ToolBlock(vec![expandable.clone()]), 40, false);
        assert!(
            text_of(&collapsed[0]).ends_with("cargo build ▸"),
            "collapsed shows the closed chevron after the detail: {:?}",
            text_of(&collapsed[0])
        );
        let expanded = super::render_line(
            &Line::ToolBlock(vec![ToolCall {
                expanded: true,
                ..expandable.clone()
            }]),
            40,
            false,
        );
        assert!(
            text_of(&expanded[0]).ends_with("cargo build ▾"),
            "expanded shows the open chevron: {:?}",
            text_of(&expanded[0])
        );
        assert!(
            !text_of(&expanded[0]).contains('▸'),
            "{:?}",
            text_of(&expanded[0])
        );
        // a bare call (no excerpt, no spill) shows neither
        let bare = super::render_line(&Line::ToolBlock(vec![no_note_call()]), 40, false);
        let top = text_of(&bare[0]);
        assert!(
            !top.contains('▸') && !top.contains('▾'),
            "no dead chevron on a non-expandable card: {top:?}"
        );
    }

    /// The ✕ chip renders on the top border, inside the chip hit zone;
    /// a chip click or any click outside closes, a body click is dead.
    #[test]
    fn close_chip_hit_zone_and_outside_click_close_todos_modal() {
        let (terminal, modal_zone, _popup) =
            draw_overlay(None, None, Some(&Modal::Todos { rows: vec![] }), None);
        let rect = modal_zone.get().expect("modal rect published");
        // the top-right 3 cells read `✕ · space · right border`: the
        // drawn glyph is inside the chip zone the routing treats as the
        // close button
        let chip_x0 = rect.x + rect.width - 3;
        let chip_row: String = (chip_x0..rect.x + rect.width)
            .map(|cx| terminal.backend().buffer()[(cx, rect.y)].symbol())
            .collect();
        assert_eq!(chip_row, "✕ ╮", "chip glyphs at {chip_x0}..{chip_row}");
        assert!(
            modal_close_hit(rect, chip_x0, rect.y),
            "a click on the ✕ chip closes"
        );
        assert!(
            !modal_close_hit(rect, rect.x + 2, rect.y + 2),
            "body clicks keep the modal open"
        );
        assert!(
            modal_close_hit(rect, 0, 0),
            "a click anywhere outside the modal closes it"
        );
    }

    /// Outside click closes the settings modal without applying
    /// anything: the loop drops the modal wholesale, so the draft edit
    /// is discarded exactly like the Esc arm (nothing is saved).
    #[test]
    fn outside_click_closes_settings_without_applying_the_edit() {
        let panel = SettingsPanel {
            model: "mock-5".into(),
            mode: ka_protocol::Mode::Ask,
            effort: None,
            selected: 0,
            edit: Some("half-typed model".into()),
            providers: vec![],
            config_path: "/tmp/ka-config".into(),
        };
        let (_terminal, modal_zone, _popup) =
            draw_overlay(None, None, Some(&Modal::Settings(panel.clone())), None);
        let rect = modal_zone.get().expect("modal rect published");
        assert!(
            modal_close_hit(rect, 0, 0),
            "outside click closes the settings modal"
        );
        assert!(
            !modal_close_hit(rect, rect.x + 2, rect.y + 1),
            "a body click (say, on the model row) neither closes nor edits"
        );
        // the value this test owns is the whole panel: dropping the
        // modal leaves it exactly as passed — no save, no apply
        assert_eq!(panel.edit.as_deref(), Some("half-typed model"));
        assert_eq!(panel.model, "mock-5");
    }

    /// The permission ask is unreachable for the mouse: no close zone
    /// is published and its frame carries no ✕ — a click can never
    /// answer (let alone deny) an exec.
    #[test]
    fn permission_ask_ignores_clicks_and_shows_no_close_chip() {
        let ask = PendingAsk {
            id: AskId("a1".into()),
            question: "run `cargo test`?".into(),
            options: vec!["allow".into(), "deny".into()],
            selected: 0,
            detail: None,
        };
        let (terminal, modal_zone, popup_zone) = draw_overlay(Some(&ask), None, None, None);
        assert!(
            modal_zone.get().is_none() && popup_zone.get().is_none(),
            "the ask publishes no close target"
        );
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(
            !text.contains(crate::icons::CLOSE),
            "the permission frame must not render a ✕ chip"
        );
    }

    /// A click outside the slash popup closes it (its Esc arm: the
    /// popup is discarded, never submitted); a click inside is left
    /// alone.
    #[test]
    fn slash_popup_closes_on_outside_or_chip_click_not_body() {
        let popup = SlashPopup {
            items: vec![("/help".into(), "show the key table".into())],
            selected: 0,
        };
        let (_terminal, modal_zone, popup_zone) = draw_overlay(None, Some(&popup), None, None);
        assert!(modal_zone.get().is_none(), "no modal is open");
        let rect = popup_zone.get().expect("popup rect published");
        let inside = ratatui::layout::Position {
            x: rect.x + 2,
            y: rect.y + 1,
        };
        let outside = ratatui::layout::Position {
            x: rect.x + 1,
            y: rect.y.saturating_sub(1),
        };
        assert!(rect.contains(inside), "sanity: the first item row");
        assert!(
            !rect.contains(outside),
            "sanity: the cell above the popup is outside"
        );
        // the close decision the loop applies to framed popups: body
        // clicks keep the popup open; outside and the ✕ chip zone
        // (top-right 3 cells, which the shared modal_frame renders)
        // both close it, discarding like Esc
        assert!(
            !modal_close_hit(rect, inside.x, inside.y),
            "a body click must not close the popup"
        );
        assert!(
            modal_close_hit(rect, outside.x, outside.y),
            "an outside click must close the popup"
        );
        let chip_x = rect.x + rect.width.saturating_sub(2);
        assert!(
            modal_close_hit(rect, chip_x, rect.y),
            "clicking the rendered ✕ chip must close the popup"
        );
    }

    /// The mode picker borrows the input box: that chunk is its popup
    /// rect. Clicking outside closes the picker — the loop drops it,
    /// which submits nothing.
    #[test]
    fn mode_picker_zone_is_the_input_box_and_outside_click_never_submits() {
        let (_terminal, modal_zone, popup_zone) = draw_overlay(
            None,
            None,
            None,
            Some(&InputPicker::Mode(ModePicker { selected: 0 })),
        );
        assert!(modal_zone.get().is_none(), "no modal is open");
        let rect = popup_zone.get().expect("picker rect published");
        assert!(
            rect.contains(ratatui::layout::Position {
                x: rect.x + 2,
                y: rect.y + 1,
            }),
            "picker cells are the close boundary"
        );
        assert!(
            !rect.contains(ratatui::layout::Position { x: 0, y: 0 }),
            "the frame corner is outside: closing drops the picker, never a submit"
        );
    }

    #[test]
    fn prompt_row_parser_splits_coords_and_args() {
        let (spec, args) = parse_prompt_row("review/repo (pr, dry)");
        assert_eq!(spec, Some(("review".into(), "repo".into())));
        assert_eq!(args, vec!["pr".to_string(), "dry".to_string()]);
        let (spec, args) = parse_prompt_row("review/repo");
        assert_eq!(spec, Some(("review".into(), "repo".into())));
        assert!(args.is_empty());
    }

    #[test]
    fn prompt_invocation_parses_kv_pairs() {
        let (server, name, args) = parse_prompt_invocation("review/repo pr=42 dry=true").unwrap();
        assert_eq!(server, "review");
        assert_eq!(name, "repo");
        assert_eq!(args.get("pr").map(String::as_str), Some("42"));
        assert_eq!(args.get("dry").map(String::as_str), Some("true"));
        assert!(parse_prompt_invocation("noseparator").is_none());
    }

    #[test]
    fn slash_mcp_refresh_and_prompt_dispatch() {
        let slash = slash_command("/mcp refresh").unwrap();
        assert!(matches!(slash.event, Some(Command::RefreshMcp)));
        // bare /mcp is a usage note, not an event
        let slash = slash_command("/mcp").unwrap();
        assert!(slash.event.is_none());
        assert!(slash.note.as_deref().is_some_and(|n| n.contains("usage")));

        let slash = slash_command("/prompt review/repo pr=42").unwrap();
        match slash.event {
            Some(Command::CallPrompt { server, name, args }) => {
                assert_eq!((server.as_str(), name.as_str()), ("review", "repo"));
                assert_eq!(args.get("pr").map(String::as_str), Some("42"));
            }
            other => panic!("expected CallPrompt, got {other:?}"),
        }
        // bare /prompt opens the picker modal
        let slash = slash_command("/prompt").unwrap();
        assert!(matches!(slash.modal, Some(ModalKind::Prompts)));
    }

    #[test]
    fn replay_digest_divider_renders_faint_row() {
        let mut lines = Transcript::default();
        feed(
            &mut lines,
            &Event::Replay {
                messages: vec![
                    ka_protocol::ReplayedMessage {
                        role: "digest".into(),
                        content: String::new(),
                        digest: true,
                        thinking: None,
                        calls: Vec::new(),
                    },
                    ka_protocol::ReplayedMessage {
                        role: "user".into(),
                        content: "after the digest".into(),
                        digest: false,
                        thinking: None,
                        calls: Vec::new(),
                    },
                ],
            },
        );
        let text = format!("{:?}", lines.entries());
        assert!(text.contains("digest"), "{text:?}");
        assert!(text.contains("after the digest"), "{text:?}");
    }

    #[test]
    fn replay_renders_thought_tool_rows_and_assistant_text() {
        let mut lines = Transcript::default();
        feed(
            &mut lines,
            &Event::Replay {
                messages: vec![
                    ka_protocol::ReplayedMessage {
                        role: "user".into(),
                        content: "list files".into(),
                        digest: false,
                        thinking: None,
                        calls: Vec::new(),
                    },
                    ka_protocol::ReplayedMessage {
                        role: "assistant".into(),
                        content: "found it".into(),
                        digest: false,
                        thinking: Some("scanning the tree".into()),
                        calls: vec![ka_protocol::ReplayedCall {
                            id: "c1".into(),
                            tool: "read".into(),
                            detail: "lib.rs".into(),
                            result: Some("fn main() {}".into()),
                            is_error: false,
                        }],
                    },
                ],
            },
        );
        let text = format!("{:?}", lines.entries());
        assert!(text.contains("scanning the tree"), "thought row: {text:?}");
        assert!(
            text.contains("ToolBlock([ToolCall { head: \"→ read · lib.rs\""),
            "tool block: {text:?}"
        );
        assert!(
            text.contains(r#"note: "fn main() {}""#) && text.contains("ok: true"),
            "merged result note + verdict: {text:?}"
        );
        assert!(text.contains("found it"), "assistant text: {text:?}");
    }

    #[test]
    fn seed_history_dedups_consecutive_and_caps() {
        let mut b = InputBuffer::default();
        b.seed_history(["a".into(), "a".into(), "b".into(), "a".into()]);
        assert_eq!(b.history.len(), 3, "consecutive duplicates collapse");
        b.history_prev();
        assert_eq!(b.text, "a", "the newest seeded item recalls first");

        // cap: 150 distinct prompts → the newest 100 survive
        let mut c = InputBuffer::default();
        c.seed_history((0..150).map(|i| format!("p{i}")));
        assert_eq!(c.history.len(), 100);
        c.history_prev();
        assert_eq!(c.text, "p149", "newest survives the cap");
    }

    #[test]
    fn replay_user_prompts_filters_to_user_text() {
        let msgs = vec![
            ReplayedMessage {
                role: "digest".into(),
                content: "compaction divider".into(),
                digest: true,
                thinking: None,
                calls: Vec::new(),
            },
            ReplayedMessage {
                role: "user".into(),
                content: "first ask".into(),
                digest: false,
                thinking: None,
                calls: Vec::new(),
            },
            ReplayedMessage {
                role: "assistant".into(),
                content: "done".into(),
                digest: false,
                thinking: None,
                calls: Vec::new(),
            },
            ReplayedMessage {
                role: "user".into(),
                content: "   ".into(),
                digest: false,
                thinking: None,
                calls: Vec::new(),
            },
            ReplayedMessage {
                role: "user".into(),
                content: "second ask".into(),
                digest: false,
                thinking: None,
                calls: Vec::new(),
            },
        ];
        let prompts: Vec<String> = replay_user_prompts(&msgs).collect();
        assert_eq!(
            prompts,
            ["first ask", "   ", "second ask"],
            "user rows oldest-first, verbatim; assistant and digest rows never appear"
        );
    }

    #[test]
    fn resumed_replay_seeds_history_up_recalls_prior_prompt() {
        // README:28 — `↑/↓ history (seeded with a resumed session's
        // earlier prompts)`. This is the exact code the run loop
        // executes when `ka -c` / `--session <id>` replays a session.
        let event = Event::Replay {
            messages: vec![
                ReplayedMessage {
                    role: "user".into(),
                    content: "list the failing tests".into(),
                    digest: false,
                    thinking: None,
                    calls: Vec::new(),
                },
                ReplayedMessage {
                    role: "assistant".into(),
                    content: "three of them".into(),
                    digest: false,
                    thinking: None,
                    calls: Vec::new(),
                },
                ReplayedMessage {
                    role: "digest".into(),
                    content: String::new(),
                    digest: true,
                    thinking: None,
                    calls: Vec::new(),
                },
                ReplayedMessage {
                    role: "user".into(),
                    content: "remember this prompt".into(),
                    digest: false,
                    thinking: None,
                    calls: Vec::new(),
                },
                ReplayedMessage {
                    role: "user".into(),
                    content: "   ".into(),
                    digest: false,
                    thinking: None,
                    calls: Vec::new(),
                },
            ],
        };
        let Event::Replay { messages } = &event else {
            panic!("the event just constructed is a Replay");
        };
        let mut input = InputBuffer::default();
        input.seed_history(replay_user_prompts(messages));

        // one Up recalls the newest prior prompt — never the trailing
        // blank, the assistant text, or the digest divider
        input.history_prev();
        assert_eq!(input.text, "remember this prompt");
        // ↑ keeps walking back through the older prompts
        input.history_prev();
        assert_eq!(input.text, "list the failing tests");
        // ↓ returns toward the present; past the newest it clears
        input.history_next();
        assert_eq!(input.text, "remember this prompt");
        input.history_next();
        assert_eq!(input.text, "", "past the newest entry the draft clears");
    }

    #[test]
    fn tree_rows_walk_the_family_parent_before_child() {
        let all = vec![
            ka_strand::StrandSummary {
                path: std::path::PathBuf::new(),
                id: "root".into(),
                ts: "2026-01-02T10:00:00Z".into(),
                title: "the root".into(),
                messages: 3,
                cost: 0.0,
                parent: None,
                name: None,
                tokens: 0,
            },
            ka_strand::StrandSummary {
                path: std::path::PathBuf::new(),
                id: "unrelated".into(),
                ts: "2026-01-03T10:00:00Z".into(),
                title: "not family".into(),
                messages: 9,
                cost: 0.0,
                parent: None,
                name: None,
                tokens: 0,
            },
            ka_strand::StrandSummary {
                path: std::path::PathBuf::new(),
                id: "kid".into(),
                ts: "2026-01-04T10:00:00Z".into(),
                title: "a child".into(),
                messages: 1,
                cost: 0.0,
                parent: Some("root".into()),
                name: None,
                tokens: 0,
            },
            ka_strand::StrandSummary {
                path: std::path::PathBuf::new(),
                id: "grandkid".into(),
                ts: "2026-01-05T10:00:00Z".into(),
                title: "a grandchild".into(),
                messages: 2,
                cost: 0.0,
                parent: Some("kid".into()),
                name: None,
                tokens: 0,
            },
        ];
        let no_records = |_: &str| vec![];
        let history = vec!["first prompt".to_string(), "second prompt".to_string()];
        let (rows, actions) = tree_modal_rows(&all, Some("root"), &history, 60, no_records);
        assert_eq!(
            actions,
            vec![
                TreeAction::Attach("root".to_string()),
                TreeAction::Attach("kid".to_string()),
                TreeAction::Attach("grandkid".to_string()),
                TreeAction::Header,
                TreeAction::Fork {
                    turns: 2,
                    prompt: "first prompt".to_string()
                },
                TreeAction::Fork {
                    turns: 1,
                    prompt: "second prompt".to_string()
                },
            ]
        );
        assert_eq!(rows.len(), 6);
        assert!(
            rows[0].starts_with("\u{25b8} "),
            "current is marked: {:?}",
            rows[0]
        );
        assert!(
            rows[1].starts_with("└─ "),
            "only child closes the branch: {:?}",
            rows[1]
        );
        assert!(
            rows[2].starts_with("   └─ "),
            "grandchild nests one level in: {:?}",
            rows[2]
        );
        assert!(rows[4].starts_with("⑂ "), "fork row marked: {:?}", rows[4]);
        assert!(!rows.iter().any(|r| r.contains("not family")));

        // from the grandkid the whole family stays reachable: the
        // ancestor chain leads, the fork section still closes the list
        let (rows2, actions2) = tree_modal_rows(&all, Some("grandkid"), &history, 60, no_records);
        assert_eq!(actions2.len(), 6);
        assert!(
            !rows2[0].starts_with(' '),
            "root unindented: {:?}",
            rows2[0]
        );
        assert!(rows2[2].starts_with("   └─ \u{25b8} "), "{:?}", rows2[2]);

        // a strand with no family renders alone, unindented; without
        // user inputs there is no fork section at all
        let (rows3, actions3) = tree_modal_rows(&all, Some("unrelated"), &[], 60, no_records);
        assert_eq!(actions3, vec![TreeAction::Attach("unrelated".to_string())]);
        assert!(!rows3[0].starts_with(' '));
    }

    #[test]
    fn tree_rows_label_fork_points_and_count_branches() {
        let msg = |id: &str, role: ka_strand::Role, content: &str| ka_strand::Record::Message {
            id: ka_protocol::RecordId(id.to_string()),
            role,
            content: content.to_string(),
            calls: Vec::new(),
            results: Vec::new(),
            thinking: None,
        };
        let header = || ka_strand::Record::Header {
            id: ka_protocol::StrandId("s".into()),
            ts: String::new(),
            cwd: String::new(),
            version: 1,
            repo: None,
            parent: None,
        };
        // root: p1 answered, p2 answered; one child forked before p2,
        // another is a full copy
        let root = vec![
            header(),
            msg("r1", ka_strand::Role::User, "fix the parser"),
            msg("r2", ka_strand::Role::Assistant, "done"),
            msg("r3", ka_strand::Role::User, "second prompt"),
            msg("r4", ka_strand::Role::Assistant, "ok"),
        ];
        let forked = vec![
            header(),
            msg("r1", ka_strand::Role::User, "fix the parser"),
            msg("r2", ka_strand::Role::Assistant, "done"),
        ];
        let copied = root.clone();
        let all = vec![
            ka_strand::StrandSummary {
                path: std::path::PathBuf::new(),
                id: "root".into(),
                ts: "2026-01-02T10:00:00Z".into(),
                title: "the root".into(),
                messages: 4,
                cost: 0.0,
                parent: None,
                name: None,
                tokens: 0,
            },
            ka_strand::StrandSummary {
                path: std::path::PathBuf::new(),
                id: "forked".into(),
                ts: "2026-01-03T10:00:00Z".into(),
                title: "fork at p2".into(),
                messages: 2,
                cost: 0.0,
                parent: Some("root".into()),
                name: None,
                tokens: 0,
            },
            ka_strand::StrandSummary {
                path: std::path::PathBuf::new(),
                id: "copied".into(),
                ts: "2026-01-04T10:00:00Z".into(),
                title: "a copy".into(),
                messages: 4,
                cost: 0.0,
                parent: Some("root".into()),
                name: None,
                tokens: 0,
            },
        ];
        let records = |id: &str| match id {
            "root" => root.clone(),
            "forked" => forked.clone(),
            "copied" => copied.clone(),
            _ => vec![],
        };
        let history = vec!["fix the parser".to_string(), "second prompt".to_string()];
        let (rows, _actions) = tree_modal_rows(&all, Some("root"), &history, 80, records);
        // forked branched where "second prompt" was dropped; copied
        // kept everything
        assert!(
            rows[1].contains("⑂ at \u{201c}second prompt\u{201d}"),
            "fork edge carries its branch point: {:?}",
            rows[1]
        );
        assert!(
            rows[2].contains("⑂ copy"),
            "full copy labeled: {:?}",
            rows[2]
        );
        // the branch section counts the fork already growing at
        // "second prompt" but leaves the untouched prompt plain
        assert!(
            rows[4].contains("fix the parser") && !rows[4].contains("↳"),
            "no-branch prompt stays plain: {:?}",
            rows[4]
        );
        assert!(
            rows[5].contains("second prompt") && rows[5].contains("↳ 1 fork"),
            "branch count rides the prompt: {:?}",
            rows[5]
        );
    }

    #[test]
    fn tree_fork_depth_counts_from_the_newest_input() {
        let history: Vec<String> = (1..=25).map(|n| format!("prompt {n}")).collect();
        let (_rows, actions) = tree_modal_rows(&[], Some("s1"), &history, 60, |_: &str| vec![]);
        let forks: Vec<&TreeAction> = actions
            .iter()
            .skip_while(|a| !matches!(a, TreeAction::Fork { .. }))
            .collect();
        assert_eq!(forks.len(), 25);
        assert!(
            matches!(
                forks.first(),
                Some(TreeAction::Fork { turns: 25, prompt }) if prompt == "prompt 1"
            ),
            "oldest input forks the whole history back: {:?}",
            forks.first()
        );
        assert!(
            matches!(
                forks.last(),
                Some(TreeAction::Fork { turns: 1, prompt }) if prompt == "prompt 25"
            ),
            "newest input forks one turn: {:?}",
            forks.last()
        );
    }

    #[test]
    fn slash_tree_opens_modal() {
        let slash = slash_command("/tree").unwrap();
        assert!(matches!(slash.modal, Some(ModalKind::Tree)));
        assert!(slash.event.is_none());
    }

    #[test]
    fn image_part_from_path_sniffs_and_encodes() {
        let dir = std::env::temp_dir().join(format!("ka-img-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let png: &[u8] = &[
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52,
        ];
        let path = dir.join("p.png");
        std::fs::write(&path, png).unwrap();
        let part = image_part_from_path(&path).unwrap();
        assert_eq!(part.media_type, "image/png");
        assert_eq!(part.data, b64encode(png));
        // unsupported file rejected
        let txt = dir.join("t.txt");
        std::fs::write(&txt, "plain").unwrap();
        assert!(image_part_from_path(&txt).is_err());
        // missing file rejected
        assert!(image_part_from_path(&dir.join("nope.png")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn handle_image_command_stages_or_reports() {
        let dir = std::env::temp_dir().join(format!("ka-imgc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let png: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        std::fs::write(dir.join("p.png"), png).unwrap();
        let mut pending: Option<ka_protocol::ImagePart> = None;
        let out = handle_image_command(
            &format!("/image {}", dir.join("p.png").display()),
            &mut pending,
        );
        assert!(out.is_some());
        assert!(out.unwrap().is_ok());
        assert!(pending.is_some(), "image staged");
        assert_eq!(pending.unwrap().media_type, "image/png");

        // non-image paths error without staging
        let mut pending: Option<ka_protocol::ImagePart> = None;
        let out = handle_image_command("/image /definitely/absent.png", &mut pending);
        assert!(matches!(out, Some(Err(_))));
        assert!(pending.is_none());

        // bare /image (no path) is not this command
        let mut pending: Option<ka_protocol::ImagePart> = None;
        assert!(handle_image_command("/image", &mut pending).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sniff_image_bytes_reads_magic_prefixes() {
        assert_eq!(
            sniff_image_bytes(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0]),
            Some("image/png")
        );
        // short prefix still sniffs: 3 signature bytes are enough
        assert_eq!(sniff_image_bytes(&[0x89, b'P', b'N']), None);
        assert_eq!(
            sniff_image_bytes(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some("image/jpeg")
        );
        assert_eq!(sniff_image_bytes(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff_image_bytes(b"GIF87a"), Some("image/gif"));
        assert_eq!(
            sniff_image_bytes(b"RIFF\x00\x00\x00\x00WEBP"),
            Some("image/webp")
        );
        assert_eq!(sniff_image_bytes(b"RIFFshort"), None);
        assert_eq!(sniff_image_bytes(b"plain text!!!"), None);
        assert_eq!(sniff_image_bytes(&[]), None);
    }

    #[test]
    fn win_to_wsl_maps_drive_paths() {
        assert_eq!(
            win_to_wsl("C:\\Users\\javier\\AppData\\Local\\Temp\\ka-clip.png"),
            Some(std::path::PathBuf::from(
                "/mnt/c/Users/javier/AppData/Local/Temp/ka-clip.png"
            ))
        );
        assert_eq!(
            win_to_wsl("c:\\x\\y"),
            Some(std::path::PathBuf::from("/mnt/c/x/y"))
        );
        // quotes and padding from console output are tolerated
        assert_eq!(
            win_to_wsl("\"D:\\a b\\c.png\"\r\n"),
            Some(std::path::PathBuf::from("/mnt/d/a b/c.png"))
        );
        // not a drive path
        assert_eq!(win_to_wsl("relative/path.png"), None);
        assert_eq!(win_to_wsl(""), None);
    }

    #[test]
    fn image_part_from_bytes_caps_and_sniffs() {
        // over the cap → rejected before any encoding
        let big = vec![0u8; 5 * 1024 * 1024 + 1];
        let err = image_part_from_bytes(&big).unwrap_err();
        assert!(err.contains("cap"), "{err}");
        // unknown bytes → unsupported type
        let err = image_part_from_bytes(b"not an image").unwrap_err();
        assert!(err.contains("unsupported"), "{err}");
        // a real png prefix round-trips
        let png: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
        let part = image_part_from_bytes(png).unwrap();
        assert_eq!(part.media_type, "image/png");
        assert_eq!(part.data, b64encode(png));
    }
    #[test]
    fn thought_blocks_toggle_individually_and_map_rows() {
        let mut t = Transcript::default();
        t.set_width(60);
        t.push(Line::User("q".into()));
        t.push(Line::Thought("a\nb\nc\nd\ne\nf".into()));
        t.push(Line::Thought("solo".into()));
        t.push(Line::Assistant("ans".into()));
        // collapsed by default: entry 1 renders 1 row, solo entry 2 = 1 row
        let rows = t.total_rows();
        // user(1) + thought(1) + thought(1) + assistant block(>=1)
        assert!(rows >= 4, "{rows}");

        // toggle block 1 open: its rendered height grows
        let before = t.rendered_rows_of(1);
        t.toggle_thought(1);
        let after = t.rendered_rows_of(1);
        assert!(after > before, "open block grows: {before} -> {after}");

        // single-line thoughts refuse to toggle
        let solo_before = t.rendered_rows_of(2);
        t.toggle_thought(2);
        assert_eq!(t.rendered_rows_of(2), solo_before);

        // row -> ref mapping: the first row of each entry maps back
        // to it (user blocks render with padding, so use real heights)
        let e = t.row_ref_at(0).unwrap();
        assert_eq!(e, RowRef::Entry(0), "row 0 is the user block");
        let first_of_1 = t.rendered_rows_of(0);
        let e1 = t.row_ref_at(first_of_1).unwrap();
        assert_eq!(
            e1,
            RowRef::Entry(1),
            "row {first_of_1} is the thought block"
        );

        // global Alt+T reset clears per-block overrides
        t.set_thoughts_open(true);
        let open_rows = t.rendered_rows_of(1);
        assert!(open_rows == after, "global open respects no overrides");

        // a block at the collapsed height carries no fold at all —
        // neither a chevron nor a click target (three lines used to
        // show a chevron that did nothing)
        t.push(Line::Thought("x\ny\nz".into()));
        let three = t.entries().len() - 1;
        assert!(!t.is_foldable(three), "3 lines fit the collapsed band");
        let (first, count) = t.entry_row_span(three).unwrap();
        let band: String = (first..first + count)
            .filter_map(|r| t.row_text(r))
            .collect();
        assert!(!band.contains("lines"), "the head carries no count: {band}");
        assert!(
            !band.contains('▸') && !band.contains('▾'),
            "no dead chevron and no more-below row: {band}"
        );
        let before = t.rendered_rows_of(three);
        t.toggle_thought(three);
        assert_eq!(t.rendered_rows_of(three), before, "toggle is a no-op");
    }

    #[test]
    fn streamed_thought_flushes_tail_then_settles_above_viewport() {
        let mut t = Transcript::default();
        t.set_width(60);
        // the flush caches the live tail band: spacer + head + the last
        // three lines + spacer, row-for-row the live rows — no jump
        t.push_thought_streamed("l1\nl2\nl3\nl4\nl5".into());
        assert_eq!(t.rendered_rows_of(0), 6, "blank + head + 3 tail + blank");

        // still visible: nothing collapses
        t.collapse_streamed_above(0);
        assert_eq!(t.rendered_rows_of(0), 6, "in view, keeps its tail band");

        // push enough entries to push the thought above a viewport
        // whose first visible row sits past its span
        for i in 0..4 {
            t.push(Line::Info(format!("filler {i}")));
        }
        let thought_end = t.rendered_rows_of(0);
        t.collapse_streamed_above(thought_end);
        assert_eq!(
            t.rendered_rows_of(0),
            7,
            "settles to the collapsed band + the more row"
        );

        // a streamed block that is still visible stays open
        t.push_thought_streamed("fresh\nstream".into());
        let fresh = t.entries().len() - 1;
        let fresh_open = t.rendered_rows_of(fresh);
        assert_eq!(fresh_open, 5, "blank + head + 2 + blank at the flush");
        t.collapse_streamed_above(thought_end);
        assert_eq!(
            t.rendered_rows_of(fresh),
            fresh_open,
            "visible streamed block keeps its tail band"
        );

        // the collapsed block still toggles open like any other
        t.toggle_thought(0);
        assert_eq!(t.rendered_rows_of(0), 8, "Alt+T/click toggle still works");
    }

    #[test]
    fn tail_band_shows_last_three_rows() {
        let text_of = |l: &ratatui::text::Line| -> String {
            l.spans.iter().map(|s| s.content.to_string()).collect()
        };
        let band = super::thought_rows_tail("l1\nl2\nl3\nl4\nl5", 60, None);
        assert_eq!(band.len(), 6, "blank + head + 3 lines + blank");
        assert!(band[0].spans.is_empty() && band[5].spans.is_empty());
        let head = text_of(&band[1]);
        assert!(!head.contains("lines"), "the head carries no count: {head}");
        assert!(head.contains("Thinking"), "label: {head}");
        assert!(head.contains('▾'), "the tail band reads as open: {head}");
        let body: Vec<String> = band[2..5].iter().map(text_of).collect();
        assert_eq!(body, vec!["│ l3", "│ l4", "│ l5"], "the tail, not the head");
        // the live head shimmers; the text (and so the height) is the
        // flushed band's exactly
        let live = super::thought_rows_tail("l1\nl2\nl3\nl4\nl5", 60, Some(1234));
        let flat: Vec<String> = band.iter().map(text_of).collect();
        let lit: Vec<String> = live.iter().map(text_of).collect();
        assert_eq!(flat, lit);
    }

    #[test]
    fn rewind_and_clear_drop_stale_thought_bookkeeping() {
        // streamed thoughts auto-register an override + streamed_open
        // entry; both are index-based, so a rewind or clear that
        // truncates entries must also drop the stale indices or they
        // attach to whatever lands on the recycled slots
        let mut t = Transcript::default();
        t.set_width(60);
        t.push(Line::User("q".into()));
        t.push_thought_streamed("a\nb\nc".into());
        assert_eq!(t.rendered_rows_of(1), 6, "streamed band stays open");
        t.rewind_user(1);
        t.push_thought_streamed("fresh\nlines".into());
        let idx = t.entries().len() - 1;
        assert_eq!(
            t.rendered_rows_of(idx),
            5,
            "the recycled index must not inherit the stale override (5 = blank+head+2+blank)"
        );
        // clear() wipes everything for the session switch
        t.clear();
        t.push_thought_streamed("x\ny".into());
        let idx = t.entries().len() - 1;
        assert_eq!(t.rendered_rows_of(idx), 5, "clear leaves no stale state");
    }

    #[test]
    fn turn_start_collapses_all_streamed() {
        let mut t = Transcript::default();
        t.set_width(60);
        t.push_thought_streamed("a\nb\nc".into());
        t.push_thought_streamed("still\nopen".into());
        assert_eq!(t.rendered_rows_of(0), 6, "blank + head + 3 + blank");
        assert_eq!(t.rendered_rows_of(1), 5, "blank + head + 2 + blank");
        // the TurnStarted backstop collapses every streamed-open entry
        // even though nothing scrolled
        t.collapse_streamed_all();
        assert_eq!(t.rendered_rows_of(0), 6);
        assert_eq!(t.rendered_rows_of(1), 5);

        // under a globally-open flag the streamed blocks also land
        // collapsed at the boundary
        let mut t = Transcript::default();
        t.set_width(60);
        t.set_thoughts_open(true);
        t.push_thought_streamed("x\ny".into());
        assert_eq!(
            t.rendered_rows_of(0),
            5,
            "expanded at flush under global-open"
        );
        t.collapse_streamed_all();
        assert_eq!(t.rendered_rows_of(0), 5, "boundary closes it");
        // the global flag itself is untouched — other blocks stay open
        t.push(Line::Thought("plain\ncached".into()));
        assert_eq!(
            t.rendered_rows_of(1),
            5,
            "non-streamed block follows the global flag"
        );
    }
}
