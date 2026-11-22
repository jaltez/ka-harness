//! The write hand: create/overwrite files (ledger-guarded on overwrite).

use std::future::Future;
use std::pin::Pin;

use serde_json::{Value, json};

use super::{Hand, HandContext, HandDef, ToolOutput};

/// The write tool.
pub struct WriteHand;

impl Hand for WriteHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "write".to_string(),
            description: "Create or overwrite a file with the given content. Overwriting an \
                existing file requires it to have been read first. A shebang first line makes \
                the file executable. Parent directories are created."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File to write" },
                    "content": { "type": "string", "description": "Full file content" }
                },
                "required": ["path", "content"]
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
                return ToolOutput::err("write: missing required 'path'");
            };
            let Some(content) = args.get("content").and_then(Value::as_str) else {
                return ToolOutput::err("write: missing required 'content'");
            };
            let path = super::read::resolve(ctx, path_str);

            if path.exists() {
                if let Err(e) = ctx.ledger.lock().verify(&path) {
                    return ToolOutput::err(format!("write refused: {e}"));
                }
            }
            if let Some(parent) = path.parent() {
                if !parent.exists() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        return ToolOutput::err(format!("write {}: {e}", path.display()));
                    }
                }
            }
            // safety net first: a failed snapshot refuses the write
            if let Err(e) = ctx.snapshots.lock().snapshot(&path) {
                return ToolOutput::err(format!(
                    "write {}: snapshot before write failed ({e}); refusing to mutate",
                    path.display()
                ));
            }
            if let Err(e) = std::fs::write(&path, content) {
                return ToolOutput::err(format!("write {}: {e}", path.display()));
            }
            if content.starts_with("#!") {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = std::fs::metadata(&path)
                        .map(|m| m.permissions().mode())
                        .unwrap_or(0o644);
                    let _ = std::fs::set_permissions(
                        &path,
                        std::fs::Permissions::from_mode(mode | 0o111),
                    );
                }
            }
            if let Ok(meta) = std::fs::metadata(&path) {
                ctx.ledger.lock().mint(&path, &meta);
            }
            ToolOutput::ok(format!(
                "wrote {} ({} bytes, {} lines)",
                path.display(),
                content.len(),
                content.lines().count()
            ))
        })
    }
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
    async fn creates_files_and_parents() {
        let dir = std::env::temp_dir().join(format!("ka-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ctx_for(&dir);
        let out = WriteHand
            .execute(
                &json!({"path": "nested/deep/f.txt", "content": "hello\n"}),
                &ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.join("nested/deep/f.txt")).unwrap(),
            "hello\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn overwrite_requires_read() {
        let dir = std::env::temp_dir().join(format!("ka-write-over-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("o.txt");
        std::fs::write(&f, "original").unwrap();
        let ctx = ctx_for(&dir);
        let out = WriteHand
            .execute(&json!({"path": "o.txt", "content": "clobber"}), &ctx)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("has not been read"));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "original");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shebang_makes_executable() {
        let dir = std::env::temp_dir().join(format!("ka-write-sh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ctx_for(&dir);
        WriteHand
            .execute(
                &json!({"path": "run.sh", "content": "#!/bin/sh\necho hi\n"}),
                &ctx,
            )
            .await;
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
