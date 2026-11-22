//! Every glyph in one place. Nocturne speaks a monochrome icon
//! language: single-column Unicode symbols that take the palette's
//! color instead of emoji that bring their own. One column wide each
//! (so layout math never meets a double-width surprise) and drawn from
//! blocks every common monospace/fallback font covers — terminals
//! without a glyph degrade here, and only here.

use std::sync::RwLock;

use ka_protocol::Mode;

// ── theme overrides ───────────────────────────────────────────────
// Themes may re-glyph the named symbols below via their `[symbols]`
// table (see [`SYMBOL_KEYS`]); everything else — tool icons, mode
// icons, modal title glyphs — stays fixed. Accessors read through the
// override map so a `/theme` switch relabels every call site at once.

/// Symbol keys a theme's `[symbols]` table may override, in stable
/// order (also the doc list for unknown-key notes).
pub const SYMBOL_KEYS: &[&str] = &[
    "prompt",
    "rail",
    "thought",
    "branch",
    "close",
    "brand",
    "strip_todos",
    "strip_skills",
    "strip_info",
    "agents",
];

static SYMBOLS: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// Install a theme's glyph overrides (keyed by [`SYMBOL_KEYS`]).
pub fn set_symbols(overrides: Option<&std::collections::HashMap<String, String>>) {
    let Ok(mut slots) = SYMBOLS.write() else {
        return;
    };
    slots.clear();
    let Some(overrides) = overrides else { return };
    for key in SYMBOL_KEYS {
        slots.push(overrides.get(*key).cloned().unwrap_or_default());
    }
}

/// The glyph for slot `i`: the theme override when set, else the
/// Nocturne default. Owned — the override lives in the lock, the
/// default in static storage, and callers embed the text into spans.
fn sym(i: usize, default: &'static str) -> String {
    let slot = SYMBOLS.read().ok().and_then(|s| s.get(i).cloned());
    match slot {
        Some(s) if !s.is_empty() => s,
        _ => default.to_string(),
    }
}

/// ✻ thinking head + effort segment lead (theme: `thought`)
pub fn thought() -> String {
    sym(2, "\u{273b}")
}
/// ⎇ git branch (theme: `branch`)
pub fn branch() -> String {
    sym(3, "\u{2387}")
}
/// ✕ overlay close chip (theme: `close`)
pub fn close() -> String {
    sym(4, "\u{2715}")
}
/// ◆ brand mark fallback (the configurable header glyph; theme: `brand`)
pub fn brand() -> String {
    sym(5, "\u{25c6}")
}
/// ❯ prompt glyph: composer lead + user-turn lead (theme: `prompt`)
pub fn prompt() -> String {
    sym(0, "\u{276f}")
}
/// ▌ rails: user turns, the selection pointer (theme: `rail`)
pub fn rail() -> String {
    sym(1, "\u{258c}")
}

/// ≣ todos button — a stacked-line glyph whose column width is stable
/// across Unicode tables (☰ is 1 col under unicode-width 0.2.0 and 2
/// under 0.2.2+, so it silently shifted every row it rode on).
/// (theme: `strip_todos`)
pub fn strip_todos() -> String {
    sym(6, "\u{2263}")
}
/// ✦ skills button (theme: `strip_skills`)
pub fn strip_skills() -> String {
    sym(7, "\u{2726}")
}
/// ? info button — ASCII, so no terminal can disagree about its width
/// (theme: `strip_info`)
pub fn strip_info() -> String {
    sym(8, "?")
}
/// ◎ skills-popup agents section header (theme: `agents`)
pub fn agents() -> String {
    sym(9, "\u{25ce}")
}

pub fn mode_icon(mode: Mode) -> &'static str {
    match mode {
        Mode::Ask => "\u{25c8}",         // ◈
        Mode::AcceptEdits => "\u{270e}", // ✎
        Mode::Allow => "\u{21af}",       // ↯
        Mode::Readonly => "\u{25ce}",    // ◎
        Mode::DontAsk => "\u{29bc}",     // ⦼
    }
}

pub fn mode_word(mode: Mode) -> &'static str {
    match mode {
        Mode::Ask => "ask",
        Mode::AcceptEdits => "accept_edits",
        Mode::Allow => "allow",
        Mode::Readonly => "readonly",
        Mode::DontAsk => "dontask",
    }
}

/// Effort intensity battery — ordinal circle fill.
pub fn effort_glyph(level: &str) -> Option<&'static str> {
    match level {
        "off" => Some("\u{25cb}"),    // ○
        "low" => Some("\u{25d4}"),    // ◔
        "medium" => Some("\u{25d1}"), // ◑
        "high" => Some("\u{25d5}"),   // ◕
        "max" => Some("\u{25cf}"),    // ●
        _ => None,                    // unknown string: caller hides the segment
    }
}

/// Per-tool glyphs; prefix match, first hit wins.
pub fn tool_icon(tool: &str) -> &'static str {
    if tool.starts_with("mcp.") {
        "\u{2301}" // ⌁
    } else if tool.starts_with("lsp_") {
        "\u{03bb}" // λ
    } else {
        match tool {
            "bash" => "$",
            "read" => "\u{2261}",     // ≡
            "grep" => "\u{2315}",     // ⌕
            "glob" => "\u{2042}",     // ⁂
            "edit" => "\u{270e}",     // ✎
            "write" => "\u{271a}",    // ✚
            "web" => "\u{25cd}",      // ◍
            "delegate" => "\u{21c4}", // ⇄
            "todo" => "\u{2263}",     // ≣
            "remember" => "\u{2691}", // ⚑
            "tasks" => "\u{22ee}",    // ⋮
            "debug" => "\u{29bf}",    // ⦿
            "undo" => "\u{21b6}",     // ↶
            _ => "\u{25c7}",          // ◇ fallback
        }
    }
}

/// Overlay title glyphs (the model picker's title is vendor-derived —
/// no glyph).
pub const MODAL_ICONS: &[(&str, &str)] = &[
    ("sessions", "\u{25f7}"),  // ◷
    ("settings", "\u{2699}"),  // ⚙ (text presentation)
    ("providers", "\u{21cc}"), // ⇌
    ("api key", "\u{2731}"),   // ✱
    ("spills", "\u{224b}"),    // ≋
    ("prompts", "\u{276f}"),   // ❯
    ("rewind", "\u{21b6}"),    // ↶
    ("memory", "\u{25eb}"),    // ◫
    ("usage", "\u{25a6}"),     // ▦
    ("context", "\u{25f0}"),   // ◰
    ("tree", "\u{2387}"),      // ⎇
    ("todos", "\u{2263}"),     // ≣
    ("skills", "\u{2726}"),    // ✦
    ("info", "?"),
    ("help", "?"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    /// Layout math everywhere assumes one column per glyph: a
    /// double-width icon would shift every row it rides on.
    #[test]
    fn strip_icons_hold_one_column_in_every_width_table() {
        // the strip's click zones and alignment are computed from these
        // widths; an East-Asian-Ambiguous glyph (☰, ⓘ) renders 1 col in
        // one table/terminal and 2 in another, shifting the whole row
        for g in [strip_todos(), strip_skills(), strip_info()] {
            assert_eq!(UnicodeWidthStr::width(g.as_str()), 1, "strip glyph {g:?}");
            assert_eq!(
                UnicodeWidthStr::width_cjk(g.as_str()),
                1,
                "strip glyph {g:?} is ambiguous-width"
            );
        }
    }

    #[test]
    fn every_glyph_is_one_column() {
        let mut all: Vec<String> = vec![
            thought(),
            branch(),
            close(),
            brand(),
            prompt(),
            rail(),
            strip_todos(),
            strip_skills(),
            strip_info(),
            agents(),
        ];
        for m in [Mode::Ask, Mode::AcceptEdits, Mode::Allow, Mode::Readonly] {
            all.push(mode_icon(m).to_string());
        }
        for t in [
            "bash", "read", "grep", "glob", "edit", "write", "web", "delegate", "todo", "remember",
            "tasks", "debug", "undo", "mcp.x", "lsp_y", "unknown",
        ] {
            all.push(tool_icon(t).to_string());
        }
        for level in ["off", "low", "medium", "high", "max"] {
            all.extend(effort_glyph(level).map(str::to_string));
        }
        all.extend(MODAL_ICONS.iter().map(|(_, g)| (*g).to_string()));
        for g in all {
            assert_eq!(
                UnicodeWidthStr::width(g.as_str()),
                1,
                "glyph {g:?} must be one column"
            );
        }
    }

    #[test]
    fn effort_battery_fills_in_order_and_unknown_hides() {
        let levels = ["off", "low", "medium", "high", "max"];
        let glyphs: Vec<_> = levels.iter().filter_map(|l| effort_glyph(l)).collect();
        assert_eq!(glyphs.len(), levels.len());
        let mut dedup = glyphs.clone();
        dedup.dedup();
        assert_eq!(dedup.len(), glyphs.len(), "each level reads distinct");
        assert_eq!(effort_glyph("nope"), None);
    }

    #[test]
    fn tool_prefixes_route_before_the_table() {
        assert_eq!(tool_icon("mcp.github"), tool_icon("mcp.other"));
        assert_eq!(tool_icon("lsp_hover"), tool_icon("lsp_refs"));
        assert_ne!(tool_icon("mcp.bash"), tool_icon("bash"));
    }
}
