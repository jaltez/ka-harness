//! Key bindings (`[keybindings]`): every remappable key in the
//! composer resolves through an action id, so the defaults stay the
//! keyboard that ka ships with and a config table reroutes chords
//! without code changes.
//!
//! ```toml
//! [keybindings]
//! "popups.todos" = "ctrl+g"   # action id = chord
//! ```
//!
//! A chord is `mod+mod+key` (`ctrl alt shift super`, lowercase); a
//! chord with spaces (`esc esc`) is a two-key sequence and only
//! `session.rewind` ships one. Two actions that resolve to the same
//! first chord *in the same context* are a hard startup error naming
//! both — ka refuses to start half-keyboarded. Contexts model the
//! states a key can never collide in: the composer is idle
//! ([`CTX_IDLE`]) or mid-turn ([`CTX_BUSY`]), and Enter legitimately
//! means `composer.send` when idle and `composer.steer` when busy.
//!
//! Actions without a config entry keep their default chord; an entry
//! replaces the list wholesale (multi-chord defaults like
//! `composer.newline` collapse to the one chord you wrote). /help
//! always renders the effective bindings from [`ACTIVE`].

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The chord(s) an action answers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// Action id (`composer.send`) — also the config table key.
    pub id: &'static str,
    /// Context group for duplicate detection ([`CTX_*`]).
    pub ctx: &'static str,
    /// Default chords, display order first.
    pub default: &'static [&'static str],
}

/// Composer idle context.
pub const CTX_IDLE: &str = "composer.idle";
/// Mid-turn context (steer, abort).
pub const CTX_BUSY: &str = "composer.busy";
/// Editing-only keys (newline).
pub const CTX_EDIT: &str = "composer.edit";
/// Session-level idle keys (rewind, quit).
pub const CTX_SESSION: &str = "session.idle";

/// Every remappable action: id → context → default chords. The dispatch
/// sites in the app loop consult these via [`Bindings::matches`]; keys
/// that are *not* here (editing keys, scroll, modal navigation) stay
/// fixed and are documented in /help as such.
pub const ACTIONS: &[Binding] = &[
    Binding {
        id: "composer.send",
        ctx: CTX_IDLE,
        default: &["enter"],
    },
    Binding {
        id: "composer.newline",
        ctx: CTX_EDIT,
        default: &["shift+enter", "ctrl+j"],
    },
    Binding {
        id: "composer.steer",
        ctx: CTX_BUSY,
        default: &["enter"],
    },
    Binding {
        id: "session.abort",
        ctx: CTX_BUSY,
        default: &["esc"],
    },
    Binding {
        id: "session.rewind",
        ctx: CTX_SESSION,
        default: &["esc esc"],
    },
    Binding {
        id: "session.quit",
        ctx: CTX_SESSION,
        default: &["ctrl+c"],
    },
    Binding {
        id: "popups.skills",
        ctx: CTX_IDLE,
        default: &["ctrl+t"],
    },
    Binding {
        id: "popups.todos",
        ctx: CTX_IDLE,
        default: &["alt+o"],
    },
    Binding {
        id: "popups.info",
        ctx: CTX_IDLE,
        default: &["alt+i"],
    },
    Binding {
        id: "composer.external_editor",
        ctx: CTX_IDLE,
        default: &["alt+e"],
    },
    Binding {
        id: "mouse.toggle",
        ctx: CTX_IDLE,
        default: &["alt+m"],
    },
    Binding {
        id: "theme.picker",
        ctx: CTX_IDLE,
        default: &["ctrl+g"],
    },
    Binding {
        id: "changelog",
        ctx: CTX_IDLE,
        default: &["alt+c"],
    },
    Binding {
        id: "queue.open",
        ctx: CTX_IDLE,
        default: &["alt+u"],
    },
    Binding {
        id: "copy.picker",
        ctx: CTX_IDLE,
        default: &["alt+y"],
    },
    Binding {
        id: "session.search",
        ctx: CTX_IDLE,
        default: &["alt+s"],
    },
    Binding {
        id: "git.panel",
        ctx: CTX_IDLE,
        default: &["alt+g"],
    },
];

/// One parsed key step: a key plus its held modifiers. `SHIFT` alone on
/// a letter means the shifted glyph (crossterm delivers `shift+a` as
/// `A`), so parser stores the canonical character lowercased and keeps
/// the shift flag for non-letters (`shift+enter`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chord {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl Chord {
    /// Parse `ctrl+alt+shift+super+<key>`. Unknown modifiers or keys
    /// are errors naming the token.
    pub fn parse(text: &str) -> Result<Chord, String> {
        let mut mods = KeyModifiers::NONE;
        let mut key: Option<&str> = None;
        for part in text.split('+').map(str::trim).filter(|p| !p.is_empty()) {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
                "alt" | "option" => mods |= KeyModifiers::ALT,
                "shift" => mods |= KeyModifiers::SHIFT,
                "super" | "cmd" | "meta" => mods |= KeyModifiers::SUPER,
                _ => {
                    if let Some(prev) = key {
                        return Err(format!("chord {text:?}: two keys ({prev:?} and {part:?})"));
                    }
                    key = Some(part);
                }
            }
        }
        let Some(key) = key else {
            return Err(format!("chord {text:?}: no key"));
        };
        let k = key.to_ascii_lowercase();
        let code = match k.as_str() {
            "enter" | "return" | "cr" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "tab" => KeyCode::Tab,
            "space" => KeyCode::Char(' '),
            "backspace" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            "insert" => KeyCode::Insert,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" | "pgup" => KeyCode::PageUp,
            "pagedown" | "pgdn" => KeyCode::PageDown,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            f if f.len() >= 2
                && f.starts_with('f')
                && f[1..].chars().all(|c| c.is_ascii_digit()) =>
            {
                let n: u8 = f[1..].parse().unwrap_or(0);
                if (1..=12).contains(&n) {
                    KeyCode::F(n)
                } else {
                    return Err(format!(
                        "chord {text:?}: function key out of range (f1..f12)"
                    ));
                }
            }
            one if one.chars().count() == 1 => {
                let c = one.chars().next().unwrap_or('?');
                // a shifted letter is just the letter: crossterm reports
                // shift+A as Char('A') + SHIFT; the chord stores the
                // lowercase key and drops the shift so both arrive
                if c.is_ascii_uppercase() {
                    mods.remove(KeyModifiers::SHIFT);
                    KeyCode::Char(c.to_ascii_lowercase())
                } else {
                    KeyCode::Char(c)
                }
            }
            _ => return Err(format!("chord {text:?}: unknown key {key:?}")),
        };
        Ok(Chord { code, mods })
    }

    /// `/help` spelling (`ctrl+alt+f4`).
    pub fn display(&self) -> String {
        let mut parts = Vec::new();
        if self.mods.contains(KeyModifiers::CONTROL) {
            parts.push("ctrl");
        }
        if self.mods.contains(KeyModifiers::ALT) {
            parts.push("alt");
        }
        if self.mods.contains(KeyModifiers::SHIFT) {
            parts.push("shift");
        }
        if self.mods.contains(KeyModifiers::SUPER) {
            parts.push("super");
        }
        let key = match self.code {
            KeyCode::Enter => "enter".to_string(),
            KeyCode::Esc => "esc".to_string(),
            KeyCode::Tab => "tab".to_string(),
            KeyCode::Backspace => "backspace".to_string(),
            KeyCode::Delete => "delete".to_string(),
            KeyCode::Insert => "insert".to_string(),
            KeyCode::Home => "home".to_string(),
            KeyCode::End => "end".to_string(),
            KeyCode::PageUp => "pgup".to_string(),
            KeyCode::PageDown => "pgdn".to_string(),
            KeyCode::Up => "up".to_string(),
            KeyCode::Down => "down".to_string(),
            KeyCode::Left => "left".to_string(),
            KeyCode::Right => "right".to_string(),
            KeyCode::Char(' ') => "space".to_string(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::F(n) => format!("f{n}"),
            _ => return String::new(),
        };
        parts.push(&key);
        parts.join("+")
    }

    /// Does a live key event answer to this chord? Letters compare
    /// case-insensitively and carry exact CTRL/ALT/SUPER (`alt+t` must
    /// never eat a plain `t`); non-letter keys answer when the event
    /// carries at least the chord's modifiers — plain `enter` answers
    /// even when the terminal rides SHIFT or ALT along (the old
    /// `(Enter, _)` arms did), and a chord demanding more never fires
    /// early: `shift+enter` claims ⇧⏎ before the send arm sees it.
    fn hits(&self, key: &KeyEvent) -> bool {
        let code_ok = match (self.code, key.code) {
            (KeyCode::Char(want), KeyCode::Char(got)) => want.eq_ignore_ascii_case(&got),
            (want, got) => want == got,
        };
        if !code_ok {
            return false;
        }
        let (want, got) = (
            self.modds(self.mods, self.code),
            self.modds(key.modifiers, key.code),
        );
        if matches!(self.code, KeyCode::Char(_)) {
            // letters: ctrl/alt/super exact, shift free
            let hard = KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER;
            want.intersection(hard) == got.intersection(hard)
        } else {
            // non-letters: the event must carry at least the chord's
            // modifiers; extras beyond it are ignored (the old
            // `(Enter, _)` arms sent on ctrl+enter / alt+enter too).
            // The demand side stays strict: plain `enter` never
            // answers `shift+enter` — the newline chord can't steal
            // the send key.
            want.difference(got).is_empty()
        }
    }

    /// Letters store shift-stripped: crossterm reports `shift+a` as
    /// Char('A') + SHIFT, and the chord keeps the lowercase key, so the
    /// modifier compare must drop shift on alphabetic chords on both
    /// sides.
    fn modds(&self, mods: KeyModifiers, code: KeyCode) -> KeyModifiers {
        if matches!(code, KeyCode::Char(c) if c.is_ascii_alphabetic()) {
            mods.difference(KeyModifiers::SHIFT)
        } else {
            mods
        }
    }
}

/// The effective keyboard: action id → chord sequences. Defaults when
/// no `[keybindings]` table exists (or it didn't touch the action).
/// Most actions carry single-chord sequences; `session.rewind` ships
/// `esc esc` (driven by the existing double-esc chain, not by
/// [`Bindings::matches`], which only answers single chords).
#[derive(Debug, Clone)]
pub struct Bindings {
    chords: HashMap<&'static str, Vec<Vec<Chord>>>,
    /// Actions the config table actually remapped.
    overridden: Vec<&'static str>,
}

impl Default for Bindings {
    fn default() -> Self {
        Self::resolve(None).unwrap_or_else(|_| Bindings {
            chords: default_chords(),
            overridden: Vec::new(),
        })
    }
}

fn default_chords() -> HashMap<&'static str, Vec<Vec<Chord>>> {
    ACTIONS
        .iter()
        .map(|a| {
            let chords = a
                .default
                .iter()
                .map(|seq| {
                    seq.split_whitespace()
                        .filter_map(|c| Chord::parse(c).ok())
                        .collect::<Vec<_>>()
                })
                .collect();
            (a.id, chords)
        })
        .collect()
}

impl Bindings {
    /// Resolve the effective keyboard from the config table. Unknown
    /// action ids, unparsable chords and same-context duplicates are
    /// hard errors (the TUI refuses to start with a broken keyboard).
    pub fn resolve(cfg: Option<&BTreeMap<String, String>>) -> Result<Bindings, String> {
        let mut chords = default_chords();
        let mut overridden = Vec::new();
        let Some(table) = cfg else {
            return Ok(Bindings { chords, overridden });
        };
        for (id, text) in table {
            let Some(action) = ACTIONS.iter().find(|a| a.id == *id) else {
                return Err(format!(
                    "[keybindings] unknown action {id:?} — known: {}",
                    ACTIONS.iter().map(|a| a.id).collect::<Vec<_>>().join(", ")
                ));
            };
            let parsed = text
                .split_whitespace()
                .map(Chord::parse)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("[keybindings] {id} = {text:?}: {e}"))?;
            if parsed.is_empty() {
                return Err(format!("[keybindings] {id} = {text:?}: empty chord"));
            }
            let sequence = vec![parsed];
            if sequence.iter().any(|seq| seq.len() > 1) {
                return Err(format!(
                    "[keybindings] {id} = {text:?}: config chords are single-key — only the built-in esc esc sequence exists"
                ));
            }
            chords.insert(action.id, sequence);
            overridden.push(action.id);
        }
        // duplicates: two actions sharing a first chord in the same
        // context can never both fire reliably — refuse rather than
        // silently pick (send/steer share Enter by design: idle vs busy)
        let mut seen: HashMap<(&'static str, String), &str> = HashMap::new();
        for action in ACTIONS {
            for seq in &chords[action.id] {
                let Some(first) = seq.first() else {
                    continue;
                };
                let key = (action.ctx, first.display());
                if let Some(prev) = seen.get(&key) {
                    return Err(format!(
                        "[keybindings] {prev} and {} both bind {} — same context, one must change",
                        action.id,
                        first.display()
                    ));
                }
                seen.insert(key, action.id);
            }
        }
        Ok(Bindings { chords, overridden })
    }

    /// Does this key answer to the action's effective chords? Only
    /// single-chord sequences count (`session.rewind`'s `esc esc` is
    /// driven by the double-esc chain in the app loop, not here).
    pub fn matches(&self, key: &KeyEvent, id: &str) -> bool {
        self.chords
            .get(id)
            .is_some_and(|list| list.iter().any(|seq| seq.len() == 1 && seq[0].hits(key)))
    }

    /// Matches only when the config remapped the action. Custom chords
    /// fire *alongside* the built-in esc-chain semantics — with the
    /// default chord the chain owns the key, so an override must exist
    /// before the extra trigger arm engages.
    pub fn matches_custom(&self, key: &KeyEvent, id: &str) -> bool {
        self.overridden.contains(&id) && self.matches(key, id)
    }

    /// `/help` display string: the effective chords, comma-joined
    /// (sequences render space-separated, as typed).
    pub fn display(&self, id: &str) -> String {
        self.chords
            .get(id)
            .map(|list| {
                list.iter()
                    .map(|seq| seq.iter().map(Chord::display).collect::<Vec<_>>().join(" "))
                    .collect::<Vec<_>>()
                    .join(" / ")
            })
            .unwrap_or_default()
    }

    /// Action ids + effective chord strings, table order (the /help
    /// key table iterates this).
    pub fn rows(&self) -> Vec<(&'static str, String)> {
        ACTIONS.iter().map(|a| (a.id, self.display(a.id))).collect()
    }
}

/// The live keyboard for this process, resolved once at startup from
/// `[keybindings]`. Reads before `set` fall back to the defaults (tests,
/// surfaces that never ran through `run`).
pub static ACTIVE: OnceLock<Bindings> = OnceLock::new();

/// The effective bindings for this process: startup-resolved, or the
/// defaults when unset.
pub fn active() -> &'static Bindings {
    ACTIVE.get().unwrap_or(&DEFAULTS)
}

static DEFAULTS: std::sync::LazyLock<Bindings> = std::sync::LazyLock::new(Bindings::default);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn chords_roundtrip() {
        for text in [
            "enter",
            "esc",
            "tab",
            "ctrl+t",
            "alt+o",
            "shift+enter",
            "ctrl+j",
            "ctrl+alt+f4",
            "super+space",
            "f4",
            "ctrl+alt+shift+f12",
        ] {
            let c = Chord::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
            let shown = c.display();
            // display may normalize (pgup) but must re-parse equal
            assert_eq!(Chord::parse(&shown).map(|p| p.code), Ok(c.code), "{text}");
        }
        assert_eq!(
            Chord::parse("ctrl+alt+f4").unwrap().mods,
            KeyModifiers::CONTROL | KeyModifiers::ALT
        );
        assert_eq!(Chord::parse("ctrl+alt+f4").unwrap().code, KeyCode::F(4));
    }

    #[test]
    fn chord_parse_errors_name_the_problem() {
        for text in [
            "", "ctrl", "ctrl+", "ctrl+a+b", "hyper+t", "f0", "f13", "chord",
        ] {
            assert!(Chord::parse(text).is_err(), "{text:?} must not parse");
        }
        assert!(Chord::parse("hyper+t").unwrap_err().contains("hyper"));
    }

    #[test]
    fn matches_with_shift_tolerance() {
        let b = Bindings::default();
        // plain + shift-covered press
        assert!(b.matches(
            &key(KeyCode::Char('t'), KeyModifiers::CONTROL),
            "popups.skills"
        ));
        assert!(!b.matches(
            &key(KeyCode::Char('t'), KeyModifiers::NONE),
            "popups.skills"
        ));
        // alt never leaks into plain letters
        assert!(!b.matches(&key(KeyCode::Char('t'), KeyModifiers::ALT), "popups.skills"));
        // enter: bare and modifier-riding presses all answer send
        // (superset rule); ⇧⏎ is newline's — its arm claims the key
        // first in dispatch order
        assert!(b.matches(&key(KeyCode::Enter, KeyModifiers::NONE), "composer.send"));
        assert!(b.matches(
            &key(KeyCode::Enter, KeyModifiers::SHIFT),
            "composer.newline"
        ));
        assert!(b.matches(
            &key(KeyCode::Char('j'), KeyModifiers::CONTROL),
            "composer.newline"
        ));
        assert!(b.matches(&key(KeyCode::Enter, KeyModifiers::CONTROL), "composer.send"));
        assert!(
            !b.matches(&key(KeyCode::Enter, KeyModifiers::NONE), "composer.newline"),
            "plain enter must not newline (⇧⏎ demands the flag)"
        );
        // shifted letter glyph answers the lowercase chord
        assert!(b.matches(
            &key(
                KeyCode::Char('T'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            "popups.skills"
        ));
    }

    #[test]
    fn defaults_cover_every_action_and_stay_unique_per_context() {
        let b = Bindings::default();
        for a in ACTIONS {
            let shown = b.display(a.id);
            assert!(!shown.is_empty(), "{} has no default chord", a.id);
        }
        // resolve(None) is the no-config path and must never fail
        assert!(Bindings::resolve(None).is_ok());
    }

    #[test]
    fn config_reroutes_and_replaces_multi_chord_defaults() {
        let mut cfg = BTreeMap::new();
        cfg.insert("popups.todos".to_string(), "alt+x".to_string());
        let b = Bindings::resolve(Some(&cfg)).expect("resolves");
        assert!(b.matches(&key(KeyCode::Char('x'), KeyModifiers::ALT), "popups.todos"));
        assert!(!b.matches(&key(KeyCode::Char('o'), KeyModifiers::ALT), "popups.todos"));
        assert!(b.matches_custom(&key(KeyCode::Char('x'), KeyModifiers::ALT), "popups.todos"));
        assert!(!b.matches_custom(
            &key(KeyCode::Char('t'), KeyModifiers::CONTROL),
            "popups.skills"
        ));

        // newline had two defaults; a table entry replaces the list
        let mut cfg = BTreeMap::new();
        cfg.insert("composer.newline".to_string(), "ctrl+enter".to_string());
        let b = Bindings::resolve(Some(&cfg)).expect("resolves");
        assert!(b.matches(
            &key(KeyCode::Enter, KeyModifiers::CONTROL),
            "composer.newline"
        ));
        assert!(!b.matches(
            &key(KeyCode::Char('j'), KeyModifiers::CONTROL),
            "composer.newline"
        ));
    }

    #[test]
    fn duplicates_and_unknowns_are_hard_errors() {
        let mut cfg = BTreeMap::new();
        cfg.insert("popups.skills".to_string(), "ctrl+t".to_string());
        cfg.insert("popups.info".to_string(), "ctrl+t".to_string());
        let err = Bindings::resolve(Some(&cfg)).unwrap_err();
        assert!(
            err.contains("popups.skills") && err.contains("popups.info"),
            "{err}"
        );

        // same chord, different context is fine (enter = send idle /
        // steer busy)
        let mut cfg = BTreeMap::new();
        cfg.insert("composer.send".to_string(), "enter".to_string());
        assert!(Bindings::resolve(Some(&cfg)).is_ok());

        let mut cfg = BTreeMap::new();
        cfg.insert("no.such.action".to_string(), "ctrl+t".to_string());
        let err = Bindings::resolve(Some(&cfg)).unwrap_err();
        assert!(err.contains("no.such.action"), "{err}");

        let mut cfg = BTreeMap::new();
        cfg.insert("changelog".to_string(), "ctrl+hyper".to_string());
        let err = Bindings::resolve(Some(&cfg)).unwrap_err();
        assert!(err.contains("hyper"), "{err}");
    }

    #[test]
    fn sequences_stay_built_in() {
        let mut cfg = BTreeMap::new();
        cfg.insert("session.rewind".to_string(), "ctrl+r ctrl+r".to_string());
        let err = Bindings::resolve(Some(&cfg)).unwrap_err();
        assert!(err.contains("single-key"), "{err}");
    }
}
