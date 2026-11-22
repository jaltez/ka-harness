//! Emoji shortcodes: `:fire:` → 🔥 on Tab. A small table (not a
//! spellchecker, not a full emoji set — that's a dictionary's worth of
//! footprint for a niche affordance): Tab on a word that starts with
//! `:` completes the shortcode — unique match expands to the glyph,
//! ambiguous prefixes extend to their longest common prefix
//! (shell-style), unknown prefixes stay untouched.

/// `(shortcode, emoji)`, no colons in the key (`:fire:` types as the
/// word `:fire:`; the table keys are bare names).
pub const EMOJI: &[(&str, &str)] = &[
    ("fire", "\u{1f525}"),
    ("sparkles", "\u{2728}"),
    ("star", "\u{2b50}"),
    ("rocket", "\u{1f680}"),
    ("tada", "\u{1f389}"),
    ("check", "\u{2705}"),
    ("x", "\u{274c}"),
    ("warning", "\u{26a0}\u{fe0f}"),
    ("bug", "\u{1f41b}"),
    ("zap", "\u{26a1}"),
    ("bulb", "\u{1f4a1}"),
    ("eyes", "\u{1f440}"),
    ("thought", "\u{1f4ad}"),
    ("memo", "\u{1f4dd}"),
    ("book", "\u{1f4d6}"),
    ("books", "\u{1f4da}"),
    ("notebook", "\u{1f4d3}"),
    ("pencil", "\u{270f}\u{fe0f}"),
    ("wrench", "\u{1f527}"),
    ("hammer", "\u{1f528}"),
    ("gear", "\u{2699}\u{fe0f}"),
    ("package", "\u{1f4e6}"),
    ("lock", "\u{1f512}"),
    ("unlock", "\u{1f513}"),
    ("key", "\u{1f511}"),
    ("search", "\u{1f50d}"),
    ("mag", "\u{1f50e}"),
    ("link", "\u{1f517}"),
    ("chains", "\u{26d3}\u{fe0f}"),
    ("paperclip", "\u{1f4ce}"),
    ("clipboard", "\u{1f4cb}"),
    ("pushpin", "\u{1f4cc}"),
    ("chart", "\u{1f4c8}"),
    ("down", "\u{1f4c9}"),
    ("up", "\u{1f4ca}"),
    ("money", "\u{1f4b0}"),
    ("gift", "\u{1f381}"),
    ("heart", "\u{2764}\u{fe0f}"),
    ("broken_heart", "\u{1f494}"),
    ("thumbsup", "\u{1f44d}"),
    ("thumbsdown", "\u{1f44e}"),
    ("wave", "\u{1f44b}"),
    ("clap", "\u{1f44f}"),
    ("pray", "\u{1f64f}"),
    ("muscle", "\u{1f4aa}"),
    ("brain", "\u{1f9e0}"),
    ("robot", "\u{1f916}"),
    ("ghost", "\u{1f47b}"),
    ("skull", "\u{1f480}"),
    ("poop", "\u{1f4a9}"),
    ("smile", "\u{1f604}"),
    ("grin", "\u{1f601}"),
    ("joy", "\u{1f602}"),
    ("sad", "\u{1f625}"),
    ("cry", "\u{1f62d}"),
    ("scream", "\u{1f631}"),
    ("think", "\u{1f914}"),
    ("shrug", "\u{1f937}"),
    ("eyes_roll", "\u{1f644}"),
    ("sleep", "\u{1f634}"),
    ("coffee", "\u{2615}"),
    ("tea", "\u{1f375}"),
    ("pizza", "\u{1f355}"),
    ("cake", "\u{1f370}"),
    ("apple", "\u{1f34e}"),
    ("banana", "\u{1f34c}"),
    ("cat", "\u{1f408}"),
    ("dog", "\u{1f415}"),
    ("bee", "\u{1f41d}"),
    ("whale", "\u{1f40b}"),
    ("seedling", "\u{1f331}"),
    ("tree", "\u{1f333}"),
    ("cactus", "\u{1f335}"),
    ("sun", "\u{2600}\u{fe0f}"),
    ("moon", "\u{1f319}"),
    ("cloud", "\u{2601}\u{fe0f}"),
    ("rainbow", "\u{1f308}"),
    ("snowflake", "\u{2744}\u{fe0f}"),
    ("earth", "\u{1f30d}"),
    ("globe", "\u{1f310}"),
    ("hourglass", "\u{23f3}"),
    ("watch", "\u{231a}"),
    ("alarm", "\u{23f0}"),
    ("calendar", "\u{1f4c5}"),
    ("mailbox", "\u{1f4eb}"),
    ("phone", "\u{1f4f1}"),
    ("computer", "\u{1f4bb}"),
    ("floppy", "\u{1f4be}"),
    ("cd", "\u{1f4bf}"),
    ("battery", "\u{1f50b}"),
    ("flashlight", "\u{1f526}"),
    ("bell", "\u{1f514}"),
    ("no_bell", "\u{1f515}"),
    ("megaphone", "\u{1f4e2}"),
    ("speech", "\u{1f4ac}"),
    ("boom", "\u{1f4a5}"),
    ("100", "\u{1f4af}"),
    ("ok_hand", "\u{1f44c}"),
];

/// The completion for a `:`-led word: `(":fir")` → `Some(":fire")` —
/// a unique shortcode (by prefix) expands straight to the glyph once
/// the name is fully typed (`(":fire")` → `Some("🔥")`); an ambiguous
/// prefix with a longer common prefix grows shell-style; otherwise
/// keep typing. `None` when the word is not a `:` word, matches
/// nothing, or is exactly `:` (too broad to complete).
pub fn complete(word: &str) -> Option<String> {
    let inner = word.strip_prefix(':')?;
    let name = inner.strip_suffix(':').unwrap_or(inner);
    if name.is_empty() {
        return None;
    }
    let hits: Vec<&(&str, &str)> = EMOJI.iter().filter(|(k, _)| k.starts_with(name)).collect();
    match hits.as_slice() {
        [only] if only.0.len() == name.len() || word.ends_with(':') => Some(only.1.to_string()),
        [] => None,
        _ => {
            let prefix = common_prefix(hits.iter().map(|h| h.0));
            if prefix.len() > name.len() {
                Some(format!(":{prefix}"))
            } else {
                None
            }
        }
    }
}

fn common_prefix<'a>(mut words: impl Iterator<Item = &'a str>) -> String {
    let Some(first) = words.next() else {
        return String::new();
    };
    let mut prefix = first.to_string();
    for word in words {
        while !word.starts_with(&prefix) {
            prefix.pop();
        }
    }
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_shape() {
        assert!(EMOJI.len() >= 80, "table is the feature: {}", EMOJI.len());
        for (key, emoji) in EMOJI {
            assert!(!key.is_empty() && !key.contains(':'), "{key:?}");
            assert_eq!(key.to_lowercase(), *key, "keys are lowercase: {key:?}");
            assert!(!emoji.is_empty());
        }
        let mut keys: Vec<_> = EMOJI.iter().map(|(k, _)| *k).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), EMOJI.len(), "duplicate shortcode");
    }

    #[test]
    fn unique_prefix_expands_to_the_glyph() {
        assert_eq!(complete(":fire:"), Some("🔥".to_string()));
        assert_eq!(complete(":fire"), Some("🔥".to_string()), ":fire is unique");
        assert_eq!(complete(":cat"), Some("🐈".to_string()));
    }

    #[test]
    fn ambiguous_prefixes_wait_for_narrowing() {
        // no unique hit and no common-prefix growth: keep typing
        assert_eq!(complete(":c"), None, "cat/cake/check/… share only the c");
        assert_eq!(complete(":th"), None, "think/thought/thumbs… share only th");
        // a wider prefix grows first, then the exact name expands
        assert_eq!(complete(":fir"), Some(":fire".to_string()));
        assert_eq!(complete(":fire"), Some("🔥".to_string()));
        // fully typed always resolves even when not prefix-unique
        assert_eq!(complete(":think:"), Some("🤔".to_string()));
    }

    #[test]
    fn non_words_and_unknowns_stay_untouched() {
        assert_eq!(complete("hello"), None);
        assert_eq!(complete(":"), None, "bare colon is too broad");
        assert_eq!(complete(":zzz"), None);
        assert_eq!(complete(":x:"), Some("❌".to_string()));
    }
}
