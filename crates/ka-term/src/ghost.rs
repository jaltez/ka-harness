//! Ghost text (TUI-09): the most probable continuation rendered dim at
//! the caret. Two sources, both purely local (no network):
//!
//! * slash commands — the first registered command matching the typed
//!   `/prefix` (Tab already completes; the ghost mirrors its top row);
//! * @-mentions — the first matching file from the mention walk, shown
//!   as the path remainder after the typed query.
//!
//! Accepting inserts the remainder via the same rules Tab uses (a
//! trailing space for argument-bearing slash commands, the colon for
//! `/skill:`); typing naturally discards the ghost because it is
//! recomputed from the draft every frame.

use crate::tui::{InputBuffer, complete_slash, mention_token, path_token};

/// One ghost suggestion: what to complete and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ghost {
    /// A slash command continuation: the full registered name
    /// (`/model`), of which the draft already carries a prefix.
    Slash { name: String },
    /// An @-mention file: the walk-relative path the token completes
    /// to, plus whether it is a directory (dirs get no trailing space).
    Mention { rel: String, is_dir: bool },
    /// An argument-syntax template (` <condition>`): what the command
    /// still expects once the bare name is typed and the slash ghost
    /// has nothing left to suggest.
    Syntax { rest: String },
}

impl Ghost {
    /// The dim text shown after the caret: the not-yet-typed remainder.
    pub fn suffix(&self, input: &str) -> String {
        match self {
            Ghost::Slash { name } => name.strip_prefix(input).unwrap_or_default().to_string(),
            Ghost::Mention { rel, .. } => {
                let cursor = input.chars().count();
                match crate::tui::path_token(input, cursor) {
                    Some((_, token)) => {
                        let query = mention_token(&token).unwrap_or_default();
                        mention_suffix(query, rel)
                    }
                    None => String::new(),
                }
            }
            // the template is the remainder itself: the draft ends at
            // the command name (or its trailing space)
            Ghost::Syntax { rest } => rest.clone(),
        }
    }
}

/// The slash-command ghost for a `/`-led, space-free draft whose cursor
/// sits at the end. Picks the first match (`available_slash_commands`
/// order: builtins first, then custom files). An exact match suggests
/// nothing — there is nothing left to type.
pub fn slash_ghost(input: &str, commands: &[(String, String)]) -> Option<Ghost> {
    if !input.starts_with('/') || input.contains(' ') {
        return None;
    }
    commands
        .iter()
        .find(|(name, _)| name.starts_with(input) && name.len() > input.len())
        .map(|(name, _)| Ghost::Slash { name: name.clone() })
}

/// The argument-syntax ghost for the two flag-bearing commands: once
/// the bare name is typed (`/goal`, `/loop` — where the slash ghost
/// has nothing left to suggest), the dim remainder shows the shape of
/// the arguments. Any typed argument ends the hint — subcommands
/// (`/goal status`) and flags (`/loop --every 30 …`) read better
/// without a template parked after them.
pub fn syntax_ghost(input: &str) -> Option<Ghost> {
    let (name, args) = match input.split_once(' ') {
        Some((name, args)) => (name, Some(args)),
        None => (input, None),
    };
    let template = match name {
        "/goal" => "<condition>",
        "/loop" => "[--every <secs>] <task>",
        "/pr" => "<n|url|branch> [fix]",
        _ => return None,
    };
    match args {
        None => Some(Ghost::Syntax {
            rest: format!(" {template}"),
        }),
        Some("") => Some(Ghost::Syntax {
            rest: template.to_string(),
        }),
        Some(_) => None,
    }
}

/// The @-mention ghost for the token at the caret (cursor at the text
/// end). `entries` is the caller's cached mention walk. Only matches
/// whose path starts with the typed query ghost — a last-segment match
/// (typing `@main` for `src/main.rs`) would need text BEFORE the caret,
/// which a ghost cannot show; Tab's popup covers those.
pub fn mention_ghost(input: &str, cursor: usize, entries: &[(String, bool)]) -> Option<Ghost> {
    if cursor < input.chars().count() {
        return None;
    }
    let (_start, token) = path_token(input, cursor)?;
    let query = mention_token(&token)?;
    // prefix search over the walk directly: a last-segment match
    // (typing `@main` for `src/main.rs`) needs text BEFORE the caret,
    // which a ghost cannot show — Tab's popup covers those
    let (rel, is_dir) = entries
        .iter()
        .find(|(rel, _)| rel.starts_with(query))?
        .clone();
    Some(Ghost::Mention { rel, is_dir })
}

/// The completion to display after the caret for a mention ghost: the
/// remainder of the walk path after the typed query.
pub fn mention_suffix(token_query: &str, rel: &str) -> String {
    rel.strip_prefix(token_query)
        .unwrap_or_default()
        .to_string()
}

/// Accept a ghost: apply the completion to the draft exactly like the
/// matching Tab path does (slash → `complete_slash`; mention → replace
/// the token with `@{rel}`, files take a trailing space).
pub fn accept(ghost: &Ghost, input: &mut InputBuffer) {
    match ghost {
        Ghost::Slash { name } => {
            input.text = complete_slash(name);
            input.cursor = input.text.chars().count();
        }
        Ghost::Mention { rel, is_dir } => {
            // the whole current token (the @-query) is replaced
            let cursor = input.cursor;
            if let Some((start, token)) = crate::tui::path_token(&input.text, cursor) {
                let insert = if *is_dir {
                    format!("@{rel}/")
                } else {
                    format!("@{rel} ")
                };
                let (text, cursor) =
                    crate::tui::complete_token(&input.text, start, token.chars().count(), &insert);
                input.text = text;
                input.cursor = cursor;
            }
        }
        // insert the template right after the typed prefix
        // (insert_str leaves the cursor parked at its end)
        Ghost::Syntax { rest } => input.insert_str(rest),
    }
}
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn cmds() -> Vec<(String, String)> {
        vec![
            ("/model".to_string(), "pick a model".to_string()),
            ("/mode".to_string(), "permission mode".to_string()),
            ("/memory".to_string(), "memory tiers".to_string()),
        ]
    }

    fn draft(text: &str) -> InputBuffer {
        let mut b = InputBuffer::default();
        b.insert_str(text);
        b.cursor = b.text.chars().count();
        b
    }

    #[test]
    fn slash_ghost_suggests_the_first_match_remainder() {
        let g = slash_ghost("/mo", &cmds()).expect("prefix matches");
        assert_eq!(g.suffix("/mo"), "del");
        let g = slash_ghost("/m", &cmds()).expect("prefix matches");
        assert_eq!(g.suffix("/m"), "odel");
    }

    #[test]
    fn slash_ghost_stays_quiet_off_command_shape() {
        assert!(slash_ghost("mo", &cmds()).is_none(), "needs the slash");
        assert!(
            slash_ghost("/mode ", &cmds()).is_none(),
            "space ends the command token"
        );
        // an exact match with no longer sibling leaves nothing to type
        let only = vec![("/mode".to_string(), "permission mode".to_string())];
        assert!(slash_ghost("/mode", &only).is_none());
    }

    #[test]
    fn slash_ghost_accept_completes_like_tab() {
        let mut b = draft("/mo");
        let g = slash_ghost("/mo", &cmds()).expect("ghost");
        accept(&g, &mut b);
        assert_eq!(b.text, "/model ");
        assert_eq!(b.cursor, "/model ".chars().count());
    }

    #[test]
    fn mention_ghost_completes_the_first_walk_match() {
        let entries = vec![
            ("src/main.rs".to_string(), false),
            ("src/tui/mod.rs".to_string(), true),
            ("README.md".to_string(), false),
        ];
        let mut b = draft("look at @sr");
        let g = mention_ghost(&b.text, b.cursor, &entries).expect("ghost");
        assert_eq!(mention_suffix("sr", "src/main.rs"), "c/main.rs");
        accept(&g, &mut b);
        assert_eq!(b.text, "look at @src/main.rs ");
    }

    #[test]
    fn mention_ghost_skips_last_segment_only_matches() {
        // `@main` matches src/main.rs by last segment, but the ghost can
        // only append AFTER the caret — leave those to Tab's popup
        let entries = vec![("src/main.rs".to_string(), false)];
        let b = draft("see @main");
        assert!(mention_ghost(&b.text, b.cursor, &entries).is_none());
    }

    #[test]
    fn mention_ghost_accepts_directories_with_a_slash() {
        let entries = vec![("src".to_string(), true), ("srcx".to_string(), true)];
        let mut b = draft("@sr");
        let g = mention_ghost(&b.text, b.cursor, &entries).expect("ghost");
        accept(&g, &mut b);
        assert_eq!(b.text, "@src/");
    }

    #[test]
    fn ghost_requires_cursor_at_the_end() {
        let mut b = draft("@sr");
        b.cursor = 1;
        assert!(mention_ghost(&b.text, b.cursor, &[]).is_none());
    }

    #[test]
    fn syntax_ghost_shows_the_argument_shape_at_the_prefix() {
        // bare name: the template rides a separating space
        let g = syntax_ghost("/goal").expect("prefix matches");
        assert_eq!(g.suffix("/goal"), " <condition>");
        let g = syntax_ghost("/loop").expect("prefix matches");
        assert_eq!(g.suffix("/loop"), " [--every <secs>] <task>");
        let g = syntax_ghost("/pr").expect("prefix matches");
        assert_eq!(g.suffix("/pr"), " <n|url|branch> [fix]");
        // trailing space: the template continues right at the caret
        let g = syntax_ghost("/goal ").expect("prefix matches");
        assert_eq!(g.suffix("/goal "), "<condition>");
    }

    #[test]
    fn syntax_ghost_stays_quiet_once_arguments_start() {
        // subcommands and flags read their own syntax
        assert!(syntax_ghost("/goal status").is_none());
        assert!(syntax_ghost("/goal all tests green").is_none());
        assert!(syntax_ghost("/loop --every 30 rerun").is_none());
        // nothing to do with these commands
        assert!(syntax_ghost("/model").is_none());
        assert!(syntax_ghost("goal").is_none());
    }

    #[test]
    fn syntax_ghost_accept_inserts_the_template() {
        let mut b = draft("/loop");
        let g = syntax_ghost("/loop").expect("ghost");
        accept(&g, &mut b);
        assert_eq!(b.text, "/loop [--every <secs>] <task>");
        assert_eq!(b.cursor, b.text.chars().count());
    }
}
