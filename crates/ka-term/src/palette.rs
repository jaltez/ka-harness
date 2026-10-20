//! TUI palette — "Nocturne": a neutral warm-graphite ground (no blue
//! cast) under a vivid accent wheel. The ground carries almost no
//! chroma, so saturated accents glow against it; each *role* owns its
//! own hue, spread around the wheel, so neighbors pop apart instead of
//! melting into one wash:
//!
//! | role | hue |
//! |---|---|
//! | ka's voice: headings, bullets, key caps, selection | violet ([`ACCENT`]) |
//! | the human: user rail, composer prompt, inline code | orange ([`USER`]) |
//! | tools: names, glyphs, running state | cyan ([`TOOL`]) |
//! | thinking | magenta-pink ([`THINK`]) |
//! | success, strings | lime ([`OK`]) |
//! | warnings | yellow ([`WARN`]) |
//! | errors | red ([`ERR`]) |
//! | links | azure ([`LINK`]) |
//!
//! Emphasis is brightness, not color: bold prose and selected rows go
//! near-white ([`STRONG`]). Structure comes from rails, rules, and type
//! weight; only genuinely different surfaces (user turns, code, tool
//! output, overlays) lift off the one canvas. The brand gradient
//! (violet → pink → orange) is reserved for identity and motion.
//!
//! Contrast tiers (WCAG relative-luminance ratios, pinned by the tests
//! below): every text tier clears 4.5:1 on every surface it sits on;
//! syntax roles clear 4.5:1 on code panels. The app paints its own
//! canvas, so it reads identically on any terminal theme.
use ka_protocol::Mode;
use ratatui::style::{Color, Modifier, Style};

// ── surfaces: one neutral ground, a few deliberate lifts ──────────
pub const BG: Color = Color::Rgb(23, 22, 21); // #171615 canvas
/// Chrome: header bar, status line, composer fill (recedes below the
/// canvas so content owns the light).
pub const BG_PANEL: Color = Color::Rgb(17, 16, 16); // #111010
/// Overlays: modals, popups, pickers — one step above the canvas so the
/// drop shadow beneath them reads as depth.
pub const BG_SURFACE: Color = Color::Rgb(32, 31, 29); // #201F1D
/// Fenced code panels and inline-code chips.
pub const BG_CODE: Color = Color::Rgb(29, 28, 27); // #1D1C1B
/// User turns: the one warm stratum, an ember band under the orange rail.
pub const BG_USER: Color = Color::Rgb(38, 30, 25); // #261E19
/// Expanded tool output and the live tool preview.
pub const BG_TOOL: Color = Color::Rgb(28, 27, 29); // #1C1B1D
/// Failed tool output: red breathing through the graphite.
pub const BG_ERR_TINT: Color = Color::Rgb(45, 26, 27); // #2D1A1B
/// Drag-selection band.
pub const BG_SELECT: Color = Color::Rgb(42, 40, 46); // #2A282E
/// Hover lift: the block under the pointer.
pub const BG_HOVER: Color = Color::Rgb(41, 40, 44); // #29282C
/// Drop shadow cast by overlays (one column right, one row down).
pub const SHADOW: Color = Color::Rgb(7, 6, 6); // #070606

// ── text ramp: warm white → stone ─────────────────────────────────
pub const FG: Color = Color::Rgb(233, 230, 225); // #E9E6E1 primary prose
/// Emphasis by brightness: bold prose, selected rows, hover glow.
pub const STRONG: Color = Color::Rgb(253, 252, 250); // #FDFCFA
/// Secondary card text (tool output).
pub const INK_DIM: Color = Color::Rgb(201, 197, 191); // #C9C5BF
pub const META: Color = Color::Rgb(170, 165, 159); // #AAA59F chrome text
pub const FAINT: Color = Color::Rgb(150, 145, 139); // #96918B quietest text tier
/// Dark ink for text on bright chips (mode pill).
pub const CHIP_FG: Color = Color::Rgb(20, 19, 18); // #141312

// ── lines: borders, rails, rules (decoration, never text) ─────────
/// Composer + overlay frames at rest.
pub const BORDER: Color = Color::Rgb(60, 57, 54); // #3C3936
/// Focused frames: a composer with a draft, an open overlay.
pub const BORDER_FOCUS: Color = Color::Rgb(128, 102, 184); // #8066B8
/// The quietest line: scrollbar track, separators.
pub const BORDER_QUIET: Color = Color::Rgb(38, 37, 36); // #262524
/// Rails and rules inside the transcript: tool threads, turn rules,
/// thinking rails.
pub const RULE: Color = Color::Rgb(54, 51, 48); // #363330

// ── the accent wheel (vivid, matched brightness) ──────────────────
/// ka's voice: headings, bullets, key caps, selection pointer, spinner.
pub const ACCENT: Color = Color::Rgb(189, 155, 255); // #BD9BFF violet
/// The human: user rail, composer prompt, inline code.
pub const USER: Color = Color::Rgb(255, 158, 100); // #FF9E64 orange
pub const TOOL: Color = Color::Rgb(95, 215, 240); // #5FD7F0 cyan
/// Thinking head label.
pub const THINK: Color = Color::Rgb(246, 142, 224); // #F68EE0 magenta-pink
pub const OK: Color = Color::Rgb(163, 226, 107); // #A3E26B lime
pub const WARN: Color = Color::Rgb(255, 216, 102); // #FFD866 yellow
pub const ERR: Color = Color::Rgb(255, 107, 120); // #FF6B78 red
pub const LINK: Color = Color::Rgb(108, 182, 255); // #6CB6FF azure
/// Selection: a lifted violet-graphite row with bright ink (the
/// pointer rail `▌` in [`ACCENT`] marks it — see `tui::selection_style`).
pub const SEL_BG: Color = Color::Rgb(43, 38, 54); // #2B2636
pub const SEL_FG: Color = STRONG;
/// Brand gradient stops: identity + motion only (logo, comet).
pub const BRAND: [Color; 3] = [ACCENT, THINK, USER];

/// Per-mode identity color: the status-line pill. Each mode owns a
/// distinct hue; free access wears peach-orange — it is the one mode
/// that runs without asking.
pub fn mode_color(mode: Mode) -> Color {
    match mode {
        Mode::Guarded => OK,
        Mode::AcceptEdits => TOOL,
        Mode::Free => USER,
        Mode::Plan => ACCENT,
    }
}

// markdown roles
pub const HEADING: Color = ACCENT;
pub const CODE_INLINE: Color = USER;
pub const CODE_BLOCK: Color = Color::Rgb(211, 216, 239); // #D3D8EF fenced code base
pub const SYNTAX_COMMENT: Color = FAINT;
pub const SYNTAX_KEYWORD: Color = ACCENT;
pub const SYNTAX_STRING: Color = OK;
pub const SYNTAX_NUMBER: Color = USER;

// ── derived styles ────────────────────────────────────────────────
pub const META_STYLE: Style = Style::new().fg(META);
pub const BORDER_QUIET_STYLE: Style = Style::new().fg(BORDER_QUIET);
pub const RULE_STYLE: Style = Style::new().fg(RULE);
pub const ACCENT_STYLE: Style = Style::new().fg(ACCENT);
pub const ACCENT_BOLD: Style = Style::new().fg(ACCENT).add_modifier(Modifier::BOLD);
/// Thinking prose: the quiet tier, italic.
pub const THOUGHT: Style = Style::new().fg(FAINT).add_modifier(Modifier::ITALIC);
pub const QUOTE: Style = Style::new().fg(META).add_modifier(Modifier::ITALIC);
/// Canvas fill, painted over the whole frame before layout.
pub const CANVAS: Style = Style::new().fg(FG).bg(BG);

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG 2.x relative-luminance contrast.
    fn contrast(fg: Color, bg: Color) -> f64 {
        fn chan(v: u8) -> f64 {
            let x = f64::from(v) / 255.0;
            if x <= 0.039_28 {
                x / 12.92
            } else {
                ((x + 0.055) / 1.055).powf(2.4)
            }
        }
        fn rel(c: Color) -> f64 {
            let Color::Rgb(r, g, b) = c else {
                panic!("palette colors are Rgb");
            };
            0.2126 * chan(r) + 0.7152 * chan(g) + 0.0722 * chan(b)
        }
        let (a, b) = (rel(fg), rel(bg));
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }

    const SURFACES: [Color; 10] = [
        BG,
        BG_PANEL,
        BG_SURFACE,
        BG_CODE,
        BG_USER,
        BG_TOOL,
        BG_ERR_TINT,
        BG_SELECT,
        BG_HOVER,
        SEL_BG,
    ];

    #[test]
    fn text_tiers_clear_contrast_on_every_surface() {
        for bg in SURFACES {
            for (name, fg, min) in [
                ("FG", FG, 10.0),
                ("STRONG", STRONG, 12.0),
                ("INK_DIM", INK_DIM, 7.0),
                ("META", META, 5.5),
                ("FAINT", FAINT, 4.5),
                ("ACCENT", ACCENT, 6.0),
                ("USER", USER, 6.0),
                ("TOOL", TOOL, 6.0),
                ("THINK", THINK, 6.0),
                ("OK", OK, 6.0),
                ("WARN", WARN, 6.0),
                ("ERR", ERR, 5.0),
                ("LINK", LINK, 6.0),
            ] {
                let c = contrast(fg, bg);
                assert!(c >= min, "{name} on {bg:?}: {c:.2} < {min}");
            }
        }
    }

    #[test]
    fn syntax_roles_clear_contrast_on_code_panels() {
        for fg in [
            CODE_BLOCK,
            SYNTAX_COMMENT,
            SYNTAX_KEYWORD,
            SYNTAX_STRING,
            SYNTAX_NUMBER,
            CODE_INLINE,
        ] {
            let c = contrast(fg, BG_CODE);
            assert!(c >= 4.5, "{fg:?} on code: {c:.2}");
        }
    }

    #[test]
    fn chip_ink_reads_on_every_mode_color() {
        for mode in [Mode::Guarded, Mode::AcceptEdits, Mode::Free, Mode::Plan] {
            let c = contrast(CHIP_FG, mode_color(mode));
            assert!(c >= 6.0, "{mode:?} chip: {c:.2}");
        }
        assert!(contrast(CHIP_FG, ACCENT) >= 6.0);
    }

    #[test]
    fn overlays_lift_and_chrome_recedes() {
        // overlays lift above the canvas (so the shadow under them
        // reads as depth); chrome recedes below it
        assert!(contrast(BG_SURFACE, SHADOW) > contrast(BG, SHADOW));
        assert!(contrast(BG_PANEL, SHADOW) < contrast(BG, SHADOW));
    }

    /// The roles that sit side by side in the transcript and chrome
    /// must read as different colors at a glance, not one wash: every
    /// pair keeps a clear RGB distance.
    #[test]
    fn role_hues_stay_apart() {
        fn rgb(c: Color) -> (f64, f64, f64) {
            let Color::Rgb(r, g, b) = c else {
                panic!("palette colors are Rgb");
            };
            (f64::from(r), f64::from(g), f64::from(b))
        }
        let roles = [
            ("ACCENT", ACCENT),
            ("USER", USER),
            ("TOOL", TOOL),
            ("THINK", THINK),
            ("OK", OK),
            ("WARN", WARN),
            ("ERR", ERR),
        ];
        for (i, (an, a)) in roles.iter().enumerate() {
            for (bn, b) in &roles[i + 1..] {
                let (a, b) = (rgb(*a), rgb(*b));
                let d = ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt();
                assert!(d >= 50.0, "{an} vs {bn} too close: {d:.0}");
            }
        }
    }
}
