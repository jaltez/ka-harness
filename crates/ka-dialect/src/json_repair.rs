//! Repairing JSON parser for streamed tool-call arguments. Providers emit
//! fragments that can end mid-string, mid-number, or with unbalanced
//! brackets; we close what's open and re-parse before giving up.

use serde_json::Value;

/// Best-effort parse of a possibly-truncated JSON object. Returns `{}` for
/// empty/garbage input rather than failing — an unparsable argument set is
/// a model bug, not a reason to crash the turn.
pub fn repair_json(raw: &str) -> Value {
    let s = raw.trim();
    if s.is_empty() {
        return Value::Object(Default::default());
    }
    if let Ok(v) = serde_json::from_str::<Value>(s) {
        return v;
    }
    let repaired = close_open(s);
    if let Ok(v) = serde_json::from_str::<Value>(&repaired) {
        return v;
    }
    // Truncated mid-key (`{"a": 1, "b"`): close_open leaves the dangling
    // key unparsed. Complete the pair and re-close before salvage.
    if s.ends_with('"') && !quote_escaped(s, s.len() - 1) {
        let completed = close_open(&format!("{s}: null"));
        if let Ok(v) = serde_json::from_str::<Value>(&completed) {
            return v;
        }
    }
    // A trailing ESCAPED quote means the stream died inside a key string
    // (`{"a": 1, "b\` + `"`): the quote above cannot be a closing one, so
    // close the string first, then complete the dangling pair.
    if s.ends_with('"') && quote_escaped(s, s.len() - 1) {
        let completed = close_open(&format!("{s}\": null"));
        if let Ok(v) = serde_json::from_str::<Value>(&completed) {
            return v;
        }
    }
    // Last resort: salvage `key: "value"` pairs by wrapping and closing.
    // A prefix that already opens an object must be closed, not re-wrapped
    // (double-`{` never parses).
    let stripped = strip_trailing_garbage(s);
    let salvaged = if stripped.starts_with('{') {
        close_open(&stripped)
    } else {
        format!("{{{stripped}}}")
    };
    serde_json::from_str::<Value>(&salvaged).unwrap_or(Value::Object(Default::default()))
}

/// Is the quote at `quote_end` escaped? Only an ODD run of
/// backslashes escapes it — `\\"` is an escaped backslash followed
/// by a real closing quote.
fn quote_escaped(s: &str, quote_end: usize) -> bool {
    s.as_bytes()[..quote_end]
        .iter()
        .rev()
        .take_while(|b| **b == b'\\')
        .count()
        % 2
        == 1
}

const MAX_NESTING_DEPTH: usize = 128;

fn close_open(s: &str) -> String {
    let mut out = s.to_string();
    // Drop a trailing fragment that cannot be part of a complete value:
    // e.g. `"key": "partial` or `"key": 12.` or a trailing `,`.
    loop {
        let trimmed = out.trim_end();
        if trimmed.ends_with(',') {
            out.truncate(trimmed.len() - 1);
            continue;
        }
        if trimmed.ends_with(':') {
            // dangling key — drop the key entirely is hard; neutralize value
            out.truncate(trimmed.len());
            out.push_str(" null");
            break;
        }
        if trimmed.ends_with('.') {
            out.truncate(trimmed.len() - 1);
            continue;
        }
        break;
    }
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escape = false;
    for ch in out.chars() {
        if in_string {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else {
            match ch {
                '"' => in_string = true,
                '{' | '[' => {
                    if stack.len() < MAX_NESTING_DEPTH {
                        stack.push(ch);
                    }
                }
                '}' if stack.last() == Some(&'{') => {
                    stack.pop();
                }
                ']' if stack.last() == Some(&'[') => {
                    stack.pop();
                }
                _ => {}
            }
        }
    }
    if in_string {
        if escape {
            // Strip a dangling trailing `\` so the closing quote is not escaped.
            out.pop();
        }
        out.push('"');
    }
    while let Some(open) = stack.pop() {
        out.push(match open {
            '{' => '}',
            _ => ']',
        });
    }
    out
}

fn strip_trailing_garbage(s: &str) -> String {
    // Cut at the last complete `"..."` pair boundary we can find.
    match s.rfind('"') {
        Some(pos) if pos > 0 && !quote_escaped(s, pos) => s[..=pos].to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn valid_json_passes_through() {
        let v = repair_json("{\"a\": 1}");
        assert_eq!(v.get("a").and_then(Value::as_i64), Some(1));
    }

    #[test]
    fn empty_becomes_empty_object() {
        assert!(repair_json("").as_object().unwrap().is_empty());
        assert!(repair_json("   ").as_object().unwrap().is_empty());
    }

    #[test]
    fn closes_unbalanced_braces_and_trailing_comma() {
        let v = repair_json("{\"path\": \"src/main.rs\",");
        assert_eq!(v.get("path").and_then(Value::as_str), Some("src/main.rs"));
    }

    #[test]
    fn closes_open_string() {
        let v = repair_json("{\"note\": \"half written");
        assert_eq!(v.get("note").and_then(Value::as_str), Some("half written"));
    }

    #[test]
    fn nested_containers_closed() {
        let v = repair_json("{\"outer\": {\"inner\": [1, 2");
        let inner = v
            .get("outer")
            .unwrap()
            .get("inner")
            .cloned()
            .unwrap_or_default();
        assert!(inner.is_array());
    }

    #[test]
    fn garbage_becomes_empty_object() {
        assert!(
            repair_json("total nonsense !!")
                .as_object()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn dangling_key_neutralized() {
        let v = repair_json("{\"a\": 1, \"b\":");
        assert_eq!(v.get("a").and_then(Value::as_i64), Some(1));
    }

    #[test]
    fn truncated_mid_key_preserves_earlier_pairs() {
        // stream cut after the key but before the colon
        let v = repair_json("{\"a\": 1, \"b\"");
        assert_eq!(v.get("a").and_then(Value::as_i64), Some(1));
    }

    #[test]
    fn salvage_does_not_double_open() {
        // close_open cannot fix the juxtaposed garbage; salvage must close
        // the existing `{`, not wrap a second one around it
        let v = repair_json("{\"cmd\": \"run\" trailing junk");
        assert_eq!(v.get("cmd").and_then(Value::as_str), Some("run"));
    }

    #[test]
    fn escaped_backslash_before_quote_is_a_real_quote() {
        // `\\"` is an escaped backslash followed by a REAL closing quote:
        // the key `b\` is complete and only the `: value` was cut. The old
        // single-char lookbehind saw `\"` and wrongly skipped the fix-up,
        // collapsing the whole repair to `{}`.
        let v = repair_json(r#"{"a":1,"b\\""#);
        assert_eq!(v.get("a").and_then(Value::as_i64), Some(1));
        assert_eq!(v.get("b\\").and_then(Value::as_null), Some(()));
    }

    #[test]
    fn quote_escaped_counts_the_backslash_run() {
        assert!(quote_escaped("\\\"", 1)); // one backslash escapes
        assert!(!quote_escaped("\\\\\"", 2)); // escaped backslash + real quote
        assert!(quote_escaped("\\\\\\\"", 3)); // three: escapes
        assert!(!quote_escaped("\"", 0)); // bare quote
    }

    // ---- hand-rolled property/fuzz tests (no new dependencies) ----

    /// Fixed-seed xorshift64* PRNG: deterministic across runs and machines.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            // xorshift cannot escape a zero state.
            Self(seed | 1)
        }

        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next_u64() % n
        }

        fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
            xs[self.below(xs.len() as u64) as usize]
        }
    }

    const SEEDS: [u64; 3] = [
        0x9E37_79B9_7F4A_7C15,
        0xDEAD_BEEF_CAFE_F00D,
        0x0123_4567_89AB_CDEF,
    ];

    /// Charset of a JSON-ish stream: structure, quotes, escapes, digits.
    const JUNK_CHARS: &[char] = &[
        '{', '}', '[', ']', '"', ':', ',', '\\', ' ', 'a', 'b', 'c', '0', '1', '\n',
    ];

    fn random_text(rng: &mut Rng, max_len: usize) -> String {
        let len = rng.below(max_len as u64 + 1) as usize;
        (0..len).map(|_| rng.pick(JUNK_CHARS)).collect()
    }

    /// String payloads that stress escaping, including non-ASCII.
    const STRING_CHARS: &[char] = &[
        'a', 'Z', '0', ' ', '"', '\\', '\n', '\t', '/', '\u{7f}', 'é', 'ß', '中', '😀',
    ];

    fn gen_string(rng: &mut Rng) -> String {
        let len = rng.below(12) as usize;
        (0..len).map(|_| rng.pick(STRING_CHARS)).collect()
    }

    fn gen_number(rng: &mut Rng) -> Value {
        match rng.below(4) {
            0 => Value::from(rng.below(2_000_001) as i64 - 1_000_000),
            1 => Value::from(-(rng.below(1_000_000) as i64)),
            // Short (< 15 significant digit) decimals only: serde_json's
            // default float parser (feature `float_roundtrip` is off) is not
            // bit-exact for 17-digit shortest representations.
            2 => {
                let magnitude = rng.below(1_000_000) as f64 / 1000.0;
                Value::from(if rng.below(2) == 0 {
                    -magnitude
                } else {
                    magnitude
                })
            }
            _ => Value::from(rng.next_u64()),
        }
    }

    fn gen_value(rng: &mut Rng, depth: u32) -> Value {
        // Depth <= 4; at the limit only leaves are emitted.
        let choice = rng.below(if depth >= 4 { 4 } else { 7 });
        match choice {
            0 => Value::Null,
            1 => Value::Bool(rng.below(2) == 0),
            2 | 3 => gen_number(rng),
            4 => Value::String(gen_string(rng)),
            5 => {
                let len = rng.below(4);
                Value::Array((0..len).map(|_| gen_value(rng, depth + 1)).collect())
            }
            _ => {
                let len = rng.below(4);
                let mut map = serde_json::Map::new();
                for _ in 0..len {
                    map.insert(gen_string(rng), gen_value(rng, depth + 1));
                }
                Value::Object(map)
            }
        }
    }

    #[test]
    fn identity_on_valid_documents() {
        for seed in SEEDS {
            let mut rng = Rng::new(seed);
            for _ in 0..150 {
                let v = gen_value(&mut rng, 0);
                let text = v.to_string();
                assert_eq!(repair_json(&text), v, "seed={seed} text={text}");
            }
        }
    }

    #[test]
    fn no_panic_on_every_char_boundary_prefix() {
        for seed in SEEDS {
            let mut rng = Rng::new(seed);
            let mut docs: Vec<String> = (0..18).map(|_| random_text(&mut rng, 200)).collect();
            for _ in 0..6 {
                docs.push(gen_value(&mut rng, 0).to_string());
            }
            for doc in &docs {
                // Every char boundary is a valid `&str` slice point.
                for (i, _) in doc.char_indices().take(200) {
                    let _ = repair_json(&doc[..i]);
                }
                let _ = repair_json(doc);
            }
        }
    }

    #[test]
    fn escaped_quote_never_collapses_object() {
        // `{"k":1,"` + n backslashes + `"`: whether the trailing quote is
        // escaped (odd n) or real (even n), the earlier pair must survive.
        for n in 1..=8 {
            let input = format!("{{\"k\":1,\"{}\"", "\\".repeat(n));
            let v = repair_json(&input);
            assert_eq!(
                v.get("k").and_then(Value::as_i64),
                Some(1),
                "n={n} input={input:?} repaired={v}"
            );
        }
    }

    #[test]
    fn escaped_string_values_round_trip() {
        const CHARS: &[char] = &['a', '"', '\\'];
        for seed in SEEDS {
            let mut rng = Rng::new(seed);
            for _ in 0..150 {
                let len = rng.below(10);
                let s: String = (0..len).map(|_| rng.pick(CHARS)).collect();
                let encoded = serde_json::to_string(&s).unwrap();
                let inner = &encoded[1..encoded.len() - 1];
                let doc = format!("{{\"k\":\"{inner}\"}}");
                let v = repair_json(&doc);
                assert_eq!(
                    v.get("k").and_then(Value::as_str),
                    Some(s.as_str()),
                    "seed={seed} doc={doc}"
                );
            }
        }
    }

    #[test]
    fn junk_fuzz_never_panics() {
        for seed in SEEDS {
            let mut rng = Rng::new(seed);
            for _ in 0..500 {
                let s = random_text(&mut rng, 200);
                let _ = repair_json(&s);
            }
        }
    }

    #[test]
    fn trailing_odd_and_even_backslashes_in_open_string() {
        // Odd trailing backslash inside an open string must be stripped before
        // closing the quote so the repaired JSON is valid.
        let v1 = repair_json(r#"{"a": "foo\"#);
        assert_eq!(v1.get("a").and_then(Value::as_str), Some("foo"));

        // Even trailing backslashes represent literal backslash(es) and stay intact.
        let v2 = repair_json(r#"{"a": "foo\\"#);
        assert_eq!(v2.get("a").and_then(Value::as_str), Some(r"foo\"));

        // 3 backslashes: 2 form one literal `\`, the 3rd dangling `\` is stripped.
        let v3 = repair_json(r#"{"a": "foo\\\"#);
        assert_eq!(v3.get("a").and_then(Value::as_str), Some(r"foo\"));

        // 4 backslashes: 2 literal `\\`.
        let v4 = repair_json(r#"{"a": "foo\\\\"#);
        assert_eq!(v4.get("a").and_then(Value::as_str), Some(r"foo\\"));
    }

    #[test]
    fn deep_nesting_is_capped_and_does_not_amplify() {
        let deep = "{".repeat(500);
        let repaired = close_open(&deep);
        assert!(
            repaired.len() <= deep.len() + MAX_NESTING_DEPTH,
            "closing brackets must be capped at {MAX_NESTING_DEPTH}"
        );
        // repair_json must not panic and falls back cleanly to `{}`.
        let v = repair_json(&deep);
        assert!(v.is_object());
    }
}
