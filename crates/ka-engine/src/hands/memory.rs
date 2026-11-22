//! The remember hand: the model stages durable memories for user
//! review (gemini's auto-memory inbox flow). Proposals land in
//! `.ka/memory/inbox.md`; nothing reaches a MEMORY.md until the user
//! accepts it from the TUI (`/memory`). Task state belongs in the todo
//! tool, not here.

use std::future::Future;
use std::pin::Pin;

use serde_json::{Value, json};

use super::{Clearance, Hand, HandContext, HandDef, ToolOutput};

/// Max staged-note length (chars).
const NOTE_CAP: usize = 500;

/// The remember tool.
pub struct RememberHand;

impl Hand for RememberHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "remember".to_string(),
            description: "Stage a durable note for the user's memory files (MEMORY.md) — \
                project conventions, preferences, corrections worth keeping across \
                sessions. Nothing is written until the user accepts it in /memory; \
                do not re-stage the same note."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "One-line note in plain imperative form" }
                },
                "required": ["note"]
            }),
            clearance: Clearance::Write,
            read_only: false,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        ctx: &'a HandContext,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            let Some(note) = args.get("note").and_then(Value::as_str) else {
                return ToolOutput::err("remember: missing required 'note'");
            };
            let note: String = note.split_whitespace().collect::<Vec<_>>().join(" ");
            if note.is_empty() {
                return ToolOutput::err("remember: note is empty");
            }
            let note = if note.chars().count() > NOTE_CAP {
                let cut: String = note.chars().take(NOTE_CAP).collect();
                cut
            } else {
                note
            };
            // project root (nearest .git ancestor, else cwd): sessions
            // launched from a subdirectory still stage into the one
            // project-level inbox the /memory modal reads
            let inbox = crate::project_root(&ctx.cwd).join(".ka/memory/inbox.md");
            if let Some(parent) = inbox.parent() {
                if std::fs::create_dir_all(parent).is_err() {
                    return ToolOutput::err("remember: cannot create .ka/memory/".to_string());
                }
            }
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            // rfc-3339-ish UTC stamp without a datetime dep: days since
            // epoch is plenty for an inbox ordering
            let line = format!("- [{stamp}] {note}\n");
            let mut body = std::fs::read_to_string(&inbox).unwrap_or_default();
            if body.lines().any(|l| l.ends_with(&note)) {
                return ToolOutput::ok("already staged".to_string());
            }
            // keep the inbox bounded: newest 50 notes
            let mut lines: Vec<String> = body.lines().map(str::to_string).collect();
            lines.push(line.trim().to_string());
            while lines.len() > 50 {
                lines.remove(0);
            }
            body = lines.join("\n");
            body.push('\n');
            match std::fs::write(&inbox, body) {
                Ok(()) => ToolOutput::ok(format!(
                    "staged for review ({}); the user accepts it via /memory — do not write \
                     memory files yourself",
                    inbox.display()
                )),
                Err(e) => ToolOutput::err(format!("remember: {e}")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::hands::{Ledger, Spill};

    fn ctx_for(dir: &std::path::Path) -> HandContext {
        HandContext {
            cwd: dir.to_path_buf(),
            ledger: Arc::new(parking_lot::Mutex::new(Ledger::default())),
            spill: Arc::new(Spill::new()),
            snapshots: Arc::new(parking_lot::Mutex::new(
                crate::hands::snapshots::Snapshots::inert(),
            )),
            jobs: Arc::new(crate::hands::jobs::JobTable::new()),
            bash_background_ms: 0,
            max_image_mb: 5,
            web_allow_private: false,
            sandbox: ka_sandbox::Policy::Off,
            shell_env: Default::default(),
        }
    }

    #[tokio::test]
    async fn stages_once_and_dedupes() {
        let dir = std::env::temp_dir().join(format!("ka-mem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ctx_for(&dir);
        let out = RememberHand
            .execute(&json!({"note": "prefer parking_lot locks"}), &ctx)
            .await;
        assert!(!out.is_error, "{}", out.content);
        let inbox = std::fs::read_to_string(dir.join(".ka/memory/inbox.md")).unwrap();
        assert!(inbox.contains("prefer parking_lot locks"), "{inbox}");
        assert!(inbox.starts_with("- ["), "stamped: {inbox}");
        // restaging the same note is a no-op
        let out = RememberHand
            .execute(&json!({"note": "prefer parking_lot locks"}), &ctx)
            .await;
        assert!(out.content.contains("already staged"), "{}", out.content);
        let inbox2 = std::fs::read_to_string(dir.join(".ka/memory/inbox.md")).unwrap();
        assert_eq!(inbox2, inbox);
        // whitespace-only notes are refused
        let out = RememberHand.execute(&json!({"note": "   "}), &ctx).await;
        assert!(out.is_error);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
