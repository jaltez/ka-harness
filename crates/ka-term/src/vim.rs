//! Composer vim mode (TUI-03): INSERT / NORMAL / VISUAL over the
//! existing [`InputBuffer`]. The buffer stays the single editing
//! backend — every mutation routes through its cursor/delete/insert
//! APIs, so undo (ctrl+z) and the kill ring keep working across vim
//! operations.
//!
//! Scope (v1): motions `h l w b 0 $ gg G j k`, operators `d c y` with
//! counts and motions, linewise `dd cc yy`, char delete `x`, paste `p`,
//! insert entry `i a I A o O`, text objects `iw aw` (operator-pending
//! and visual), visual selection with `d y c x`, undo `u`. Counts
//! prefix motions and operators (`3w`, `d2w`, `2dd`). Everything else
//! (marks, named registers, `:` ex lines, `r R t f`, search `/ ?`)
//! stays out on purpose.
//!
//! The surface owns the toggle: `/vim` flips it per session and the
//! preference lives on the [`Vim`] state (no config file, by design —
//! the surface-local `[tui] vim_mode` key was dropped from the plan).
//!
//! Ownership note: the whole layer is free functions over
//! `&mut InputBuffer` (state read/written through `input.vim`). A
//! `Vim::handle(&mut self, &mut InputBuffer)` method shape would need
//! two live mutable borrows of the same buffer, which the borrow
//! checker rightly refuses.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::tui::InputBuffer;

/// Vim editing mode. `Insert` is the composer's native behavior — every
/// key that is not Esc falls through to the normal readline handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Insert,
    Normal,
    Visual,
}

/// Result of feeding one key to the vim layer: consumed here, or left
/// for the composer's standard match (send, completion, readline
/// chords, arrows, scrolling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// The vim layer used this key.
    Handled,
    /// Not a vim key in this mode: run the composer's own handling.
    Passthrough,
}

/// Vim state for one composer. Lives on the [`InputBuffer`] (see
/// `InputBuffer::vim`) so it dies with the draft buffer, session-scoped.
/// The fields are open by design: the layer's functions and tests read
/// and drive them directly.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Vim {
    /// Mode enabled for this session (`/vim` toggles). Off = pure
    /// passthrough.
    pub on: bool,
    /// Current editing mode.
    pub mode: Mode,
    /// VISUAL selection anchor (char index); `None` outside visual.
    pub anchor: Option<usize>,
    /// Unnamed register: `(text, linewise)`. `d`/`c`/`x`/`y`/`dd`/`yy`
    /// write it, `p` reads it.
    pub register: Option<(String, bool)>,
    /// Pending count digits (`3w` → 3). `0` = none.
    pub count: u32,
    /// Operator awaiting its motion or text object (`d`, `c`, `y`).
    pub op: Option<char>,
    /// `g` seen (`gg` pending).
    pub g: bool,
    /// Text-object namespace pending (`i`/`a` after an operator).
    pub obj: Option<char>,
    /// The last visual motion was word-exclusive (`w`/`b`): the
    /// selection ends AT the cursor instead of through it — `v w d`
    /// drops `hello ` and keeps `brave`, like vim.
    pub excl: bool,
}

impl Vim {
    /// Turn the mode on/off (the `/vim` toggle). Enabling always starts
    /// in Insert so typing keeps working immediately; toggling clears
    /// any pending operator state.
    pub fn toggle(&mut self) -> bool {
        self.on = !self.on;
        self.reset_pending();
        if self.on {
            self.mode = Mode::Insert;
        }
        self.on
    }

    /// The composer-title tag for the current mode, when the mode is
    /// on: `INSERT` / `NORMAL` / `VISUAL` (vim's `-- INSERT --` shape).
    pub fn label(&self) -> Option<&'static str> {
        if !self.on {
            return None;
        }
        match self.mode {
            Mode::Insert => Some("INSERT"),
            Mode::Normal => Some("NORMAL"),
            Mode::Visual => Some("VISUAL"),
        }
    }

    /// Clear transient composition state (counts, operators, objects).
    fn reset_pending(&mut self) {
        self.anchor = None;
        self.op = None;
        self.g = false;
        self.obj = None;
        self.count = 0;
        self.excl = false;
    }
}

/// Feed one key. Only meaningful when `input.vim.on`; INSERT consumes
/// just Esc (back to NORMAL) and passes everything else through so
/// readline behavior is untouched.
pub fn handle(input: &mut InputBuffer, key: &KeyEvent) -> Flow {
    if !input.vim.on {
        return Flow::Passthrough;
    }
    // readline chords and window keys never belong to vim
    if matches!(
        key.code,
        KeyCode::Enter
            | KeyCode::Tab
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End
    ) {
        return Flow::Passthrough;
    }
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
    {
        return Flow::Passthrough;
    }
    match key.code {
        KeyCode::Esc => {
            match input.vim.mode {
                Mode::Insert => {
                    // vim: leave insert, cursor steps back off the
                    // just-typed char
                    input.vim.mode = Mode::Normal;
                    if input.cursor > 0 {
                        input.cursor -= 1;
                    }
                }
                Mode::Normal => {
                    // composer-friendly: Esc returns to typing (the
                    // busy-abort Esc chain is consulted BEFORE this
                    // layer, so aborting never regresses)
                    input.vim.mode = Mode::Insert;
                }
                Mode::Visual => {
                    let (lo, _) = selection(input);
                    input.cursor = lo;
                    input.vim.mode = Mode::Normal;
                }
            }
            input.vim.anchor = None;
            input.vim.reset_pending();
            Flow::Handled
        }
        KeyCode::Char(c) => char_key(input, c),
        KeyCode::Delete => {
            if input.vim.mode == Mode::Insert {
                Flow::Passthrough
            } else {
                x_delete(input);
                input.vim.reset_pending();
                Flow::Handled
            }
        }
        KeyCode::Backspace => {
            if input.vim.mode == Mode::Insert {
                Flow::Passthrough
            } else {
                // vim: backspace moves left in normal/visual
                for _ in 0..take_count(input).max(1) {
                    motion(input, 'h', 1);
                }
                input.vim.reset_pending();
                Flow::Handled
            }
        }
        _ => Flow::Passthrough,
    }
}

fn take_count(input: &mut InputBuffer) -> u32 {
    std::mem::take(&mut input.vim.count)
}

fn char_key(input: &mut InputBuffer, c: char) -> Flow {
    if input.vim.mode == Mode::Insert {
        return Flow::Passthrough;
    }
    // pending `g`: only gg matters; anything else processes fresh
    if input.vim.g {
        input.vim.g = false;
        if c == 'g' {
            take_count(input);
            input.cursor = 0;
            return Flow::Handled;
        }
        return char_key(input, c);
    }
    // pending count: digits accumulate (0 is a motion unless a count is
    // already running). With an operator pending the op branch below
    // owns digits (d0 = motion, d3 = count).
    if c.is_ascii_digit() && input.vim.op.is_none() {
        if c == '0' && input.vim.count == 0 {
            motion(input, '0', 1);
            input.vim.reset_pending();
            return Flow::Handled;
        }
        input.vim.count =
            (input.vim.count.saturating_mul(10) + c.to_digit(10).unwrap_or(0)).min(9_999);
        return Flow::Handled;
    }
    // pending text-object namespace: iw / aw
    if input.vim.obj.is_some() {
        let obj = input.vim.obj.take();
        if c == 'w' {
            take_count(input);
            return object_op(input, obj.unwrap_or('i'));
        }
        // unknown object: abort quietly, reprocess
        input.vim.count = 0;
        return char_key(input, c);
    }
    // operator pending: doubled = linewise, digit = count (d3w),
    // motion = ranged (0 is the line-start motion, never a count here),
    // i/a = text object
    if let Some(op) = input.vim.op {
        if c == op {
            let count = take_count(input).max(1);
            input.vim.op = None;
            return linewise(op, input, count);
        }
        if c.is_ascii_digit() && (c != '0' || input.vim.count > 0) {
            input.vim.count =
                (input.vim.count.saturating_mul(10) + c.to_digit(10).unwrap_or(0)).min(9_999);
            return Flow::Handled;
        }
        if is_motion(c) {
            let count = take_count(input).max(1);
            input.vim.op = None;
            return with_motion(op, input, c, count);
        }
        if c == 'i' || c == 'a' {
            input.vim.obj = Some(c);
            return Flow::Handled;
        }
        // anything else aborts the operator
        input.vim.op = None;
        input.vim.count = 0;
        return char_key(input, c);
    }
    match c {
        'd' | 'c' | 'y' => {
            if input.vim.mode == Mode::Visual {
                // visual mode: the operator acts on the selection now
                visual_op(input, c);
                input.vim.reset_pending();
            } else {
                input.vim.op = Some(c);
            }
        }
        'g' => {
            input.vim.g = true;
        }
        'x' => {
            let count = take_count(input).max(1);
            if input.vim.mode == Mode::Visual {
                visual_op(input, 'd');
            } else {
                // one range, one register write (vim `3x` yanks all 3)
                let len = input.text.chars().count();
                if input.cursor < len {
                    apply_range(
                        'd',
                        input,
                        input.cursor,
                        (input.cursor + count as usize).min(len),
                        false,
                    );
                }
            }
            input.vim.reset_pending();
        }
        'p' => {
            let count = take_count(input).max(1);
            paste(input, count);
            input.vim.reset_pending();
        }
        'u' => {
            input.undo();
            input.vim.reset_pending();
        }
        'v' => {
            input.vim.reset_pending();
            if input.vim.mode == Mode::Visual {
                input.vim.mode = Mode::Normal;
                input.vim.anchor = None;
            } else {
                input.vim.mode = Mode::Visual;
                input.vim.anchor = Some(input.cursor);
            }
        }
        'i' => {
            if input.vim.mode == Mode::Visual {
                // vim: i/a name text objects inside a visual selection
                input.vim.obj = Some('i');
            } else {
                input.vim.reset_pending();
                input.vim.mode = Mode::Insert;
            }
        }
        'I' => {
            input.vim.reset_pending();
            input.line_home();
            input.vim.mode = Mode::Insert;
        }
        'a' => {
            if input.vim.mode == Mode::Visual {
                input.vim.obj = Some('a');
            } else {
                input.vim.reset_pending();
                let len = input.text.chars().count();
                if input.cursor < len {
                    input.cursor += 1;
                }
                input.vim.mode = Mode::Insert;
            }
        }
        'A' => {
            input.vim.reset_pending();
            input.line_end();
            input.vim.mode = Mode::Insert;
        }
        'o' => {
            if input.vim.mode == Mode::Visual {
                // vim: jump to the other end of the selection
                let anchor = input.vim.anchor.unwrap_or(input.cursor);
                input.vim.anchor = Some(input.cursor);
                input.cursor = anchor;
            } else {
                input.vim.reset_pending();
                input.line_end();
                input.newline();
                input.vim.mode = Mode::Insert;
            }
        }
        'O' => {
            input.vim.reset_pending();
            input.line_home();
            input.newline();
            input.cursor -= 1; // sit on the new empty line
            input.vim.mode = Mode::Insert;
        }
        // shorthand operator forms
        'D' => {
            input.vim.op = None;
            let _ = with_motion('d', input, '$', 1);
            input.vim.reset_pending();
        }
        'C' => {
            input.vim.op = None;
            let _ = with_motion('c', input, '$', 1);
            input.vim.reset_pending();
        }
        'Y' => {
            input.vim.op = None;
            let _ = with_motion('y', input, '$', 1);
            input.vim.reset_pending();
        }
        m if is_motion(m) => {
            let count = take_count(input).max(1);
            if input.vim.mode == Mode::Visual {
                visual_motion(input, m, count);
                // word motions are exclusive: the selection ends AT the
                // cursor (not through it), like vim's `v w`. The anchor
                // and the flag survive the motion — only the count
                // resets.
                input.vim.excl = matches!(m, 'w' | 'b');
            } else {
                motion(input, m, count);
                input.vim.reset_pending();
            }
        }
        _ => {
            input.vim.reset_pending();
        }
    }
    Flow::Handled
}

/// Vim word motion forward: skip the current word, then the following
/// run, landing on the next word's first char (InputBuffer::word_right
/// stops on the whitespace instead — readline lands short of vim).
fn word_fwd(input: &mut InputBuffer) {
    let chars: Vec<char> = input.text.chars().collect();
    let len = chars.len();
    let ws = |c: char| matches!(c, ' ' | '\n' | '\t');
    let mut i = input.cursor.min(len);
    while i < len && !ws(chars[i]) {
        i += 1;
    }
    while i < len && ws(chars[i]) {
        i += 1;
    }
    input.cursor = i;
}

/// A motion typed while an operator is pending (normal mode): apply the
/// operator over the range cursor→motion-target.
fn with_motion(op: char, input: &mut InputBuffer, m: char, count: u32) -> Flow {
    let from = input.cursor;
    let to = motion(input, m, count).unwrap_or(input.cursor);
    let (lo, hi) = order(from, to);
    apply_range(op, input, lo, hi, false);
    Flow::Handled
}

/// Linewise `dd` / `cc` / `yy` with a count.
fn linewise(op: char, input: &mut InputBuffer, count: u32) -> Flow {
    let (row, _) = input.cursor_row_col();
    let rows = input.rows().len();
    let last = (row + count as usize - 1).min(rows.saturating_sub(1));
    let lo = line_start(input, row);
    let hi = (line_end(input, last) + 1).min(input.text.chars().count());
    apply_range(op, input, lo, hi, true);
    Flow::Handled
}

/// iw / aw over an operator (normal) or the selection (visual).
fn object_op(input: &mut InputBuffer, ns: char) -> Flow {
    let (olo, ohi) = text_object(input, input.cursor, ns == 'a');
    if input.vim.mode == Mode::Visual {
        let (vlo, vhi) = selection(input);
        let (lo, hi) = (vlo.min(olo), vhi.max(ohi));
        if let Some(op) = input.vim.op.take() {
            apply_range(op, input, lo, hi, false);
        } else {
            input.vim.anchor = Some(lo);
            input.cursor = hi.saturating_sub(1);
        }
        return Flow::Handled;
    }
    let Some(op) = input.vim.op.take() else {
        return Flow::Handled;
    };
    apply_range(op, input, olo, ohi, false);
    Flow::Handled
}

/// Apply `d` / `c` / `y` over `[lo, hi)`; `linewise` marks the register
/// so `p` pastes below the current line.
fn apply_range(op: char, input: &mut InputBuffer, lo: usize, hi: usize, linewise: bool) {
    let hi = hi.min(input.text.chars().count());
    if lo >= hi {
        return;
    }
    let text: String = input.text.chars().skip(lo).take(hi - lo).collect();
    match op {
        'y' => {
            input.vim.register = Some((text, linewise));
            input.cursor = lo;
        }
        'd' | 'x' => {
            input.vim.register = Some((text, linewise));
            input.delete_range(lo, hi);
        }
        'c' => {
            input.vim.register = Some((text, linewise));
            input.delete_range(lo, hi);
            input.vim.mode = Mode::Insert;
        }
        _ => {}
    }
}

/// Visual-mode motion: move the cursor, keep the anchor.
fn visual_motion(input: &mut InputBuffer, m: char, count: u32) {
    let _ = motion(input, m, count);
}

/// The current visual selection as ordered char bounds (inclusive of
/// the cursor char, like vim's charwise visual).
fn selection(input: &InputBuffer) -> (usize, usize) {
    let anchor = input.vim.anchor.unwrap_or(input.cursor);
    let (a, b) = (anchor, input.cursor);
    if input.vim.excl {
        // word-exclusive: the landing char is the boundary, not part of
        // the selection (vim's `v w` keeps `hello ` out of `foo bar`)
        if a <= b {
            (a.min(b), b)
        } else {
            (b + 1, a + 1)
        }
    } else if a <= b {
        (a, b + 1)
    } else {
        (b, a + 1)
    }
}

/// d / y / c over the current visual selection.
fn visual_op(input: &mut InputBuffer, op: char) {
    let (lo, hi) = selection(input);
    apply_range(op, input, lo, hi, false);
    if input.vim.mode == Mode::Visual {
        input.vim.mode = Mode::Normal;
        input.vim.anchor = None;
    }
}

/// `x` at the cursor (one char, or the selection in visual).
fn x_delete(input: &mut InputBuffer) {
    if input.vim.mode == Mode::Visual {
        visual_op(input, 'd');
        return;
    }
    let len = input.text.chars().count();
    if input.cursor >= len {
        return;
    }
    apply_range('d', input, input.cursor, input.cursor + 1, false);
}

/// `p`: paste the register. Charwise after the cursor char; linewise
/// below the current line. A count repeats the paste.
fn paste(input: &mut InputBuffer, count: u32) {
    let Some((text, linewise)) = input.vim.register.clone() else {
        return;
    };
    let repeated = text.repeat(count as usize);
    if linewise {
        if input.text.is_empty() {
            // pasting the only line back into an empty buffer: no
            // leading newline (vim keeps the buffer one line tall)
            let repeated = repeated.trim_end_matches('\n').to_string();
            input.insert_str(&repeated);
            input.cursor = 0;
        } else {
            input.line_end();
            let at = input.cursor;
            input.insert_str(&format!("\n{repeated}"));
            // cursor lands on the first pasted line
            input.cursor = at + 1;
        }
    } else {
        let len = input.text.chars().count();
        if input.cursor < len {
            input.cursor += 1;
        }
        let at = input.cursor;
        input.insert_str(&repeated);
        input.cursor = at + repeated.chars().count();
    }
}

fn order(a: usize, b: usize) -> (usize, usize) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Motions the operator/routing layer understands.
fn is_motion(c: char) -> bool {
    matches!(c, 'h' | 'l' | 'w' | 'b' | '0' | '$' | 'G' | 'j' | 'k')
}

/// Apply a motion `count` times, returning the landing char index
/// (already applied to the cursor). `G` is text-end here; `gg` is
/// routed before reaching this fn.
fn motion(input: &mut InputBuffer, m: char, count: u32) -> Option<usize> {
    for _ in 0..count.max(1) {
        match m {
            'h' => input.left(),
            'l' => input.right(),
            'w' => word_fwd(input),
            'b' => input.word_left(),
            '0' => input.line_home(),
            '$' => input.line_end(),
            'G' => input.end(),
            'j' => {
                input.move_down();
            }
            'k' => {
                input.move_up();
            }
            _ => return None,
        }
    }
    Some(input.cursor)
}

/// Start / end char index of display line `row`.
fn line_start(input: &InputBuffer, row: usize) -> usize {
    input
        .rows()
        .iter()
        .take(row)
        .map(|r| r.chars().count() + 1)
        .sum()
}

fn line_end(input: &InputBuffer, row: usize) -> usize {
    line_start(input, row) + input.rows().get(row).map_or(0, |r| r.chars().count())
}

/// The `iw` / `aw` text object at `cursor` as `[lo, hi)` char bounds.
/// On whitespace `iw` takes the following word (vim's cursor-on-space
/// behavior); `aw` adds the trailing run (or the leading one when the
/// word ends the text).
fn text_object(input: &InputBuffer, cursor: usize, around: bool) -> (usize, usize) {
    let chars: Vec<char> = input.text.chars().collect();
    let len = chars.len();
    let ws = |c: char| matches!(c, ' ' | '\n' | '\t');
    let (mut lo, mut hi) = if len == 0 {
        (0, 0)
    } else {
        let at = cursor.min(len - 1);
        if ws(chars[at]) {
            // whitespace under the cursor: iw grabs the next word if
            // any, else the whitespace run itself
            let mut s = at;
            while s < len && ws(chars[s]) {
                s += 1;
            }
            if s == len {
                let mut e = at;
                while e > 0 && ws(chars[e - 1]) {
                    e -= 1;
                }
                (e, at + 1)
            } else {
                let mut e = s;
                while e < len && !ws(chars[e]) {
                    e += 1;
                }
                (s, e)
            }
        } else {
            let mut s = at;
            while s > 0 && !ws(chars[s - 1]) {
                s -= 1;
            }
            let mut e = at + 1;
            while e < len && !ws(chars[e]) {
                e += 1;
            }
            (s, e)
        }
    };
    if around {
        // absorb the trailing run; none → absorb the leading run
        let mut e = hi;
        while e < len && ws(chars[e]) {
            e += 1;
        }
        if e > hi {
            hi = e;
        } else {
            while lo > 0 && ws(chars[lo - 1]) {
                lo -= 1;
            }
        }
    }
    (lo, hi)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn buf(text: &str, cursor: usize) -> InputBuffer {
        let mut b = InputBuffer::default();
        b.insert_str(text);
        b.cursor = cursor;
        b
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn feed(b: &mut InputBuffer, cs: &str) {
        for c in cs.chars() {
            handle(b, &key(c));
        }
    }

    fn normal_on(cursor: usize) -> InputBuffer {
        let mut b = buf("hello brave new world", cursor);
        b.vim.on = true;
        b.vim.mode = Mode::Normal;
        b
    }

    #[test]
    fn motions_move_and_count() {
        let mut b = normal_on(0);
        feed(&mut b, "w");
        assert_eq!(b.cursor, 6); // hello| brave
        // vim counts WORDS: brave → new → world
        feed(&mut b, "2w");
        assert_eq!(b.cursor, 16);
        feed(&mut b, "b");
        assert_eq!(b.cursor, 12);
        feed(&mut b, "$");
        assert_eq!(b.cursor, 21);
        feed(&mut b, "0");
        assert_eq!(b.cursor, 0);
        feed(&mut b, "G");
        assert_eq!(b.cursor, 21);
        feed(&mut b, "gg");
        assert_eq!(b.cursor, 0);
    }

    #[test]
    fn x_deletes_with_count_and_register() {
        let mut b = normal_on(0);
        feed(&mut b, "3x");
        assert_eq!(b.text, "lo brave new world");
        assert_eq!(b.vim.register, Some(("hel".to_string(), false)));
    }

    #[test]
    fn operator_motion_ranges_delete_and_yank() {
        let mut b = normal_on(6);
        feed(&mut b, "dw");
        assert_eq!(b.text, "hello new world");
        assert_eq!(b.cursor, 6);
        feed(&mut b, "y$");
        assert_eq!(b.vim.register, Some(("new world".to_string(), false)));
        assert_eq!(b.text, "hello new world");
        feed(&mut b, "d0");
        assert_eq!(b.text, "new world");
        assert_eq!(b.cursor, 0);
    }

    #[test]
    fn dd_yanks_linerwise_and_p_pastes_below() {
        let mut b = normal_on(0);
        feed(&mut b, "dd");
        assert_eq!(b.text, "");
        feed(&mut b, "p");
        // linewise paste restores the line below (the only line again)
        assert_eq!(b.text, "hello brave new world");
    }

    #[test]
    fn cc_changes_lines_into_insert() {
        let mut b = normal_on(3);
        feed(&mut b, "cc");
        assert_eq!(b.text, "");
        assert_eq!(b.vim.mode, Mode::Insert);
        for c in "rewritten".chars() {
            // insert mode passes typing through to the composer
            assert_eq!(handle(&mut b, &key(c)), Flow::Passthrough);
            b.insert(c);
        }
        assert_eq!(b.text, "rewritten");
    }

    #[test]
    fn text_objects_iw_and_aw() {
        let mut b = normal_on(6); // on `brave`
        feed(&mut b, "yiw");
        assert_eq!(b.vim.register, Some(("brave".to_string(), false)));
        feed(&mut b, "daw");
        assert_eq!(b.text, "hello new world");
    }

    #[test]
    fn visual_selects_and_operates() {
        let mut b = normal_on(0);
        feed(&mut b, "v");
        assert_eq!(b.vim.mode, Mode::Visual);
        feed(&mut b, "w");
        feed(&mut b, "d");
        assert_eq!(b.text, "brave new world");
        assert_eq!(b.vim.mode, Mode::Normal);
    }

    #[test]
    fn visual_iw_extends_to_word() {
        let mut b = normal_on(0);
        feed(&mut b, "v");
        // caret at 0 → iw extends the selection across the word
        feed(&mut b, "iw");
        feed(&mut b, "y");
        assert_eq!(b.vim.register, Some(("hello".to_string(), false)));
    }

    #[test]
    fn insert_entry_keys_place_the_cursor() {
        let mut b = normal_on(0);
        feed(&mut b, "A");
        assert_eq!(b.cursor, 21);
        assert_eq!(b.vim.mode, Mode::Insert);
        b.insert('!');
        assert_eq!(b.text, "hello brave new world!");
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(handle(&mut b, &esc), Flow::Handled);
        assert_eq!(b.vim.mode, Mode::Normal);
        assert_eq!(b.cursor, 21); // stepped back onto the typed char
        feed(&mut b, "o");
        assert_eq!(b.text, "hello brave new world!\n");
        assert_eq!(b.vim.mode, Mode::Insert);
    }

    #[test]
    fn counts_prefix_operators_and_motions() {
        let mut b = normal_on(0);
        // count between operator and motion (d2w) and before it (2dw)
        feed(&mut b, "d2w");
        assert_eq!(b.text, "new world");
    }

    #[test]
    fn counts_before_operators() {
        let mut b = normal_on(0);
        feed(&mut b, "2dw");
        assert_eq!(b.text, "new world");
    }

    #[test]
    fn insert_mode_passes_everything_but_esc() {
        let mut b = normal_on(0);
        b.vim.mode = Mode::Insert;
        assert_eq!(handle(&mut b, &key('d')), Flow::Passthrough);
        assert_eq!(handle(&mut b, &key('x')), Flow::Passthrough);
    }

    #[test]
    fn readline_chords_pass_through_in_every_mode() {
        let mut b = normal_on(4);
        feed(&mut b, "i");
        assert_eq!(b.vim.mode, Mode::Insert);
        let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(handle(&mut b, &ctrl_d), Flow::Passthrough);
    }

    #[test]
    fn toggle_resets_state_and_reports() {
        let mut v = Vim::default();
        assert!(v.toggle(), "off → on");
        assert!(v.on);
        assert_eq!(v.mode, Mode::Insert);
        assert_eq!(v.label(), Some("INSERT"));
        assert!(!v.toggle(), "on → off");
        assert_eq!(v.label(), None);
    }

    #[test]
    fn multiline_j_k_move_rows() {
        let mut b = normal_on(0);
        b.insert_str("\nsecond line");
        b.cursor = 0;
        feed(&mut b, "j");
        assert_eq!(b.cursor_row_col(), (1, 0));
        feed(&mut b, "k");
        assert_eq!(b.cursor_row_col(), (0, 0));
    }

    #[test]
    fn undo_via_u_restores_delete() {
        let mut b = normal_on(0);
        feed(&mut b, "x");
        assert_eq!(b.text, "ello brave new world");
        feed(&mut b, "u");
        assert_eq!(b.text, "hello brave new world");
    }

    #[test]
    fn off_mode_is_pure_passthrough() {
        let mut b = normal_on(0);
        b.vim.on = false;
        assert_eq!(handle(&mut b, &key('d')), Flow::Passthrough);
    }
}
