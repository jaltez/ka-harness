//! Theme loading (`[tui] theme`): a TOML file recolors the Nocturne
//! palette token by token, live. Files live in `.ka/themes/<name>.toml`
//! (project, trust-gated — an untrusted project's themes never load)
//! and `~/.config/ka/themes/<name>.toml` (user); the project layer
//! shadows the user layer.
//!
//! Format:
//!
//! ```toml
//! [color]
//! bg = "#1a1714"        # hex (#rgb, #rrggbb) …
//! accent = 135          # … or an ANSI index 0-255
//! [symbols]
//! prompt = "❯"          # glyph overrides (see icons::SYMBOL_KEYS)
//! ```
//!
//! Every palette token in [`TOKENS`] is a valid `color` key; tokens the
//! file omits keep the Nocturne value, tokens it invents are rejected
//! with a note (the rest still applies). The recolor itself rides the
//! render backend: each cell's style colors are mapped
//! default→override at draw time (see `TuiTerminal`), so switching a
//! theme needs no re-render pass — only a full repaint, which
//! `/theme`'s `terminal.clear()` already triggers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use ratatui::style::Color;

use crate::icons;

/// Every palette token a theme may set: `(token name, Nocturne
/// default)`. Aliased tokens (`heading` = `accent`, `sel_fg` =
/// `strong`, …) are their own entries: the map is built in this order,
/// so when a theme sets two aliases that share a default to different
/// values the later one wins (same fate either way — the render backend
/// maps colors, not token names).
pub const TOKENS: &[(&str, Color)] = &[
    // surfaces
    ("bg", crate::palette::BG),
    ("bg_panel", crate::palette::BG_PANEL),
    ("bg_surface", crate::palette::BG_SURFACE),
    ("bg_code", crate::palette::BG_CODE),
    ("bg_user", crate::palette::BG_USER),
    ("bg_tool", crate::palette::BG_TOOL),
    ("bg_err_tint", crate::palette::BG_ERR_TINT),
    ("bg_select", crate::palette::BG_SELECT),
    ("bg_hover", crate::palette::BG_HOVER),
    ("shadow", crate::palette::SHADOW),
    // text ramp
    ("fg", crate::palette::FG),
    ("strong", crate::palette::STRONG),
    ("ink_dim", crate::palette::INK_DIM),
    ("meta", crate::palette::META),
    ("faint", crate::palette::FAINT),
    ("chip_fg", crate::palette::CHIP_FG),
    // lines
    ("border", crate::palette::BORDER),
    ("border_focus", crate::palette::BORDER_FOCUS),
    ("border_quiet", crate::palette::BORDER_QUIET),
    ("rule", crate::palette::RULE),
    // accent wheel
    ("accent", crate::palette::ACCENT),
    ("user", crate::palette::USER),
    ("tool", crate::palette::TOOL),
    ("think", crate::palette::THINK),
    ("ok", crate::palette::OK),
    ("warn", crate::palette::WARN),
    ("err", crate::palette::ERR),
    ("link", crate::palette::LINK),
    ("sel_bg", crate::palette::SEL_BG),
    ("sel_fg", crate::palette::SEL_FG),
    // markdown roles
    ("heading", crate::palette::HEADING),
    ("code_inline", crate::palette::CODE_INLINE),
    ("code_block", crate::palette::CODE_BLOCK),
    ("syntax_comment", crate::palette::SYNTAX_COMMENT),
    ("syntax_keyword", crate::palette::SYNTAX_KEYWORD),
    ("syntax_string", crate::palette::SYNTAX_STRING),
    ("syntax_number", crate::palette::SYNTAX_NUMBER),
];

/// One loaded theme: the token → color overrides plus any notes about
/// ignored keys (surfaced in the transcript, never fatal).
#[derive(Debug, Clone, Default)]
pub struct Theme {
    /// Theme name as loaded (`default` = the built-in palette).
    pub name: String,
    /// Token name → color, only for tokens the file sets.
    pub colors: HashMap<String, Color>,
    /// Glyph key → glyph, only for symbols the file sets.
    pub symbols: HashMap<String, String>,
    /// Non-fatal notes: unknown tokens, unparsable values.
    pub notes: Vec<String>,
}

/// `#rgb`/`#rrggbb`/`rrggbb` hex or an ANSI index 0-255.
pub fn parse_color(value: &str) -> Option<Color> {
    let v = value.trim();
    if !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()) {
        // a bare number is always an ANSI index — never hex
        return v.parse::<u8>().ok().map(Color::Indexed);
    }
    let hex = v.strip_prefix('#').unwrap_or(v);
    let hex = hex.strip_prefix("0x").unwrap_or(hex);
    match hex.len() {
        6 => u32::from_str_radix(hex, 16)
            .ok()
            .map(|rgb| Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)),
        3 => u32::from_str_radix(hex, 16).ok().map(|rgb| {
            let (r, g, b) = ((rgb >> 8) & 0xF, (rgb >> 4) & 0xF, rgb & 0xF);
            Color::Rgb((r * 17) as u8, (g * 17) as u8, (b * 17) as u8)
        }),
        _ => None,
    }
}

/// Parse a theme TOML. Missing tokens fall back to Nocturne (they are
/// simply absent from `colors`); unknown tokens and unparsable values
/// become notes — the rest of the theme still applies.
pub fn from_toml(name: &str, text: &str) -> Result<Theme, String> {
    let doc: toml::Table = toml::from_str(text).map_err(|e| format!("theme {name}: {e}"))?;
    let mut theme = Theme {
        name: name.to_string(),
        ..Default::default()
    };
    if let Some(color) = doc.get("color") {
        let table = color
            .as_table()
            .ok_or_else(|| format!("theme {name}: [color] must be a table"))?;
        for (key, value) in table {
            let Some(token) = TOKENS.iter().find(|(t, _)| t == key) else {
                theme.notes.push(format!(
                    "theme {name}: unknown color token {key:?} ignored — try /help for the token list"
                ));
                continue;
            };
            // TOML strings for hex, integers for ANSI indexes
            let parsed = match value {
                toml::Value::String(s) => parse_color(s),
                toml::Value::Integer(i) if (0..=255).contains(i) => Some(Color::Indexed(*i as u8)),
                _ => None,
            };
            match parsed {
                Some(c) => {
                    theme.colors.insert(token.0.to_string(), c);
                }
                None => theme.notes.push(format!(
                    "theme {name}: bad value for {key:?} (use #rrggbb or an ANSI index 0-255)"
                )),
            }
        }
    }
    if let Some(symbols) = doc.get("symbols") {
        let table = symbols
            .as_table()
            .ok_or_else(|| format!("theme {name}: [symbols] must be a table"))?;
        for (key, value) in table {
            let Some(text) = value.as_str() else {
                theme
                    .notes
                    .push(format!("theme {name}: symbols.{key} must be a string"));
                continue;
            };
            if !icons::SYMBOL_KEYS.contains(&key.as_str()) {
                theme.notes.push(format!(
                    "theme {name}: unknown symbol {key:?} ignored (known: {})",
                    icons::SYMBOL_KEYS.join(", ")
                ));
                continue;
            }
            theme.symbols.insert(key.to_string(), text.to_string());
        }
    }
    for key in ["color", "symbols"] {
        if let Some(other) = doc.get(key) {
            if other.as_table().is_none() {
                return Err(format!("theme {name}: [{key}] must be a table"));
            }
        }
    }
    let extra: Vec<&String> = doc
        .keys()
        .filter(|k| *k != "color" && *k != "symbols")
        .collect();
    if !extra.is_empty() {
        theme.notes.push(format!(
            "theme {name}: unknown table(s) {} ignored (expected [color] and [symbols])",
            extra
                .iter()
                .map(|s| format!("[{s}]"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(theme)
}

/// Theme search paths: `.ka/themes` under the project root (only when
/// the project is trusted) and `~/.config/ka/themes`.
pub fn theme_dirs(cwd: &Path, project_trusted: bool) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if project_trusted {
        dirs.push(ka_engine::project_root(cwd).join(".ka/themes"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let mut dir = PathBuf::from(home);
        dir.extend([".config", "ka", "themes"]);
        dirs.push(dir);
    }
    dirs
}

/// Theme names available to this session (project layer first, then
/// user), sorted and de-duplicated.
pub fn available(cwd: &Path, project_trusted: bool) -> Vec<String> {
    let mut names = Vec::new();
    for dir in theme_dirs(cwd, project_trusted) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "toml") {
                continue;
            }
            let name = path.file_stem().unwrap_or_default().to_string_lossy();
            if !name.is_empty() && !names.iter().any(|n| *n == name) {
                names.push(name.into_owned());
            }
        }
    }
    names.sort();
    names
}

/// Resolve + parse one theme by name: project layer shadows user.
/// `Err` when the name exists in neither layer or the TOML is broken.
pub fn load(name: &str, cwd: &Path, project_trusted: bool) -> Result<Theme, String> {
    for dir in theme_dirs(cwd, project_trusted) {
        let path = dir.join(format!("{name}.toml"));
        if let Ok(text) = std::fs::read_to_string(&path) {
            return from_toml(name, &text);
        }
    }
    Err(format!(
        "theme {name:?} not found — look in .ka/themes/ (trusted projects) and ~/.config/ka/themes/"
    ))
}

// ── runtime recolor ───────────────────────────────────────────────

/// default Nocturne color → themed color. `None` = the built-in
/// palette (no mapping work per cell).
static TINT: RwLock<Option<Arc<HashMap<Color, Color>>>> = RwLock::new(None);

/// The live default→override map the render backend consults. `None`
/// when Nocturne is active (every color passes through untouched).
pub fn tint() -> Option<Arc<HashMap<Color, Color>>> {
    TINT.read().ok().and_then(|g| g.clone())
}

/// Activate a theme (live): rebuild the tint map from the token
/// overrides and push the glyph overrides into the icon layer. `None`
/// restores the Nocturne defaults.
pub fn apply(theme: Option<&Theme>) {
    let map = theme.map(|t| {
        let mut map: HashMap<Color, Color> = HashMap::with_capacity(TOKENS.len());
        // TOKENS order fixes alias resolution: later tokens win
        for (token, default) in TOKENS {
            if let Some(over) = t.colors.get(*token) {
                map.insert(*default, *over);
            }
        }
        map
    });
    if let Ok(mut g) = TINT.write() {
        *g = map.map(Arc::new);
    }
    // glyph overrides ride along (icons read through the accessor fns)
    icons::set_symbols(theme.map(|t| &t.symbols));
}

/// Whether the project layer is trusted at `cwd` — gates both
/// `.ka/themes/` and the project `.ka/ka.toml`.
pub fn surface_trusted(cwd: &Path) -> bool {
    ka_engine::trust::project_trusted(&ka_engine::project_root(cwd))
}

/// The layered `[tui]`/`[keybindings]` config the surface applies at
/// startup: defaults < user `~/.config/ka/ka.toml` < project
/// `.ka/ka.toml` (only when the project is trusted). The main binary
/// owns the richer chain (profiles, `--config`, env layer); the TUI
/// only ever needs these two files.
pub fn surface_config(cwd: &Path) -> Result<ka_engine::Config, String> {
    let project = surface_trusted(cwd);
    let user = ka_engine::config::user_config_path();
    let project_path = ka_engine::project_root(cwd).join(".ka/ka.toml");
    let mut cfg = ka_engine::Config::default();
    for path in [Some(user), project.then_some(project_path)]
        .into_iter()
        .flatten()
    {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let layer = ka_engine::Config::parse_layer(&text, &path.display().to_string())
            .map_err(|e| e.to_string())?;
        cfg.overlay(layer);
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn parse_color_reads_hex_and_index() {
        assert_eq!(parse_color("#ff9e64"), Some(crate::palette::USER));
        assert_eq!(parse_color("ff9e64"), Some(crate::palette::USER));
        assert_eq!(parse_color("#f91"), parse_color("#ff9911"));
        assert_eq!(parse_color("9"), Some(Color::Indexed(9)));
        assert_eq!(parse_color("256"), None, "ANSI indexes stop at 255");
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(parse_color("nope"), None);
    }

    #[test]
    fn from_toml_overrides_notes_and_defaults() {
        let theme = from_toml(
            "dusk",
            "[color]\naccent = \"#123456\"\nbogus = \"#ffffff\"\n\n[symbols]\nprompt = \">\"\nnope = \"x\"\n",
        )
        .expect("parses");
        assert_eq!(
            theme.colors.get("accent"),
            Some(&Color::Rgb(0x12, 0x34, 0x56))
        );
        assert!(
            !theme.colors.contains_key("bg"),
            "missing tokens stay Nocturne"
        );
        assert_eq!(theme.symbols.get("prompt").map(String::as_str), Some(">"));
        assert_eq!(
            theme.notes.len(),
            2,
            "unknown token + unknown symbol: {:?}",
            theme.notes
        );
        assert!(theme.notes[0].contains("bogus"));
        assert!(theme.notes[1].contains("nope"));
    }

    #[test]
    fn from_toml_rejects_broken_files() {
        assert!(from_toml("x", "color = 3").is_err());
        // named colors are unsupported: a note, and the token stays
        // Nocturne — the rest of the file still applies
        let t = from_toml("x", "[color]\naccent = \"lime\"\nbg = 4\n").expect("parses");
        assert!(
            t.notes.len() == 1 && t.notes[0].contains("accent"),
            "{:?}",
            t.notes
        );
        assert_eq!(t.colors.get("bg"), Some(&Color::Indexed(4)));
        assert!(from_toml("x", "color = 3").is_err());
        assert!(from_toml("x", "color = 3\n").is_err());
        // out-of-range and unparsable VALUES are notes, not failures —
        // the rest of the theme still applies
        let t = from_toml("x", "[color]\naccent = 300\nok = \"#010203\"\n").expect("parses");
        assert!(
            t.notes.len() == 1 && t.notes[0].contains("accent"),
            "{:?}",
            t.notes
        );
        assert_eq!(t.colors.get("ok"), Some(&Color::Rgb(1, 2, 3)));
    }

    #[test]
    fn token_names_cover_the_palette_aliases() {
        // every constant a renderer can reach through palette:: must be
        // settable, or a theme silently keeps Nocturne there
        let names: Vec<_> = TOKENS.iter().map(|(t, _)| *t).collect();
        for token in [
            "bg",
            "fg",
            "accent",
            "strong",
            "sel_bg",
            "code_block",
            "syntax_string",
        ] {
            assert!(names.contains(&token), "missing theme token {token:?}");
        }
        // aliases are distinct entries so both spellings work
        assert!(names.contains(&"heading") && names.contains(&"sel_fg"));
    }

    #[test]
    fn apply_maps_defaults_and_none_restores() {
        let theme = from_toml("t", "[color]\naccent = \"#010203\"\n").expect("parses");
        apply(Some(&theme));
        let map = tint().expect("theme active");
        assert_eq!(map.get(&crate::palette::ACCENT), Some(&Color::Rgb(1, 2, 3)));
        assert!(
            !map.contains_key(&crate::palette::BG),
            "unset tokens unmapped"
        );
        apply(None);
        assert!(tint().is_none());
    }
}
