//! Color + motion effects: pure functions of `(text, clock)` → styled
//! spans. Nothing here owns a timer — animation only advances while the
//! event loop is already ticking (a turn in flight, a toast alive), so
//! an idle ka burns zero CPU. Truecolor (`Color::Rgb`) in, truecolor
//! out; any other color passes through unblended.
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;

/// Linear blend of two Rgb colors at `t` ∈ [0, 1] (clamped). Non-Rgb
/// inputs snap to the nearer endpoint.
pub fn lerp(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    match (a, b) {
        (Color::Rgb(r0, g0, b0), Color::Rgb(r1, g1, b1)) => {
            let mix =
                |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
            Color::Rgb(mix(r0, r1), mix(g0, g1), mix(b0, b1))
        }
        _ if t < 0.5 => a,
        _ => b,
    }
}

/// Sample a multi-stop ramp at `t` ∈ [0, 1]: stops are evenly spaced.
/// An empty ramp yields `Color::Reset`; one stop is a flat color.
pub fn ramp(stops: &[Color], t: f32) -> Color {
    match stops {
        [] => Color::Reset,
        [only] => *only,
        _ => {
            let t = t.clamp(0.0, 1.0) * (stops.len() - 1) as f32;
            let i = (t.floor() as usize).min(stops.len() - 2);
            lerp(stops[i], stops[i + 1], t - i as f32)
        }
    }
}

/// `text` painted across a ramp, one color step per display column
/// (wide glyphs take the color of their left column). `style` supplies
/// everything but the foreground (bg, modifiers).
pub fn gradient(text: &str, stops: &[Color], style: Style) -> Vec<Span<'static>> {
    let total: usize = text.chars().map(|c| c.width().unwrap_or(0)).sum();
    let span = total.saturating_sub(1).max(1) as f32;
    let mut col = 0usize;
    text.chars()
        .map(|c| {
            let fg = ramp(stops, col as f32 / span);
            col += c.width().unwrap_or(0);
            Span::styled(c.to_string(), style.fg(fg))
        })
        .collect()
}

/// Intensity (0..=1) of a highlight band of half-width `radius` columns
/// sweeping left→right across `len` columns, one column per `step_ms`.
/// The band starts and ends fully off-stage (a pause between passes),
/// so the sweep reads as light passing over the text, not a blink.
pub fn sweep(col: usize, len: usize, ms: u128, step_ms: u128, radius: f32) -> f32 {
    let lead = radius.ceil() as usize * 2;
    let cycle = (len + lead * 2).max(1) as u128;
    let head = ((ms / step_ms.max(1)) % cycle) as f32 - lead as f32;
    let d = (col as f32 - head).abs();
    (1.0 - d / radius.max(0.5)).clamp(0.0, 1.0)
}

/// Shimmer: `text` in `base` with a `hi` light band sweeping across it —
/// the live "working" label. Pure function of the clock.
pub fn shimmer(text: &str, base: Color, hi: Color, ms: u128, style: Style) -> Vec<Span<'static>> {
    let len: usize = text.chars().map(|c| c.width().unwrap_or(0)).sum();
    let mut col = 0usize;
    text.chars()
        .map(|c| {
            let fg = lerp(base, hi, sweep(col, len, ms, 55, 3.0));
            col += c.width().unwrap_or(0);
            Span::styled(c.to_string(), style.fg(fg))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Color = Color::Rgb(0, 0, 0);
    const B: Color = Color::Rgb(200, 100, 50);

    #[test]
    fn lerp_hits_endpoints_midpoint_and_clamps() {
        assert_eq!(lerp(A, B, 0.0), A);
        assert_eq!(lerp(A, B, 1.0), B);
        assert_eq!(lerp(A, B, 0.5), Color::Rgb(100, 50, 25));
        assert_eq!(lerp(A, B, -3.0), A);
        assert_eq!(lerp(A, B, 9.0), B);
        // non-Rgb never panics: snaps to the nearer end
        assert_eq!(lerp(Color::Red, B, 0.2), Color::Red);
        assert_eq!(lerp(Color::Red, B, 0.8), B);
    }

    #[test]
    fn ramp_passes_through_every_stop() {
        let c = Color::Rgb(10, 20, 30);
        assert_eq!(ramp(&[A, B, c], 0.0), A);
        assert_eq!(ramp(&[A, B, c], 0.5), B);
        assert_eq!(ramp(&[A, B, c], 1.0), c);
        assert_eq!(ramp(&[c], 0.7), c);
        assert_eq!(ramp(&[], 0.7), Color::Reset);
    }

    #[test]
    fn gradient_spans_cover_text_and_end_on_last_stop() {
        let spans = gradient("ka ✦", &[A, B], Style::new());
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "ka ✦");
        assert_eq!(spans[0].style.fg, Some(A));
        assert_eq!(spans.last().and_then(|s| s.style.fg), Some(B));
    }

    #[test]
    fn sweep_band_travels_and_rests_offstage() {
        // at some clock the band peaks on each column, in order
        let peak = |col: usize| (0..2000u128).find(|ms| sweep(col, 10, ms * 10, 10, 3.0) >= 1.0);
        let (p0, p5, p9) = (peak(0), peak(5), peak(9));
        assert!(p0.is_some() && p5 > p0 && p9 > p5, "{p0:?} {p5:?} {p9:?}");
        // a full cycle has a dark gap: every column off at once
        assert!((0..200u128).any(|t| (0..10).all(|c| sweep(c, 10, t * 10, 10, 3.0) == 0.0)));
    }

    #[test]
    fn shimmer_keeps_text_and_lights_only_near_the_band() {
        let spans = shimmer("working", A, B, 0, Style::new());
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "working");
        // the band starts off-stage left: the whole word is at base
        assert!(spans.iter().all(|s| s.style.fg == Some(A)));
    }
}
