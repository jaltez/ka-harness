//! Strand store: one session = one append-only JSONL file ("strand"), a tree
//! of [`Record`]s with a single live tip. Branching ("offshoot") is a tip
//! move; splitting copies a prefix into a new file. Phase 3 adds the
//! session-file management used by the engine: ids, settings replay,
//! listing, and dangling-turn synthesis.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ka_protocol::{Effort, Mode, RecordId, StrandId};
use serde::{Deserialize, Serialize};

/// Git snapshot carried in the strand header (read-only awareness).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoSnapshot {
    /// Current branch name (`HEAD` when detached).
    pub branch: String,
    /// Dirty (modified/untracked) file paths, capped in count.
    pub dirty: Vec<String>,
}

/// Who authored a message record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The human user.
    User,
    /// The assistant/model.
    Assistant,
    /// Tool output rendered as a message.
    Tool,
    /// System-level text.
    System,
}

/// A tool call persisted with an assistant message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredCall {
    /// Provider call id.
    pub id: String,
    /// Tool name.
    pub tool: String,
    /// Parsed arguments.
    pub arguments: serde_json::Value,
}

/// A tool result persisted with a tool-role message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredResult {
    /// The call this answers.
    pub call_id: String,
    /// Output text.
    pub content: String,
    /// Whether the tool reported an error.
    pub is_error: bool,
}

/// One line of a strand file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum Record {
    /// First line of every strand.
    Header {
        /// Strand identifier.
        id: StrandId,
        /// Creation timestamp (RFC 3339).
        ts: String,
        /// Working directory at creation.
        cwd: String,
        /// Strand format version.
        version: u32,
        /// Repo state at creation, if inside a work tree.
        repo: Option<RepoSnapshot>,
        /// Parent strand (forks); absent for fresh sessions.
        #[serde(default)]
        parent: Option<String>,
    },
    /// A conversation message (with tool calls/results when present).
    Message {
        /// Record identifier.
        id: RecordId,
        /// Authoring role.
        role: Role,
        /// Message content.
        content: String,
        /// Tool calls issued with this assistant message.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        calls: Vec<StoredCall>,
        /// Tool results carried by a tool-role message.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        results: Vec<StoredResult>,
        /// Assistant reasoning captured with this message.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
    },
    /// A settings change (model / effort / mode / feature toggles).
    Change {
        /// Record identifier.
        id: RecordId,
        /// New model selector, if changed.
        model: Option<String>,
        /// New effort, if changed.
        effort: Option<Effort>,
        /// New permission mode, if changed.
        mode: Option<Mode>,
        /// Feature-toggle snapshot, if changed: the full disabled-spec
        /// list in the shared grammar (`agents`, `mcp:github`, …).
        /// Presence replaces the whole set; absent leaves it alone.
        /// Additive.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        features: Option<Vec<String>>,
        /// New sandbox mode, if changed (`"off"` | `"fs"`). Additive.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sandbox: Option<String>,
    },
    /// A digest (compaction) boundary.
    Digest {
        /// Record identifier.
        id: RecordId,
        /// Summary replacing the collapsed history.
        summary: String,
        /// Kept history starts at this record.
        kept_from: RecordId,
    },
    /// A clear/reset boundary marker.
    Boundary {
        /// Record identifier.
        id: RecordId,
    },
    /// Conversation rewind: history is truncated after `kept_from`.
    Rewind {
        /// Record identifier.
        id: RecordId,
        /// Last kept message record; everything after it is discarded.
        kept_from: RecordId,
    },
    /// Extension-owned opaque record (namespaced; engine reserves `x.ka.*`).
    Custom {
        /// Record identifier.
        id: RecordId,
        /// Reverse-domain-ish namespace.
        ns: String,
        /// Opaque payload.
        data: serde_json::Value,
    },
    /// A stored title override (forks mark themselves this way; the
    /// summary title comes from here instead of the first user message).
    Title {
        /// Record identifier.
        id: RecordId,
        /// The display title.
        title: String,
    },
    /// A stored session name (memorable alias): set by the user, last
    /// one wins, resolvable by `ka --session <name>` (exact, then
    /// unique prefix) before title matching.
    Name {
        /// Record identifier.
        id: RecordId,
        /// The session's memorable name.
        name: String,
    },
    /// Per-turn token/cost accounting, appended by the engine at each
    /// TurnFinished. `list()` sums these into the summary fields.
    Usage {
        /// Record identifier.
        id: RecordId,
        /// Turn cost in USD (0.0 when unpriced/unknown).
        cost: f64,
        /// Input tokens.
        input: u64,
        /// Output tokens.
        output: u64,
        /// Cache-read tokens.
        cache_read: u64,
        /// Cache-write tokens.
        cache_write: u64,
    },
}

impl Record {
    /// The record's identifier, regardless of kind.
    pub fn id(&self) -> Option<&RecordId> {
        match self {
            Record::Header { .. } => None,
            Record::Message { id, .. }
            | Record::Change { id, .. }
            | Record::Digest { id, .. }
            | Record::Boundary { id, .. }
            | Record::Rewind { id, .. }
            | Record::Title { id, .. }
            | Record::Name { id, .. }
            | Record::Usage { id, .. }
            | Record::Custom { id, .. } => Some(id),
        }
    }
}

static RECORD_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a fresh, roughly-sortable record id.
pub fn new_record_id() -> RecordId {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let n = RECORD_COUNTER.fetch_add(1, Ordering::Relaxed);
    RecordId(format!("r{millis:x}-{n:x}"))
}

/// Generate a fresh strand id: `s{millis:x}-{seq:02x}{rand:010x}` — the
/// millis head keeps listing order stable across seconds, the per-process
/// sequence orders bursts inside one millisecond, and the 40 random bits
/// make ids referenceable by prefix (`ka --session 806890f4`).
pub fn new_strand_id() -> StrandId {
    static NONCE: AtomicU64 = AtomicU64::new(0);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let n = NONCE.fetch_add(1, Ordering::Relaxed);
    // RandomState seeds differ per process (and per call); mixing in the
    // counter keeps same-process ids distinct too.
    use std::hash::{BuildHasher, Hasher as _};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(millis.rotate_left(32) ^ n);
    let rand40 = hasher.finish() & 0xFF_FFFF_FFFF;
    StrandId(format!("s{millis:x}-{n:02x}{rand40:010x}"))
}

thread_local! {
    static DATA_DIR_OVERRIDE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Override the data root for this thread (engine tests).
#[doc(hidden)]
pub fn set_data_dir_for_tests(dir: PathBuf) {
    DATA_DIR_OVERRIDE.with(|d| *d.borrow_mut() = Some(dir));
}

/// Data root: `KA_DATA_DIR` (tests) > platform data base (see
/// [`data_base`]) + `ka`.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = DATA_DIR_OVERRIDE.with(|d| d.borrow().clone()) {
        return dir;
    }
    if let Ok(dir) = std::env::var("KA_DATA_DIR") {
        return PathBuf::from(dir);
    }
    data_base(
        std::env::var("XDG_DATA_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
        std::env::var("LOCALAPPDATA").ok().as_deref(),
        cfg!(windows),
    )
    .join("ka")
}

/// Platform data base, factored pure for tests. Unix: `XDG_DATA_HOME` >
/// `~/.local/share` > temp. Windows: `%LOCALAPPDATA%` first — the store
/// must not depend on the shell (Git Bash sets `HOME`, PowerShell does
/// not, so a HOME-first chain flip-flops the store between terminals) —
/// then the same HOME fallback, then temp.
pub fn data_base(
    xdg: Option<&str>,
    home: Option<&str>,
    localappdata: Option<&str>,
    windows: bool,
) -> PathBuf {
    let home_dir = home.map(|h| PathBuf::from(h).join(".local/share"));
    if windows {
        return localappdata
            .map(PathBuf::from)
            .or(home_dir)
            .unwrap_or_else(std::env::temp_dir);
    }
    xdg.map(PathBuf::from)
        .or(home_dir)
        .unwrap_or_else(std::env::temp_dir)
}

fn encode_cwd(cwd: &Path) -> String {
    let s = cwd.to_string_lossy().replace('/', "-");
    let s = if cfg!(windows) {
        sanitize_windows_key(s)
    } else {
        s
    };
    format!("-{s}")
}

/// Windows store-key sanitizing: `\` would nest components under the
/// strand dir and `:` is an illegal NTFS name character (`C:\x\y` would
/// otherwise encode to the unusable `-C:\x\y`). Both become `-`. Unix
/// cwd strings never contain `\`, and `:` is legal-but-rare there, so
/// Unix encodings stay byte-identical.
fn sanitize_windows_key(s: String) -> String {
    s.replace(['\\', ':'], "-")
}

/// The strand directory for a working directory.
pub fn strand_dir(cwd: &Path) -> PathBuf {
    data_dir().join("strands").join(encode_cwd(cwd))
}

/// Windows long paths: past ~240 chars, std::fs calls fail at MAX_PATH
/// unless the path carries the `\\?\` verbatim prefix (which skips Win32
/// normalization). Apply it only when needed — short paths keep the
/// normal spelling, so the conversion's blast radius is exactly the
/// deep-cwd case it exists for. Non-Windows: identity.
fn store_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        const THRESHOLD: usize = 240;
        if path.as_os_str().len() >= THRESHOLD {
            if let Some(v) = verbatim(path) {
                return v;
            }
        }
        path.to_path_buf()
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

/// Lexical `\\?\` conversion for absolute Windows paths. `None` for
/// anything unusual (relative paths, device prefixes) — the caller then
/// keeps the plain spelling rather than guessing.
#[cfg(windows)]
fn verbatim(path: &Path) -> Option<PathBuf> {
    use std::path::{Component, Prefix};
    let mut out = PathBuf::from(r"\\?\");
    for comp in path.components() {
        match comp {
            Component::Prefix(p) => match p.kind() {
                Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                    out.push(format!("{}:", letter));
                }
                Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                    out =
                        PathBuf::from(format!(r"\\?\UNC\{}\{}", server.display(), share.display()));
                }
                _ => return None,
            },
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(s) => out.push(s),
        }
    }
    Some(out)
}

/// A strand under management: path (None until first append for fresh
/// strands — opening the TUI no longer litters empty session files),
/// records, live settings.
#[derive(Debug)]
pub struct StrandFile {
    path: Option<PathBuf>,
    /// Working directory, kept for lazy materialization.
    cwd: PathBuf,
    records: Vec<Record>,
    settings: Settings,
}

/// Replayed session settings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    /// Last-set model selector.
    pub model: Option<String>,
    /// Last-set effort.
    pub effort: Option<Effort>,
    /// Last-set permission mode.
    pub mode: Option<Mode>,
    /// Last feature-toggle snapshot: the full disabled-spec list. None
    /// until the session records one (presence matters — an empty
    /// snapshot is a real "everything on").
    pub features: Option<Vec<String>>,
    /// Last-set sandbox mode (`"off"` | `"fs"`).
    pub sandbox: Option<String>,
    /// True when the file ended on a dangling (user-tail) turn.
    pub dangling: bool,
}

impl Settings {
    /// Fold one `Record::Change` into the settings (shared by open-time
    /// replay and live append). Feature/sandbox fields are snapshots:
    /// presence replaces, absence leaves the accumulated value alone.
    fn apply(&mut self, change: &Record) {
        let Record::Change {
            model,
            effort,
            mode,
            features,
            sandbox,
            ..
        } = change
        else {
            return;
        };
        if let Some(m) = model {
            self.model = Some(m.clone());
        }
        if let Some(e) = effort {
            self.effort = Some(*e);
        }
        if let Some(m) = mode {
            self.mode = Some(*m);
        }
        if let Some(list) = features {
            self.features = Some(list.clone());
        }
        if let Some(s) = sandbox {
            self.sandbox = Some(s.clone());
        }
    }
}

impl StrandFile {
    /// Create a fresh strand for `cwd`. The file is materialized on the
    /// first append — a session that never gets a message leaves nothing
    /// on disk.
    pub fn create(cwd: &Path, repo: Option<RepoSnapshot>) -> std::io::Result<Self> {
        let id = new_strand_id();
        let header = Record::Header {
            id: id.clone(),
            ts: now_rfc3339(),
            cwd: cwd.to_string_lossy().into_owned(),
            version: 1,
            repo,
            parent: None,
        };
        Ok(Self {
            path: None,
            cwd: cwd.to_path_buf(),
            records: vec![header],
            settings: Settings::default(),
        })
    }

    /// Mark this strand as a fork of `parent` (tree navigation).
    pub fn set_parent(&mut self, parent: &str) {
        if let Some(Record::Header { parent: p, .. }) = self.records.first_mut() {
            *p = Some(parent.to_string());
        }
    }

    /// Materialize the strand file on first use: directory + header.
    fn materialize(&mut self) -> std::io::Result<PathBuf> {
        if let Some(path) = &self.path {
            return Ok(path.clone());
        }
        let dir = store_path(&strand_dir(&self.cwd));
        std::fs::create_dir_all(&dir)?;
        let header = self
            .records
            .first()
            .and_then(|r| match r {
                Record::Header { id, ts, .. } => {
                    Some(format!("{}_{}.jsonl", ts.replace(':', ""), id.0))
                }
                _ => None,
            })
            .ok_or_else(|| std::io::Error::other("strand missing header"))?;
        let path = dir.join(header);
        let mut writer = StrandWriter::open(&path)?;
        let first = self
            .records
            .first()
            .ok_or_else(|| std::io::Error::other("strand missing header"))?;
        writer.append(first)?;
        self.path = Some(path.clone());
        Ok(path)
    }

    /// Open an existing strand, replaying settings and detecting dangling
    /// turns. Missing file is an error (use listing to pick paths).
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let records = read(path)?;
        if records.is_empty() || !matches!(records[0], Record::Header { .. }) {
            return Err(std::io::Error::other(
                "not a strand file (missing header record)",
            ));
        }
        let mut settings = Settings::default();
        for record in &records {
            settings.apply(record);
        }
        settings.dangling = matches!(
            records.last(),
            Some(Record::Message {
                role: Role::User,
                ..
            })
        );
        Ok(Self {
            path: Some(path.to_path_buf()),
            cwd: path.parent().map(|p| p.to_path_buf()).unwrap_or_default(),
            records,
            settings,
        })
    }

    /// Synthesize an aborted marker for a dangling turn, if present.
    pub fn synthesize_aborted(&mut self) -> std::io::Result<bool> {
        if !self.settings.dangling {
            return Ok(false);
        }
        let record = Record::Message {
            id: new_record_id(),
            role: Role::Assistant,
            content: "(turn interrupted)".to_string(),
            calls: Vec::new(),
            results: Vec::new(),
            thinking: None,
        };
        self.append(record)?;
        self.settings.dangling = false;
        Ok(true)
    }

    /// Append a record (writes through to disk).
    pub fn append(&mut self, record: Record) -> std::io::Result<()> {
        self.settings.apply(&record);
        if matches!(
            record,
            Record::Message {
                role: Role::User,
                ..
            }
        ) {
            self.settings.dangling = true;
        } else if matches!(record, Record::Message { .. }) {
            self.settings.dangling = false;
        }
        let path = self.materialize()?;
        let mut writer = StrandWriter::open(&path)?;
        writer.append(&record)?;
        self.records.push(record);
        Ok(())
    }

    /// The strand's file path (None until the first append).
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// All records in order.
    pub fn records(&self) -> &[Record] {
        &self.records
    }

    /// Replayed settings.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Take the settings (for engine bootstrap).
    pub fn into_settings(self) -> Settings {
        self.settings
    }

    /// Records appended after `since` (index into records).
    pub fn records_since(&self, since: usize) -> &[Record] {
        self.records.get(since..).unwrap_or(&[])
    }

    /// Current record count.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the strand holds only its header.
    pub fn is_empty(&self) -> bool {
        self.records.len() <= 1
    }
}

/// One line of the writer. Kept separate so raw appending stays cheap.
pub struct StrandWriter {
    file: File,
}

impl StrandWriter {
    /// Open (or create) a strand file for appending.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { file })
    }

    /// Append one record and flush. Appends are the only writes strands get.
    pub fn append(&mut self, record: &Record) -> std::io::Result<()> {
        let mut line = serde_json::to_string(record)
            .map_err(|e| std::io::Error::other(format!("record serialize: {e}")))?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.file.flush()
    }
}

/// Summary of a strand for pickers.
#[derive(Debug, Clone, PartialEq)]
pub struct StrandSummary {
    /// File path.
    pub path: PathBuf,
    /// Strand id.
    pub id: String,
    /// Creation timestamp.
    pub ts: String,
    /// Stored title, first user message (truncated), or "(empty)".
    pub title: String,
    /// Message count.
    pub messages: usize,
    /// Summed cost in USD across the strand's Usage records.
    pub cost: f64,
    /// Parent strand id when this strand is a fork.
    pub parent: Option<String>,
    /// Memorable name (last `Name` record wins); `ka --session <name>`
    /// resolves on it before title matching.
    pub name: Option<String>,
    /// Summed tokens (input + output + cache reads/writes) across the
    /// strand's Usage records; 0 for strands without usage accounting.
    pub tokens: u64,
}

/// List strands for a working directory, newest first. Scans cheaply:
/// full parse for the header line only, `"record":…` prefix tests for
/// counting/summing, and one partial probe for the title — no
/// per-record deserialization of calls/results.
/// Just the fields a summary needs off a message line.
#[derive(serde::Deserialize)]
struct TitleProbe {
    role: Option<Role>,
    content: Option<String>,
}

/// Just the fields a summary needs off a usage line.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct UsageProbe {
    cost: f64,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

pub fn list(cwd: &Path) -> std::io::Result<Vec<StrandSummary>> {
    let dir = strand_dir(cwd);
    let mut summaries = Vec::new();
    let entries = match std::fs::read_dir(store_path(&dir)) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(summaries),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        if let Some(summary) = summarize(&path) {
            summaries.push(summary);
        }
    }
    // ids embed millisecond timestamps (s{millis:x}), so they sort newer
    // first even when RFC 3339 timestamps tie at second granularity.
    summaries.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(summaries)
}

/// One strand's header (first line only): `(id, ts, parent)`.
fn header_of(path: &Path) -> Option<(String, String, Option<String>)> {
    let file = File::open(path).ok()?;
    let first = BufReader::new(file).lines().next()?.ok()?;
    let Ok(Record::Header { id, ts, parent, .. }) = serde_json::from_str(&first) else {
        return None;
    };
    Some((id.0, ts, parent))
}

/// Fully summarize one strand file (title/messages/cost/tokens).
fn summarize(path: &Path) -> Option<StrandSummary> {
    let (id, ts, parent) = header_of(path)?;
    let file = File::open(path).ok()?;
    let lines = BufReader::new(file).lines();
    let mut title = String::new();
    let mut name: Option<String> = None;
    let mut messages = 0usize;
    let mut cost = 0.0f64;
    let mut tokens = 0u64;
    for line in lines.map_while(Result::ok) {
        if line.starts_with("{\"record\":\"message\"") {
            messages += 1;
            if title.is_empty() {
                if let Ok(probe) = serde_json::from_str::<TitleProbe>(&line) {
                    if probe.role == Some(Role::User) {
                        if let Some(content) = probe.content {
                            let first_line = content
                                .lines()
                                .next()
                                .unwrap_or("")
                                .chars()
                                .take(60)
                                .collect::<String>();
                            if !first_line.is_empty() {
                                title = first_line;
                            }
                        }
                    }
                }
            }
        } else if line.starts_with("{\"record\":\"title\"") {
            // stored title (forks): wins over the first-user-message probe
            if let Ok(Record::Title { title: t, .. }) = serde_json::from_str(&line) {
                title = t;
            }
        } else if line.starts_with("{\"record\":\"name\"") {
            // memorable name: last one wins, purely an alias (content
            // stays the title's job)
            if let Ok(Record::Name { name: n, .. }) = serde_json::from_str(&line) {
                name = Some(n);
            }
        } else if line.starts_with("{\"record\":\"usage\"") {
            if let Ok(probe) = serde_json::from_str::<UsageProbe>(&line) {
                cost += probe.cost;
                tokens += probe.input + probe.output + probe.cache_read + probe.cache_write;
            }
        }
    }
    if title.is_empty() {
        title = "(empty)".to_string();
    }
    Some(StrandSummary {
        path: path.to_path_buf(),
        id,
        ts,
        title,
        messages,
        cost,
        parent,
        name,
        tokens,
    })
}

/// Every strand file under `cwd`'s strand dir, header-only (first line
/// each — cheap; ids sort newest-first like [`list`]).
fn headers(cwd: &Path) -> std::io::Result<Vec<(PathBuf, String)>> {
    let dir = strand_dir(cwd);
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(store_path(&dir)) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        if let Some((id, _, _)) = header_of(&path) {
            out.push((path, id));
        }
    }
    out.sort_by(|a, b| b.1.cmp(&a.1));
    Ok(out)
}

/// Most recent strand for a working directory. Header-only directory
/// scan; just the newest file is fully parsed.
pub fn latest(cwd: &Path) -> std::io::Result<Option<StrandSummary>> {
    Ok(headers(cwd)?
        .into_iter()
        .next()
        .and_then(|(path, _)| summarize(&path)))
}

/// Outcome of resolving a user-supplied session reference.
#[derive(Debug, Clone, PartialEq)]
pub enum IdMatch {
    /// No strand matched.
    None,
    /// Exactly one strand matched the prefix.
    Unique(StrandSummary),
    /// Several matched; the user must be more specific.
    Ambiguous(Vec<StrandSummary>),
}

/// Resolve a session reference: full id, id prefix, or an existing file
/// path. Ids embed their creation millis, so any prefix of the random
/// tail (or the whole id) works.
pub fn resolve_id(cwd: &Path, needle: &str) -> std::io::Result<IdMatch> {
    let needle = needle.trim();
    if needle.is_empty() {
        return Ok(IdMatch::None);
    }
    let path = Path::new(needle);
    if path.exists() {
        return Ok(IdMatch::Unique(StrandSummary {
            path: path.to_path_buf(),
            id: needle.to_string(),
            ts: String::new(),
            title: String::new(),
            messages: 0,
            cost: 0.0,
            tokens: 0,
            name: None,
            parent: None,
        }));
    }
    let mut matches: Vec<StrandSummary> = strands_filter(cwd, needle)?;
    match matches.len() {
        0 => Ok(IdMatch::None),
        1 => Ok(IdMatch::Unique(matches.swap_remove(0))),
        _ => Ok(IdMatch::Ambiguous(matches)),
    }
}

fn strands_filter(cwd: &Path, needle: &str) -> std::io::Result<Vec<StrandSummary>> {
    // header-only scan (first line per file); full summaries are built
    // only for actual matches — a prefix resolve must not read every
    // byte of every strand
    Ok(headers(cwd)?
        .into_iter()
        .filter(|(_, id)| {
            id.starts_with(needle)
                || id
                    .split_once('-')
                    .is_some_and(|(_, tail)| tail.starts_with(needle))
        })
        .filter_map(|(path, _)| summarize(&path))
        .collect())
}

/// Resolve a session reference by its memorable name: exact match
/// first (newest strand wins on collisions), then a unique name
/// prefix. Callers try this after [`resolve_id`] and before any
/// title-substring matching.
pub fn resolve_name(cwd: &Path, needle: &str) -> std::io::Result<IdMatch> {
    let needle = needle.trim();
    if needle.is_empty() {
        return Ok(IdMatch::None);
    }
    let named: Vec<StrandSummary> = list(cwd)?
        .into_iter()
        .filter(|s| s.name.as_deref().is_some_and(|n| !n.is_empty()))
        .collect();
    // exact: newest wins (list is newest-first)
    if let Some(exact) = named.iter().find(|s| s.name.as_deref() == Some(needle)) {
        return Ok(IdMatch::Unique(exact.clone()));
    }
    let mut prefix: Vec<StrandSummary> = named
        .into_iter()
        .filter(|s| s.name.as_deref().is_some_and(|n| n.starts_with(needle)))
        .collect();
    match prefix.len() {
        0 => Ok(IdMatch::None),
        1 => Ok(IdMatch::Unique(prefix.swap_remove(0))),
        _ => Ok(IdMatch::Ambiguous(prefix)),
    }
}

impl IdMatch {
    /// The matched summary (None/Ambiguous → None).
    pub fn into_summary(self) -> Option<StrandSummary> {
        match self {
            IdMatch::Unique(s) => Some(s),
            _ => None,
        }
    }
}

/// The display title for a strand's records: the stored [`Record::Title`]
/// when present, else the first user message (truncated), else
/// `"(empty)"`. Shared by [`list`] and the engine's fork path.
pub fn title_of(records: &[Record]) -> String {
    let mut title = String::new();
    for record in records {
        match record {
            Record::Title { title: t, .. } => return t.clone(),
            Record::Message {
                role: Role::User,
                content,
                ..
            } if title.is_empty() => {
                let first_line = content
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(60)
                    .collect::<String>();
                if !first_line.is_empty() {
                    title = first_line;
                }
            }
            _ => {}
        }
    }
    if title.is_empty() {
        "(empty)".to_string()
    } else {
        title
    }
}

/// Current time as RFC 3339 UTC (shared by strand headers and
/// checkpoint labels).
pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // days→date civil algorithm (Howard Hinnant) compressed
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Read every record from a strand file. Malformed lines fail with the
/// offending line number; a missing file is an empty vec (fresh strand).
pub fn read(path: &Path) -> std::io::Result<Vec<Record>> {
    let file = match File::open(store_path(path)) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    let mut iter = BufReader::new(file).lines().enumerate();
    while let Some((idx, line)) = iter.next() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let record: Record = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                // a torn FINAL line (crash mid-append; the writer has no
                // fsync) must not make the whole session unresumable —
                // skip it. Mid-file corruption stays a hard error.
                let exhausted = iter.next().is_none();
                if exhausted {
                    break;
                }
                return Err(std::io::Error::other(format!(
                    "{}:{}: malformed record: {e}",
                    path.display(),
                    idx + 1
                )));
            }
        };
        out.push(record);
    }
    Ok(out)
}

/// Render records as markdown — the exact shape the CLI's `ka export`
/// writes to a file or stdout (shared with the TUI's `/export`).
pub fn render_markdown(records: &[Record]) -> String {
    let mut md = String::from("# ka session\n\n");
    for r in records {
        match r {
            Record::Header { id, ts, .. } => {
                md.push_str(&format!("> strand `{}` at {}\n\n", id.0, ts));
            }
            Record::Message { role, content, .. } => {
                let who = match role {
                    Role::User => "**you**",
                    Role::Tool => "*tool*",
                    _ => "**ka**",
                };
                if !content.trim().is_empty() {
                    md.push_str(&format!("### {who}\n\n{}\n\n", content.trim()));
                }
            }
            Record::Digest { summary, .. } => {
                md.push_str(&format!("### *digest*\n\n> {}\n\n", summary.trim()));
            }
            Record::Rewind { .. } => md.push_str("### *rewound*\n\n"),
            _ => {}
        }
    }
    md
}

/// Render a strand export as a self-contained HTML page: the markdown
/// rides in an inert escaped island and a vendored renderer turns it
/// into DOM offline — no dependencies. Shared by the CLI's
/// `ka export --html` and the TUI's `/export --html`.
pub fn render_html(title: &str, markdown: &str) -> String {
    const TEMPLATE: &str = include_str!("export_template.html");
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    TEMPLATE
        .replace("__KA_TITLE__", &esc(title))
        .replace("__KA_MARKDOWN__", &esc(markdown))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn windows_cwd_keys_are_flat_and_legal() {
        // `C:\Users\x\proj` must not survive as `-C:\Users\x\proj`: the
        // backslashes would nest components under strands/ and `-C:` is
        // an illegal NTFS name (colon), so persistence would fail with
        // os error 123 on every append.
        assert_eq!(
            sanitize_windows_key("C:\\Users\\x\\proj".to_string()),
            "C--Users-x-proj"
        );
        // Unix encodings are untouched by the Windows branch.
        assert_eq!(
            sanitize_windows_key("/home/u/proj".to_string()),
            "/home/u/proj"
        );
    }

    #[test]
    fn data_base_orders_by_platform() {
        let (xdg, home, local) = (
            Some("/xdg".to_string()),
            Some("/home/u".to_string()),
            Some("C:\\Users\\u\\AppData\\Local".to_string()),
        );
        // Unix: XDG > HOME > temp (historical behavior, unchanged).
        assert_eq!(
            data_base(xdg.as_deref(), home.as_deref(), local.as_deref(), false),
            PathBuf::from("/xdg")
        );
        assert_eq!(
            data_base(None, home.as_deref(), local.as_deref(), false),
            PathBuf::from("/home/u/.local/share")
        );
        // Windows: LOCALAPPDATA first — shell-independent (Git Bash sets
        // HOME, PowerShell does not; the store must not move between
        // them) — then HOME, then temp.
        assert_eq!(
            data_base(xdg.as_deref(), home.as_deref(), local.as_deref(), true),
            PathBuf::from("C:\\Users\\u\\AppData\\Local")
        );
        assert_eq!(
            data_base(None, home.as_deref(), None, true),
            PathBuf::from("/home/u/.local/share")
        );
        assert_eq!(data_base(None, None, None, true), std::env::temp_dir());
    }

    fn temp_data(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ka-strand3-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        DATA_DIR_OVERRIDE.with(|d| *d.borrow_mut() = Some(dir.clone()));
        dir
    }

    #[test]
    fn strandfile_roundtrip_with_tools_and_settings() {
        let data = temp_data("roundtrip");
        let cwd = PathBuf::from("/tmp/proj");
        let mut strand = StrandFile::create(
            &cwd,
            Some(RepoSnapshot {
                branch: "main".into(),
                dirty: vec!["x".into()],
            }),
        )
        .unwrap();
        strand
            .append(Record::Message {
                id: new_record_id(),
                role: Role::User,
                content: "go".into(),
                calls: Vec::new(),
                results: Vec::new(),
                thinking: None,
            })
            .unwrap();
        strand
            .append(Record::Message {
                id: new_record_id(),
                role: Role::Assistant,
                content: "working".into(),
                calls: vec![StoredCall {
                    id: "c1".into(),
                    tool: "read".into(),
                    arguments: serde_json::json!({"path": "x"}),
                }],
                results: Vec::new(),
                thinking: None,
            })
            .unwrap();
        strand
            .append(Record::Message {
                id: new_record_id(),
                role: Role::Tool,
                content: String::new(),
                calls: Vec::new(),
                results: vec![StoredResult {
                    call_id: "c1".into(),
                    content: "file body".into(),
                    is_error: false,
                }],
                thinking: None,
            })
            .unwrap();
        strand
            .append(Record::Change {
                id: new_record_id(),
                model: Some("openai/gpt-5.1@high".into()),
                effort: Some(Effort::High),
                mode: Some(Mode::Allow),
                features: None,
                sandbox: None,
            })
            .unwrap();

        let reopened = StrandFile::open(strand.path().unwrap()).unwrap();
        assert_eq!(reopened.records().len(), 5);
        let settings = reopened.settings();
        assert_eq!(settings.model.as_deref(), Some("openai/gpt-5.1@high"));
        assert_eq!(settings.mode, Some(Mode::Allow));
        assert!(!settings.dangling);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn feature_toggles_replay_as_snapshots() {
        let data = temp_data("features");
        let mut strand = StrandFile::create(&PathBuf::from("/tmp/proj"), None).unwrap();
        // first snapshot disables two specs
        strand
            .append(Record::Change {
                id: new_record_id(),
                model: None,
                effort: None,
                mode: None,
                features: Some(vec!["agents".into(), "mcp:github".into()]),
                sandbox: Some("fs".into()),
            })
            .unwrap();
        // a model change in between must not touch the toggle set
        strand
            .append(Record::Change {
                id: new_record_id(),
                model: Some("zai/glm-5.3".into()),
                effort: None,
                mode: None,
                features: None,
                sandbox: None,
            })
            .unwrap();
        // second snapshot replaces the whole set
        strand
            .append(Record::Change {
                id: new_record_id(),
                model: None,
                effort: None,
                mode: None,
                features: Some(vec!["tool:bash".into()]),
                sandbox: None,
            })
            .unwrap();
        let settings = StrandFile::open(strand.path().unwrap())
            .unwrap()
            .into_settings();
        assert_eq!(settings.features, Some(vec!["tool:bash".to_string()]));
        assert_eq!(settings.sandbox.as_deref(), Some("fs"));
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn legacy_change_lines_still_decode() {
        // strands recorded before the additive fields existed
        let data = temp_data("legacy-change");
        let dir = store_path(&strand_dir(&PathBuf::from("/tmp/legacy")));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("20260101T000000_s.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"record":"header","id":"s","ts":"2026-01-01T00:00:00Z","cwd":"/tmp","#,
                r#""version":1}"#,
                "\n",
                r#"{"record":"change","id":"r1","model":"m","effort":"high","mode":"free"}"#,
                "\n",
            ),
        )
        .unwrap();
        let settings = StrandFile::open(&path).unwrap().into_settings();
        assert_eq!(settings.model.as_deref(), Some("m"));
        assert_eq!(settings.features, None);
        assert_eq!(settings.sandbox, None);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn dangling_turn_detected_and_synthesized() {
        let data = temp_data("dangling");
        let cwd = PathBuf::from("/tmp/proj2");
        let mut strand = StrandFile::create(&cwd, None).unwrap();
        strand
            .append(Record::Message {
                id: new_record_id(),
                role: Role::User,
                content: "hello?".into(),
                calls: Vec::new(),
                results: Vec::new(),
                thinking: None,
            })
            .unwrap();
        assert!(
            StrandFile::open(strand.path().unwrap())
                .unwrap()
                .settings()
                .dangling
        );

        let mut resumed = StrandFile::open(strand.path().unwrap()).unwrap();
        assert!(resumed.synthesize_aborted().unwrap());
        let clean = StrandFile::open(strand.path().unwrap()).unwrap();
        assert!(!clean.settings().dangling);
        match clean.records().last() {
            Some(Record::Message { content, .. }) => assert_eq!(content, "(turn interrupted)"),
            other => panic!("expected synthesized message, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn listing_newest_first_with_titles() {
        let data = temp_data("listing");
        let cwd = PathBuf::from("/tmp/proj3");
        for prompt in ["first session query", "second session query"] {
            let mut strand = StrandFile::create(&cwd, None).unwrap();
            strand
                .append(Record::Message {
                    id: new_record_id(),
                    role: Role::User,
                    content: prompt.into(),
                    calls: Vec::new(),
                    results: Vec::new(),
                    thinking: None,
                })
                .unwrap();
        }
        let listed = list(&cwd).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed[0].title.contains("second"), "{}", listed[0].title);
        assert!(listed[0].messages >= 1);
        assert!(latest(&cwd).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn fresh_strand_materializes_on_first_append() {
        let data = temp_data("lazy");
        let cwd = PathBuf::from("/tmp/proj-lazy");
        let mut strand = StrandFile::create(&cwd, None).unwrap();
        assert!(strand.path().is_none(), "no file before the first message");
        strand
            .append(Record::Message {
                id: new_record_id(),
                role: Role::User,
                content: "first message".into(),
                calls: Vec::new(),
                results: Vec::new(),
                thinking: None,
            })
            .unwrap();
        let path = strand.path().expect("materialized").to_path_buf();
        assert!(path.exists());
        let records = read(&path).unwrap();
        assert!(matches!(records[0], Record::Header { .. }));
        assert_eq!(records.len(), 2, "header + first message");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn listing_survives_a_corrupted_tail() {
        let data = temp_data("corrupt");
        let cwd = PathBuf::from("/tmp/proj-corrupt");
        let mut strand = StrandFile::create(&cwd, None).unwrap();
        strand
            .append(Record::Message {
                id: new_record_id(),
                role: Role::User,
                content: "survives corruption".into(),
                calls: Vec::new(),
                results: Vec::new(),
                thinking: None,
            })
            .unwrap();
        let path = strand.path().unwrap().to_path_buf();
        // append garbage mid-file (simulates a torn write)
        let mut writer = StrandWriter::open(&path).unwrap();
        writer
            .append(&Record::Message {
                id: new_record_id(),
                role: Role::Assistant,
                content: "fine".into(),
                calls: Vec::new(),
                results: Vec::new(),
                thinking: None,
            })
            .unwrap();
        drop(writer);
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"{not json at all\n").unwrap();
        }
        let listed = list(&cwd).unwrap();
        assert_eq!(listed.len(), 1, "corrupted tail must not hide the strand");
        assert!(listed[0].title.contains("survives corruption"));
        assert!(listed[0].messages >= 2);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn read_skips_torn_final_line_but_errors_midfile() {
        let data = temp_data("torn");
        let cwd = PathBuf::from("/tmp/proj-torn");
        let mut strand = StrandFile::create(&cwd, None).unwrap();
        strand
            .append(Record::Message {
                id: new_record_id(),
                role: Role::User,
                content: "kept".into(),
                calls: Vec::new(),
                results: Vec::new(),
                thinking: None,
            })
            .unwrap();
        let path = strand.path().unwrap().to_path_buf();
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"{\"record\":\"mes").unwrap(); // torn final line
        }
        let records = read(&path).unwrap();
        assert!(records.len() >= 2, "torn tail skipped, session resumable");
        // mid-file garbage stays a hard error (real corruption)
        let mut mangled = std::fs::read_to_string(&path).unwrap();
        mangled.push_str("{\"record\":\"message\"}\n{\"record\":\"usage\"}\n");
        std::fs::write(&path, mangled).unwrap();
        assert!(read(&path).is_err(), "mid-file malformed record errors");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn strand_ids_are_long_hashed_and_distinct() {
        let a = new_strand_id().0;
        let b = new_strand_id().0;
        assert_ne!(a, b);
        assert!(a.starts_with('s'));
        // s{millis:x}-{12 hex}
        let (head, tail) = a.split_once('-').unwrap();
        assert!(head.starts_with('s') && head.len() > 1, "{a}");
        assert_eq!(tail.len(), 12, "random tail is 12 hex chars: {a}");
        assert!(tail.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn resolve_id_matches_full_prefix_and_tail() {
        let data = temp_data("resolve");
        let cwd = PathBuf::from("/tmp/proj-resolve");
        let mut strands = Vec::new();
        for prompt in ["alpha session", "beta session"] {
            let mut s = StrandFile::create(&cwd, None).unwrap();
            s.append(Record::Message {
                id: new_record_id(),
                role: Role::User,
                content: prompt.into(),
                calls: Vec::new(),
                results: Vec::new(),
                thinking: None,
            })
            .unwrap();
            strands.push(s);
        }
        let listed = list(&cwd).unwrap();
        let target = &listed[1]; // oldest = "alpha"
        // full id
        match resolve_id(&cwd, &target.id).unwrap() {
            IdMatch::Unique(s) => assert_eq!(s.id, target.id),
            other => panic!("full id must resolve uniquely: {other:?}"),
        }
        // leading prefix (millis heads collide for same-ms sessions, so
        // include the tail start to pin one session)
        let head: String = target.id.chars().take(6).collect();
        match resolve_id(&cwd, &head).unwrap() {
            IdMatch::Ambiguous(cands) => assert_eq!(cands.len(), 2),
            other => panic!("same-millis head should be ambiguous: {other:?}"),
        }
        let (head_part, tail_part) = target.id.split_once('-').unwrap();
        let pinned = format!("{head_part}-{tail_part}");
        match resolve_id(&cwd, &pinned).unwrap() {
            IdMatch::Unique(s) => assert_eq!(s.id, target.id),
            other => panic!("full id must resolve: {other:?}"),
        }
        // random-tail prefix
        let tail = target.id.split_once('-').unwrap().1;
        let tail4: String = tail.chars().take(4).collect();
        match resolve_id(&cwd, &tail4).unwrap() {
            IdMatch::Unique(s) => assert_eq!(s.id, target.id),
            other => panic!("tail prefix must resolve: {other:?}"),
        }
        // garbage
        assert!(matches!(
            resolve_id(&cwd, "zz-nothing").unwrap(),
            IdMatch::None
        ));
        // empty
        assert!(matches!(resolve_id(&cwd, "  ").unwrap(), IdMatch::None));
        drop(strands);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn name_records_surface_in_summaries_and_resolve() {
        let data = temp_data("names");
        let cwd = PathBuf::from("/tmp/proj-names");
        let mut a = StrandFile::create(&cwd, None).unwrap();
        a.append(Record::Message {
            id: new_record_id(),
            role: Role::User,
            content: "fix the pump".into(),
            calls: Vec::new(),
            results: Vec::new(),
            thinking: None,
        })
        .unwrap();
        a.append(Record::Name {
            id: new_record_id(),
            name: "pump".into(),
        })
        .unwrap();
        // a rename later in the file wins
        a.append(Record::Name {
            id: new_record_id(),
            name: "fox".into(),
        })
        .unwrap();
        let mut b = StrandFile::create(&cwd, None).unwrap();
        b.append(Record::Name {
            id: new_record_id(),
            name: "ferret".into(),
        })
        .unwrap();
        drop((a, b));
        // summaries carry the LAST name; the title stays content
        let listed = list(&cwd).unwrap();
        let fox = listed
            .iter()
            .find(|s| s.name.as_deref() == Some("fox"))
            .expect("last name wins");
        assert_eq!(fox.title, "fix the pump", "name is an alias, not a title");
        // exact resolution
        match resolve_name(&cwd, "fox").unwrap() {
            IdMatch::Unique(s) => assert_eq!(s.id, fox.id),
            other => panic!("exact name must resolve: {other:?}"),
        }
        // unique prefix resolution
        match resolve_name(&cwd, "fer").unwrap() {
            IdMatch::Unique(s) => assert_eq!(s.name.as_deref(), Some("ferret")),
            other => panic!("name prefix must resolve: {other:?}"),
        }
        // shared prefix of "fox"/"ferret" is ambiguous
        assert!(matches!(
            resolve_name(&cwd, "f").unwrap(),
            IdMatch::Ambiguous(_)
        ));
        // unknown and empty
        assert!(matches!(
            resolve_name(&cwd, "badger").unwrap(),
            IdMatch::None
        ));
        assert!(matches!(resolve_name(&cwd, "  ").unwrap(), IdMatch::None));
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn duplicate_exact_names_resolve_newest() {
        let data = temp_data("names-dup");
        let cwd = PathBuf::from("/tmp/proj-names-dup");
        for _ in 0..2 {
            let mut s = StrandFile::create(&cwd, None).unwrap();
            s.append(Record::Name {
                id: new_record_id(),
                name: "twin".into(),
            })
            .unwrap();
        }
        // list() is newest-first, so exact matches resolve the newest
        let listed = list(&cwd).unwrap();
        assert_eq!(listed.len(), 2);
        let newest = listed.first().unwrap();
        match resolve_name(&cwd, "twin").unwrap() {
            IdMatch::Unique(s) => assert_eq!(s.id, newest.id),
            other => panic!("exact duplicate name must resolve newest: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn ids_are_fresh_and_sortableish() {
        let a = new_record_id();
        let b = new_record_id();
        assert_ne!(a, b);
        assert!(a.0.starts_with('r'));
        assert!(new_strand_id().0.starts_with('s'));
    }

    #[test]
    fn render_markdown_roundtrips_the_export_shape() {
        let records = vec![
            Record::Header {
                id: StrandId("s1".into()),
                ts: "2026-09-02T10:00:00Z".into(),
                cwd: "/tmp/proj".into(),
                version: 1,
                repo: None,
                parent: None,
            },
            Record::Message {
                id: new_record_id(),
                role: Role::User,
                content: "hi\n there".into(),
                calls: Vec::new(),
                results: Vec::new(),
                thinking: None,
            },
            Record::Message {
                id: new_record_id(),
                role: Role::Assistant,
                content: "**bold**".into(),
                calls: Vec::new(),
                results: Vec::new(),
                thinking: None,
            },
            Record::Digest {
                id: new_record_id(),
                summary: "compacted".into(),
                kept_from: new_record_id(),
            },
            Record::Message {
                id: new_record_id(),
                role: Role::Tool,
                content: "  ".into(),
                calls: Vec::new(),
                results: Vec::new(),
                thinking: None,
            },
        ];
        let md = render_markdown(&records);
        assert!(md.starts_with("# ka session\n\n"));
        assert!(md.contains("> strand `s1` at 2026-09-02T10:00:00Z"));
        assert!(md.contains("### **you**\n\nhi\n there"));
        assert!(md.contains("### **ka**\n\n**bold**"));
        assert!(md.contains("### *digest*\n\n> compacted"));
        assert!(!md.contains("*tool*"), "blank tool content is skipped");
    }

    #[test]
    fn usage_record_serde_roundtrip() {
        let record = Record::Usage {
            id: new_record_id(),
            cost: 0.0125,
            input: 1_234,
            output: 567,
            cache_read: 89,
            cache_write: 12,
        };
        let line = serde_json::to_string(&record).unwrap();
        assert!(line.starts_with("{\"record\":\"usage\""), "{line}");
        let back: Record = serde_json::from_str(&line).unwrap();
        assert_eq!(back, record);
    }

    #[test]
    fn thinking_roundtrips_and_stays_absent_when_none() {
        let record = Record::Message {
            id: new_record_id(),
            role: Role::Assistant,
            content: "answer".into(),
            calls: Vec::new(),
            results: Vec::new(),
            thinking: Some("pondering".into()),
        };
        let line = serde_json::to_string(&record).unwrap();
        assert!(line.contains("pondering"), "{line}");
        let back: Record = serde_json::from_str(&line).unwrap();
        assert_eq!(back, record);

        // None never serializes (the JSONL stays minimal)…
        let bare = serde_json::to_string(&Record::Message {
            id: new_record_id(),
            role: Role::Assistant,
            content: "plain".into(),
            calls: Vec::new(),
            results: Vec::new(),
            thinking: None,
        })
        .unwrap();
        assert!(!bare.contains("thinking"), "{bare}");
        // …and old strands written before the field parse as None
        let old = r#"{"record":"message","id":"r1","role":"assistant","content":"old"}"#;
        let parsed: Record = serde_json::from_str(old).unwrap();
        match parsed {
            Record::Message { thinking, .. } => assert!(thinking.is_none()),
            other => panic!("expected message, got {other:?}"),
        }
    }

    #[test]
    fn list_sums_usage_and_honors_title_override() {
        let data = temp_data("usage-sum");
        let cwd = data.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut strand = StrandFile::create(&cwd, None).unwrap();
        for (cost, input, output) in [(0.01, 100, 50), (0.02, 200, 80)] {
            strand
                .append(Record::Message {
                    id: new_record_id(),
                    role: Role::User,
                    content: "question".into(),
                    calls: Vec::new(),
                    results: Vec::new(),
                    thinking: None,
                })
                .unwrap();
            strand
                .append(Record::Usage {
                    id: new_record_id(),
                    cost,
                    input,
                    output,
                    cache_read: 10,
                    cache_write: 5,
                })
                .unwrap();
        }
        let path = strand.path().unwrap().to_path_buf();
        drop(strand);

        let summaries = list(&cwd).unwrap();
        assert_eq!(summaries.len(), 1);
        let s = &summaries[0];
        assert_eq!(s.messages, 2);
        assert!((s.cost - 0.03).abs() < 1e-9, "cost {}", s.cost);
        assert_eq!(s.tokens, 100 + 50 + 10 + 5 + 200 + 80 + 10 + 5);

        // title override (the fork marker) wins over the message probe
        let mut titled = StrandFile::open(&path).unwrap();
        titled
            .append(Record::Title {
                id: new_record_id(),
                title: "question (fork)".into(),
            })
            .unwrap();
        drop(titled);
        let summaries = list(&cwd).unwrap();
        assert_eq!(summaries[0].title, "question (fork)");
    }

    #[test]
    fn summary_defaults_are_zero_without_usage_records() {
        let data = temp_data("usage-zero");
        let cwd = data.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut strand = StrandFile::create(&cwd, None).unwrap();
        strand
            .append(Record::Message {
                id: new_record_id(),
                role: Role::User,
                content: "plain".into(),
                calls: Vec::new(),
                results: Vec::new(),
                thinking: None,
            })
            .unwrap();
        let s = list(&cwd).unwrap().remove(0);
        assert_eq!((s.cost, s.tokens), (0.0, 0));
    }

    #[test]
    fn html_export_escapes_and_marks_the_island() {
        let page = render_html(
            "t<title>",
            "# Head\n\n<script>alert(1)</script>\n\n- item `code`",
        );
        assert!(!page.contains("__KA_TITLE__"), "title is filled");
        assert!(page.contains("t&lt;title&gt;"), "title is html-escaped");
        assert!(page.contains("&lt;script&gt;"), "markdown is inert");
        assert!(
            !page.contains("<script>alert"),
            "raw script must not pass through"
        );
        assert!(page.contains("text/markdown"), "island present");
    }
}
