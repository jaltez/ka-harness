//! Terminal primitives for ka surfaces: a raw-mode guard and pure key
//! decoding. The full TUI (transcript, editor, footer) lands in Phase 3 on
//! top of ratatui; this crate keeps the input contract frozen and testable.

use std::io;

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

pub mod fx;
pub mod icons;
pub mod markdown;
pub mod observer;
pub mod palette;
pub mod tui;

/// A decoded keypress, surface-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A plain character (any modifier combination resolves to `Other`).
    Char(char),
    /// Enter.
    Enter,
    /// Escape.
    Esc,
    /// Tab.
    Tab,
    /// Backspace.
    Backspace,
    /// Arrow up.
    Up,
    /// Arrow down.
    Down,
    /// Arrow left.
    Left,
    /// Arrow right.
    Right,
    /// Ctrl+C (interrupt).
    CtrlC,
    /// Anything else.
    Other,
}

/// Map a crossterm event to a [`Key`]. Non-key events and key releases map
/// to `None`.
pub fn map_key(ev: &TermEvent) -> Option<Key> {
    let TermEvent::Key(key) = ev else {
        return None;
    };
    if key.kind == KeyEventKind::Release {
        return None;
    }
    Some(map_key_event(key))
}

/// Map a key event (press or repeat) to a [`Key`].
pub fn map_key_event(key: &KeyEvent) -> Key {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        if let KeyCode::Char('c') = key.code {
            return Key::CtrlC;
        }
        return Key::Other;
    }
    if !key.modifiers.is_empty() {
        return Key::Other;
    }
    match key.code {
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Tab => Key::Tab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        _ => Key::Other,
    }
}

/// Raw-mode guard: enables raw mode on creation, restores on drop. Failure
/// to restore is ignored (the terminal is already gone in the worst case).
pub struct Terminal {
    active: bool,
}

impl Terminal {
    /// Enter raw mode.
    pub fn new() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self { active: true })
    }

    /// Restore cooked mode early, if still active.
    pub fn restore(&mut self) {
        if self.active {
            let _ = crossterm::terminal::disable_raw_mode();
            self.active = false;
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Current terminal size as (columns, rows).
pub fn size() -> io::Result<(u16, u16)> {
    crossterm::terminal::size()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crossterm::event::KeyEventState;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn plain_keys_map() {
        assert_eq!(
            map_key_event(&key(KeyCode::Char('a'), KeyModifiers::NONE)),
            Key::Char('a')
        );
        assert_eq!(
            map_key_event(&key(KeyCode::Enter, KeyModifiers::NONE)),
            Key::Enter
        );
        assert_eq!(
            map_key_event(&key(KeyCode::Esc, KeyModifiers::NONE)),
            Key::Esc
        );
        assert_eq!(
            map_key_event(&key(KeyCode::Tab, KeyModifiers::NONE)),
            Key::Tab
        );
        assert_eq!(
            map_key_event(&key(KeyCode::Backspace, KeyModifiers::NONE)),
            Key::Backspace
        );
        assert_eq!(
            map_key_event(&key(KeyCode::Up, KeyModifiers::NONE)),
            Key::Up
        );
        assert_eq!(
            map_key_event(&key(KeyCode::Down, KeyModifiers::NONE)),
            Key::Down
        );
        assert_eq!(
            map_key_event(&key(KeyCode::F(1), KeyModifiers::NONE)),
            Key::Other
        );
    }

    #[test]
    fn ctrl_c_maps_interrupt() {
        assert_eq!(
            map_key_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Key::CtrlC
        );
        assert_eq!(
            map_key_event(&key(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            Key::Other
        );
    }

    #[test]
    fn releases_and_non_keys_are_none() {
        let release = KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };
        assert!(map_key(&TermEvent::Key(release)).is_none());
        assert!(map_key(&TermEvent::Resize(80, 24)).is_none());
        assert_eq!(
            map_key(&TermEvent::Key(key(KeyCode::Char('x'), KeyModifiers::NONE))),
            Some(Key::Char('x'))
        );
    }
}
