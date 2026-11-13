//! Hands: ka's tools. Every Hand declares a clearance tier and annotations;
//! the engine gates execution through them and routes results through the
//! caps/spill hygiene pipe.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

pub mod bash;
pub mod delegate;
pub use bash::BashHand;
pub mod bashp;
pub mod debug;
pub mod edit;
pub mod git;
pub mod glob;
pub mod grep;
pub mod jobs;
pub mod lsp_tools;
pub mod lsp_write;
pub mod memory;
pub mod pathfinder;
pub mod protected;
pub mod read;
pub mod secrets;
pub mod snapshots;
pub mod tasks;
pub mod todo;
pub mod web;
pub mod write;

/// Execution clearance tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Clearance {
    /// Read-only tools: always allowed.
    Read,
    /// Mutating tools: confirmed by rule, mode, or user.
    Write,
    /// Arbitrary execution: the strongest gate.
    Exec,
}

/// What one tool invocation produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    /// Text content returned to the model.
    pub content: String,
    /// Whether this is an error result.
    pub is_error: bool,
    /// Spill pointer if full output was parked on disk.
    pub spill: Option<String>,
    /// Images produced by the tool (read hand on an image file).
    pub images: Vec<ka_protocol::ImagePart>,
}

impl ToolOutput {
    /// A successful output.
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            spill: None,
            images: Vec::new(),
        }
    }

    /// An error output (fed back to the model to self-correct).
    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            spill: None,
            images: Vec::new(),
        }
    }
}

/// A unified diff between the old and new content of `path`, capped at
/// `max_lines` rendered lines (a `… N more lines` trailer notes the
/// remainder) and each rendered line at [`LINE_CAP`] chars. Either side
/// over 2000 lines diffs only its first 2000 lines, noting the
/// truncation. Unchanged content → empty string; sides differing only
/// in trailing newline or line endings say so instead of implying
/// truncation.
pub fn unified_diff(path: &str, old: &str, new: &str, max_lines: usize) -> String {
    /// Sides are truncated to this many lines before diffing (LCS is
    /// quadratic in side length).
    const SIDE_CAP: usize = 2000;
    /// Context lines shown around each change run.
    const CONTEXT: usize = 3;
    /// Long lines render truncated to this many chars — a diff row of a
    /// minified file must not flood an ask card or the model context.
    const LINE_CAP: usize = 240;

    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let a_trunc = a.len() > SIDE_CAP;
    let b_trunc = b.len() > SIDE_CAP;
    let a = &a[..a.len().min(SIDE_CAP)];
    let b = &b[..b.len().min(SIDE_CAP)];
    let (n, m) = (a.len(), b.len());
    if old == new {
        return String::new();
    }

    // LCS table (flat, row-major; (n+1)*(m+1) cells)
    let mut lcs = vec![0u32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[at(i, j)] = if a[i] == b[j] {
                lcs[at(i + 1, j + 1)] + 1
            } else {
                lcs[at(i + 1, j)].max(lcs[at(i, j + 1)])
            };
        }
    }

    // walk to per-line ops: (tag, a-index, b-index)
    let mut ops: Vec<(char, usize, usize)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push((' ', i, j));
            i += 1;
            j += 1;
        } else if lcs[at(i + 1, j)] >= lcs[at(i, j + 1)] {
            ops.push(('-', i, j));
            i += 1;
        } else {
            ops.push(('+', i, j));
            j += 1;
        }
    }
    while i < n {
        ops.push(('-', i, j));
        i += 1;
    }
    while j < m {
        ops.push(('+', i, j));
        j += 1;
    }

    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    // change op indices grouped into hunks whenever their gaps are
    // within 2*CONTEXT+1 ops of each other
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, (tag, _, _))| *tag != ' ')
        .map(|(idx, _)| idx)
        .collect();
    if changed.is_empty() {
        if a_trunc || b_trunc {
            // sides differ only beyond the side cap: no line-level hunks
            out.push_str(&format!(
                "@@ … sides over {SIDE_CAP} lines; diff truncated\n"
            ));
        } else {
            // identical line lists, different bytes: a trailing-newline
            // or line-ending-only change
            out.push_str("@@ no line-level changes (trailing newline or line endings differ)\n");
        }
        return out;
    }
    let mut groups: Vec<(usize, usize)> = Vec::new();
    let mut start = changed[0];
    let mut prev = changed[0];
    for &idx in &changed[1..] {
        if idx - prev > 2 * CONTEXT + 1 {
            groups.push((start, prev));
            start = idx;
        }
        prev = idx;
    }
    groups.push((start, prev));

    let mut lines_out: Vec<String> = Vec::new();
    for (first, last) in groups {
        let lo = first.saturating_sub(CONTEXT);
        let hi = (last + CONTEXT).min(ops.len() - 1);
        let a_len = ops[lo..=hi].iter().filter(|(t, _, _)| *t != '+').count();
        let b_len = ops[lo..=hi].iter().filter(|(t, _, _)| *t != '-').count();
        let a_start = if n == 0 { 0 } else { ops[lo].1 + 1 };
        let b_start = if m == 0 { 0 } else { ops[lo].2 + 1 };
        lines_out.push(format!("@@ -{a_start},{a_len} +{b_start},{b_len} @@"));
        for (tag, ai, bi) in &ops[lo..=hi] {
            let text = match tag {
                '-' => a[*ai],
                '+' => b[*bi],
                _ => a[*ai],
            };
            let rendered: String = if text.chars().count() > LINE_CAP {
                let cut: String = text.chars().take(LINE_CAP).collect();
                let more = text.chars().count() - LINE_CAP;
                format!("{cut} … {more} more chars")
            } else {
                text.to_string()
            };
            lines_out.push(format!("{tag}{rendered}"));
        }
    }
    if a_trunc || b_trunc {
        lines_out.push(format!("… sides over {SIDE_CAP} lines; diff truncated"));
    }
    if lines_out.len() > max_lines {
        let more = lines_out.len() - max_lines;
        lines_out.truncate(max_lines);
        lines_out.push(format!("… {more} more lines"));
    }
    out.push_str(&lines_out.join("\n"));
    out.push('\n');
    out
}

/// Static definition of a hand (model-facing contract).
#[derive(Debug, Clone)]
pub struct HandDef {
    /// Tool name as the model sees it.
    pub name: String,
    /// One-paragraph description for the model.
    pub description: String,
    /// JSON schema for the arguments object.
    pub parameters: Value,
    /// Clearance tier.
    pub clearance: Clearance,
    /// Whether this hand only reads (never mutates state).
    pub read_only: bool,
}

/// A tool. Async via boxed futures, dyn-safe like Speaker.
pub trait Hand: Send + Sync {
    /// The definition (built per hand; registries are constructed once).
    fn def(&self) -> HandDef;

    /// Clearance tier for THIS call. Defaults to the hand's static tier;
    /// override for argument-dependent tiers (jobs: kill = exec).
    fn clearance_for(&self, _args: &Value) -> Clearance {
        self.def().clearance
    }

    /// Execute with parsed arguments.
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        ctx: &'a HandContext,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>>;
}

/// Per-invocation context shared with hands. The ledger and spill store are
/// shared by reference so every hand sees the same read-tracking state.
#[derive(Clone)]
pub struct HandContext {
    /// Working directory for relative paths.
    pub cwd: PathBuf,
    /// Shared read ledger.
    pub ledger: std::sync::Arc<parking_lot::Mutex<Ledger>>,
    /// Shared spill store.
    pub spill: std::sync::Arc<Spill>,
    /// Shared pre-mutation snapshot journal (inert for readonly voices).
    pub snapshots: std::sync::Arc<parking_lot::Mutex<snapshots::Snapshots>>,
    /// Shared table of auto-backgrounded bash jobs (bash hand registers,
    /// the jobs hand lists/kills, the engine kills the rest at shutdown).
    pub jobs: std::sync::Arc<jobs::JobTable>,
    /// Bash auto-background threshold in ms (0 = never background).
    pub bash_background_ms: u64,
    /// Read-hand image size cap in MB (0 = unlimited).
    pub max_image_mb: u32,
    /// Web fetches may target private hosts ([tools.web]).
    pub web_allow_private: bool,
    /// Sandbox policy for bash children ([sandbox]).
    pub sandbox: ka_sandbox::Policy,
}

/// The read ledger: files the model has read, with their stamps. Edits
/// refuse files that are absent (read first) or changed since read.
#[derive(Debug, Default)]
pub struct Ledger {
    stamps: HashMap<PathBuf, FileStamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    mtime: SystemTime,
    size: u64,
    /// Wall-clock instant the read was recorded — the recency signal
    /// for [`Ledger::hot_paths`] (file mtime says when the FILE
    /// changed, not when it was read).
    read_at: std::time::Instant,
}

impl Ledger {
    /// Record a fresh read of `path`.
    pub fn mint(&mut self, path: &Path, meta: &std::fs::Metadata) {
        self.stamps.insert(
            path.to_path_buf(),
            FileStamp {
                mtime: meta.modified().unwrap_or(UNIX_EPOCH),
                size: meta.len(),
                read_at: std::time::Instant::now(),
            },
        );
    }

    /// `Ok(())` when `path` was read and is unchanged since.
    pub fn verify(&self, path: &Path) -> Result<(), String> {
        let display = path.display();
        let Some(stamp) = self.lookup(path) else {
            return Err(format!(
                "{display} has not been read yet; read it before editing"
            ));
        };
        match std::fs::metadata(path) {
            Ok(meta) => {
                let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
                if mtime != stamp.mtime || meta.len() != stamp.size {
                    Err(format!(
                        "{display} changed since it was read; re-read it first"
                    ))
                } else {
                    Ok(())
                }
            }
            Err(e) => Err(format!("{display}: {e}")),
        }
    }

    /// Whether `path` is currently tracked (read this session).
    pub fn is_tracked(&self, path: &Path) -> bool {
        self.lookup(path).is_some()
    }

    /// Stamp lookup: direct hit first, then the canonical form. The
    /// read hands key stamps by the model's path (`~/x`, cwd-joined,
    /// symlinks intact), while server-decoded URI paths may take the
    /// symlink-resolved form — the drift check must not silently miss
    /// because the two spellings differ.
    fn lookup(&self, path: &Path) -> Option<FileStamp> {
        if let Some(stamp) = self.stamps.get(path) {
            return Some(*stamp);
        }
        let canon = std::fs::canonicalize(path).ok()?;
        self.stamps.get(&canon).copied()
    }

    /// Drop all stamps (after arbitrary shell execution, any file may have
    /// changed — conservative and cheap).
    pub fn invalidate_all(&mut self) {
        self.stamps.clear();
    }

    /// Number of tracked files.
    pub fn len(&self) -> usize {
        self.stamps.len()
    }

    /// Whether nothing is tracked.
    pub fn is_empty(&self) -> bool {
        self.stamps.is_empty()
    }

    /// Paths currently tracked (read this session) — scoped rules
    /// activate when their globs match one of these.
    pub fn tracked_paths(&self) -> Vec<PathBuf> {
        self.stamps.keys().cloned().collect()
    }

    /// The `n` most recently read/edited paths (ledger-hot), newest
    /// first — the post-digest re-read set.
    pub fn hot_paths(&self, n: usize) -> Vec<PathBuf> {
        let mut stamps: Vec<(PathBuf, std::time::Instant)> = self
            .stamps
            .iter()
            .map(|(p, s)| (p.clone(), s.read_at))
            .collect();
        stamps.sort_by_key(|(_, read_at)| std::cmp::Reverse(*read_at));
        stamps.into_iter().take(n).map(|(p, _)| p).collect()
    }
}

/// Spill store: oversized tool outputs parked on disk, referenced as
/// `spill://<id>` from the capped excerpt returned to the model.
pub struct Spill {
    dir: PathBuf,
}

impl Default for Spill {
    fn default() -> Self {
        Self::new()
    }
}

impl Spill {
    /// Spill directory under the state root (created lazily).
    pub fn new() -> Self {
        Self { dir: spills_root() }
    }

    /// Create an empty spill slot for streamed (backgrounded) output;
    /// returns the file path and its `spill://<id>` pointer. The file is
    /// created eagerly so tail reads never race creation.
    pub fn slot(&self) -> std::io::Result<(PathBuf, String)> {
        std::fs::create_dir_all(&self.dir)?;
        let n = SPILL_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = format!(
            "job-{}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
            n
        );
        let path = self.dir.join(&id);
        std::fs::write(&path, b"")?;
        Ok((path, format!("spill://{id}")))
    }

    /// Park `content`, returning its `spill://<id>` pointer.
    pub fn park(&self, content: &str) -> std::io::Result<String> {
        std::fs::create_dir_all(&self.dir)?;
        // process-wide counter: same-ms equal-length parks must not
        // collide (millis+length alone overwrite each other)
        let n = SPILL_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = format!(
            "{}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
            n
        );
        std::fs::write(self.dir.join(&id), content)?;
        Ok(format!("spill://{id}"))
    }
}

/// Process-wide spill id counter (shared by [`Spill::slot`] and
/// [`Spill::park`] so ids never collide).
static SPILL_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The spill directory under the state root.
pub(crate) fn spills_root() -> PathBuf {
    std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .map(|base| base.join("ka/spills"))
        .unwrap_or_else(|_| std::env::temp_dir().join("ka-spills"))
}

/// Spill retention window: files older than this are GC'd at startup.
pub(crate) const SPILL_RETENTION: std::time::Duration =
    std::time::Duration::from_secs(7 * 24 * 60 * 60);

/// Opportunistic spill-directory GC: delete every regular file (spill
/// outputs and `.done` markers live here) whose mtime is older than
/// `older_than` before `now`. Best-effort — errors are ignored, no
/// logging, never fatal. Snapshot blobs live elsewhere and are not
/// touched.
pub(crate) fn gc_spills_with_now(root: &Path, now: SystemTime, older_than: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        if now.duration_since(mtime).is_ok_and(|age| age > older_than) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// [`gc_spills_with_now`] with the wall clock and the default
/// 7-day retention.
pub(crate) fn gc_spills(root: &Path) {
    gc_spills_with_now(root, SystemTime::now(), SPILL_RETENTION);
}

/// The full registry wired for Phase 2: an internally-owned todo slot.
pub fn registry() -> Vec<std::sync::Arc<dyn Hand>> {
    registry_with_pathfinder(
        std::sync::Arc::new(parking_lot::RwLock::new(
            pathfinder::PathfinderSource::default(),
        )),
        todo::slot(),
        std::sync::Arc::new(jobs::JobTable::new()),
    )
}

/// Registry with externally-owned pathfinder bootstrap slot (engine) and
/// todo slot (voice — it forwards the list as `Event::Todos`); `jobs` is
/// the shared auto-backgrounded-job table the bash hand promotes into and
/// the jobs hand serves.
pub fn registry_with_pathfinder(
    slot: std::sync::Arc<parking_lot::RwLock<pathfinder::PathfinderSource>>,
    todos: todo::TodoSlot,
    jobs: std::sync::Arc<jobs::JobTable>,
) -> Vec<std::sync::Arc<dyn Hand>> {
    vec![
        std::sync::Arc::new(read::ReadHand),
        std::sync::Arc::new(edit::EditHand),
        std::sync::Arc::new(write::WriteHand),
        std::sync::Arc::new(bash::BashHand),
        std::sync::Arc::new(glob::GlobHand),
        std::sync::Arc::new(grep::GrepHand),
        std::sync::Arc::new(pathfinder::PathfinderHand::from_slot(slot)),
        std::sync::Arc::new(todo::TodoHand::new(todos)),
        std::sync::Arc::new(jobs::JobsHand::new(jobs)),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn newline_only_change_reports_itself_not_truncation() {
        let d = unified_diff("f.rs", "a\nb\n", "a\nb", 24);
        assert!(
            d.contains("no line-level changes"),
            "newline-only change mislabelled: {d}"
        );
        assert!(!d.contains("truncated"), "{d}");
    }

    #[test]
    fn crlf_lf_change_reports_itself_not_truncation() {
        let d = unified_diff("f.rs", "a\r\nb\r\n", "a\nb\n", 24);
        assert!(
            d.contains("no line-level changes"),
            "line-ending-only change mislabelled: {d}"
        );
    }

    #[test]
    fn long_diff_lines_render_capped() {
        let long = "x".repeat(2000);
        let d = unified_diff("f.rs", "", &format!("{long}\n"), 24);
        assert!(d.contains("more chars"), "line cap not applied: {d}");
        assert!(d.chars().count() < 600, "capped line still huge");
    }

    #[test]
    fn unified_diff_empty_when_unchanged() {
        assert_eq!(unified_diff("f.rs", "a\nb\n", "a\nb\n", 24), "");
        assert_eq!(unified_diff("f.rs", "", "", 24), "");
    }

    #[test]
    fn unified_diff_renders_a_single_change_with_context() {
        let d = unified_diff("f.rs", "one\ntwo\nthree", "one\ntwo\nthree!", 24);
        assert_eq!(
            d,
            "--- a/f.rs\n+++ b/f.rs\n@@ -1,3 +1,3 @@\n one\n two\n-three\n+three!\n"
        );
    }

    #[test]
    fn unified_diff_new_file_and_removal_headers() {
        // empty old side: -0,0 start like git
        let d = unified_diff("n.rs", "", "hello\n", 24);
        assert!(
            d.starts_with("--- a/n.rs\n+++ b/n.rs\n@@ -0,0 +1,1 @@\n+hello\n"),
            "{d}"
        );
        // empty new side: deletion hunk
        let d = unified_diff("n.rs", "hello\n", "", 24);
        assert!(
            d.starts_with("--- a/n.rs\n+++ b/n.rs\n@@ -1,1 +0,0 @@\n-hello\n"),
            "{d}"
        );
    }

    #[test]
    fn unified_diff_hunks_merge_only_near_changes() {
        // two changes 10 lines apart → two hunks
        let old: String = (0..20).map(|i| format!("line{i}\n")).collect();
        let mut new_lines: Vec<String> = (0..20).map(|i| format!("line{i}")).collect();
        new_lines[0].push('!');
        new_lines[19].push('!');
        let new: String = new_lines.iter().map(|l| l.clone() + "\n").collect();
        let d = unified_diff("f.txt", &old, &new, 64);
        assert_eq!(d.lines().filter(|l| l.starts_with("@@")).count(), 2, "{d}");
        // a second change 4 lines from the first merges into one hunk
        let mut near: Vec<String> = (0..20).map(|i| format!("line{i}")).collect();
        near[0].push('!');
        near[4].push('!');
        let new: String = near.into_iter().map(|l| l + "\n").collect();
        let d = unified_diff("f.txt", &old, &new, 64);
        assert_eq!(d.lines().filter(|l| l.starts_with("@@")).count(), 1, "{d}");
    }

    #[test]
    fn unified_diff_caps_rendered_lines_and_notes_side_truncation() {
        let old: String = (0..50).map(|i| format!("line{i}\n")).collect();
        let new: String = (0..50).map(|i| format!("ln{i}\n")).collect();
        let d = unified_diff("f.txt", &old, &new, 10);
        // the two file-header lines stay uncapped; hunk lines fit the budget
        let body = d.lines().skip(2).count();
        assert!(body <= 11, "{body} lines: {d}");
        assert!(d.contains("more lines"), "{d}");
        // differing sides over the 2000-line cap truncate with a note,
        // never hang; identical sides stay empty regardless of size
        let big: String = std::iter::repeat_n("x\n", 3000).collect();
        assert_eq!(unified_diff("f.txt", &big, &big, 24), "");
        let big_new = format!("{big}y\n");
        let d = unified_diff("f.txt", &big, &big_new, 24);
        assert!(d.contains("diff truncated"), "{d}");
    }

    #[test]
    fn ledger_roundtrip_and_staleness() {
        let dir = std::env::temp_dir().join(format!("ka-ledger-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "one").unwrap();

        let mut ledger = Ledger::default();
        assert!(ledger.verify(&file).is_err(), "untracked file must fail");

        let meta = std::fs::metadata(&file).unwrap();
        ledger.mint(&file, &meta);
        assert!(ledger.verify(&file).is_ok());

        std::fs::write(&file, "two — longer").unwrap();
        assert!(ledger.verify(&file).is_err(), "size change must be caught");

        let meta = std::fs::metadata(&file).unwrap();
        ledger.mint(&file, &meta);
        // same length, newer mtime
        std::fs::write(&file, "TWO — longer!").unwrap();
        assert!(ledger.verify(&file).is_err(), "mtime change must be caught");

        ledger.invalidate_all();
        assert!(ledger.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registries_carry_todo_and_grow_by_one() {
        let base = [
            "read",
            "edit",
            "write",
            "bash",
            "glob",
            "grep",
            "pathfinder",
        ];
        let with = registry_with_pathfinder(
            std::sync::Arc::new(parking_lot::RwLock::new(
                pathfinder::PathfinderSource::default(),
            )),
            todo::slot(),
            std::sync::Arc::new(jobs::JobTable::new()),
        );
        let names: Vec<String> = with.iter().map(|h| h.def().name).collect();
        assert_eq!(names.len(), base.len() + 2, "todo + jobs grow the registry");
        assert!(names.iter().any(|n| n == "todo"), "names: {names:?}");
        assert_eq!(registry().len(), with.len(), "both registries match");
        let todo = with.iter().find(|h| h.def().name == "todo").unwrap();
        let def = todo.def();
        assert_eq!(def.clearance, Clearance::Read, "todo must auto-allow");
        assert!(def.read_only);
    }

    #[test]
    fn spill_parks_and_points() {
        let spill = Spill::new();
        let ptr = spill.park("huge output").unwrap();
        assert!(ptr.starts_with("spill://"), "{ptr}");
        let id = ptr.trim_start_matches("spill://");
        let path = spill.dir.join(id);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "huge output");
        let _ = std::fs::remove_file(&path);
        // same-ms equal-length parks must not collide: both pointers
        // read back their OWN content
        let a = spill.park("first one!").unwrap();
        let b = spill.park("second one").unwrap();
        assert_ne!(a, b, "park ids collided: {a}");
        let pa = spill.dir.join(a.trim_start_matches("spill://"));
        let pb = spill.dir.join(b.trim_start_matches("spill://"));
        assert_eq!(std::fs::read_to_string(&pa).unwrap(), "first one!");
        assert_eq!(std::fs::read_to_string(&pb).unwrap(), "second one");
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);
    }

    #[test]
    fn hot_paths_order_follows_read_time() {
        let dir = std::env::temp_dir().join(format!("ka-hotpaths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // B's file is created FIRST (older mtime), then A's
        let fb = dir.join("older-mtime.txt");
        std::fs::write(&fb, "b").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let fa = dir.join("newer-mtime.txt");
        std::fs::write(&fa, "a").unwrap();

        let mut ledger = Ledger::default();
        // read A first, then B — B is the more recent READ despite the
        // older file mtime, so B must rank first
        ledger.mint(&fa, &std::fs::metadata(&fa).unwrap());
        std::thread::sleep(std::time::Duration::from_millis(20));
        ledger.mint(&fb, &std::fs::metadata(&fb).unwrap());
        let hot = ledger.hot_paths(2);
        assert_eq!(hot[0], fb, "most recently READ ranks first: {hot:?}");
        assert_eq!(hot[1], fa, "{hot:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gc_spills_removes_only_stale() {
        let dir = std::env::temp_dir().join(format!("ka-gcspills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // real mtimes cannot be fabricated without deps and two files
        // written milliseconds apart age identically, so test the two
        // ends of the age comparison instead of a same-run split:
        // a synthetic clock far past creation removes everything…
        let old = dir.join("old-spill");
        std::fs::write(&old, "x").unwrap();
        std::fs::write(dir.join("old-spill.done"), "0").unwrap();
        let mtime = std::fs::metadata(&old).unwrap().modified().unwrap();
        let now = mtime + std::time::Duration::from_secs(8 * 24 * 60 * 60);
        gc_spills_with_now(&dir, now, SPILL_RETENTION);
        assert!(!old.exists(), "8-day-old spill must be removed");
        assert!(!dir.join("old-spill.done").exists(), "stale .done removed");
        // …and a clock at creation time removes nothing
        std::fs::write(&old, "x").unwrap();
        std::fs::write(dir.join("old-spill.done"), "0").unwrap();
        gc_spills_with_now(&dir, mtime, SPILL_RETENTION);
        assert!(old.exists(), "just-written spill must survive");
        assert!(dir.join("old-spill.done").exists(), "fresh .done survives");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
