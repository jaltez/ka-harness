//! LSP navigation hands: `symbols`, `definition`, `references`, and a
//! pull-based `diagnostics`. The budget-safe repo map — every query
//! rides the hand-rolled client in [`crate::lsp`] (no new dependency)
//! and answers like an IDE would, so the model stops grep-guessing
//! where symbols live (the aider repo-map / crush `definition`+`
//! references` lesson, without tree-sitter's weight).
//!
//! All four are read-only, Read-tier hands. Errors are ordinary tool
//! errors ("no [lsp.commands] entry for rust"); they never block a turn
//! that does not opt into them because the engine only registers these
//! hands when `[lsp] enable = true` and at least one command is
//! configured.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};

use super::{Hand, HandContext, HandDef, ToolOutput};
use crate::lsp::{
    self, LspManager, identifier_byte_offset, kind_label, parse_locations, parse_symbols,
    path_of_uri, utf16_column,
};

/// Max rendered symbol hits.
const MAX_SYMBOLS: usize = 30;
/// Max rendered definition targets.
const MAX_DEFINITIONS: usize = 10;
/// Max rendered reference sites (+ a counted trailer beyond that).
const MAX_REFERENCES: usize = 30;
/// Max files in the project diagnostics view.
const MAX_FILES: usize = 15;

/// Build the four navigation hands over one manager.
pub fn hands(lsp: Arc<LspManager>) -> Vec<Arc<dyn Hand>> {
    vec![
        Arc::new(SymbolsHand(lsp.clone())),
        Arc::new(DefinitionHand(lsp.clone())),
        Arc::new(ReferencesHand(lsp.clone())),
        Arc::new(DiagnosticsHand(lsp)),
    ]
}

/// Path display: relative to the cwd when inside it, else absolute.
fn display_path(ctx: &HandContext, uri: &str) -> String {
    let Some(path) = path_of_uri(uri) else {
        return uri.to_string();
    };
    let abs = Path::new(&path);
    match abs.strip_prefix(&ctx.cwd) {
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => path,
    }
}

/// Resolve `{file, line, symbol}` into the LSP query position: reads
/// the line from disk, finds the identifier with word boundaries, and
/// converts to 0-based line + UTF-16 character.
async fn resolve_position(
    lsp: &LspManager,
    ctx: &HandContext,
    file: &str,
    line: u64,
    symbol: &str,
) -> Result<(serde_json::Value, String), String> {
    let path = super::read::resolve(ctx, file);
    if !path.is_file() {
        return Err(format!("definition/references: no file {file}"));
    }
    if line == 0 {
        return Err("line numbers are 1-based".to_string());
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("read {file}: {e}"))?;
    let Some(line_text) = text.lines().nth((line - 1) as usize) else {
        return Err(format!("{file} has fewer than {line} lines"));
    };
    let Some(offset) = identifier_byte_offset(line_text, symbol) else {
        return Err(format!("{symbol:?} is not an identifier on {file}:{line}"));
    };
    let uri = lsp.open_if_needed(&path).await?;
    let language = lsp::language_for(&path)
        .map(str::to_string)
        .unwrap_or_default();
    Ok((
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": line - 1, "character": utf16_column(line_text, offset) },
        }),
        language,
    ))
}

/// The `symbols` hand: workspace-wide symbol search (the repo map on
/// demand — a ~30-line answer to "where does X live?").
pub struct SymbolsHand(Arc<LspManager>);

impl Hand for SymbolsHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "symbols".to_string(),
            description: "Search workspace symbols by name across configured language \
                servers (rust, python, ...). Returns `path:line name kind` hits — use it \
                to map where code lives before reading files."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Fuzzy/prefix name query (e.g. HandContext)" },
                    "language": { "type": "string", "description": "Restrict to one configured language (e.g. rust); default: all" }
                },
                "required": ["query"]
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
            let Some(query) = args.get("query").and_then(Value::as_str) else {
                return ToolOutput::err("symbols: missing required 'query'");
            };
            let languages = match args.get("language").and_then(Value::as_str) {
                Some(one) => vec![one.to_string()],
                None => self.0.configured_languages(),
            };
            if languages.is_empty() {
                return ToolOutput::err(
                    "symbols: no [lsp.commands] servers configured; add e.g. \
                     [lsp.commands] rust = \"rust-analyzer\" and set [lsp] enable = true"
                        .to_string(),
                );
            }
            let mut hits = Vec::new();
            let mut skipped = Vec::new();
            for lang in &languages {
                match self
                    .0
                    .request(lang, "workspace/symbol", json!({ "query": query }))
                    .await
                {
                    Ok(result) => hits.extend(parse_symbols(&result)),
                    Err(e) => skipped.push(format!("{lang}: {e}")),
                }
            }
            if hits.is_empty() {
                let mut out = format!("no workspace symbols matched {query:?}");
                if !skipped.is_empty() && skipped.len() == languages.len() {
                    out = skipped.join("; ");
                }
                return ToolOutput::ok(out);
            }
            let mut rows: Vec<String> = hits
                .iter()
                .map(|h| {
                    let container = h
                        .container
                        .as_deref()
                        .filter(|c| !c.is_empty())
                        .map(|c| format!(" in {c}"))
                        .unwrap_or_default();
                    format!(
                        "{}:{}  {} ({}{})",
                        display_path(ctx, &h.uri),
                        h.line + 1,
                        h.name,
                        kind_label(h.kind),
                        container
                    )
                })
                .collect();
            rows.sort();
            rows.dedup();
            let total = rows.len();
            rows.truncate(MAX_SYMBOLS);
            let mut out = rows.join("\n");
            if total > MAX_SYMBOLS {
                out.push_str(&format!("\n(+{} more)", total - MAX_SYMBOLS));
            }
            if !skipped.is_empty() && skipped.len() < languages.len() {
                out.push_str(&format!("\n(skipped {})", skipped.join("; ")));
            }
            ToolOutput::ok(out)
        })
    }
}

/// Shared body for definition/references: position resolution → one
/// LSP request → rendered locations.
async fn locate(
    lsp: &LspManager,
    ctx: &HandContext,
    args: &Value,
    method: &str,
    extra_params: Value,
    cap: usize,
    empty_note: &str,
) -> ToolOutput {
    let (Some(file), Some(line), Some(symbol)) = (
        args.get("file").and_then(Value::as_str),
        args.get("line").and_then(Value::as_u64),
        args.get("symbol").and_then(Value::as_str),
    ) else {
        return ToolOutput::err(format!("{method}: need 'file', 'line' (1-based), 'symbol'"));
    };
    let (text_document_position, language) =
        match resolve_position(lsp, ctx, file, line, symbol).await {
            Ok(v) => v,
            Err(e) => return ToolOutput::err(format!("{method}: {e}")),
        };
    let mut params = text_document_position;
    if let (Value::Object(map), Value::Object(extra)) = (&mut params, extra_params) {
        for (k, v) in extra {
            map.insert(k, v);
        }
    }
    let result = match lsp.request(&language, method, params).await {
        Ok(r) => r,
        Err(e) => return ToolOutput::err(format!("{method}: {e}")),
    };
    let mut locs = parse_locations(&result);
    locs.sort_by(|a, b| (&a.uri, a.line, a.character).cmp(&(&b.uri, b.line, b.character)));
    locs.dedup_by(|a, b| a.uri == b.uri && a.line == b.line && a.character == b.character);
    if locs.is_empty() {
        return ToolOutput::ok(empty_note.to_string());
    }
    let total = locs.len();
    let mut rows: Vec<String> = locs
        .iter()
        .map(|l| {
            format!(
                "{}:{}:{}",
                display_path(ctx, &l.uri),
                l.line + 1,
                l.character + 1
            )
        })
        .collect();
    rows.truncate(cap);
    let mut out = rows.join("\n");
    if total > cap {
        out.push_str(&format!("\n(+{} more of {total})", total - cap));
    }
    ToolOutput::ok(out)
}

/// The `definition` hand: where is this symbol defined?
pub struct DefinitionHand(Arc<LspManager>);

impl Hand for DefinitionHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "definition".to_string(),
            description: "Resolve the definition of `symbol` used on `file:line` \
                (1-based). Returns `path:line:col` targets — navigate like an IDE \
                instead of grepping."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": "File containing the use site" },
                    "line": { "type": "integer", "description": "1-based line of the identifier" },
                    "symbol": { "type": "string", "description": "Identifier text as it appears on that line" }
                },
                "required": ["file", "line", "symbol"]
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
            locate(
                &self.0,
                ctx,
                args,
                "textDocument/definition",
                json!({}),
                MAX_DEFINITIONS,
                "no definition found (built-in, external, or unresolved)",
            )
            .await
        })
    }
}

/// The `references` hand: where is this symbol used?
pub struct ReferencesHand(Arc<LspManager>);

impl Hand for ReferencesHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "references".to_string(),
            description: "Find usages of `symbol` at `file:line` (1-based), excluding \
                its declaration. Returns `path:line:col` sites, capped with a count."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": "File containing the symbol" },
                    "line": { "type": "integer", "description": "1-based line of the identifier" },
                    "symbol": { "type": "string", "description": "Identifier text as it appears on that line" }
                },
                "required": ["file", "line", "symbol"]
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
            locate(
                &self.0,
                ctx,
                args,
                "textDocument/references",
                json!({ "context": { "includeDeclaration": false } }),
                MAX_REFERENCES,
                "no references found",
            )
            .await
        })
    }
}

/// The `diagnostics` hand: pull current diagnostics on demand — one
/// file, or the whole project (everything the servers have published).
pub struct DiagnosticsHand(Arc<LspManager>);

impl Hand for DiagnosticsHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "diagnostics".to_string(),
            description: "Current language-server diagnostics: pass `file` for one \
                file, omit it for every file the servers have reported on. Run it \
                after edits to self-correct before the user sees errors."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": "One file (default: project-wide)" }
                }
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
            if let Some(file) = args.get("file").and_then(Value::as_str) {
                let path = super::read::resolve(ctx, file);
                return match self.0.diagnostics(&path) {
                    Some(lines) if lines.is_empty() => {
                        ToolOutput::ok("clean — no diagnostics for current content".to_string())
                    }
                    Some(lines) => ToolOutput::ok(lines.join("\n")),
                    None => ToolOutput::ok(
                        "no diagnostics published yet for this content (server may \
                         still be computing)"
                            .to_string(),
                    ),
                };
            }
            let all = self.0.all_diagnostics();
            if all.is_empty() {
                return ToolOutput::ok(
                    "no diagnostics cached — open/edit files so the servers track \
                     them, or pass a specific `file`"
                        .to_string(),
                );
            }
            let total_files = all.len();
            let mut out = String::new();
            for (idx, (path, diags)) in all.iter().enumerate() {
                if idx >= MAX_FILES {
                    out.push_str(&format!("\n(+{} more files)", total_files - MAX_FILES));
                    break;
                }
                if idx > 0 {
                    out.push('\n');
                }
                out.push_str(&format!(
                    "{} — {} finding(s)\n{}",
                    path,
                    diags.len(),
                    lsp::render_diags(diags).join("\n")
                ));
            }
            ToolOutput::ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn ctx_for(dir: &std::path::Path) -> HandContext {
        HandContext {
            cwd: dir.to_path_buf(),
            ledger: Arc::new(parking_lot::Mutex::new(super::super::Ledger::default())),
            spill: Arc::new(super::super::Spill::new()),
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

    #[test]
    fn identifier_offsets_respect_word_boundaries() {
        assert_eq!(
            identifier_byte_offset("let foo = foo_bar();", "foo"),
            Some(4)
        );
        assert_eq!(
            identifier_byte_offset("let foo = foo_bar();", "foo_bar"),
            Some(10)
        );
        assert_eq!(identifier_byte_offset("let foo = foo_bar();", "bar"), None);
        assert_eq!(identifier_byte_offset("use a::b;", "a"), Some(4));
    }

    #[test]
    fn utf16_columns_count_units_not_bytes() {
        // "é" is one UTF-16 unit but two bytes; "日" is one unit, three bytes
        let line = "let é = \"日\";";
        let offset = identifier_byte_offset(line, "日").unwrap();
        assert_eq!(utf16_column(line, offset), 9);
    }

    #[test]
    fn uri_round_trip_and_decode() {
        let p = "/tmp/s p/q.rs";
        assert_eq!(path_of_uri(&lsp::uri_for(Path::new(p))).as_deref(), Some(p));
        assert_eq!(path_of_uri("file:///a%2Fb").as_deref(), Some("/a/b"));
        assert_eq!(path_of_uri("https://x/y"), None);
    }

    #[test]
    fn symbols_and_locations_parse() {
        let result = json!([
            {
                "name": "Hand",
                "kind": 23,
                "containerName": "hands",
                "location": {
                    "uri": "file:///w/crates/hands/mod.rs",
                    "range": { "start": { "line": 40, "character": 0 } }
                }
            }
        ]);
        let hits = parse_symbols(&result);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Hand");
        assert_eq!(kind_label(hits[0].kind), "struct");
        // location shapes: single, array, LocationLink
        let single =
            json!({ "uri": "file:///a.rs", "range": { "start": { "line": 3, "character": 7 } } });
        assert_eq!(parse_locations(&single)[0].line, 3);
        let links = json!([{ "targetUri": "file:///b.rs", "targetRange": { "start": { "line": 9, "character": 1 } } }]);
        assert_eq!(parse_locations(&links)[0].uri, "file:///b.rs");
        assert!(parse_locations(&Value::Null).is_empty());
    }

    #[tokio::test]
    async fn diagnostics_hand_reports_clean_and_missing() {
        let dir = std::env::temp_dir();
        let ctx = ctx_for(&dir);
        let mgr = LspManager::new(
            &dir,
            &crate::config::Lsp {
                enable: Some(true),
                commands: None,
                write_through: None,
            },
        );
        let out = DiagnosticsHand(Arc::new(mgr))
            .execute(&json!({}), &ctx)
            .await;
        assert!(
            out.content.contains("no diagnostics cached"),
            "{}",
            out.content
        );
    }

    /// A fake language server: acks `initialize`, answers every request
    /// (`definition` → one Location, `workspace/symbol` → one hit),
    /// publishes nothing. Exercised through the hands so the whole
    /// request/response route (pending-map routing included) is covered.
    #[cfg(unix)]
    #[tokio::test]
    async fn navigation_hands_round_trip_via_fake_server() {
        let dir = std::env::temp_dir().join(format!("ka-lspnav-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.rs"), "use foo::Bar;\nfn main() {}\n").unwrap();
        std::fs::write(dir.join("other.rs"), "pub struct Bar;\n").unwrap();
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
    elif method == "textDocument/definition":
        reply(msg, {{"uri": "file://" + root + "/other.rs",
                     "range": {{"start": {{"line": 41, "character": 2}}}}}})
    elif method == "workspace/symbol":
        reply(msg, [{{"name": "Bar", "kind": 23, "containerName": "foo",
                      "location": {{"uri": "file://" + root + "/other.rs",
                                    "range": {{"start": {{"line": 41, "character": 2}}}}}}}}])
    # notifications and unknown requests get no response
"#
            ),
        )
        .unwrap();
        let mgr = Arc::new(LspManager::new(
            &dir,
            &crate::config::Lsp {
                enable: Some(true),
                commands: Some(
                    [("rust".to_string(), format!("python3 {}", fake.display()))]
                        .into_iter()
                        .collect(),
                ),
                write_through: None,
            },
        ));
        mgr.start_all();
        let ctx = ctx_for(&dir);
        // definition: fake server always answers other.rs:42:3
        let out = DefinitionHand(mgr.clone())
            .execute(&json!({"file": "x.rs", "line": 1, "symbol": "Bar"}), &ctx)
            .await;
        assert!(
            out.content.contains("other.rs:42:3"),
            "def: {}",
            out.content
        );
        assert!(
            !out.is_error,
            "def must be a normal result: {}",
            out.content
        );
        // symbol missing from the line → instructive error
        let out = DefinitionHand(mgr.clone())
            .execute(&json!({"file": "x.rs", "line": 1, "symbol": "Baz"}), &ctx)
            .await;
        assert!(out.is_error, "err: {}", out.content);
        assert!(
            out.content.contains("not an identifier"),
            "err: {}",
            out.content
        );
        // workspace symbols render `path:line name (kind in container)`
        let out = SymbolsHand(mgr.clone())
            .execute(&json!({"query": "Bar"}), &ctx)
            .await;
        assert!(
            out.content.contains("other.rs:42  Bar (struct in foo)"),
            "sym: {}",
            out.content
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
