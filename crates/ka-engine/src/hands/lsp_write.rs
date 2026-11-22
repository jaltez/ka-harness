//! LSP write-through hands: `lsp_rename`, `lsp_actions`, and
//! `lsp_format` — ka acts *through* the language server, not just reads
//! from it (Phase 8.1; the omp lesson, on ka's terms). Registered only
//! when `[lsp] write_through = true`, all gated at Write tier.
//!
//! The ledger floor holds, with one documented carve-out: a file the
//! model has read refuses to change underneath it (drift = refuse,
//! same as `edit`), while ripple files the model never opened are
//! server-vouched — snapshotted for `/undo`, ledger-minted on apply,
//! capped (50 files / 64 edits per file / 256 KB of inserted text),
//! and never outside the working directory or on a protected path.
//! Every apply is validate-then-mutate: all files are checked before
//! any file is written.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};

use super::{Hand, HandContext, HandDef, ToolOutput, unified_diff};
use crate::lsp::{self, LspManager, identifier_byte_offset, utf16_column};

/// Max files whose diffs are rendered into the tool result (the rest
/// are counted).
const MAX_DIFF_FILES: usize = 8;
/// Max rendered diff lines per file.
const DIFF_LINES: usize = 40;
/// Max code actions listed.
const MAX_ACTIONS: usize = 20;

/// Path display: relative to the cwd when inside it, else absolute.
fn rel(ctx: &HandContext, path: &Path) -> String {
    match path.strip_prefix(&ctx.cwd) {
        Ok(r) => r.to_string_lossy().replace('\\', "/"),
        Err(_) => path.display().to_string(),
    }
}

/// Apply a `WorkspaceEdit` through the ka write path: validate every
/// file first (protected paths, in-cwd, ledger drift on tracked files,
/// edit ranges), then snapshot → write → touch → mint. `notes` collects
/// one unified-diff block per changed file. Returns the number of files
/// written.
async fn apply_workspace_edit(
    edit: &Value,
    lsp: &LspManager,
    ctx: &HandContext,
    notes: &mut Vec<String>,
) -> Result<usize, String> {
    let files = lsp::parse_workspace_edit(edit)?;
    if files.is_empty() {
        return Ok(0);
    }
    // pass 1: validate everything, mutate nothing
    let mut plan: Vec<(PathBuf, String, String)> = Vec::new();
    for (uri_path, edits) in files {
        // URI-decoded paths get the same lexical normalization as
        // model-supplied ones: `%2E%2E` decodes to `..`, which must not
        // smuggle the target past the containment check
        let path = super::protected::normalize(&uri_path);
        let disp = rel(ctx, &path);
        if let Some(why) = super::protected::reason(&ctx.cwd, &path.to_string_lossy()) {
            return Err(format!("refusing: {disp} is protected ({why})"));
        }
        if !path.starts_with(&ctx.cwd) {
            return Err(format!("refusing: {disp} is outside the working directory"));
        }
        {
            let ledger = ctx.ledger.lock();
            if ledger.is_tracked(&path) {
                if let Err(e) = ledger.verify(&path) {
                    return Err(format!("{e}; language-server edit refused"));
                }
            }
        }
        let old = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                return Err(format!(
                    "read {disp}: {e} — the server's view is stale; nothing was applied"
                ));
            }
        };
        let new = lsp::apply_text_edits(&old, &edits).map_err(|e| format!("{disp}: {e}"))?;
        if new != old {
            plan.push((path, old, new));
        }
    }
    // pass 2: mutate (snapshot → write → mint, like edit/write)
    for (path, old, new) in &plan {
        if let Err(e) = ctx.snapshots.lock().snapshot(path) {
            return Err(format!(
                "snapshot {} failed ({e}); refusing further writes — {} file(s) already applied",
                rel(ctx, path),
                plan.iter().position(|(p, _, _)| p == path).unwrap_or(0)
            ));
        }
        if let Err(e) = std::fs::write(path, new) {
            return Err(format!("write {}: {e}", rel(ctx, path)));
        }
        // sync the server's buffer: an open document whose disk content
        // changes without a didChange poisons the next position query —
        // the server resolves fresh-disk positions against stale text
        let _ = lsp.touch(path, new).await;
        if let Ok(meta) = std::fs::metadata(path) {
            ctx.ledger.lock().mint(path, &meta);
        }
        notes.push(unified_diff(&rel(ctx, path), old, new, DIFF_LINES));
    }
    Ok(plan.len())
}

/// Diagnostics block for the anchor file, if the server reports any.
async fn diagnostics_note(lsp: &LspManager, path: &Path) -> String {
    match lsp.refresh(path).await {
        Some(lines) => format!(
            "\n<lsp-diagnostics note=\"informational context\">\n{}\n</lsp-diagnostics>",
            lines.join("\n")
        ),
        None => String::new(),
    }
}

/// Render collected diff notes with a file cap and a counted trailer.
fn render_notes(notes: &[String]) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let total = notes.len();
    let shown: Vec<&str> = notes
        .iter()
        .take(MAX_DIFF_FILES)
        .map(|n| n.as_str())
        .collect();
    let mut out = shown.join("\n");
    if total > MAX_DIFF_FILES {
        out.push_str(&format!(
            "\n(+{} more changed files)",
            total - MAX_DIFF_FILES
        ));
    }
    out
}

/// Resolve `{file, line, symbol?}` into (path, uri, language, LSP range
/// covering the identifier — or the whole line when `symbol` is None).
/// Mirrors `lsp_tools::resolve_position`; returns the file path too so
/// write hands can feedback-loop diagnostics onto it.
async fn resolve_anchor(
    lsp: &LspManager,
    ctx: &HandContext,
    file: &str,
    line: u64,
    symbol: Option<&str>,
) -> Result<(PathBuf, String, String, Value), String> {
    let path = super::read::resolve(ctx, file);
    if !path.is_file() {
        return Err(format!("no file {file}"));
    }
    if line == 0 {
        return Err("line numbers are 1-based".to_string());
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("read {file}: {e}"))?;
    let Some(line_text) = text.lines().nth((line - 1) as usize) else {
        return Err(format!("{file} has fewer than {line} lines"));
    };
    let (start, end) = match symbol {
        Some(symbol) => {
            let Some(offset) = identifier_byte_offset(line_text, symbol) else {
                return Err(format!("{symbol:?} is not an identifier on {file}:{line}"));
            };
            (
                utf16_column(line_text, offset),
                utf16_column(line_text, offset + symbol.len()),
            )
        }
        None => (0, line_text.encode_utf16().count() as u64),
    };
    let uri = lsp.open_if_needed(&path).await?;
    let language = lsp::language_for(&path)
        .map(str::to_string)
        .unwrap_or_default();
    let range = json!({
        "start": { "line": line - 1, "character": start },
        "end": { "line": line - 1, "character": end },
    });
    Ok((path, uri, language, range))
}

/// Build the three write-through hands over one manager.
pub fn hands(lsp: Arc<LspManager>) -> Vec<Arc<dyn Hand>> {
    vec![
        Arc::new(RenameHand(lsp.clone())),
        Arc::new(ActionsHand(lsp.clone())),
        Arc::new(FormatHand(lsp)),
    ]
}

/// The `lsp_rename` hand: rename a symbol through the server (all
/// references update), or — `kind: "file"` — move/rename a file with
/// the servers' ripple edits applied first (`workspace/willRenameFiles`).
pub struct RenameHand(Arc<LspManager>);

impl Hand for RenameHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "lsp_rename".to_string(),
            description: "Rename through the language server so every reference updates. \
                Default kind renames a symbol: {file, line (1-based), symbol, new_name}. \
                kind \"file\" moves/renames a file: {kind, from, to} — the servers' ripple \
                edits (imports, re-exports) apply first. Refuses files that changed since \
                you read them; other touched files are snapshotted and can be undone with /undo."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "description": "\"symbol\" (default) or \"file\"" },
                    "file": { "type": "string", "description": "File containing the symbol (symbol kind)" },
                    "line": { "type": "integer", "description": "1-based line of the identifier (symbol kind)" },
                    "symbol": { "type": "string", "description": "Identifier text as it appears on that line (symbol kind)" },
                    "new_name": { "type": "string", "description": "New symbol name (symbol kind)" },
                    "from": { "type": "string", "description": "Existing file path (file kind)" },
                    "to": { "type": "string", "description": "New file path, must not exist (file kind)" }
                }
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
            let kind = args.get("kind").and_then(Value::as_str).unwrap_or("symbol");
            match kind {
                "symbol" => self.rename_symbol(args, ctx).await,
                "file" => self.move_file(args, ctx).await,
                other => ToolOutput::err(format!(
                    "lsp_rename: unknown kind {other:?} (use \"symbol\" or \"file\")"
                )),
            }
        })
    }
}

impl RenameHand {
    async fn rename_symbol(&self, args: &Value, ctx: &HandContext) -> ToolOutput {
        let (Some(file), Some(line), Some(symbol), Some(new_name)) = (
            args.get("file").and_then(Value::as_str),
            args.get("line").and_then(Value::as_u64),
            args.get("symbol").and_then(Value::as_str),
            args.get("new_name").and_then(Value::as_str),
        ) else {
            return ToolOutput::err(
                "lsp_rename: need 'file', 'line' (1-based), 'symbol', 'new_name'".to_string(),
            );
        };
        let (path, uri, language, range) =
            match resolve_anchor(&self.0, ctx, file, line, Some(symbol)).await {
                Ok(v) => v,
                Err(e) => return ToolOutput::err(format!("lsp_rename: {e}")),
            };
        // cheap validity precheck: the server says whether the position
        // is even renameable. Null = no (a string, a comment). A server
        // that does not implement prepareRename errors — fall through
        // and let the real rename answer.
        if let Ok(Value::Null) = self
            .0
            .request(
                &language,
                "textDocument/prepareRename",
                json!({
                    "textDocument": { "uri": uri },
                    "position": range.get("start").cloned().unwrap_or(Value::Null),
                }),
            )
            .await
        {
            return ToolOutput::err(format!(
                "lsp_rename: {symbol:?} at {file}:{line} is not a renameable symbol \
                 (prepareRename says no)"
            ));
        }
        let params = json!({
            "textDocument": { "uri": uri },
            "position": range.get("start").cloned().unwrap_or(Value::Null),
            "newName": new_name,
        });
        let result = match self
            .0
            .request(&language, "textDocument/rename", params)
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(format!("lsp_rename: {e}")),
        };
        let mut notes = Vec::new();
        let files = match apply_workspace_edit(&result, &self.0, ctx, &mut notes).await {
            Ok(n) => n,
            Err(e) => return ToolOutput::err(format!("lsp_rename: {e}")),
        };
        if files == 0 {
            return ToolOutput::ok(format!(
                "lsp_rename: server returned no changes for {symbol:?} — nothing renamed"
            ));
        }
        let diags = diagnostics_note(&self.0, &path).await;
        ToolOutput::ok(format!(
            "renamed {symbol} → {new_name} across {files} file(s)\n{}\n{}",
            render_notes(&notes),
            diags
        ))
    }

    async fn move_file(&self, args: &Value, ctx: &HandContext) -> ToolOutput {
        let (Some(from), Some(to)) = (
            args.get("from").and_then(Value::as_str),
            args.get("to").and_then(Value::as_str),
        ) else {
            return ToolOutput::err("lsp_rename: need 'from' and 'to'".to_string());
        };
        let from = super::protected::normalize(&super::read::resolve(ctx, from));
        let to = super::protected::normalize(&super::read::resolve(ctx, to));
        if !from.is_file() {
            return ToolOutput::err(format!("lsp_rename: no file {}", rel(ctx, &from)));
        }
        if to.exists() {
            return ToolOutput::err(format!(
                "lsp_rename: {} already exists; refusing to overwrite",
                rel(ctx, &to)
            ));
        }
        // moves get the full write-path treatment on BOTH endpoints:
        // `.git/hooks/pre-commit` as target (or source) is a protected
        // ask, never a silent Write-tier rename
        for (label, p) in [("from", &from), ("to", &to)] {
            if let Some(why) = super::protected::reason(&ctx.cwd, &p.to_string_lossy()) {
                return ToolOutput::err(format!(
                    "lsp_rename: {label} {} is protected ({why})",
                    rel(ctx, p)
                ));
            }
        }
        if !from.starts_with(&ctx.cwd) || !to.starts_with(&ctx.cwd) {
            return ToolOutput::err(
                "lsp_rename: file moves stay inside the working directory".to_string(),
            );
        }
        // the def's drift promise covers the moved file itself
        {
            let ledger = ctx.ledger.lock();
            if ledger.is_tracked(&from) {
                if let Err(e) = ledger.verify(&from) {
                    return ToolOutput::err(format!("lsp_rename: {e}; move refused"));
                }
            }
        }
        // ripple edits first: ask every configured server what a move
        // of this file should rewrite (imports, re-exports, barrels)
        let old_uri = lsp::uri_for(&from);
        let new_uri = lsp::uri_for(&to);
        let params = json!({
            "files": [{ "oldUri": old_uri, "newUri": new_uri }]
        });
        let mut notes = Vec::new();
        let mut ripple_files = 0usize;
        let mut skipped = Vec::new();
        for lang in self.0.configured_languages() {
            match self
                .0
                .request(&lang, "workspace/willRenameFiles", params.clone())
                .await
            {
                Ok(edit) => match apply_workspace_edit(&edit, &self.0, ctx, &mut notes).await {
                    Ok(n) => ripple_files += n,
                    Err(e) => {
                        return ToolOutput::err(format!(
                            "lsp_rename: ripple edits partially applied ({ripple_files} \
                             file(s)), move aborted — {e}"
                        ));
                    }
                },
                Err(e) => skipped.push(format!("{lang}: {e}")),
            }
        }
        // then the move itself
        if let Err(e) = ctx.snapshots.lock().snapshot(&from) {
            return ToolOutput::err(format!(
                "lsp_rename: snapshot before move failed ({e}); refusing (ripple edits \
                 already applied: {ripple_files} file(s))"
            ));
        }
        if let Some(parent) = to.parent() {
            if !parent.exists() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    return ToolOutput::err(format!("lsp_rename: {e}"));
                }
            }
        }
        if let Err(e) = std::fs::rename(&from, &to) {
            return ToolOutput::err(format!(
                "lsp_rename: move failed ({e}); ripple edits already applied: \
                 {ripple_files} file(s)"
            ));
        }
        if let Ok(meta) = std::fs::metadata(&to) {
            ctx.ledger.lock().mint(&to, &meta);
        }
        // journal the new path as a creation: /undo must remove the
        // moved copy, or restoring the old path leaves the file
        // duplicated
        if let Err(e) = ctx.snapshots.lock().record_creation(&to) {
            return ToolOutput::err(format!(
                "lsp_rename: journaling the move failed ({e}); the move happened — \
                 undo cannot remove {}; delete it manually if undesired",
                rel(ctx, &to)
            ));
        }
        // tell the servers: drop the old URI, the file moved
        self.0.close_document(&from).await;
        for lang in self.0.configured_languages() {
            let _ = self
                .0
                .notify(&lang, "workspace/didRenameFiles", params.clone())
                .await;
        }
        let diags = diagnostics_note(&self.0, &to).await;
        let mut out = format!(
            "moved {} → {} (ripple edits in {ripple_files} file(s))\n{}\n{}",
            rel(ctx, &from),
            rel(ctx, &to),
            render_notes(&notes),
            diags
        );
        if !skipped.is_empty() {
            out.push_str(&format!("\n(skipped {})", skipped.join("; ")));
        }
        ToolOutput::ok(out)
    }
}

/// The `lsp_actions` hand: list available code actions at a position
/// (Read tier), or run one (Write tier) — applying the server's
/// `WorkspaceEdit` or executing its command, including any
/// `workspace/applyEdit` the server sends while the command runs.
pub struct ActionsHand(Arc<LspManager>);

impl Hand for ActionsHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "lsp_actions".to_string(),
            description: "Language-server code actions (quick fixes, refactors) at a file \
                position. Default mode lists what the server offers. mode \"run\" applies \
                one: pick by 1-based number from the listing or exact title. Symbol is \
                optional — omit it to target the whole line."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": "File to act on" },
                    "line": { "type": "integer", "description": "1-based line" },
                    "symbol": { "type": "string", "description": "Identifier on that line (optional)" },
                    "mode": { "type": "string", "description": "\"list\" (default) or \"run\"" },
                    "pick": { "type": ["integer", "string"], "description": "1-based index from the listing, or exact title (mode run)" }
                },
                "required": ["file", "line"]
            }),
            clearance: super::Clearance::Read,
            read_only: false,
        }
    }

    /// Listing reads; running a code action mutates (Write tier).
    fn clearance_for(&self, args: &Value) -> super::Clearance {
        if args.get("mode").and_then(Value::as_str) == Some("run") {
            super::Clearance::Write
        } else {
            super::Clearance::Read
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        ctx: &'a HandContext,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            let (Some(file), Some(line)) = (
                args.get("file").and_then(Value::as_str),
                args.get("line").and_then(Value::as_u64),
            ) else {
                return ToolOutput::err("lsp_actions: need 'file' and 'line' (1-based)");
            };
            let (path, uri, language, range) = match resolve_anchor(
                &self.0,
                ctx,
                file,
                line,
                args.get("symbol").and_then(Value::as_str),
            )
            .await
            {
                Ok(v) => v,
                Err(e) => return ToolOutput::err(format!("lsp_actions: {e}")),
            };
            let actions = match self
                .0
                .request(
                    &language,
                    "textDocument/codeAction",
                    json!({
                        "textDocument": { "uri": uri },
                        "range": range,
                        "context": { "diagnostics": [], "triggerKind": 1 }
                    }),
                )
                .await
            {
                Ok(r) => r,
                Err(e) => return ToolOutput::err(format!("lsp_actions: {e}")),
            };
            let Some(items) = actions.as_array() else {
                return ToolOutput::ok("no code actions offered here".to_string());
            };
            let mode = args.get("mode").and_then(Value::as_str).unwrap_or("list");
            if mode != "run" {
                return list_actions(items);
            }
            // run: pick by 1-based index or exact title; a non-positive
            // or non-integral number is invalid, not "the first one" —
            // picking is a Write-tier mutation
            let pick = args.get("pick");
            let idx = match pick {
                Some(Value::Number(n)) => match n.as_u64() {
                    Some(n) if n >= 1 => n - 1,
                    _ => u64::MAX,
                },
                Some(Value::String(title)) => items
                    .iter()
                    .position(|a| a.get("title").and_then(Value::as_str) == Some(title.as_str()))
                    .map(|i| i as u64)
                    .unwrap_or(u64::MAX),
                _ => u64::MAX,
            };
            let Some(action) = items.get(idx as usize) else {
                return ToolOutput::err(format!(
                    "lsp_actions: no action {pick:?} — list first, then pick by number or title"
                ));
            };
            self.run_action(&language, action, ctx, &path).await
        })
    }
}

impl ActionsHand {
    /// Run one code action: resolve `data`-only actions first, then
    /// apply the edit or execute the command (draining reverse
    /// `applyEdit` requests the whole time).
    async fn run_action<'a>(
        &self,
        language: &str,
        action: &Value,
        ctx: &'a HandContext,
        path: &Path,
    ) -> ToolOutput {
        let mut action = action.clone();
        if action.get("edit").is_none()
            && action.get("command").is_none()
            && action.get("data").is_some()
        {
            match self
                .0
                .request(language, "codeAction/resolve", action.clone())
                .await
            {
                Ok(resolved) => action = resolved,
                Err(e) => return ToolOutput::err(format!("lsp_actions: {e}")),
            }
        }
        let title = action
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("action")
            .to_string();
        let mut notes: Vec<String> = Vec::new();
        if let Some(edit) = action.get("edit") {
            match apply_workspace_edit(edit, &self.0, ctx, &mut notes).await {
                Ok(0) => {
                    return ToolOutput::ok(format!("ran {title:?}: server returned no changes"));
                }
                Ok(n) => notes.insert(0, format!("ran {title:?} — {n} file(s) changed")),
                Err(e) => return ToolOutput::err(format!("lsp_actions: {e}")),
            }
        } else if let Some(command) = action.get("command") {
            let params = json!({
                "command": command.get("command").cloned().unwrap_or(Value::Null),
                "arguments": command.get("arguments").cloned().unwrap_or(Value::Null),
            });
            // the server may send workspace/applyEdit *while* the
            // command is outstanding — request_with_reverse claims and
            // applies those through the same write path
            let shared_notes = Arc::new(Mutex::new(Vec::<String>::new()));
            let applied_files = Arc::new(Mutex::new(0usize));
            let lsp = self.0.clone();
            let on_edit = {
                let notes = shared_notes.clone();
                let count = applied_files.clone();
                let lsp = lsp.clone();
                move |edit: Value| -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
                    let notes = notes.clone();
                    let count = count.clone();
                    let lsp = lsp.clone(); // cheap Arc clone; the async block owns it
                    Box::pin(async move {
                        let mut n = Vec::new();
                        match apply_workspace_edit(&edit, &lsp, ctx, &mut n).await {
                            Ok(k) => {
                                *count.lock() += k;
                                notes.lock().extend(n);
                                true
                            }
                            Err(e) => {
                                notes.lock().push(format!("[applyEdit refused: {e}]"));
                                false
                            }
                        }
                    })
                }
            };
            match self
                .0
                .request_with_reverse(language, "workspace/executeCommand", params, on_edit)
                .await
            {
                Ok(_) => {
                    // a reverse request can also land right at the end
                    for (rid, edit) in self.0.claim_reverse_edits(language) {
                        let mut n = Vec::new();
                        let applied = match apply_workspace_edit(&edit, &self.0, ctx, &mut n).await
                        {
                            Ok(k) => {
                                *applied_files.lock() += k;
                                true
                            }
                            Err(e) => {
                                n.push(format!("[applyEdit refused: {e}]"));
                                false
                            }
                        };
                        shared_notes.lock().extend(n);
                        let result = if applied {
                            json!({ "applied": true })
                        } else {
                            json!({ "applied": false, "failureReason": "ka: edit refused" })
                        };
                        let _ = self.0.respond_request(language, rid, result).await;
                    }
                    notes.push(format!(
                        "ran command {title:?} ({} file(s) changed by applyEdit)",
                        *applied_files.lock()
                    ));
                }
                Err(e) => return ToolOutput::err(format!("lsp_actions: {e}")),
            }
            notes.extend(shared_notes.lock().clone());
        } else {
            return ToolOutput::err(format!(
                "lsp_actions: server returned neither edit nor command for {title:?}"
            ));
        }
        let diags = diagnostics_note(&self.0, path).await;
        ToolOutput::ok(format!(
            "{}\n{}\n{}",
            notes[0],
            render_notes(&notes[1..]),
            diags
        ))
    }
}

/// The `lsp_format` hand: whole-file `textDocument/formatting` through
/// the same ledger path as every server-proposed edit (drift refusal,
/// snapshot, caps, diff).
pub struct FormatHand(Arc<LspManager>);

impl Hand for FormatHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "lsp_format".to_string(),
            description: "Format a file through the language server (textDocument/formatting). \
                The server's edits apply through the same ledger path as every write: files \
                read-then-drifted refuse, the pre-format bytes are snapshotted, a diff is shown. \
                tabSize/insertSpaces are client hints; servers configured on their own (\
                rust-analyzer.toml, pyrightconfig) keep their settings."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": "File to format" },
                    "tab_size": { "type": "integer", "description": "Client hint (default 4)" },
                    "insert_spaces": { "type": "boolean", "description": "Client hint (default true)" }
                },
                "required": ["file"]
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
            let Some(file) = args.get("file").and_then(Value::as_str) else {
                return ToolOutput::err("lsp_format: need 'file'");
            };
            let path = super::protected::normalize(&super::read::resolve(ctx, file));
            if !path.is_file() {
                return ToolOutput::err(format!("lsp_format: no file {file}"));
            }
            let uri = match self.0.open_if_needed(&path).await {
                Ok(u) => u,
                Err(e) => return ToolOutput::err(format!("lsp_format: {e}")),
            };
            let language = lsp::language_for(&path)
                .map(str::to_string)
                .unwrap_or_default();
            let params = json!({
                "textDocument": { "uri": uri },
                "options": {
                    "tabSize": args.get("tab_size").and_then(Value::as_u64).unwrap_or(4),
                    "insertSpaces": args
                        .get("insert_spaces")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                }
            });
            let edits = match self
                .0
                .request(&language, "textDocument/formatting", params)
                .await
            {
                Ok(r) => r,
                Err(e) => return ToolOutput::err(format!("lsp_format: {e}")),
            };
            let offered = edits.as_array().is_some_and(|a| !a.is_empty());
            if !offered {
                return ToolOutput::ok(format!(
                    "lsp_format: {} is already formatted",
                    rel(ctx, &path)
                ));
            }
            // reuse the whole write path: wrap the TextEdit array in the
            // `changes` shape the applier parses
            let wrapped = json!({ "changes": { uri: edits } });
            let mut notes = Vec::new();
            match apply_workspace_edit(&wrapped, &self.0, ctx, &mut notes).await {
                Ok(0) => {
                    ToolOutput::ok(format!("lsp_format: {} already formatted", rel(ctx, &path)))
                }
                Ok(n) => {
                    let diags = diagnostics_note(&self.0, &path).await;
                    ToolOutput::ok(format!(
                        "formatted {} ({} edit(s))\n{}\n{}",
                        rel(ctx, &path),
                        n,
                        render_notes(&notes),
                        diags
                    ))
                }
                Err(e) => ToolOutput::err(format!("lsp_format: {e}")),
            }
        })
    }
}

/// Render a code-action listing (the `list` mode).
fn list_actions(items: &[Value]) -> ToolOutput {
    if items.is_empty() {
        return ToolOutput::ok("no code actions offered here".to_string());
    }
    let mut rows = Vec::new();
    for (i, a) in items.iter().enumerate().take(MAX_ACTIONS) {
        let title = a.get("title").and_then(Value::as_str).unwrap_or("?");
        let kind = a.get("kind").and_then(Value::as_str).unwrap_or("action");
        let preferred = if a.get("isPreferred").and_then(Value::as_bool) == Some(true) {
            " *"
        } else {
            ""
        };
        rows.push(format!("{}. {} ({kind}){preferred}", i + 1, title));
    }
    let mut out = rows.join("\n");
    if items.len() > MAX_ACTIONS {
        out.push_str(&format!("\n(+{} more)", items.len() - MAX_ACTIONS));
    }
    out.push_str("\n(run with mode \"run\" and pick a number or title)");
    ToolOutput::ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::hands::{Ledger, Spill};

    fn ctx_for(dir: &std::path::Path) -> HandContext {
        HandContext {
            cwd: dir.to_path_buf(),
            ledger: Arc::new(Mutex::new(Ledger::default())),
            spill: Arc::new(Spill::new()),
            snapshots: Arc::new(Mutex::new(crate::hands::snapshots::Snapshots::inert())),
            jobs: Arc::new(crate::hands::jobs::JobTable::new()),
            bash_background_ms: 0,
            max_image_mb: 5,
            web_allow_private: false,
            sandbox: ka_sandbox::Policy::Off,
            shell_env: Default::default(),
        }
    }

    fn edit_json(path: &Path, sl: u64, sc: u64, el: u64, ec: u64, new_text: &str) -> Value {
        json!({
            "changes": {
                lsp::uri_for(path): [{
                    "range": {
                        "start": { "line": sl, "character": sc },
                        "end": { "line": el, "character": ec }
                    },
                    "newText": new_text
                }]
            }
        })
    }

    fn dir_for(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ka-lspw-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An inert manager for direct applier tests: `touch` is a no-op
    /// there, which is fine — the fake-server tests cover the sync
    /// path.
    fn mgr() -> LspManager {
        LspManager::new(Path::new("/tmp"), &crate::config::Lsp::default())
    }

    #[cfg(unix)]
    fn fake_write_server(dir: &std::path::Path) -> LspManager {
        fake_server(dir, false)
    }

    #[tokio::test]
    async fn applies_edits_and_mints_ledger() {
        let dir = dir_for("apply");
        let f = dir.join("a.rs");
        std::fs::write(&f, "fn old() {}\n").unwrap();
        let ctx = ctx_for(&dir);
        // model read this file: it is ledger-tracked
        let meta = std::fs::metadata(&f).unwrap();
        ctx.ledger.lock().mint(&f, &meta);

        let mut notes = Vec::new();
        let n = apply_workspace_edit(&edit_json(&f, 0, 3, 0, 6, "new"), &mgr(), &ctx, &mut notes)
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "fn new() {}\n");
        assert!(!notes[0].is_empty(), "a diff note is rendered");
        // ledger re-minted: a follow-up verify passes
        assert!(ctx.ledger.lock().verify(&f).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unread_ripple_files_are_server_vouched() {
        let dir = dir_for("ripple");
        let f = dir.join("b.rs");
        std::fs::write(&f, "use x::old;\n").unwrap();
        let ctx = ctx_for(&dir);
        // never read: not in the ledger — still allowed (server-vouched)
        let mut notes = Vec::new();
        let n = apply_workspace_edit(&edit_json(&f, 0, 7, 0, 10, "new"), &mgr(), &ctx, &mut notes)
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "use x::new;\n");
        // and now tracked for follow-up edits
        assert!(ctx.ledger.lock().verify(&f).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn drifted_file_refuses_whole_edit() {
        let dir = dir_for("drift");
        let tracked = dir.join("tracked.rs");
        let other = dir.join("other.rs");
        std::fs::write(&tracked, "fn old() {}\n").unwrap();
        std::fs::write(&other, "use x::old;\n").unwrap();
        let ctx = ctx_for(&dir);
        let meta = std::fs::metadata(&tracked).unwrap();
        ctx.ledger.lock().mint(&tracked, &meta);
        // the tracked file changes after the read
        std::fs::write(&tracked, "fn old() { /* user edit */ }\n").unwrap();

        let edit = json!({
            "changes": {
                lsp::uri_for(&tracked): [{
                    "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 6}},
                    "newText": "new"
                }],
                lsp::uri_for(&other): [{
                    "range": {"start": {"line": 0, "character": 7}, "end": {"line": 0, "character": 10}},
                    "newText": "new"
                }],
            }
        });
        let mut notes = Vec::new();
        let err = apply_workspace_edit(&edit, &mgr(), &ctx, &mut notes)
            .await
            .unwrap_err();
        assert!(err.contains("changed since"), "{err}");
        // nothing was applied — validation precedes mutation
        assert_eq!(
            std::fs::read_to_string(&other).unwrap(),
            "use x::old;\n",
            "no partial application"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn protected_and_outside_paths_refuse() {
        let dir = dir_for("protected");
        let hook = dir.join(".git/hooks/pre-commit");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(&hook, "#!/bin/sh\n").unwrap();
        let ctx = ctx_for(&dir);
        let m = mgr();
        let mut notes = Vec::new();
        let err = apply_workspace_edit(&edit_json(&hook, 0, 0, 0, 0, "evil"), &m, &ctx, &mut notes)
            .await
            .unwrap_err();
        assert!(err.contains("protected"), "{err}");

        let outside = std::env::temp_dir().join(format!("ka-outside-{}", std::process::id()));
        std::fs::write(&outside, "x\n").unwrap();
        let err = apply_workspace_edit(&edit_json(&outside, 0, 0, 0, 1, "y"), &m, &ctx, &mut notes)
            .await
            .unwrap_err();
        assert!(err.contains("outside the working directory"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&outside);
    }

    /// A fake language server for write-through round trips: acks
    /// initialize, then answers per-method canned behaviors driven by a
    /// python script. Covers rename (WorkspaceEdit), codeAction list,
    /// and the executeCommand → applyEdit reverse-request path. With
    /// `ripple`, willRenameFiles answers a small WorkspaceEdit (first
    /// char of the moved file → `y`, plus importer.rs's `x` → `y`);
    /// otherwise it answers null immediately (the fast "no ripples"
    /// path — ripple application itself is covered by the applier
    /// tests).
    #[cfg(unix)]
    fn fake_server(dir: &std::path::Path, ripple: bool) -> LspManager {
        let will_rename = if ripple {
            r#"    elif method == "workspace/willRenameFiles":
        old = msg["params"]["files"][0]["oldUri"]
        imp = "file://" + root + "/importer.rs"
        reply(msg, {"changes": {old: [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "newText": "y"}], imp: [{"range": {"start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 5}}, "newText": "y"}]}})
"#
        } else {
            r#"    elif method == "workspace/willRenameFiles":
        reply(msg, None)
"#
        };
        let fake = dir.join("fake.py");
        let root = dir.display().to_string().replace('\\', "/");
        std::fs::write(
            &fake,
            format!(
                r#"import sys, json
root = {root:?}
def send(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(("Content-Length: %d\r\n\r\n" % len(b)).encode() + b)
    sys.stdout.buffer.flush()
def reply(msg, result):
    send({{"jsonrpc": "2.0", "id": msg["id"], "result": result}})
while True:
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line in (b"\r\n", b"\n"):
            break
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":")[1].strip())
    if length is None:
        continue
    msg = json.loads(sys.stdin.buffer.read(length))
    method = msg.get("method")
    if method == "initialize":
        reply(msg, {{"capabilities": {{}}}})
    elif method == "textDocument/rename":
        uri = msg["params"]["textDocument"]["uri"]
        new = msg["params"]["newName"]
        reply(msg, {{"changes": {{uri: [
            {{"range": {{"start": {{"line": 0, "character": 8}}, "end": {{"line": 0, "character": 11}}}},
              "newText": new}}]}}}})
    elif method == "textDocument/codeAction":
        uri = msg["params"]["textDocument"]["uri"]
        reply(msg, [
            {{"title": "Import fix", "kind": "quickfix",
              "edit": {{"changes": {{uri: [
                  {{"range": {{"start": {{"line": 0, "character": 0}}, "end": {{"line": 0, "character": 0}}}},
                   "newText": "use fixed;\n"}}]}}}}}},
            {{"title": "Run organizer", "kind": "source",
              "command": {{"title": "Run organizer", "command": "organize"}}}}])
    elif method == "workspace/executeCommand":
        uri = "file://" + root + "/x.rs"
        # the omp pattern: send applyEdit as a reverse request BEFORE
        # answering the command
        send({{"jsonrpc": "2.0", "id": 9001, "method": "workspace/applyEdit",
              "params": {{"edit": {{"changes": {{uri: [
                  {{"range": {{"start": {{"line": 0, "character": 0}}, "end": {{"line": 0, "character": 0}}}},
                   "newText": "// organized\n"}}]}}}}}}}})
        reply(msg, None)
    elif method == "textDocument/prepareRename":
        # char 0 is "not renameable" in this fixture
        if msg["params"]["position"]["character"] == 0:
            reply(msg, None)
        else:
            reply(msg, {{"range": {{"start": {{"line": 0, "character": 8}}, "end": {{"line": 0, "character": 11}}}}, "placeholder": "old"}})
    elif method == "textDocument/formatting":
        # tabSize 2 means "already clean" in this fixture
        if msg["params"]["options"]["tabSize"] == 2:
            reply(msg, None)
        else:
            uri = msg["params"]["textDocument"]["uri"]
            reply(msg, [{{"range": {{"start": {{"line": 0, "character": 0}}, "end": {{"line": 0, "character": 0}}}}, "newText": "formatted "}}, {{"range": {{"start": {{"line": 1, "character": 0}}, "end": {{"line": 3, "character": 0}}}}, "newText": ""}}])
{will_rename}    # notifications and unknown requests: no response
"#
            ),
        )
        .unwrap();
        LspManager::new(
            dir,
            &crate::config::Lsp {
                enable: Some(true),
                commands: Some(
                    [("rust".to_string(), format!("python3 {}", fake.display()))]
                        .into_iter()
                        .collect(),
                ),
                write_through: Some(true),
            },
        )
    }

    #[cfg(unix)]
    fn which_python3() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Rename round trip through the fake server: the WorkspaceEdit is
    /// applied to disk with a diff and the ledger is fresh afterwards.
    #[cfg(unix)]
    #[tokio::test]
    async fn rename_round_trip_via_fake_server() {
        if !which_python3() {
            return;
        }
        let dir = dir_for("rename");
        std::fs::write(dir.join("x.rs"), "use ka::old;\nfn main() {}\n").unwrap();
        let mgr = Arc::new(fake_write_server(&dir));
        mgr.start_all();
        let ctx = ctx_for(&dir);
        let out = RenameHand(mgr.clone())
            .execute(
                &json!({"file": "x.rs", "line": 1, "symbol": "old", "new_name": "fresh"}),
                &ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("renamed old → fresh across 1 file(s)"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("-use ka::old;") && out.content.contains("+use ka::fresh;"),
            "diff in result: {}",
            out.content
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("x.rs")).unwrap(),
            "use ka::fresh;\nfn main() {}\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Code actions: listing is Read tier; running the command action
    /// survives the applyEdit reverse request sent *before* the command
    /// response (the deadlock guard) and applies it through the write
    /// path.
    #[cfg(unix)]
    #[tokio::test]
    async fn actions_list_and_run_with_reverse_apply_edit() {
        if !which_python3() {
            return;
        }
        let dir = dir_for("actions");
        std::fs::write(dir.join("x.rs"), "fn main() {}\n").unwrap();
        let mgr = Arc::new(fake_write_server(&dir));
        mgr.start_all();
        let ctx = ctx_for(&dir);
        let hand = ActionsHand(mgr.clone());
        let args = json!({"file": "x.rs", "line": 1});
        // listing: Read tier, numbered titles
        assert_eq!(hand.clearance_for(&args), super::super::Clearance::Read);
        let out = hand.execute(&args, &ctx).await;
        assert!(
            out.content.contains("1. Import fix (quickfix)"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("2. Run organizer (source)"),
            "{}",
            out.content
        );
        // running: Write tier
        let run_args = json!({"file": "x.rs", "line": 1, "mode": "run", "pick": 2});
        assert_eq!(
            hand.clearance_for(&run_args),
            super::super::Clearance::Write
        );
        let out = hand.execute(&run_args, &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("ran command"), "{}", out.content);
        // the reverse applyEdit landed on disk
        assert_eq!(
            std::fs::read_to_string(dir.join("x.rs")).unwrap(),
            "// organized\nfn main() {}\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The edit-backed action applies its WorkspaceEdit directly.
    #[cfg(unix)]
    #[tokio::test]
    async fn actions_run_edit_backed_action() {
        if !which_python3() {
            return;
        }
        let dir = dir_for("action-edit");
        std::fs::write(dir.join("x.rs"), "fn main() {}\n").unwrap();
        let mgr = Arc::new(fake_write_server(&dir));
        mgr.start_all();
        let ctx = ctx_for(&dir);
        let out = ActionsHand(mgr.clone())
            .execute(
                &json!({"file": "x.rs", "line": 1, "mode": "run", "pick": "Import fix"}),
                &ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.join("x.rs")).unwrap(),
            "use fixed;\nfn main() {}\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// File moves consult willRenameFiles, apply ripple edits, move,
    /// then notify didRenameFiles. The fake answers willRenameFiles
    /// with null (no ripple) — this covers the plumbing, not the ripple
    /// application (already covered by the applier tests).
    #[cfg(unix)]
    #[tokio::test]
    async fn file_move_round_trip() {
        if !which_python3() {
            return;
        }
        let dir = dir_for("move");
        std::fs::write(dir.join("x.rs"), "fn main() {}\n").unwrap();
        let mgr = Arc::new(fake_write_server(&dir));
        mgr.start_all();
        let ctx = ctx_for(&dir);
        let out = RenameHand(mgr.clone())
            .execute(
                &json!({"kind": "file", "from": "x.rs", "to": "y/deep.rs"}),
                &ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("moved x.rs → y/deep.rs"),
            "{}",
            out.content
        );
        assert!(dir.join("y/deep.rs").is_file());
        assert!(!dir.join("x.rs").exists());
        // refuse to overwrite an existing target
        std::fs::write(dir.join("z.rs"), "x\n").unwrap();
        let out = RenameHand(mgr.clone())
            .execute(
                &json!({"kind": "file", "from": "y/deep.rs", "to": "z.rs"}),
                &ctx,
            )
            .await;
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("already exists"), "{}", out.content);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Tier contract: rename is always Write; actions flip per mode.
    #[test]
    fn tier_contracts() {
        let mgr = Arc::new(LspManager::new(
            Path::new("/tmp"),
            &crate::config::Lsp::default(),
        ));
        assert_eq!(
            RenameHand(mgr.clone()).def().clearance,
            super::super::Clearance::Write
        );
        assert!(!RenameHand(mgr.clone()).def().read_only);
        let actions = ActionsHand(mgr.clone());
        assert_eq!(
            actions.def().clearance,
            super::super::Clearance::Read,
            "static tier = listing"
        );
        assert_eq!(
            actions.clearance_for(&json!({"file": "a.rs", "line": 1})),
            super::super::Clearance::Read
        );
        assert_eq!(
            actions.clearance_for(&json!({"file": "a.rs", "line": 1, "mode": "run", "pick": 1})),
            super::super::Clearance::Write
        );
        assert_eq!(
            FormatHand(mgr).def().clearance,
            super::super::Clearance::Write,
            "formatting rewrites the file"
        );
    }

    /// The drift check survives path-form divergence: the ledger keyed
    /// the canonical spelling (the model read through the real dir),
    /// the server sends the symlinked one — tracked either way.
    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_spelling_still_drift_checks() {
        let dir = dir_for("canon");
        let real = dir.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let f = real.join("a.rs");
        std::fs::write(&f, "fn old() {}\n").unwrap();
        std::os::unix::fs::symlink(&real, dir.join("link")).unwrap();
        let ctx = ctx_for(&dir);
        let meta = std::fs::metadata(&f).unwrap();
        ctx.ledger.lock().mint(&f, &meta);

        // via the link, unchanged: allowed — tracked through canonicalize
        let via_link = dir.join("link").join("a.rs");
        let mut notes = Vec::new();
        let n = apply_workspace_edit(
            &edit_json(&via_link, 0, 3, 0, 6, "new"),
            &mgr(),
            &ctx,
            &mut notes,
        )
        .await
        .unwrap();
        assert_eq!(n, 1, "symlinked spelling resolves to the tracked file");

        // drifted: refused through the same spelling
        std::fs::write(&f, "fn old() {{ /* drifted */ }}\n").unwrap();
        let mut notes = Vec::new();
        let err = apply_workspace_edit(
            &edit_json(&via_link, 0, 3, 0, 6, "new"),
            &mgr(),
            &ctx,
            &mut notes,
        )
        .await
        .unwrap_err();
        assert!(err.contains("changed since"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// /undo covers ripple files: a live snapshot journal records every
    /// changed file and undo pops them newest-first.
    #[tokio::test]
    async fn undo_restores_every_changed_file() {
        let data = dir_for("snap-data");
        ka_strand::set_data_dir_for_tests(data.clone());
        let dir = dir_for("undo");
        let a = dir.join("a.rs");
        let b = dir.join("b.rs");
        std::fs::write(&a, "use x::old;\n").unwrap();
        std::fs::write(&b, "use y::old;\n").unwrap();
        let mut snaps = crate::hands::snapshots::Snapshots::open(&dir);
        snaps.set_strand("s-test");
        let ctx = HandContext {
            snapshots: Arc::new(Mutex::new(snaps)),
            ..ctx_for(&dir)
        };
        let edit = json!({
            "changes": {
                lsp::uri_for(&a): [{ "range": {"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 9}}, "newText": "new" }],
                lsp::uri_for(&b): [{ "range": {"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 9}}, "newText": "new" }]
            }
        });
        let mut notes = Vec::new();
        let n = apply_workspace_edit(&edit, &mgr(), &ctx, &mut notes)
            .await
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(
            ctx.snapshots.lock().entries().len(),
            2,
            "one snapshot per changed file"
        );
        let e1 = ctx.snapshots.lock().undo().unwrap().unwrap();
        assert_eq!(e1.path, b);
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "use y::old;\n");
        let e2 = ctx.snapshots.lock().undo().unwrap().unwrap();
        assert_eq!(e2.path, a);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "use x::old;\n");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// A move journaled with record_creation undoes cleanly: /undo
    /// removes the moved copy, the next undo restores the old path —
    /// the file is not duplicated after an undo.
    #[tokio::test]
    async fn move_undo_removes_the_moved_copy() {
        let data = dir_for("snap-move");
        ka_strand::set_data_dir_for_tests(data.clone());
        let dir = dir_for("move-undo");
        let from = dir.join("x.rs");
        std::fs::write(&from, "body\n").unwrap();
        let mut snaps = crate::hands::snapshots::Snapshots::open(&dir);
        snaps.set_strand("s-test");
        let to = dir.join("y.rs");
        // the move: snapshot old → rename → journal the creation
        snaps.snapshot(&from).unwrap();
        std::fs::rename(&from, &to).unwrap();
        snaps.record_creation(&to).unwrap();
        let e1 = snaps.undo().unwrap().unwrap();
        assert_eq!(e1.path, to);
        assert!(!to.exists(), "undo removes the moved copy");
        let e2 = snaps.undo().unwrap().unwrap();
        assert_eq!(e2.path, from);
        assert_eq!(std::fs::read_to_string(&from).unwrap(), "body\n");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// Ripple edits land BEFORE the move: the fake rewrites the moved
    /// file's first character, and only the moved copy carries it. The
    /// sibling import rewrite applies too.
    #[cfg(unix)]
    #[tokio::test]
    async fn file_move_applies_ripple_edits_before_the_move() {
        if !which_python3() {
            return;
        }
        let dir = dir_for("move-ripple");
        std::fs::write(dir.join("x.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("importer.rs"), "use x::main;\n").unwrap();
        let mgr = Arc::new(fake_server(&dir, true));
        mgr.start_all();
        let ctx = ctx_for(&dir);
        let out = RenameHand(mgr.clone())
            .execute(
                &json!({"kind": "file", "from": "x.rs", "to": "y/deep.rs"}),
                &ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        // the moved copy carries the rewrite of the OLD path — proof
        // the ripple edit applied before std::fs::rename
        assert_eq!(
            std::fs::read_to_string(dir.join("y/deep.rs")).unwrap(),
            "yn main() {}\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("importer.rs")).unwrap(),
            "use y::main;\n"
        );
        assert!(!dir.join("x.rs").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A drifted ripple file aborts the whole move: validation precedes
    /// mutation, so neither the ripple edit nor the move lands.
    #[cfg(unix)]
    #[tokio::test]
    async fn file_move_aborts_when_ripple_file_drifted() {
        if !which_python3() {
            return;
        }
        let dir = dir_for("move-abort");
        std::fs::write(dir.join("x.rs"), "fn main() {}\n").unwrap();
        let imp = dir.join("importer.rs");
        std::fs::write(&imp, "use x::main;\n").unwrap();
        let mgr = Arc::new(fake_server(&dir, true));
        mgr.start_all();
        let ctx = ctx_for(&dir);
        // the model read importer.rs; it changed since
        let meta = std::fs::metadata(&imp).unwrap();
        ctx.ledger.lock().mint(&imp, &meta);
        std::fs::write(&imp, "use x::main; // drifted\n").unwrap();
        let out = RenameHand(mgr.clone())
            .execute(
                &json!({"kind": "file", "from": "x.rs", "to": "y/deep.rs"}),
                &ctx,
            )
            .await;
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("partially applied"), "{}", out.content);
        assert!(dir.join("x.rs").is_file(), "the move was aborted");
        assert!(!dir.join("y").exists());
        assert_eq!(
            std::fs::read_to_string(&imp).unwrap(),
            "use x::main; // drifted\n",
            "no partial application"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The moved file itself honors the def's drift promise.
    #[cfg(unix)]
    #[tokio::test]
    async fn file_move_refuses_a_drifted_source() {
        if !which_python3() {
            return;
        }
        let dir = dir_for("move-drift");
        let x = dir.join("x.rs");
        std::fs::write(&x, "fn main() {}\n").unwrap();
        let mgr = Arc::new(fake_server(&dir, false));
        mgr.start_all();
        let ctx = ctx_for(&dir);
        let meta = std::fs::metadata(&x).unwrap();
        ctx.ledger.lock().mint(&x, &meta);
        std::fs::write(&x, "fn main() {{ /* drifted */ }}\n").unwrap();
        let out = RenameHand(mgr.clone())
            .execute(&json!({"kind": "file", "from": "x.rs", "to": "y.rs"}), &ctx)
            .await;
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("move refused"), "{}", out.content);
        assert!(x.is_file(), "nothing moved");
        assert!(!dir.join("y.rs").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `pick` is validated: 0 / out-of-range are an error, never a
    /// silent "run the first action".
    #[cfg(unix)]
    #[tokio::test]
    async fn invalid_pick_never_runs_the_first_action() {
        if !which_python3() {
            return;
        }
        let dir = dir_for("pick");
        let x = dir.join("x.rs");
        std::fs::write(&x, "fn main() {}\n").unwrap();
        let mgr = Arc::new(fake_server(&dir, false));
        mgr.start_all();
        let ctx = ctx_for(&dir);
        for bad in [0u64, 3] {
            let out = ActionsHand(mgr.clone())
                .execute(
                    &json!({"file": "x.rs", "line": 1, "mode": "run", "pick": bad}),
                    &ctx,
                )
                .await;
            assert!(out.is_error, "pick {bad} must error: {}", out.content);
            assert!(out.content.contains("no action"), "{}", out.content);
        }
        assert_eq!(
            std::fs::read_to_string(&x).unwrap(),
            "fn main() {}\n",
            "nothing ran"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whole-file formatting through the applier: the server's edits
    /// land with a diff; a clean answer reports "already formatted".
    #[cfg(unix)]
    #[tokio::test]
    async fn format_round_trip_and_already_clean() {
        if !which_python3() {
            return;
        }
        let dir = dir_for("format");
        let x = dir.join("x.rs");
        std::fs::write(&x, "fn main() {}\n\n").unwrap();
        let mgr = Arc::new(fake_server(&dir, false));
        mgr.start_all();
        let ctx = ctx_for(&dir);
        let out = FormatHand(mgr.clone())
            .execute(&json!({"file": "x.rs"}), &ctx)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(&x).unwrap(),
            "formatted fn main() {}\n"
        );
        assert!(
            out.content.contains("+formatted fn main()"),
            "{}",
            out.content
        );
        // tab_size 2 is the fixture's "already clean" answer
        let out = FormatHand(mgr)
            .execute(&json!({"file": "x.rs", "tab_size": 2}), &ctx)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("already formatted"), "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(&x).unwrap(),
            "formatted fn main() {}\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// prepareRename precheck: char 0 is "not renameable" in the
    /// fixture — the hand refuses before any rename round trip.
    #[cfg(unix)]
    #[tokio::test]
    async fn prepare_rename_refusal() {
        if !which_python3() {
            return;
        }
        let dir = dir_for("prepare");
        let x = dir.join("x.rs");
        std::fs::write(&x, "old(x) {}\n").unwrap();
        let mgr = Arc::new(fake_server(&dir, false));
        mgr.start_all();
        let ctx = ctx_for(&dir);
        let out = RenameHand(mgr.clone())
            .execute(
                &json!({"file": "x.rs", "line": 1, "symbol": "old", "new_name": "new"}),
                &ctx,
            )
            .await;
        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.contains("not a renameable symbol"),
            "{}",
            out.content
        );
        assert_eq!(
            std::fs::read_to_string(&x).unwrap(),
            "old(x) {}\n",
            "nothing renamed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
