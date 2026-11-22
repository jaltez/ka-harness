//! The glob hand: gitignore-aware filename matching, bounded results.

use std::future::Future;
use std::pin::Pin;

use serde_json::{Value, json};

use super::{Hand, HandContext, HandDef, ToolOutput};

/// Maximum matches returned.
pub const MAX_RESULTS: usize = 200;

/// The glob tool.
pub struct GlobHand;

impl Hand for GlobHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "glob".to_string(),
            description: "Find files by glob pattern (e.g. `src/**/*.rs`), respecting \
                .gitignore. Results capped at 200, sorted newest-first."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob pattern (*, ?, **)" },
                    "path": { "type": "string", "description": "Root directory (default: cwd)" }
                },
                "required": ["pattern"]
            }),
            clearance: super::Clearance::Read,
            read_only: true,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        ctx: &'a HandContext,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
                return ToolOutput::err("glob: missing required 'pattern'");
            };
            let root = args
                .get("path")
                .and_then(Value::as_str)
                .map(|p| super::read::resolve(ctx, p))
                .unwrap_or_else(|| ctx.cwd.clone());
            let Some(matcher) = glob_regex(pattern) else {
                return ToolOutput::err(format!("glob: cannot translate pattern {pattern:?}"));
            };

            // Blocking filesystem walk: keep it off the async runtime so
            // a repo-wide glob cannot stall event pumping (ka runs a
            // single-threaded executor).
            let joined = tokio::task::spawn_blocking(move || walk_matches(&root, &matcher))
                .await
                .map_err(|e| format!("glob: walk task failed: {e}"));
            match joined {
                Ok(out) if out.is_empty() => ToolOutput::ok("no matches"),
                Ok(out) => ToolOutput::ok(out),
                Err(msg) => ToolOutput::err(msg),
            }
        })
    }
}

/// Blocking worker: walk the root (gitignore-aware), newest-first matches.
fn walk_matches(root: &std::path::Path, matcher: &regex::Regex) -> String {
    let mut hits: Vec<(std::time::SystemTime, String)> = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .build();
    for entry in walker.flatten() {
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if rel_str.is_empty() {
            continue;
        }
        if matcher.is_match(&rel_str) {
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            hits.push((mtime, rel_str));
        }
        if hits.len() >= MAX_RESULTS * 2 {
            break;
        }
    }
    if hits.is_empty() {
        return String::new();
    }
    hits.sort_by_key(|h| std::cmp::Reverse(h.0));
    hits.truncate(MAX_RESULTS);
    let mut out = String::new();
    for (_, rel) in &hits {
        out.push_str(rel);
        out.push('\n');
    }
    if hits.len() == MAX_RESULTS {
        out.push_str("[...capped at 200 matches]\n");
    }
    out
}

/// Translate a glob pattern to a regex for full-path matching.
fn glob_regex(pattern: &str) -> Option<regex::Regex> {
    let mut re = String::from("(?s)^");
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        re.push_str("(.*/)?");
                    } else {
                        re.push_str(".*");
                    }
                } else {
                    re.push_str("[^/]*");
                }
            }
            '?' => re.push_str("[^/]"),
            '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\' => {
                re.push('\\');
                re.push(c);
            }
            c => re.push(c),
        }
    }
    re.push('$');
    regex::Regex::new(&re).ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;

    use parking_lot::Mutex;

    use super::*;
    use crate::hands::{Ledger, Spill};

    fn ctx_for(dir: &std::path::Path) -> HandContext {
        HandContext {
            cwd: dir.to_path_buf(),
            ledger: Arc::new(Mutex::new(Ledger::default())),
            spill: Arc::new(Spill::new()),
            snapshots: Arc::new(parking_lot::Mutex::new(
                crate::hands::snapshots::Snapshots::inert(),
            )),
            jobs: std::sync::Arc::new(crate::hands::jobs::JobTable::new()),
            bash_background_ms: 0,
            max_image_mb: 5,
            web_allow_private: false,
            sandbox: ka_sandbox::Policy::Off,
            shell_env: Default::default(),
        }
    }

    #[test]
    fn glob_patterns_translate() {
        let r = glob_regex("src/**/*.rs").unwrap();
        assert!(r.is_match("src/a.rs"));
        assert!(r.is_match("src/x/y/a.rs"));
        assert!(!r.is_match("src/a.ts"));
        let r = glob_regex("*.toml").unwrap();
        assert!(r.is_match("Cargo.toml"));
        assert!(
            !r.is_match("sub/Cargo.toml"),
            "single * must not cross directories"
        );
    }

    #[tokio::test]
    async fn respects_gitignore() {
        let dir = std::env::temp_dir().join(format!("ka-glob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(dir.join("src/a.rs"), "x").unwrap();
        std::fs::write(dir.join("target/junk.rs"), "x").unwrap();
        // gitignore only applies inside git repos for the `ignore` crate
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .output()
            .unwrap();

        let ctx = ctx_for(&dir);
        let out = GlobHand.execute(&json!({"pattern": "**/*.rs"}), &ctx).await;
        assert!(out.content.contains("src/a.rs"), "{}", out.content);
        assert!(
            !out.content.contains("junk.rs"),
            "gitignore must be respected:\n{}",
            out.content
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
