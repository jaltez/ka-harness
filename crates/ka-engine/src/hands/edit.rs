//! The edit hand: exact-match replacement guarded by the read ledger.

use std::future::Future;
use std::pin::Pin;

use serde_json::{Value, json};

use super::{Hand, HandContext, HandDef, ToolOutput};

/// The edit tool.
pub struct EditHand;

impl Hand for EditHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "edit".to_string(),
            description: "Edit a file by exact string replacement. `old` must match exactly \
                once unless replace_all is true. The file must have been read first and must \
                not have changed since."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File to edit" },
                    "old": { "type": "string", "description": "Exact text to replace" },
                    "new": { "type": "string", "description": "Replacement text" },
                    "replace_all": { "type": "boolean", "description": "Replace every occurrence" }
                },
                "required": ["path", "old", "new"]
            }),
            clearance: super::Clearance::Write,
            read_only: false,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        ctx: &'a HandContext,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            let Some(path_str) = args.get("path").and_then(Value::as_str) else {
                return ToolOutput::err("edit: missing required 'path'");
            };
            let (Some(old), Some(new)) = (
                args.get("old").and_then(Value::as_str),
                args.get("new").and_then(Value::as_str),
            ) else {
                return ToolOutput::err("edit: missing required 'old'/'new'");
            };
            if old == new {
                return ToolOutput::err("edit: old and new are identical");
            }
            let path = super::read::resolve(ctx, path_str);

            if let Err(e) = ctx.ledger.lock().verify(&path) {
                return ToolOutput::err(format!("edit refused: {e}"));
            }
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => return ToolOutput::err(format!("edit {}: {e}", path.display())),
            };
            let count = text.matches(old).count();
            let replace_all = args
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if count == 0 {
                return ToolOutput::err(format!(
                    "edit {}: `old` not found. It must match the file exactly (whitespace included); re-read the file.",
                    path.display()
                ));
            }
            if count > 1 && !replace_all {
                return ToolOutput::err(format!(
                    "edit {}: `old` matches {count} times; add \"replace_all\": true or use a longer, unique `old`.",
                    path.display()
                ));
            }
            let updated = if replace_all {
                text.replace(old, new)
            } else {
                text.replacen(old, new, 1)
            };
            // safety net first: a failed snapshot refuses the edit
            if let Err(e) = ctx.snapshots.lock().snapshot(&path) {
                return ToolOutput::err(format!(
                    "edit {}: snapshot before edit failed ({e}); refusing to mutate",
                    path.display()
                ));
            }
            if let Err(e) = std::fs::write(&path, &updated) {
                return ToolOutput::err(format!("edit {}: {e}", path.display()));
            }
            // keep the ledger fresh for follow-up edits
            if let Ok(meta) = std::fs::metadata(&path) {
                ctx.ledger.lock().mint(&path, &meta);
            }
            let preview = first_changed_line(old, new);
            ToolOutput::ok(format!(
                "edited {}: {} replacement(s)\n~ {}",
                path.display(),
                if replace_all { count } else { 1 },
                preview
            ))
        })
    }
}

fn first_changed_line(old: &str, new: &str) -> String {
    let old_line = old.lines().next().unwrap_or("");
    let new_line = new.lines().next().unwrap_or("");
    format!("{old_line} → {new_line}")
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

    #[tokio::test]
    async fn edits_unique_match() {
        let dir = std::env::temp_dir().join(format!("ka-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("e.txt");
        std::fs::write(&f, "alpha\nbeta\ngamma\n").unwrap();
        let ctx = ctx_for(&dir);
        let meta = std::fs::metadata(&f).unwrap();
        ctx.ledger.lock().mint(&f, &meta);

        let out = EditHand
            .execute(
                &json!({"path": "e.txt", "old": "beta", "new": "BETA"}),
                &ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "alpha\nBETA\ngamma\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn refuses_unread_file() {
        let dir = std::env::temp_dir().join(format!("ka-edit-unread-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("u.txt"), "x\n").unwrap();
        let ctx = ctx_for(&dir);
        let out = EditHand
            .execute(&json!({"path": "u.txt", "old": "x", "new": "y"}), &ctx)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("has not been read"), "{}", out.content);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn refuses_changed_since_read() {
        let dir = std::env::temp_dir().join(format!("ka-edit-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("s.txt");
        std::fs::write(&f, "x\n").unwrap();
        let ctx = ctx_for(&dir);
        let meta = std::fs::metadata(&f).unwrap();
        ctx.ledger.lock().mint(&f, &meta);
        std::fs::write(&f, "changed externally\n").unwrap();
        let out = EditHand
            .execute(
                &json!({"path": "s.txt", "old": "changed externally", "new": "y"}),
                &ctx,
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("changed since"), "{}", out.content);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ambiguous_match_demands_replace_all() {
        let dir = std::env::temp_dir().join(format!("ka-edit-multi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("m.txt");
        std::fs::write(&f, "a\na\na\n").unwrap();
        let ctx = ctx_for(&dir);
        let meta = std::fs::metadata(&f).unwrap();
        ctx.ledger.lock().mint(&f, &meta);
        let out = EditHand
            .execute(&json!({"path": "m.txt", "old": "a", "new": "b"}), &ctx)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("3 times"));
        let out = EditHand
            .execute(
                &json!({"path": "m.txt", "old": "a", "new": "b", "replace_all": true}),
                &ctx,
            )
            .await;
        assert!(!out.is_error);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "b\nb\nb\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
