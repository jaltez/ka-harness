//! Session recaps (TUI-12): the one-line note shown when the TUI
//! regains focus after a long absence, when a resumed session opens
//! with activity today, and on an explicit `/recap`. Sources, in
//! priority order: the strand's persisted recap (`StrandSummary.recap`
//! — the last `x.ka.recap` record, surfaced by `ka_strand::list`), then
//! a local (model-free) summary built from the surface's own
//! transcript. Nothing is invented — an empty transcript yields no
//! recap. Raw strand records are never read here.

use std::path::Path;

use crate::tui::{Line, Transcript};

/// A strand counts as "active today" when its file changed within this
/// window (record lines carry no timestamps; the mtime is the honest
/// signal ka-term has).
const ACTIVE_WINDOW: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// The recap note for `session`: the persisted strand recap when the
/// strand moved in the last day, else the local turn summary from the
/// transcript the surface holds. `None` = nothing to say.
pub fn recap_note(session: &str, cwd: &Path, transcript: &Transcript) -> Option<String> {
    stored_recap(session, cwd).or_else(|| recap_from_transcript(transcript))
}

/// The latest persisted recap for `session` — the last `x.ka.recap`
/// record, read through `ka_strand::list`'s summary (no raw records) —
/// clamped to one line, and only while the strand is active today.
pub fn stored_recap(session: &str, cwd: &Path) -> Option<String> {
    if session.is_empty() {
        return None;
    }
    let summaries = ka_strand::list(cwd).ok()?;
    let summary = summaries.into_iter().find(|s| s.id == session)?;
    if !active_today(&summary) {
        return None;
    }
    summary.recap.map(|text| one_line(&text))
}

/// Whether the strand file was written within [`ACTIVE_WINDOW`].
fn active_today(summary: &ka_strand::StrandSummary) -> bool {
    std::fs::metadata(&summary.path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age <= ACTIVE_WINDOW)
}

/// A model-free recap from the surface's own transcript: how many user
/// turns it holds and what the last one asked. Assistant/tool/report
/// rows are context, not the ask — the prompt is the recap.
pub fn recap_from_transcript(transcript: &Transcript) -> Option<String> {
    let mut turns = 0usize;
    let mut last_prompt: Option<String> = None;
    for entry in transcript.entries() {
        if let Line::User(text) = entry {
            let text = text.trim();
            if text.is_empty() || text.starts_with('/') {
                // slash-command turns are actions, not asks
                continue;
            }
            turns += 1;
            last_prompt = Some(text.to_string());
        }
    }
    let prompt = last_prompt?;
    Some(one_line(&format!(
        "{turns} turn{} in context · last: {prompt}",
        if turns == 1 { "" } else { "s" }
    )))
}

/// Clamp a recap to one transcript line: first non-empty line,
/// whitespace collapsed, capped at 160 chars with an ellipsis.
fn one_line(text: &str) -> String {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let mut out = String::new();
    let mut gap = false;
    for c in first.trim().chars() {
        if c.is_whitespace() {
            gap = true;
            continue;
        }
        if gap && !out.is_empty() {
            out.push(' ');
        }
        gap = false;
        out.push(c);
    }
    if out.chars().count() > 160 {
        let clipped: String = out.chars().take(159).collect();
        return format!("{clipped}…");
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn transcript_with(prompts: &[&str]) -> Transcript {
        let mut t = Transcript::default();
        for p in prompts {
            t.push(Line::User((*p).to_string()));
        }
        t
    }

    #[test]
    fn local_recap_counts_turns_and_carries_the_last_prompt() {
        let t = transcript_with(&["fix the bug", "then write tests"]);
        assert_eq!(
            recap_from_transcript(&t).as_deref(),
            Some("2 turns in context · last: then write tests")
        );
    }

    #[test]
    fn slash_turns_are_actions_not_asks() {
        let t = transcript_with(&["/compact", "the real ask"]);
        assert_eq!(
            recap_from_transcript(&t).as_deref(),
            Some("1 turn in context · last: the real ask")
        );
    }

    #[test]
    fn empty_or_slash_only_transcripts_stay_silent() {
        assert!(recap_from_transcript(&Transcript::default()).is_none());
        assert!(recap_from_transcript(&transcript_with(&["/help", "/tasks"])).is_none());
    }

    #[test]
    fn long_prompts_clamp_to_one_line() {
        let long = "word ".repeat(80);
        let t = transcript_with(&[long.as_str()]);
        let recap = recap_from_transcript(&t).expect("recap");
        assert_eq!(recap.lines().count(), 1);
        assert!(recap.chars().count() <= 160, "{recap}");
        assert!(recap.ends_with('…'));
    }

    #[test]
    fn stored_recap_needs_a_session_and_an_active_strand() {
        let cwd = std::env::temp_dir();
        assert!(stored_recap("", &cwd).is_none());
        assert!(stored_recap("sdefinitely-not-here", &cwd).is_none());
    }
}
