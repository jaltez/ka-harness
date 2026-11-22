//! Alt-screen handoff (TUI-07). ka always paints fullscreen; `/screen`
//! arms the clean handoff: leaving the mode (or quitting while armed)
//! drops back to the primary screen and prints the transcript there as
//! plain text with separator rules, so the conversation lands in the
//! terminal's native scrollback. Without the arm, exit restores the
//! pre-launch screen and the transcript vanishes with the alt buffer.

use std::path::Path;

use crate::tui::Transcript;

/// The scrollback dump for the current transcript: a header rule, the
/// plain-text transcript (one block per turn, blank-line separated),
/// and a closing rule. Width only sizes the rules.
pub fn dump_text(transcript: &Transcript, session: &str, width: usize) -> String {
    let label = if session.is_empty() {
        "session".to_string()
    } else {
        crate::tui::short_session(session)
            .map(str::to_string)
            .unwrap_or_else(|| session.to_string())
    };
    let mut out = String::new();
    out.push_str(&rule(&format!(" ka · {label} "), width));
    out.push('\n');
    for row in crate::plain::transcript_rows(transcript) {
        out.push_str(&row);
        out.push('\n');
    }
    out.push_str(&rule(" end of transcript ", width));
    out.push('\n');
    out
}

/// A full-width rule around a label: `──── ka · fox ────`.
fn rule(label: &str, width: usize) -> String {
    let dashes = width.saturating_sub(label.chars().count());
    let (left, right) = (dashes / 2, dashes - dashes / 2);
    format!("{}{label}{}", "─".repeat(left), "─".repeat(right))
}

/// Where the project stores strands — the resume path's session lookup
/// anchor (used by the TUI's resume recap).
pub fn strand_cwd(cwd: &Path) -> std::path::PathBuf {
    cwd.to_path_buf()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use crate::screen::dump_text;
    use crate::tui::{Line, Transcript};

    #[test]
    fn dump_has_header_transcript_and_footer_rules() {
        let mut t = Transcript::default();
        t.push(Line::User("hi".into()));
        let dump = dump_text(&t, "sabc123", 40);
        let lines: Vec<&str> = dump.lines().collect();
        assert!(lines[0].contains("ka · sabc123"), "{lines:?}");
        assert!(lines[0].starts_with('─'), "{lines:?}");
        assert!(lines.contains(&"user: hi"), "{lines:?}");
        assert!(
            lines.last().unwrap().contains("end of transcript"),
            "{lines:?}"
        );
    }

    #[test]
    fn rules_fill_the_width() {
        let dump = dump_text(&Transcript::default(), "", 30);
        for line in dump.lines() {
            assert_eq!(line.chars().count(), 30, "{line}");
        }
    }
}
