//! Opt-in language-server integration: a hand-rolled LSP client over
//! stdio (JSON-RPC with `Content-Length` framing — no new dependency,
//! same precedent as the MCP client). Diagnostics and navigation by
//! default; with `[lsp] write_through = true`, also rename and code
//! actions that act through the server (`hands/lsp_write.rs`) and
//! `workspace/applyEdit` reverse requests claimed by those hands.
//! No completions, no hover.
//!
//! Contract (the opencode #9102 lesson): diagnostics are informational
//! context appended to a *successful* edit/write tool result. They never
//! affect tool success, clearance verdicts, or exit codes. Rendering is
//! capped (20 items / 2 000 bytes) so a noisy server cannot flood the
//! context window.
//!
//! Concurrency model: [`LspManager`] is a cheap cloneable handle over
//! shared state. `touch` never blocks on a server handshake — the first
//! touch of a language kicks off `initialize` in a background task and
//! that edit simply gets no diagnostics; later touches send full-text
//! didChanges through the ready writer. The diagnostics cache is
//! version-aware: `touch` invalidates the URI and bumps its document
//! version, and publishes tagged with an older version are dropped, so
//! an in-flight publish for the previous edit cannot attach to the
//! current one (servers that omit the version tag are taken as-is —
//! eventual consistency, the LSP ceiling).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use tokio::io::BufReader;
use tokio::process::{Child, ChildStdin};
use tokio::sync::oneshot;

use crate::config::Lsp as LspConfig;

/// Max rendered diagnostic items.
const MAX_ITEMS: usize = 20;
/// Max rendered bytes (the `(+N more)` trailer may exceed this).
const MAX_BYTES: usize = 2_000;
/// Per-line message cap (chars) — one huge trait-mismatch listing must
/// not break the byte budget.
const MESSAGE_CAP: usize = 300;
/// How long the background starter waits for the `initialize` response.
const INIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// How long `request` waits for a language to become ready.
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// How long `request` waits for a response (cold rust-analyzer indexes
/// can make the first workspace query slow).
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Bound on queued `workspace/applyEdit` reverse requests — beyond it a
/// request is answered `applied: false` rather than parked.
const MAX_REVERSE: usize = 16;

/// One cached diagnostic (normalized off the wire).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diag {
    /// 1=error 2=warning 3=info 4=hint (0 = unset).
    pub severity: u8,
    /// 0-based LSP line.
    pub line: u64,
    pub message: String,
    pub source: Option<String>,
}

/// Extension → language id (only these get diagnostics).
pub fn language_for(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "go" => "go",
        "c" | "cc" | "cpp" | "h" | "hpp" => "c",
        "java" => "java",
        "rb" => "ruby",
        _ => return None,
    })
}

/// `file://` URI with minimal percent-encoding (everything outside the
/// unreserved set except `/`).
pub fn uri_for(path: &Path) -> String {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut out = String::from("file://");
    for &b in abs.as_os_str().as_encoded_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Render cached diagnostics: errors/warnings first (then info/hint),
/// stable by line, capped at [`MAX_ITEMS`] items and [`MAX_BYTES`] bytes
/// with a `(+N more)` trailer.
pub fn render_diags(diags: &[Diag]) -> Vec<String> {
    let severity_rank = |d: &Diag| match d.severity {
        1 => 0,
        2 => 1,
        _ => 2,
    };
    let mut sorted: Vec<&Diag> = diags.iter().collect();
    sorted.sort_by_key(|d| (severity_rank(d), d.line));
    let mut lines: Vec<String> = Vec::with_capacity(MAX_ITEMS + 1);
    let mut bytes = 0usize;
    let mut shown = 0usize;
    for d in sorted.iter() {
        if shown >= MAX_ITEMS {
            break;
        }
        let label = match d.severity {
            1 => "error",
            2 => "warning",
            3 => "info",
            4 => "hint",
            _ => "diagnostic",
        };
        let message = truncate_chars(&d.message, MESSAGE_CAP);
        let line = format!(
            "{label} L{}: {message} ({})",
            d.line + 1,
            d.source.as_deref().unwrap_or("unknown")
        );
        if shown > 0 && bytes + line.len() > MAX_BYTES {
            break;
        }
        bytes += line.len();
        lines.push(line);
        shown += 1;
    }
    let hidden = sorted.len().saturating_sub(shown);
    if hidden > 0 {
        lines.push(format!("(+{hidden} more)"));
    }
    lines
}

/// Char-boundary-truncate, ellipsis-marked, borrowed when short.
fn truncate_chars(s: &str, cap: usize) -> std::borrow::Cow<'_, str> {
    if s.chars().count() > cap {
        let cut: String = s.chars().take(cap).collect();
        std::borrow::Cow::Owned(format!("{cut}…"))
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

/// Decode a `file://` URI back to a filesystem path (percent-decoding,
/// `localhost` authority tolerated). `None` for other schemes.
pub fn path_of_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let path = if let Some(after) = rest.strip_prefix("localhost/") {
        format!("/{after}")
    } else if rest.starts_with('/') {
        rest.to_string()
    } else {
        // a non-empty foreign authority is not a local file
        let slash = rest.find('/')?;
        rest[slash..].to_string()
    };
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = |b: u8| -> Option<u8> {
                match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                }
            };
            match (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            ) {
                (Some(hi), Some(lo)) => {
                    out.push(hi * 16 + lo);
                    i += 3;
                }
                _ => {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// One `workspace/symbol` hit, normalized off the wire.
pub struct SymbolHit {
    pub name: String,
    /// LSP SymbolKind number.
    pub kind: u64,
    pub container: Option<String>,
    pub uri: String,
    /// 0-based LSP line.
    pub line: u64,
}

/// One normalized location (definition/references results).
pub struct Loc {
    pub uri: String,
    /// 0-based LSP line.
    pub line: u64,
    /// 0-based LSP character.
    pub character: u64,
}

/// Short label for an LSP SymbolKind number.
pub fn kind_label(kind: u64) -> &'static str {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "ctor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "const",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enum-member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type-param",
        _ => "symbol",
    }
}

/// Parse a `workspace/symbol` result (an array of `SymbolInformation`).
pub fn parse_symbols(result: &serde_json::Value) -> Vec<SymbolHit> {
    result
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|s| {
                    Some(SymbolHit {
                        name: s.get("name")?.as_str()?.to_string(),
                        kind: s
                            .get("kind")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0),
                        container: s
                            .get("containerName")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string),
                        uri: s.get("location")?.get("uri")?.as_str()?.to_string(),
                        line: s["location"]["range"]["start"]["line"]
                            .as_u64()
                            .unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse definition/references results: `null`, one `Location`, an
/// array of `Location`s, or an array of `LocationLink`s.
pub fn parse_locations(result: &serde_json::Value) -> Vec<Loc> {
    let one = |v: &serde_json::Value| -> Option<Loc> {
        // LocationLink fields take precedence when present
        let (uri, range) = if v.get("targetUri").is_some() {
            (v.get("targetUri")?, v.get("targetRange")?)
        } else {
            (v.get("uri")?, v.get("range")?)
        };
        Some(Loc {
            uri: uri.as_str()?.to_string(),
            line: range["start"]["line"].as_u64().unwrap_or(0),
            character: range["start"]["character"].as_u64().unwrap_or(0),
        })
    };
    match result {
        serde_json::Value::Null => Vec::new(),
        serde_json::Value::Array(items) => items.iter().filter_map(one).collect(),
        v => one(v).into_iter().collect(),
    }
}

/// Byte offset of `name` used as a standalone identifier on `line`
/// (word-boundary checked), `None` when absent.
pub fn identifier_byte_offset(line: &str, name: &str) -> Option<usize> {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut from = 0;
    while let Some(hit) = line[from..].find(name) {
        let start = from + hit;
        let end = start + name.len();
        let before_ok = line[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_word(c));
        let after_ok = line[end..].chars().next().is_none_or(|c| !is_word(c));
        if before_ok && after_ok {
            return Some(start);
        }
        from = end;
    }
    None
}

/// UTF-16 column (the LSP position unit) for a byte offset in `line`.
pub fn utf16_column(line: &str, byte_offset: usize) -> u64 {
    let prefix = line.get(..byte_offset).unwrap_or(line);
    prefix.encode_utf16().count() as u64
}

/// Byte offset for an LSP `{line, character}` position in `text`, both
/// 0-based UTF-16-unit counts. Spec-clamped rather than erroring: a
/// line past the document end resolves to the end of text, a character
/// past the line end to the line end (LSP 3.17 "if the character value
/// is greater than the line length it defaults back to the line
/// length"). Line terminators are `\n`; a `\r` before one is ordinary
/// line content.
pub fn utf16_to_byte(text: &str, line: u64, character: u64) -> usize {
    let mut offset = 0usize;
    for (current_line, piece) in text.split('\n').enumerate() {
        if current_line as u64 == line {
            let mut units = 0u64;
            for (byte_idx, ch) in piece.char_indices() {
                if units >= character {
                    return offset + byte_idx;
                }
                units += ch.len_utf16() as u64;
            }
            return offset + piece.len();
        }
        offset += piece.len() + 1;
    }
    // line past the document: end of text
    text.len()
}

/// One normalized `TextEdit` off the wire (UTF-16 range → replacement).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEdit {
    /// 0-based start line / character (UTF-16 units).
    pub start_line: u64,
    pub start_char: u64,
    pub end_line: u64,
    pub end_char: u64,
    pub new_text: String,
}

/// Parse a `WorkspaceEdit` (`changes` map and/or `documentChanges`
/// array of edit entries) into per-file edit lists, path-resolved and
/// capped. File create/rename/delete `documentChanges` are refused —
/// ka's write path only applies text edits.
pub fn parse_workspace_edit(
    edit: &serde_json::Value,
) -> Result<Vec<(PathBuf, Vec<RawEdit>)>, String> {
    /// Cap sanity: a server response must not drive a mega-edit.
    const MAX_FILES: usize = 50;
    const MAX_NEWTEXT_BYTES: usize = 256 * 1024;

    let parse_edit = |e: &serde_json::Value| -> Option<RawEdit> {
        Some(RawEdit {
            start_line: e["range"]["start"]["line"].as_u64()?,
            start_char: e["range"]["start"]["character"].as_u64()?,
            end_line: e["range"]["end"]["line"].as_u64()?,
            end_char: e["range"]["end"]["character"].as_u64()?,
            new_text: e
                .get("newText")
                .and_then(serde_json::Value::as_str)?
                .to_string(),
        })
    };

    // documentChanges wins when a server sends both shapes. Parsing is
    // fail-closed: a malformed entry (bad range, missing newText, no
    // edits/kind) refuses the whole edit — a partially applied rename
    // is worse than none.
    let mut files: Vec<(PathBuf, Vec<RawEdit>)> = Vec::new();
    if let Some(changes) = edit.get("documentChanges").and_then(|c| c.as_array()) {
        for change in changes {
            if let Some(items) = change.get("edits").and_then(|e| e.as_array()) {
                let uri = change["textDocument"]["uri"].as_str().unwrap_or_default();
                let edits: Vec<RawEdit> = items
                    .iter()
                    .map(parse_edit)
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| {
                        format!("workspace edit has a malformed entry for {uri:?}; refusing")
                    })?;
                push_file(&mut files, uri, edits)?;
            } else if let Some(kind) = change.get("kind").and_then(|k| k.as_str()) {
                return Err(format!(
                    "workspace edit contains a file {kind} operation — only text edits \
                     are supported; nothing was applied"
                ));
            } else {
                return Err("workspace edit entry has neither edits nor kind; refusing".to_string());
            }
        }
    } else if let Some(changes) = edit.get("changes").and_then(|c| c.as_object()) {
        for (uri, items) in changes {
            let items = items.as_array().ok_or_else(|| {
                format!("workspace edit entry for {uri:?} is not an array; refusing")
            })?;
            let edits: Vec<RawEdit> = items
                .iter()
                .map(parse_edit)
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    format!("workspace edit has a malformed entry for {uri:?}; refusing")
                })?;
            push_file(&mut files, uri, edits)?;
        }
    }
    if files.len() > MAX_FILES {
        return Err(format!(
            "workspace edit touches {} files (cap {MAX_FILES}); refusing",
            files.len()
        ));
    }
    let total_bytes: usize = files
        .iter()
        .flat_map(|(_, edits)| edits.iter().map(|e| e.new_text.len()))
        .sum();
    if total_bytes > MAX_NEWTEXT_BYTES {
        return Err(format!(
            "workspace edit inserts {total_bytes} bytes (cap {MAX_NEWTEXT_BYTES}); refusing"
        ));
    }
    Ok(files)
}

/// Resolve one URI → path and append its edit list, merging when the
/// same file appears twice.
fn push_file(
    files: &mut Vec<(PathBuf, Vec<RawEdit>)>,
    uri: &str,
    edits: Vec<RawEdit>,
) -> Result<(), String> {
    const MAX_EDITS_PER_FILE: usize = 64;
    let Some(path) = path_of_uri(uri) else {
        return Err(format!(
            "workspace edit targets non-file URI {uri:?}; refusing"
        ));
    };
    let path = PathBuf::from(path);
    if let Some(existing) = files.iter_mut().find(|(p, _)| *p == path) {
        existing.1.extend(edits);
        if existing.1.len() > MAX_EDITS_PER_FILE {
            return Err(format!(
                "workspace edit has {} edits for {} (cap {MAX_EDITS_PER_FILE}); refusing",
                existing.1.len(),
                path.display()
            ));
        }
    } else {
        if edits.len() > MAX_EDITS_PER_FILE {
            return Err(format!(
                "workspace edit has {} edits for {} (cap {MAX_EDITS_PER_FILE}); refusing",
                edits.len(),
                path.display()
            ));
        }
        files.push((path, edits));
    }
    Ok(())
}

/// Apply one file's edits to its text: UTF-16 ranges → byte ranges
/// (spec-clamped), non-overlap enforced, applied last-to-first so
/// earlier positions stay valid. Overlapping edits are a server bug —
/// refused whole rather than merged.
pub fn apply_text_edits(text: &str, edits: &[RawEdit]) -> Result<String, String> {
    let mut spans: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|e| {
            let start = utf16_to_byte(text, e.start_line, e.start_char);
            let end = utf16_to_byte(text, e.end_line, e.end_char).max(start);
            (start, end, e.new_text.as_str())
        })
        .collect();
    spans.sort_by_key(|(s, e, _)| (*s, *e));
    for pair in spans.windows(2) {
        let (prev_end, _) = (pair[0].1, pair[0].2);
        let (next_start, _) = (pair[1].0, pair[1].1);
        if next_start < prev_end {
            return Err(
                "overlapping edits from the language server; refusing to merge".to_string(),
            );
        }
    }
    let mut out = text.to_string();
    for (start, end, new_text) in spans.iter().rev() {
        out.replace_range(start..end, new_text);
    }
    Ok(out)
}

/// Diagnostics cache (URI → latest publish for the current content).
/// `None` marks "no publish for this content yet".
type Cache = parking_lot::Mutex<HashMap<String, Option<Vec<Diag>>>>;
/// Document versions we have sent (URI → version), shared with the
/// reader tasks for stale-publish filtering.
type Versions = parking_lot::Mutex<HashMap<String, i64>>;
/// Request responses owed to callers (id → waiter). The reader task
/// completes the matching sender; everyone else times out.
type Pending = parking_lot::Mutex<HashMap<i64, oneshot::Sender<serde_json::Value>>>;
/// Server→client `workspace/applyEdit` requests waiting for a write-
/// through hand to claim them (per language). Bounded: a request that
/// arrives when the queue is full is answered `applied: false` instead
/// of parked — servers must never hang on an unclaimed edit.
type Reverse = parking_lot::Mutex<Vec<ReverseReq>>;

/// One queued reverse request.
struct ReverseReq {
    lang: String,
    id: i64,
    edit: serde_json::Value,
}

/// A ready server child for one language.
struct ServerProc {
    _child: Child,
    /// Taken out while a write is in flight so the server-table lock is
    /// never held across an await (guards are `!Send`).
    writer: Option<ChildStdin>,
}
/// Per-language server lifecycle.
enum Server {
    /// Background `initialize` handshake in flight.
    Starting,
    Ready(ServerProc),
}

struct Inner {
    servers: HashMap<String, Server>,
    /// Languages whose server failed to spawn or initialize; never
    /// retried within the session.
    failed: HashSet<String>,
}

/// Everything the manager, background starters, and reader tasks share.
struct Shared {
    enabled: bool,
    commands: BTreeMap<String, String>,
    inner: parking_lot::Mutex<Inner>,
    cache: std::sync::Arc<Cache>,
    versions: std::sync::Arc<Versions>,
    /// Request ids handed out so far (initialize owns 1).
    next_id: std::sync::atomic::AtomicI64,
    pending: std::sync::Arc<Pending>,
    /// Unclaimed `workspace/applyEdit` reverse requests.
    reverse: std::sync::Arc<Reverse>,
    /// Serializes every pipe writer (touch/didChange, didOpen, and
    /// requests share one server stdin). Without it, two concurrent
    /// hands both find the writer taken and one fails spuriously with
    /// "not writable".
    write_lock: tokio::sync::Mutex<()>,
    cwd: PathBuf,
}

/// Diagnostics manager: a cheap cloneable handle, inert unless
/// `[lsp] enable = true`. Servers die with the last handle (children
/// carry `kill_on_drop`).
#[derive(Clone)]
pub struct LspManager {
    shared: std::sync::Arc<Shared>,
}

impl LspManager {
    /// New manager; a no-op unless the config enables it.
    pub fn new(cwd: &Path, cfg: &LspConfig) -> Self {
        Self {
            shared: std::sync::Arc::new(Shared {
                enabled: cfg.enable == Some(true),
                commands: cfg.commands.clone().unwrap_or_default(),
                inner: parking_lot::Mutex::new(Inner {
                    servers: HashMap::new(),
                    failed: HashSet::new(),
                }),
                cache: std::sync::Arc::new(parking_lot::Mutex::new(HashMap::new())),
                versions: std::sync::Arc::new(parking_lot::Mutex::new(HashMap::new())),
                next_id: std::sync::atomic::AtomicI64::new(2),
                pending: std::sync::Arc::new(parking_lot::Mutex::new(HashMap::new())),
                reverse: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
                write_lock: tokio::sync::Mutex::new(()),
                cwd: cwd.to_path_buf(),
            }),
        }
    }

    /// Report a file's new content. Returns `true` when a didOpen/
    /// didChange was actually written to a ready server (so callers
    /// know a publish may follow); `false` covers disabled, unknown
    /// language, unconfigured, failed, and still-starting servers —
    /// skip polling entirely. The first touch of a language only kicks
    /// off the background handshake; that edit gets no diagnostics.
    pub async fn touch(&self, path: &Path, new_text: &str) -> bool {
        if !self.shared.enabled {
            return false;
        }
        let Some(lang) = language_for(path) else {
            return false;
        };
        let command = self.shared.commands.get(lang).cloned();
        {
            let mut inner = self.shared.inner.lock();
            if inner.failed.contains(lang) {
                return false;
            }
            match inner.servers.get(lang) {
                Some(Server::Ready(_)) | Some(Server::Starting) => {}
                None => {
                    let Some(command) = command else { return false };
                    inner.servers.insert(lang.to_string(), Server::Starting);
                    drop(inner);
                    let shared = self.shared.clone();
                    let lang = lang.to_string();
                    let uri = uri_for(path);
                    let text = new_text.to_string();
                    tokio::spawn(async move {
                        start_server(&shared, &lang, &command, Some((&uri, &text))).await;
                    });
                    return false;
                }
            }
        }
        // ready: bump version, invalidate the cache, write the
        // full-text update (lock released while awaiting the pipe)
        let uri = uri_for(path);
        let version = {
            let mut versions = self.shared.versions.lock();
            let v = versions.entry(uri.clone()).or_insert(0);
            *v += 1;
            *v
        };
        self.shared.cache.lock().insert(uri.clone(), None);
        let msg = if version == 1 {
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {
                    "textDocument": {
                        "uri": uri,
                        "languageId": lang,
                        "version": 1,
                        "text": new_text,
                    }
                }
            })
        } else {
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": { "uri": uri, "version": version },
                    "contentChanges": [{ "text": new_text }],
                }
            })
        };
        // ready: write through the shared lock (never holds the table
        // lock across the pipe await; a dead pipe fails the language
        // for the session)
        match self.write_to_server(lang, &msg).await {
            Ok(()) => true,
            Err(()) => false,
        }
    }

    /// Latest rendered diagnostics for `path` (`None` when the server
    /// has not published for the current content yet — distinguishes
    /// "wait" from "clean, nothing to report"). Locks only the shared
    /// cache, never the server table.
    pub fn diagnostics(&self, path: &Path) -> Option<Vec<String>> {
        cached(&self.shared.cache, &uri_for(path))
    }

    /// Languages with a configured server command (sorted).
    pub fn configured_languages(&self) -> Vec<String> {
        self.shared.commands.keys().cloned().collect()
    }

    /// Eagerly kick off every configured server's handshake (engine
    /// start). Idempotent — starting/ready/failed languages are skipped.
    /// Lets the navigation tools work before any file was edited.
    pub fn start_all(&self) {
        if !self.shared.enabled {
            return;
        }
        for (lang, command) in self.shared.commands.clone() {
            {
                let mut inner = self.shared.inner.lock();
                if inner.failed.contains(&lang) || inner.servers.contains_key(&lang) {
                    continue;
                }
                inner.servers.insert(lang.clone(), Server::Starting);
            }
            let shared = self.shared.clone();
            tokio::spawn(async move {
                start_server(&shared, &lang, &command, None).await;
            });
        }
    }

    /// Wait (≤ [`READY_TIMEOUT`]) for `lang`'s server to be ready to
    /// serve requests, with the reason it cannot.
    async fn ensure_ready(&self, lang: &str) -> Result<(), String> {
        if !self.shared.enabled {
            return Err("lsp disabled — set [lsp] enable = true".to_string());
        }
        if !self.shared.commands.contains_key(lang) {
            return Err(format!("no [lsp.commands] entry for {lang:?}"));
        }
        let deadline = std::time::Instant::now() + READY_TIMEOUT;
        loop {
            {
                let inner = self.shared.inner.lock();
                if inner.failed.contains(lang) {
                    return Err(format!("{lang} language server failed to start"));
                }
                if matches!(inner.servers.get(lang), Some(Server::Ready(_))) {
                    return Ok(());
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "{lang} language server still starting; retry shortly"
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// One JSON-RPC request → result on `lang`'s ready server.
    pub async fn request(
        &self,
        lang: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.ensure_ready(lang).await?;
        let id = self
            .shared
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = oneshot::channel::<serde_json::Value>();
        self.shared.pending.lock().insert(id, tx);
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if self.write_to_server(lang, &msg).await.is_err() {
            self.shared.pending.lock().remove(&id);
            return Err(format!("{lang} language server is not writable"));
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(resp)) => {
                if let Some(err) = resp.get("error") {
                    let code = err
                        .get("code")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0);
                    let text = err
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?");
                    return Err(format!("server error {code}: {text}"));
                }
                Ok(resp
                    .get("result")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null))
            }
            _ => {
                self.shared.pending.lock().remove(&id);
                Err(format!("{lang} language server timed out"))
            }
        }
    }

    /// Take every queued `workspace/applyEdit` reverse request for
    /// `lang` (leaving other languages' entries in place).
    pub fn claim_reverse_edits(&self, lang: &str) -> Vec<(i64, serde_json::Value)> {
        let mut q = self.shared.reverse.lock();
        let mut out = Vec::new();
        let mut i = 0;
        while i < q.len() {
            if q[i].lang == lang {
                let r = q.remove(i);
                out.push((r.id, r.edit));
            } else {
                i += 1;
            }
        }
        out
    }

    /// Answer a server→client request we claimed via
    /// [`Self::claim_reverse_edits`].
    pub async fn respond_request(
        &self,
        lang: &str,
        id: i64,
        result: serde_json::Value,
    ) -> Result<(), String> {
        let msg = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
        send_message(&self.shared, lang, &msg)
            .await
            .map_err(|_| format!("{lang} language server is not writable"))
    }

    /// One notification (no response expected) to `lang`'s ready server.
    pub async fn notify(
        &self,
        lang: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(), String> {
        // fire-and-forget: a still-starting server never opened the
        // document anyway, and the didRenameFiles fan-out hits every
        // configured server serially — waiting ≤READY_TIMEOUT per
        // language would only stall the hand. Skip instead.
        if !self.is_ready(lang) {
            return Err(format!(
                "{lang} language server not ready; notification skipped"
            ));
        }
        let msg = serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params });
        send_message(&self.shared, lang, &msg)
            .await
            .map_err(|_| format!("{lang} language server is not writable"))
    }

    /// Whether `lang`'s server finished its handshake — no waiting.
    pub fn is_ready(&self, lang: &str) -> bool {
        matches!(
            self.shared.inner.lock().servers.get(lang),
            Some(Server::Ready(_))
        )
    }

    /// Close a document the servers track (didClose + cache/version
    /// cleanup) — called after a file moves so servers drop the old URI.
    /// Best-effort: a not-yet-ready server simply never opened it.
    pub async fn close_document(&self, path: &Path) {
        let uri = uri_for(path);
        self.shared.versions.lock().remove(&uri);
        self.shared.cache.lock().remove(&uri);
        if let Some(lang) = language_for(path) {
            let msg = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didClose",
                "params": { "textDocument": { "uri": uri } }
            });
            let _ = send_message(&self.shared, lang, &msg).await;
        }
    }

    /// One request that simultaneously claims `workspace/applyEdit`
    /// reverse requests from the same server while its response is
    /// pending. Servers commonly answer `executeCommand` by *sending*
    /// `applyEdit` first and finishing only once it is applied — a
    /// plain request would deadlock until the timeout. Each claimed
    /// edit goes through `on_edit`; the reply reports whether it
    /// applied.
    pub async fn request_with_reverse<'a>(
        &self,
        lang: &str,
        method: &str,
        params: serde_json::Value,
        mut on_edit: impl FnMut(
            serde_json::Value,
        ) -> Pin<std::boxed::Box<dyn Future<Output = bool> + Send + 'a>>,
    ) -> Result<serde_json::Value, String> {
        self.ensure_ready(lang).await?;
        let id = self
            .shared
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, mut rx) = oneshot::channel::<serde_json::Value>();
        self.shared.pending.lock().insert(id, tx);
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if send_message(&self.shared, lang, &msg).await.is_err() {
            self.shared.pending.lock().remove(&id);
            return Err(format!("{lang} language server is not writable"));
        }
        let deadline = tokio::time::Instant::now() + REQUEST_TIMEOUT;
        let outcome: Result<serde_json::Value, String> = loop {
            let response = tokio::select! {
                r = &mut rx => {
                    match r {
                        Ok(resp) => resp,
                        Err(_) => {
                            self.shared.pending.lock().remove(&id);
                            break Err(format!(
                                "{lang} language server dropped the request"
                            ));
                        }
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                    if tokio::time::Instant::now() >= deadline {
                        self.shared.pending.lock().remove(&id);
                        break Err(format!("{lang} language server timed out"));
                    }
                    serde_json::Value::Null
                }
            };
            // the sleep branch yields Null; a real response is always
            // a JSON-RPC envelope object
            if !response.is_null() {
                if let Some(err) = response.get("error") {
                    let code = err
                        .get("code")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0);
                    let text = err
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?");
                    break Err(format!("server error {code}: {text}"));
                }
                break Ok(response
                    .get("result")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null));
            }
            // drain any reverse applyEdit that arrived while waiting
            for (rid, edit) in self.claim_reverse_edits(lang) {
                let applied = on_edit(edit).await;
                let result = if applied {
                    serde_json::json!({ "applied": true })
                } else {
                    serde_json::json!({
                        "applied": false,
                        "failureReason": "ka: edit refused (ledger drift, protected path, or caps)",
                    })
                };
                let _ = self.respond_request(lang, rid, result).await;
            }
        };
        if outcome.is_err() {
            // the outer request failed: answer any stranded applyEdit
            // truthfully instead of parking it (a later, unrelated op
            // would otherwise claim and apply it out of context)
            for (rid, _edit) in self.claim_reverse_edits(lang) {
                let _ = self
                    .respond_request(
                        lang,
                        rid,
                        serde_json::json!({
                            "applied": false,
                            "failureReason":
                                "ka: outer request failed; edit not applied",
                        }),
                    )
                    .await;
            }
        }
        outcome
    }

    /// Feed `path`'s on-disk content to its server and wait (≤2.5 s) for
    /// fresh diagnostics — the write-through hands' feedback loop.
    /// `None` = nothing to report (disabled/unknown/unconfigured, no
    /// publish in the window, or a clean file).
    pub async fn refresh(&self, path: &Path) -> Option<Vec<String>> {
        let text = std::fs::read_to_string(path).ok()?;
        if !self.touch(path, &text).await {
            return None;
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(2500);
        let mut rendered = self.diagnostics(path);
        while rendered.is_none() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            rendered = self.diagnostics(path);
        }
        rendered.filter(|lines| !lines.is_empty())
    }

    /// Ensure the language server has `path` open with its on-disk
    /// content, so position queries (definition/references) see current
    /// truth. Returns the document URI.
    pub async fn open_if_needed(&self, path: &Path) -> Result<String, String> {
        let lang = language_for(path)
            .ok_or_else(|| format!("no language mapping for {}", path.display()))?
            .to_string();
        let uri = uri_for(path);
        if self.shared.versions.lock().contains_key(&uri) {
            return Ok(uri);
        }
        self.ensure_ready(&lang).await?;
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let version = {
            let mut versions = self.shared.versions.lock();
            let v = versions.entry(uri.clone()).or_insert(0);
            *v += 1;
            *v
        };
        self.shared.cache.lock().insert(uri.clone(), None);
        let open = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": lang,
                    "version": version,
                    "text": text,
                }
            }
        });
        if self.write_to_server(&lang, &open).await.is_err() {
            return Err(format!("{lang} language server is not writable"));
        }
        Ok(uri)
    }

    /// Write one message to `lang`'s ready server (writer take/put-back
    /// dance keeps the table lock off the pipe await). A dead pipe
    /// fails the language for the session — same policy as `touch`.
    async fn write_to_server(&self, lang: &str, msg: &serde_json::Value) -> Result<(), ()> {
        send_message(&self.shared, lang, msg).await
    }

    /// Snapshot of every file's cached diagnostics (display path,
    /// non-empty diags), sorted by path — the `diagnostics` hand's
    /// project view.
    pub fn all_diagnostics(&self) -> Vec<(String, Vec<Diag>)> {
        let cache = self.shared.cache.lock();
        let mut out: Vec<(String, Vec<Diag>)> = cache
            .iter()
            .filter_map(|(uri, diags)| {
                let diags = diags.clone()?;
                if diags.is_empty() {
                    return None;
                }
                Some((path_of_uri(uri).unwrap_or_else(|| uri.clone()), diags))
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

/// Read the cache for one URI.
fn cached(cache: &Cache, uri: &str) -> Option<Vec<String>> {
    match cache.lock().get(uri) {
        Some(Some(diags)) => Some(render_diags(diags)),
        _ => None,
    }
}

/// Background handshake: spawn the server, `initialize` → `initialized`
/// → optionally didOpen the content that kicked us off, then publish it
/// ready. Any failure marks the language failed for the session (no
/// retries).
async fn start_server(
    shared: &std::sync::Arc<Shared>,
    lang: &str,
    command: &str,
    first_doc: Option<(&str, &str)>,
) {
    let fail = |shared: &Shared, lang: &str| {
        let mut inner = shared.inner.lock();
        inner.servers.remove(lang);
        inner.failed.insert(lang.to_string());
    };
    let mut argv = command.split_whitespace().map(str::to_string);
    let Some(program) = argv.next() else {
        fail(shared, lang);
        return;
    };
    let args: Vec<String> = argv.collect();
    let mut child = match tokio::process::Command::new(&program)
        .args(&args)
        .current_dir(&shared.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(_) => {
            fail(shared, lang);
            return;
        }
    };
    let mut writer = match child.stdin.take() {
        Some(w) => w,
        None => {
            fail(shared, lang);
            return;
        }
    };
    let reader = child.stdout.take();
    let (init_tx, init_rx) = oneshot::channel::<bool>();
    let Some(reader) = reader else {
        fail(shared, lang);
        return;
    };
    tokio::spawn(read_loop(
        BufReader::new(reader),
        shared.clone(),
        lang.to_string(),
        init_tx,
    ));
    let init = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "processId": null,
            "rootUri": uri_for(&shared.cwd),
            "capabilities": {},
        },
    });
    if write_msg(&mut writer, &init).await.is_err() {
        fail(shared, lang);
        return;
    }
    // wait for the initialize response before further traffic
    if !matches!(
        tokio::time::timeout(INIT_TIMEOUT, init_rx).await,
        Ok(Ok(true))
    ) {
        fail(shared, lang);
        return;
    }
    let initialized = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {},
    });
    if write_msg(&mut writer, &initialized).await.is_err() {
        fail(shared, lang);
        return;
    }
    // didOpen the content that kicked us off (version 1) when there was one
    if let Some((first_uri, first_text)) = first_doc {
        shared.versions.lock().insert(first_uri.to_string(), 1);
        let open = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": first_uri,
                    "languageId": lang,
                    "version": 1,
                    "text": first_text,
                }
            }
        });
        if write_msg(&mut writer, &open).await.is_err() {
            fail(shared, lang);
            return;
        }
    }
    shared.inner.lock().servers.insert(
        lang.to_string(),
        Server::Ready(ServerProc {
            _child: child,
            writer: Some(writer),
        }),
    );
}

/// Write one framed JSON-RPC message (the shared wire codec does the
/// framing; this wrapper is the serde boundary).
async fn write_msg(w: &mut ChildStdin, msg: &serde_json::Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    crate::wire::write_frame(w, &body).await
}

/// Write one message to `lang`'s ready server (writer take/put-back
/// dance keeps the table lock off the pipe await). A dead pipe fails
/// the language for the session — same policy as `touch`. Shared by the
/// manager methods and the read loop (which answers server→client
/// requests on the same pipe).
async fn send_message(shared: &Shared, lang: &str, msg: &serde_json::Value) -> Result<(), ()> {
    let _guard = shared.write_lock.lock().await;
    let mut writer = {
        let mut inner = shared.inner.lock();
        inner.servers.get_mut(lang).and_then(|s| match s {
            Server::Ready(p) => p.writer.take(),
            _ => None,
        })
    };
    let Some(w) = writer.as_mut() else {
        return Err(());
    };
    if write_msg(w, msg).await.is_err() {
        let mut inner = shared.inner.lock();
        inner.servers.remove(lang);
        inner.failed.insert(lang.to_string());
        return Err(());
    }
    let mut inner = shared.inner.lock();
    if let Some(Server::Ready(p)) = inner.servers.get_mut(lang) {
        p.writer = writer.take();
    }
    Ok(())
}

/// Reply to a server→client request WITHOUT stalling this loop: the
/// write path can block on `write_lock` (a slow consumer of the
/// server's stdin), and this loop is the only stdout drainer — parking
/// it inline on the lock can wedge the pipe. Spawned, best-effort, like
/// every reply here.
async fn reply_off_loop(shared: &std::sync::Arc<Shared>, lang: &str, reply: serde_json::Value) {
    let shared = shared.clone();
    let lang = lang.to_string();
    tokio::spawn(async move {
        let _ = send_message(&shared, &lang, &reply).await;
    });
}

/// Read framed messages off a server's stdout, caching
/// publishDiagnostics (version-filtered against what we sent),
/// signaling the initialize response, completing pending requests, and
/// dispatching server→client requests: `workspace/applyEdit` is queued
/// for a write-through hand to claim (bounded; overflow is answered
/// `applied: false`), everything else gets a polite `null` result — the
/// pre-write-through behavior was silence, which left servers waiting.
/// Runs until the pipe closes.
async fn read_loop<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    mut reader: R,
    shared: std::sync::Arc<Shared>,
    lang: String,
    init_tx: oneshot::Sender<bool>,
) {
    let mut init_tx = Some(init_tx);
    loop {
        let body = match crate::wire::read_frame(&mut reader).await {
            Some(b) => b,
            None => break,
        };
        let Ok(msg) = serde_json::from_slice::<serde_json::Value>(&body) else {
            continue;
        };
        let method = msg.get("method").and_then(|m| m.as_str());
        // responses carry an id and no method
        if method.is_none() {
            if let Some(id) = msg.get("id").and_then(serde_json::Value::as_i64) {
                if id == 1 {
                    if let Some(tx) = init_tx.take() {
                        let _ = tx.send(msg.get("result").is_some());
                    }
                } else if let Some(tx) = shared.pending.lock().remove(&id) {
                    let _ = tx.send(msg);
                }
                continue;
            }
        }
        if method == Some("textDocument/publishDiagnostics") {
            let Some(uri) = msg["params"]["uri"].as_str() else {
                continue;
            };
            // stale-publish filter: a publish tagged with a version
            // older than what we last sent describes previous content
            if let Some(v) = msg["params"]["version"].as_i64() {
                let current = shared.versions.lock().get(uri).copied().unwrap_or(i64::MAX);
                if v < current {
                    continue;
                }
            }
            let diags: Vec<Diag> = msg["params"]["diagnostics"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|d| {
                            let message = d.get("message")?.as_str()?.to_string();
                            Some(Diag {
                                severity: d
                                    .get("severity")
                                    .and_then(serde_json::Value::as_u64)
                                    .unwrap_or(0) as u8,
                                line: d["range"]["start"]["line"].as_u64().unwrap_or(0),
                                message,
                                source: d
                                    .get("source")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::to_string),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            shared.cache.lock().insert(uri.to_string(), Some(diags));
            continue;
        }
        // server→client requests (id + method): queue applyEdit for a
        // write-through hand, answer everything else with a null result
        if let (Some(method), Some(id)) =
            (method, msg.get("id").and_then(serde_json::Value::as_i64))
        {
            if method == "workspace/applyEdit" {
                let edit = msg["params"]["edit"].clone();
                let queued = {
                    let mut q = shared.reverse.lock();
                    if q.len() < MAX_REVERSE {
                        q.push(ReverseReq {
                            lang: lang.clone(),
                            id,
                            edit,
                        });
                        true
                    } else {
                        false
                    }
                };
                if !queued {
                    let reply = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "applied": false,
                            "failureReason": "ka: reverse-request queue full",
                        }
                    });
                    reply_off_loop(&shared, &lang, reply).await;
                }
            } else if method == "workspace/configuration" {
                // spec: one result item per requested section; a bare
                // null is wrong-typed and strict servers reject it
                let n = msg["params"]["items"].as_array().map_or(1, Vec::len);
                reply_off_loop(
                    &shared,
                    &lang,
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": vec![serde_json::Value::Null; n]
                    }),
                )
                .await;
            } else {
                reply_off_loop(
                    &shared,
                    &lang,
                    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": null }),
                )
                .await;
            }
        }
        // everything else (logMessage, progress, ...) is ignored
    }
    // pipe closed: the server died. Wake a handshake still waiting on
    // the initialize response, then demote so requests fail fast
    // instead of hanging to REQUEST_TIMEOUT on a dead stdin.
    if let Some(tx) = init_tx.take() {
        let _ = tx.send(false);
    }
    let mut inner = shared.inner.lock();
    inner.servers.remove(&lang);
    inner.failed.insert(lang);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn diag(sev: u8, line: u64, message: &str) -> Diag {
        Diag {
            severity: sev,
            line,
            message: message.to_string(),
            source: Some("test-ls".to_string()),
        }
    }

    fn live_manager(dir: &Path, command: String) -> LspManager {
        LspManager::new(
            dir,
            &LspConfig {
                enable: Some(true),
                commands: Some(BTreeMap::from([("rust".to_string(), command)])),
                write_through: None,
            },
        )
    }

    #[test]
    fn caps_at_20_items() {
        let diags: Vec<Diag> = (0..25).map(|i| diag(1, i, "boom")).collect();
        let lines = render_diags(&diags);
        assert_eq!(lines.len(), 21, "20 items + trailer");
        assert_eq!(lines.last().unwrap(), "(+5 more)");
    }

    #[test]
    fn caps_at_2000_bytes_and_lines() {
        let msg = "x".repeat(300);
        let diags: Vec<Diag> = (0..40).map(|i| diag(1, i, &msg)).collect();
        let lines = render_diags(&diags);
        let body: usize = lines.iter().map(|l| l.len()).sum();
        assert!(body < MAX_BYTES + 64, "body {body} stays near the cap");
        assert!(
            lines.last().unwrap().starts_with("(+"),
            "truncation is noted: {:?}",
            lines.last()
        );
        // a single pathological message is capped within its line
        let one = render_diags(&[diag(1, 0, &"y".repeat(5_000))]);
        assert!(
            one[0].len() < MESSAGE_CAP + 64,
            "line capped: {}",
            one[0].len()
        );
    }

    #[test]
    fn errors_and_warnings_sort_first() {
        let diags = vec![diag(3, 1, "info"), diag(2, 5, "warn"), diag(1, 9, "bad")];
        let lines = render_diags(&diags);
        assert!(lines[0].starts_with("error L10"));
        assert!(lines[1].starts_with("warning L6"));
        assert!(lines[2].starts_with("info L2"));
    }

    #[test]
    fn renders_source_and_line_one_based() {
        let lines = render_diags(&[diag(1, 11, "expected `;`")]);
        assert_eq!(lines[0], "error L12: expected `;` (test-ls)");
    }

    #[test]
    fn language_table_and_uris() {
        assert_eq!(language_for(Path::new("a/b.rs")), Some("rust"));
        assert_eq!(language_for(Path::new("x.TS")), Some("typescript"));
        assert_eq!(language_for(Path::new("a.txt")), None);
        assert_eq!(
            uri_for(Path::new("/tmp/s p/q.rs")),
            "file:///tmp/s%20p/q.rs"
        );
    }

    #[test]
    fn utf16_to_byte_maps_and_clamps() {
        let text = "fn a() {}\nlet é = \"日\";\nlast\n";
        assert_eq!(utf16_to_byte(text, 0, 0), 0);
        assert_eq!(utf16_to_byte(text, 1, 0), 10, "line 1 starts after \\n");
        // "let " = 4 units and 4 bytes
        assert_eq!(utf16_to_byte(text, 1, 4), 14);
        // "let é = \"" = 9 UTF-16 units (é is one) but 10 bytes
        assert_eq!(utf16_to_byte(text, 1, 9), 20);
        // character past the line clamps to the line end
        assert_eq!(utf16_to_byte(text, 1, 999), 10 + "let é = \"日\";".len());
        // line past the document clamps to the end of text
        assert_eq!(utf16_to_byte(text, 99, 0), text.len());
    }

    fn raw(sl: u64, sc: u64, el: u64, ec: u64, nt: &str) -> RawEdit {
        RawEdit {
            start_line: sl,
            start_char: sc,
            end_line: el,
            end_char: ec,
            new_text: nt.to_string(),
        }
    }

    #[test]
    fn text_edits_apply_and_refuse_overlap() {
        let text = "aaa\nbbb\nccc\n";
        let out = apply_text_edits(text, &[raw(1, 0, 1, 3, "B"), raw(0, 0, 0, 3, "A")]).unwrap();
        assert_eq!(out, "A\nB\nccc\n");
        // zero-width insert
        let out = apply_text_edits(text, &[raw(2, 0, 2, 0, "// ")]).unwrap();
        assert_eq!(out, "aaa\nbbb\n// ccc\n");
        // multi-unit characters inside the replaced span
        let uni = "let é = 1;\n";
        let out = apply_text_edits(uni, &[raw(0, 4, 0, 5, "èè")]).unwrap();
        assert_eq!(out, "let èè = 1;\n");
        // overlapping edits are refused whole
        assert!(
            apply_text_edits(text, &[raw(0, 0, 1, 0, "x"), raw(0, 1, 0, 2, "y")]).is_err(),
            "overlap must refuse"
        );
    }

    #[test]
    fn workspace_edits_parse_both_shapes_and_refuse_file_ops() {
        let a = uri_for(Path::new("/w/a.rs"));
        let b = uri_for(Path::new("/w/b.rs"));
        let edit = |nt: &str| {
            serde_json::json!({
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 3 }
                },
                "newText": nt
            })
        };
        let changes = serde_json::json!({
            "changes": { a.clone(): [edit("x")], b.clone(): [edit("y")] }
        });
        let files = parse_workspace_edit(&changes).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].0, PathBuf::from("/w/a.rs"));
        assert_eq!(files[0].1[0].new_text, "x");

        let doc_changes = serde_json::json!({
            "documentChanges": [
                { "textDocument": { "uri": a, "version": 1 }, "edits": [edit("z")] }
            ]
        });
        let files = parse_workspace_edit(&doc_changes).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1[0].new_text, "z");

        let file_op = serde_json::json!({
            "documentChanges": [
                { "kind": "rename", "oldUri": a, "newUri": b }
            ]
        });
        let err = parse_workspace_edit(&file_op).unwrap_err();
        assert!(err.contains("file rename"), "{err}");
        // non-file URIs refuse
        let bad = serde_json::json!({ "changes": { "untitled:Untitled-1": [edit("x")] } });
        assert!(parse_workspace_edit(&bad).is_err());
    }

    #[test]
    fn workspace_edit_caps_refuse() {
        let edit = |nt: &str| {
            serde_json::json!({
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 1 }
                },
                "newText": nt
            })
        };
        // 51 files > cap 50 — README: "capped (50 files, ...)"
        let mut changes = serde_json::Map::new();
        for i in 0..51 {
            changes.insert(
                uri_for(Path::new(&format!("/w/f{i}.rs"))),
                serde_json::json!([edit("x")]),
            );
        }
        let err = parse_workspace_edit(&serde_json::json!({ "changes": changes })).unwrap_err();
        assert!(err.contains("cap 50"), "{err}");
        // 65 edits for one file > cap 64 (README: "64 edits per file")
        let a = uri_for(Path::new("/w/a.rs"));
        let many: Vec<_> = (0..65).map(|_| edit("x")).collect();
        let err = parse_workspace_edit(&serde_json::json!({ "changes": { a.clone(): many } }))
            .unwrap_err();
        assert!(err.contains("cap 64"), "{err}");
        // > 256 KB of inserted text (README: "256 KB of inserted text")
        let big = "x".repeat(256 * 1024 + 1);
        let err = parse_workspace_edit(&serde_json::json!({ "changes": { a: [edit(&big)] } }))
            .unwrap_err();
        assert!(err.contains("cap 262144"), "{err}");
    }

    #[test]
    fn malformed_edits_refuse_whole() {
        let a = uri_for(Path::new("/w/a.rs"));
        // missing range: not silently skipped — a partially applied
        // rename is worse than none
        let bad_range = serde_json::json!({ "changes": { a.clone(): [ { "newText": "x" } ] } });
        assert!(parse_workspace_edit(&bad_range).is_err());
        // missing newText: absence is not an implied deletion
        let bad_text = serde_json::json!({ "changes": { a.clone(): [ {
            "range": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1} }
        } ] } });
        assert!(parse_workspace_edit(&bad_text).is_err());
        // documentChanges entry with neither edits nor kind
        let neither =
            serde_json::json!({ "documentChanges": [ { "textDocument": { "uri": a } } ] });
        let err = parse_workspace_edit(&neither).unwrap_err();
        assert!(err.contains("neither edits nor kind"), "{err}");
        // changes value that is not an array
        let not_array = serde_json::json!({ "changes": { a: "nope" } });
        assert!(parse_workspace_edit(&not_array).is_err());
    }

    #[tokio::test]
    async fn manager_is_inert_without_enable() {
        let m = LspManager::new(
            Path::new("/tmp"),
            &LspConfig {
                enable: None,
                commands: Some(BTreeMap::from([(
                    "rust".to_string(),
                    "rust-analyzer".to_string(),
                )])),
                write_through: None,
            },
        );
        assert!(
            !m.touch(Path::new("/tmp/x.rs"), "fn a() {}").await,
            "disabled touch reports false so callers skip polling"
        );
        assert!(m.diagnostics(Path::new("/tmp/x.rs")).is_none());
    }

    #[tokio::test]
    async fn dead_command_marks_language_failed_without_panicking() {
        let dir = std::env::temp_dir();
        let m = live_manager(&dir, "ka-definitely-missing-lsp-binary".to_string());
        assert!(
            !m.touch(Path::new("/tmp/x.rs"), "fn a() {}").await,
            "first touch only kicks the background start"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !m.shared.inner.lock().failed.contains("rust") && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            m.shared.inner.lock().failed.contains("rust"),
            "missing binary is recorded (asynchronously)"
        );
    }

    /// A fake stdio server publishing exactly once (right after
    /// startup) and staying silent on didChange — the worst-case stale
    /// publisher. Regression for "the previous edit's diagnostics
    /// attach to the next edit": touch must invalidate the cache so the
    /// poll sees "nothing yet", never the old publish.
    #[cfg(unix)]
    #[tokio::test]
    async fn touch_invalidates_previous_publish() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("ka-lsp-fake-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("fake.sh");
        let mut f = std::fs::File::create(&fake).unwrap();
        writeln!(
            f,
            r#"#!/bin/sh
# minimal LSP fake: ack initialize, publish ONE diagnostic shortly after
# (didOpen lands in that window), then stay silently alive
resp='{{"jsonrpc":"2.0","id":1,"result":{{"capabilities":{{}}}}}}'
printf 'Content-Length: %d\r\n\r\n%s' "${{#resp}}" "$resp"
sleep 0.4
diag='{{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{{"uri":"{}","diagnostics":[{{"range":{{"start":{{"line":0,"character":0}},"end":{{"line":0,"character":1}}}},"severity":1,"message":"STALE-ERROR","source":"fake"}}]}}}}'
printf 'Content-Length: %d\r\n\r\n%s' "${{#diag}}" "$diag"
cat > /dev/null
"#,
            uri_for(&dir.join("x.rs")),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake, perms).unwrap();
        }
        let m = live_manager(&dir, format!("sh {}", fake.display()));
        let src = dir.join("x.rs");
        // first touch kicks the background handshake; wait for the
        // publish it triggers
        assert!(
            !m.touch(&src, "let x: u32 = 1;").await,
            "starting server reports false"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut first = m.diagnostics(&src);
        while first.is_none() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            first = m.diagnostics(&src);
        }
        let first = first.expect("fake server published");
        assert!(
            first.iter().any(|l| l.contains("STALE-ERROR")),
            "first publish landed: {first:?}"
        );
        // second edit: the cache must be invalidated immediately and the
        // silent fake server never republishes — a poll must see None,
        // never the STALE-ERROR from edit 1
        assert!(m.touch(&src, "let x: u64 = 1;").await, "didChange sent");
        assert!(
            m.diagnostics(&src).is_none(),
            "touch invalidates the previous publish"
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            m.diagnostics(&src).is_none(),
            "stale diagnostics must not reattach from a silent server"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
