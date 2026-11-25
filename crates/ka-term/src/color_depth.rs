//! Color at the depth the terminal can show.
//!
//! The palette is 24-bit. A terminal that cannot show 24-bit color
//! approximates every color on its own, each client by a different
//! rule, and that is how the palette loses its relationships on a
//! remote or a phone client: dim text stops reading as dim, surfaces
//! stop matching, accents drift together. So the terminal is asked
//! once what it can show, and every color is fitted to the nearest one
//! it shows exactly.

use std::sync::LazyLock;

use ratatui::style::Color;

/// How many colors the terminal shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Depth {
    /// 24-bit: the palette goes out as it is.
    TrueColor,
    /// xterm's 256: the sixteen system colors, a 6 by 6 by 6 cube and
    /// a 24-step gray ramp.
    TwoFiftySix,
    /// The sixteen ANSI colors, whatever the terminal's theme makes them.
    Sixteen,
}

/// What a terminal says about itself. `COLORTERM` announces 24-bit
/// color with `truecolor` or `24bit`, and a `TERM` from the `direct`
/// family (`xterm-direct`) does the same. A `TERM` naming `256color`
/// announces 256. Anything else gets the sixteen ANSI colors.
///
/// This is crossterm's rule with two gaps closed: that one ignored a
/// `256color` TERM whenever COLORTERM was set to anything at all, and
/// did not know `direct`. It reads the environment itself, so it
/// cannot be tried against a table; this can.
pub fn depth_from(term: Option<&str>, colorterm: Option<&str>) -> Depth {
    let says = |value: Option<&str>, words: &[&str]| {
        value.is_some_and(|v| {
            let v = v.to_ascii_lowercase();
            words.iter().any(|word| v.contains(word))
        })
    };
    if says(colorterm, &["truecolor", "24bit"]) || says(term, &["direct"]) {
        Depth::TrueColor
    } else if says(term, &["256color"]) || says(colorterm, &["256"]) {
        Depth::TwoFiftySix
    } else {
        Depth::Sixteen
    }
}

/// This terminal's depth, read once.
pub fn terminal_depth() -> Depth {
    static DEPTH: LazyLock<Depth> = LazyLock::new(|| {
        depth_from(
            std::env::var("TERM").ok().as_deref(),
            std::env::var("COLORTERM").ok().as_deref(),
        )
    });
    *DEPTH
}

/// The sixteen ANSI colors in xterm's own theme, in index order.
const ANSI_XTERM: [[u8; 3]; 16] = [
    [0, 0, 0],
    [205, 0, 0],
    [0, 205, 0],
    [205, 205, 0],
    [0, 0, 238],
    [205, 0, 205],
    [0, 205, 205],
    [229, 229, 229],
    [127, 127, 127],
    [255, 0, 0],
    [0, 255, 0],
    [255, 255, 0],
    [92, 92, 255],
    [255, 0, 255],
    [0, 255, 255],
    [255, 255, 255],
];

/// The same sixteen by index, in the classic escape form every
/// terminal with sixteen colors reads.
const ANSI: [Color; 16] = [
    Color::Black,
    Color::Red,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Magenta,
    Color::Cyan,
    Color::Gray,
    Color::DarkGray,
    Color::LightRed,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightBlue,
    Color::LightMagenta,
    Color::LightCyan,
    Color::White,
];

/// xterm's 256 colors: the sixteen above, a cube on six levels, a
/// gray ramp.
const fn xterm_256() -> [[u8; 3]; 256] {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let mut out = [[0u8; 3]; 256];
    let mut i = 0;
    while i < 256 {
        out[i] = if i < 16 {
            ANSI_XTERM[i]
        } else if i < 232 {
            let c = i - 16;
            [LEVELS[c / 36], LEVELS[(c / 6) % 6], LEVELS[c % 6]]
        } else {
            let g = (8 + (i - 232) * 10) as u8;
            [g, g, g]
        };
        i += 1;
    }
    out
}

static XTERM_256: [[u8; 3]; 256] = xterm_256();

/// Below this lightness a color is a surface or a line, not a color
/// anyone reads: at sixteen colors it becomes the terminal's black.
/// Every surface and plain border in the palette sits under 0.35 and
/// the focus border, the darkest color that must stay one, at 0.57.
const DARK: f32 = 0.40;

/// A color in OKLab, a space where the distance between two colors
/// tracks how different they look, so that "the nearest color" means
/// what it should.
#[derive(Clone, Copy)]
struct Lab {
    l: f32,
    a: f32,
    b: f32,
}

impl Lab {
    fn of([r, g, b]: [u8; 3]) -> Self {
        fn linear(byte: u8) -> f32 {
            let c = f32::from(byte) / 255.0;
            if c <= 0.040_45 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        let (r, g, b) = (linear(r), linear(g), linear(b));
        let l = (0.412_221_47 * r + 0.536_332_54 * g + 0.051_445_993 * b).cbrt();
        let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
        let s = (0.088_302_46 * r + 0.281_718_84 * g + 0.629_978_7 * b).cbrt();
        Self {
            l: 0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
            a: 1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
            b: 0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
        }
    }

    fn distance_to(self, other: Self) -> f32 {
        (self.l - other.l).powi(2) + (self.a - other.a).powi(2) + (self.b - other.b).powi(2)
    }
}

static LABS_256: LazyLock<[Lab; 256]> = LazyLock::new(|| XTERM_256.map(Lab::of));

/// The index in [`XTERM_256`] nearest `rgb`, among `pool`.
fn nearest(pool: &[Lab], rgb: [u8; 3]) -> usize {
    let query = Lab::of(rgb);
    pool.iter()
        .enumerate()
        .map(|(index, candidate)| (query.distance_to(*candidate), index))
        .min_by(|x, y| x.0.total_cmp(&y.0))
        .map_or(0, |(_, index)| index)
}

impl Depth {
    /// The nearest color this depth shows exactly. A color already at
    /// or below this depth is left alone: reset, an ANSI color, a
    /// palette index this depth has.
    pub fn fit(self, color: Color) -> Color {
        match self.index(color) {
            Some(index) if index < 16 => ANSI[index],
            Some(index) => Color::Indexed(index as u8),
            None => color,
        }
    }

    /// The palette index `color` becomes at this depth; `None` when it
    /// goes out as it is.
    fn index(self, color: Color) -> Option<usize> {
        if self == Self::TrueColor {
            return None;
        }
        let rgb = match color {
            Color::Rgb(r, g, b) => [r, g, b],
            // reset and the sixteen ANSI colors are already at every
            // depth, and so is a palette index this depth has
            Color::Indexed(n) if self == Self::TwoFiftySix || n < 16 => return None,
            Color::Indexed(n) => XTERM_256[usize::from(n)],
            _ => return None,
        };
        Some(match self {
            Self::Sixteen if Lab::of(rgb).l < DARK => 0,
            Self::Sixteen => nearest(&LABS_256[..16], rgb),
            _ => nearest(&LABS_256[..], rgb),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Relative luminance, for the contrast between a text and its surface.
    fn luminance(rgb: [u8; 3]) -> f32 {
        fn linear(byte: u8) -> f32 {
            let c = f32::from(byte) / 255.0;
            if c <= 0.040_45 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        0.212_6 * linear(rgb[0]) + 0.715_2 * linear(rgb[1]) + 0.072_2 * linear(rgb[2])
    }

    fn contrast(text: [u8; 3], surface: [u8; 3]) -> f32 {
        let (a, b) = (luminance(text) + 0.05, luminance(surface) + 0.05);
        a.max(b) / a.min(b)
    }

    fn shown(depth: Depth, rgb: [u8; 3]) -> [u8; 3] {
        match depth.index(Color::Rgb(rgb[0], rgb[1], rgb[2])) {
            Some(index) => XTERM_256[index],
            None => rgb,
        }
    }

    const ACCENTS: [[u8; 3]; 8] = [
        [189, 155, 255],
        [255, 158, 100],
        [95, 215, 240],
        [246, 142, 224],
        [163, 226, 107],
        [255, 216, 102],
        [255, 107, 120],
        [108, 182, 255],
    ];

    #[test]
    fn depth_is_what_the_terminal_announces() {
        // the usual desktop
        assert_eq!(depth_from(None, Some("truecolor")), Depth::TrueColor);
        assert_eq!(
            depth_from(Some("xterm-256color"), Some("24bit")),
            Depth::TrueColor
        );
        assert_eq!(depth_from(Some("xterm-direct"), None), Depth::TrueColor);
        // 256 by either name, also beside a COLORTERM that says nothing useful
        assert_eq!(depth_from(Some("xterm-256color"), None), Depth::TwoFiftySix);
        assert_eq!(
            depth_from(Some("screen-256color"), Some("1")),
            Depth::TwoFiftySix
        );
        // a client that announces nothing gets the sixteen ANSI colors
        assert_eq!(depth_from(Some("xterm"), None), Depth::Sixteen);
        assert_eq!(depth_from(Some("screen"), None), Depth::Sixteen);
        assert_eq!(depth_from(Some("linux"), None), Depth::Sixteen);
        assert_eq!(depth_from(None, None), Depth::Sixteen);
    }

    #[test]
    fn true_color_changes_nothing() {
        for color in [
            Color::Rgb(1, 2, 3),
            Color::Indexed(203),
            Color::Reset,
            Color::DarkGray,
        ] {
            assert_eq!(Depth::TrueColor.fit(color), color);
        }
    }

    #[test]
    fn at_256_no_24_bit_color_leaves_and_accents_stay_apart() {
        // the known answers, so a change in the math shows up as a number
        assert_eq!(
            Depth::TwoFiftySix.fit(Color::Rgb(189, 155, 255)),
            Color::Indexed(147)
        );
        assert_eq!(
            Depth::TwoFiftySix.fit(Color::Rgb(255, 107, 120)),
            Color::Indexed(203)
        );
        assert_eq!(
            Depth::TwoFiftySix.fit(Color::Rgb(23, 22, 21)),
            Color::Indexed(233)
        );
        // every accent keeps its own entry
        let targets: Vec<Color> = ACCENTS
            .iter()
            .map(|c| Depth::TwoFiftySix.fit(Color::Rgb(c[0], c[1], c[2])))
            .collect();
        for (i, a) in targets.iter().enumerate() {
            for b in &targets[i + 1..] {
                assert_ne!(a, b, "two accents share {a:?}");
            }
        }
    }

    #[test]
    fn at_256_text_stays_as_readable_on_its_surface() {
        // (text, surface) pairs the transcript, composer and panels draw
        for (text, surface) in [
            ([233, 230, 225], [23, 22, 21]),
            ([233, 230, 225], [50, 31, 29]),
            ([233, 230, 225], [56, 30, 25]),
            ([170, 165, 159], [50, 31, 29]),
            ([150, 145, 139], [23, 22, 21]),
            ([253, 252, 250], [43, 38, 54]),
            ([20, 19, 18], [255, 216, 102]),
            ([255, 107, 120], [69, 26, 27]),
        ] {
            let before = contrast(text, surface);
            let after = contrast(
                shown(Depth::TwoFiftySix, text),
                shown(Depth::TwoFiftySix, surface),
            );
            // readable, and close to what it was: the widest loss is the
            // error text on the error panel, 5.38 to 4.75, because the
            // palette has no entry near that dark red tint
            assert!(
                after >= 4.5 && after >= before * 0.85,
                "{text:?} on {surface:?}: {before:.2} before, {after:.2} after"
            );
        }
    }

    #[test]
    fn at_sixteen_tinted_surfaces_are_black_and_accents_stay_colors() {
        // the selection and hover fills and the error tint, which would land on pure red
        for surface in [
            [66u8, 40, 46],
            [65, 40, 44],
            [69, 26, 27],
            [50, 31, 29],
            [23, 22, 21],
        ] {
            assert_eq!(
                Depth::Sixteen.fit(Color::Rgb(surface[0], surface[1], surface[2])),
                Color::Black,
                "{surface:?}"
            );
        }
        assert_eq!(Depth::Sixteen.fit(Color::Rgb(233, 230, 225)), Color::Gray);
        // text on every one of those surfaces is still readable
        for surface in [[66u8, 40, 46], [50, 31, 29], [23, 22, 21]] {
            assert!(
                contrast(
                    shown(Depth::Sixteen, [233, 230, 225]),
                    shown(Depth::Sixteen, surface)
                ) >= 4.5
            );
        }
        // an accent is never black, white or either gray
        for accent in ACCENTS {
            let fitted = Depth::Sixteen.fit(Color::Rgb(accent[0], accent[1], accent[2]));
            assert!(
                !matches!(
                    fitted,
                    Color::Black | Color::White | Color::Gray | Color::DarkGray | Color::Reset
                ),
                "{accent:?} became {fitted:?}"
            );
        }
    }

    #[test]
    fn a_color_already_at_the_depth_passes_and_one_above_it_is_fitted() {
        // an index a 256-color terminal has
        assert_eq!(
            Depth::TwoFiftySix.fit(Color::Indexed(203)),
            Color::Indexed(203)
        );
        // an index a sixteen-color terminal has, and one it does not (a theme may name either)
        assert_eq!(Depth::Sixteen.fit(Color::Indexed(3)), Color::Indexed(3));
        assert_eq!(Depth::Sixteen.fit(Color::Indexed(203)), Color::LightRed);
        assert_eq!(Depth::Sixteen.fit(Color::Rgb(255, 95, 95)), Color::LightRed);
        for depth in [Depth::TwoFiftySix, Depth::Sixteen] {
            assert_eq!(depth.fit(Color::Reset), Color::Reset);
            assert_eq!(depth.fit(Color::Cyan), Color::Cyan);
        }
    }
}
