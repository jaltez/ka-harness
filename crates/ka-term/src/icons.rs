//! Every glyph in one place. Nocturne speaks a monochrome icon
//! language: single-column Unicode symbols that take the palette's
//! color instead of emoji that bring their own. One column wide each
//! (so layout math never meets a double-width surprise) and drawn from
//! blocks every common monospace/fallback font covers — terminals
//! without a glyph degrade here, and only here.

use ka_protocol::Mode;

/// ✻ thinking head + effort segment lead
pub const THOUGHT: &str = "\u{273b}";
/// ⎇ git branch
pub const BRANCH: &str = "\u{2387}";
/// ✕ overlay close chip
pub const CLOSE: &str = "\u{2715}";
/// ◆ brand mark fallback (the configurable header glyph)
pub const BRAND: &str = "\u{25c6}";
/// ❯ prompt glyph: composer lead + user-turn lead
pub const PROMPT: &str = "\u{276f}";
/// ▌ rails: user turns, the selection pointer
pub const RAIL: &str = "\u{258c}";

/// ≣ todos button — a stacked-line glyph whose column width is stable
/// across Unicode tables (☰ is 1 col under unicode-width 0.2.0 and 2
/// under 0.2.2+, so it silently shifted every row it rode on).
pub const STRIP_TODOS: &str = "\u{2263}";
/// ✦ skills button
pub const STRIP_SKILLS: &str = "\u{2726}";
/// ? info button — ASCII, so no terminal can disagree about its width
pub const STRIP_INFO: &str = "?";

pub fn mode_icon(mode: Mode) -> &'static str {
    match mode {
        Mode::Guarded => "\u{25c8}",     // ◈
        Mode::AcceptEdits => "\u{270e}", // ✎
        Mode::Free => "\u{21af}",        // ↯
        Mode::Plan => "\u{25ce}",        // ◎
    }
}

pub fn mode_word(mode: Mode) -> &'static str {
    match mode {
        Mode::Guarded => "guarded",
        Mode::AcceptEdits => "accept edits",
        Mode::Free => "full access",
        Mode::Plan => "plan",
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

/// ◎ skills-popup agents section header
pub const AGENTS: &str = "\u{25ce}";

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
        for g in [STRIP_TODOS, STRIP_SKILLS, STRIP_INFO] {
            assert_eq!(UnicodeWidthStr::width(g), 1, "strip glyph {g:?}");
            assert_eq!(
                UnicodeWidthStr::width_cjk(g),
                1,
                "strip glyph {g:?} is ambiguous-width"
            );
        }
    }

    #[test]
    fn every_glyph_is_one_column() {
        let mut all: Vec<&str> = vec![
            THOUGHT,
            BRANCH,
            CLOSE,
            BRAND,
            PROMPT,
            RAIL,
            STRIP_TODOS,
            STRIP_SKILLS,
            STRIP_INFO,
            AGENTS,
        ];
        for m in [Mode::Guarded, Mode::AcceptEdits, Mode::Free, Mode::Plan] {
            all.push(mode_icon(m));
        }
        for t in [
            "bash", "read", "grep", "glob", "edit", "write", "web", "delegate", "todo", "remember",
            "tasks", "debug", "undo", "mcp.x", "lsp_y", "unknown",
        ] {
            all.push(tool_icon(t));
        }
        for level in ["off", "low", "medium", "high", "max"] {
            all.extend(effort_glyph(level));
        }
        all.extend(MODAL_ICONS.iter().map(|(_, g)| *g));
        for g in all {
            assert_eq!(
                UnicodeWidthStr::width(g),
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
