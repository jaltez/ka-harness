//! File snapshots: the safety net under `edit`/`write`. Before a hand
//! mutates a file, its current bytes are parked under the data dir and
//! journaled per strand; `/undo` (or `ka undo`) restores the most recent
//! one. Manifest-first: the journal is the source of truth, blobs are
//! content copies named by sequence number.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One journal entry: what was about to be overwritten, and where the
/// original bytes went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapEntry {
    /// Monotonic sequence (blob file name).
    pub seq: u64,
    /// Strand that performed the mutation (undo is session-scoped).
    pub strand: String,
    /// Absolute path of the mutated file.
    pub path: PathBuf,
    /// Whether the file existed before the mutation (false = creation;
    /// undo deletes).
    pub existed: bool,
    /// RFC 3339-ish timestamp.
    pub ts: String,
    /// Stored blob file name: the pid suffix makes blob names unique
    /// across sessions sharing the store, and undo must read the
    /// snapshotting session's blob, not one named after its own pid.
    /// Empty on pre-existing journals (legacy `{seq}.blob` spelling).
    #[serde(default)]
    pub blob: String,
}

/// The snapshot store. Shared by the engine (strand tracking, undo) and
/// the hands (pre-mutation snapshot) through `Arc<Mutex<_>>`.
#[derive(Debug)]
pub struct Snapshots {
    root: Option<PathBuf>,
    strand: String,
    seq: u64,
    manifest: Vec<SnapEntry>,
}

impl Snapshots {
    /// Open (or create) the store for a working directory. Inert on any
    /// filesystem failure — snapshotting must never block a turn from
    /// starting, only a mutation can be refused later.
    pub fn open(cwd: &Path) -> Self {
        let dir = encode(cwd);
        let root = ka_strand::data_dir().join("snapshots").join(dir);
        let manifest: Vec<SnapEntry> = std::fs::read_to_string(root.join("manifest.jsonl"))
            .ok()
            .map(|text| {
                text.lines()
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let seq = manifest.iter().map(|e| e.seq).max().unwrap_or(0);
        Self {
            root: Some(root),
            strand: String::new(),
            seq,
            manifest,
        }
    }

    /// A no-op store (tests, readonly voices): snapshots and undo are
    /// silently skipped.
    pub fn inert() -> Self {
        Self {
            root: None,
            strand: String::new(),
            seq: 0,
            manifest: Vec::new(),
        }
    }

    /// Whether snapshots actually persist.
    pub fn live(&self) -> bool {
        self.root.is_some()
    }

    /// Point the journal at the active strand (engine calls on attach).
    pub fn set_strand(&mut self, strand: impl Into<String>) {
        self.strand = strand.into();
    }

    /// The active strand id.
    pub fn strand(&self) -> &str {
        &self.strand
    }

    /// Park the current bytes of `path` before a mutation. Returns the
    /// entry, or None when inert. Errors refuse the caller's mutation.
    pub fn snapshot(&mut self, path: &Path) -> io::Result<Option<SnapEntry>> {
        let Some(root) = self.root.clone() else {
            return Ok(None);
        };
        let root = root.as_path();
        if self.strand.is_empty() {
            return Err(io::Error::other("snapshot journal has no active strand"));
        }
        self.seq += 1;
        let existed = path.is_file();
        // pid-suffixed blob names: a second session whose seq starts
        // from the same manifest max must not overwrite this session's
        // blobs (the journal stays the truth); the name rides the
        // journal entry so a later session's undo finds THIS blob
        let blob_name = format!("{}-{}.blob", self.seq, std::process::id());
        let entry = SnapEntry {
            seq: self.seq,
            strand: self.strand.clone(),
            path: path.to_path_buf(),
            existed,
            ts: now_stamp(),
            blob: blob_name.clone(),
        };
        if existed {
            let blob = root.join(&blob_name);
            std::fs::create_dir_all(root)?;
            std::fs::copy(path, &blob)?;
        }
        self.manifest.push(entry.clone());
        self.append_journal(root, &entry)?;
        Ok(Some(entry))
    }

    /// Journal the first appearance of a file — no prior bytes to park;
    /// undo removes it. Used by the file-move path: after a rename, the
    /// new path must vanish again on /undo, or the file ends up
    /// duplicated (old path restored, moved copy stranded).
    pub fn record_creation(&mut self, path: &Path) -> io::Result<Option<SnapEntry>> {
        let Some(root) = self.root.clone() else {
            return Ok(None);
        };
        if self.strand.is_empty() {
            return Err(io::Error::other("snapshot journal has no active strand"));
        }
        self.seq += 1;
        let entry = SnapEntry {
            seq: self.seq,
            strand: self.strand.clone(),
            path: path.to_path_buf(),
            existed: false,
            ts: now_stamp(),
            blob: String::new(),
        };
        self.manifest.push(entry.clone());
        self.append_journal(root.as_path(), &entry)?;
        Ok(Some(entry))
    }

    /// Restore the latest snapshot of the active strand (pops it).
    pub fn undo(&mut self) -> io::Result<Option<SnapEntry>> {
        let Some(root) = self.root.clone() else {
            return Ok(None);
        };
        let root = root.as_path();
        let Some(idx) = self.manifest.iter().rposition(|e| e.strand == self.strand) else {
            return Ok(None);
        };
        let entry = self.manifest.remove(idx);
        if entry.existed {
            // the blob name recorded at snapshot time — an undo from a
            // later session must read the snapshotting session's blob,
            // not one named after its own pid; legacy journals (no
            // recorded name) keep the pre-pid `{seq}.blob` spelling
            let blob = if entry.blob.is_empty() {
                root.join(format!("{}.blob", entry.seq))
            } else {
                root.join(&entry.blob)
            };
            let bytes = std::fs::read(&blob)?;
            if let Some(dir) = entry.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&entry.path, bytes)?;
        } else if entry.path.exists() {
            std::fs::remove_file(&entry.path)?;
        }
        self.rewrite_journal(root, &entry)?;
        Ok(Some(entry))
    }

    /// Journal entries, oldest first.
    pub fn entries(&self) -> &[SnapEntry] {
        &self.manifest
    }

    fn append_journal(&mut self, root: &Path, entry: &SnapEntry) -> io::Result<()> {
        use std::io::Write;
        std::fs::create_dir_all(root)?;
        let mut line = serde_json::to_string(entry)
            .map_err(|e| io::Error::other(format!("entry serialize: {e}")))?;
        line.push('\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("manifest.jsonl"))?;
        file.write_all(line.as_bytes())
    }

    /// Rewrite the journal from the ON-DISK manifest minus the undone
    /// entry: sibling sessions may have appended lines this process's
    /// in-memory copy never saw, and they must survive an undo.
    fn rewrite_journal(&self, root: &Path, undone: &SnapEntry) -> io::Result<()> {
        let disk: Vec<SnapEntry> = std::fs::read_to_string(root.join("manifest.jsonl"))
            .ok()
            .map(|text| {
                text.lines()
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut text = String::new();
        for entry in disk {
            if &entry == undone {
                continue;
            }
            if let Ok(line) = serde_json::to_string(&entry) {
                text.push_str(&line);
                text.push('\n');
            }
        }
        std::fs::write(root.join("manifest.jsonl"), text)
    }
}

fn encode(cwd: &Path) -> String {
    format!("-{}", cwd.to_string_lossy().replace('/', "-"))
}

fn now_stamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn store(tag: &str) -> (Snapshots, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ka-snap-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        ka_strand::set_data_dir_for_tests(dir.clone());
        let cwd = dir.join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        (Snapshots::open(&cwd), dir)
    }

    #[test]
    fn snapshot_then_undo_restores_bytes() {
        let (mut snaps, _dir) = store("roundtrip");
        snaps.set_strand("s1");
        let f = std::env::temp_dir().join(format!("ka-snap-file-{}", std::process::id()));
        std::fs::write(&f, "original bytes").unwrap();

        snaps.snapshot(&f).unwrap().unwrap();
        std::fs::write(&f, "mutated").unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "mutated");

        let undone = snaps.undo().unwrap().unwrap();
        assert_eq!(undone.path, f);
        assert!(undone.existed);
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "original bytes",
            "undo restores the pre-mutation bytes"
        );
        assert!(snaps.undo().unwrap().is_none(), "journal empty after undo");
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn undo_of_creation_deletes_the_file() {
        let (mut snaps, _dir) = store("create");
        snaps.set_strand("s1");
        let f = std::env::temp_dir().join(format!("ka-snap-new-{}", std::process::id()));
        let _ = std::fs::remove_file(&f);
        snaps.snapshot(&f).unwrap().unwrap();
        std::fs::write(&f, "fresh").unwrap();
        let undone = snaps.undo().unwrap().unwrap();
        assert!(!undone.existed);
        assert!(!f.exists(), "undo of a creation removes the file");
    }

    #[test]
    fn undo_is_strand_scoped() {
        let (mut snaps, _dir) = store("scoped");
        snaps.set_strand("sA");
        let f = std::env::temp_dir().join(format!("ka-snap-scope-{}", std::process::id()));
        std::fs::write(&f, "A0").unwrap();
        snaps.snapshot(&f).unwrap();
        // switch strands mid-journal
        snaps.set_strand("sB");
        std::fs::write(&f, "B0").unwrap();
        snaps.snapshot(&f).unwrap();
        std::fs::write(&f, "B1").unwrap();

        let undone = snaps.undo().unwrap().unwrap();
        assert_eq!(undone.strand, "sB");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "B0");
        // B journal exhausted; A's snapshots are untouched
        assert!(snaps.undo().unwrap().is_none());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "B0");
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn journal_survives_reopen() {
        let (mut snaps, dir) = store("reopen");
        snaps.set_strand("s1");
        let f = dir.join("persisted.txt");
        std::fs::write(&f, "v1").unwrap();
        snaps.snapshot(&f).unwrap();
        drop(snaps);

        let mut reopened = Snapshots::open(&dir.join("repo"));
        reopened.set_strand("s1");
        std::fs::write(&f, "v2").unwrap();
        reopened.undo().unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");
    }

    #[test]
    fn inert_store_is_a_no_op() {
        let mut snaps = Snapshots::inert();
        snaps.set_strand("s1");
        assert!(!snaps.live());
        assert!(snaps.snapshot(Path::new("/any")).unwrap().is_none());
        assert!(snaps.undo().unwrap().is_none());
    }

    #[test]
    fn snapshot_without_strand_refuses() {
        let (mut snaps, _dir) = store("nostrand");
        let f = std::env::temp_dir().join(format!("ka-snap-ns-{}", std::process::id()));
        std::fs::write(&f, "x").unwrap();
        assert!(snaps.snapshot(&f).is_err(), "no strand = refuse mutation");
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn undo_keeps_sibling_session_entries() {
        let (mut snaps, dir) = store("sibling");
        snaps.set_strand("mine");
        let f = dir.join("mine.txt");
        std::fs::write(&f, "v1").unwrap();
        snaps.snapshot(&f).unwrap();
        std::fs::write(&f, "v2").unwrap();

        // a sibling session (same repo root) appends its own journal
        // line and blob behind our back
        let repo_root = dir.join("repo");
        let snap_root = ka_strand::data_dir()
            .join("snapshots")
            .join(encode(&repo_root));
        let sibling = SnapEntry {
            seq: 1,
            strand: "theirs".into(),
            path: dir.join("theirs.txt"),
            existed: true,
            ts: "1".into(),
            blob: "1-999999.blob".into(),
        };
        {
            use std::io::Write;
            let mut m = std::fs::OpenOptions::new()
                .append(true)
                .open(snap_root.join("manifest.jsonl"))
                .unwrap();
            writeln!(m, "{}", serde_json::to_string(&sibling).unwrap()).unwrap();
        }
        std::fs::write(snap_root.join("1-999999.blob"), "sibling bytes").unwrap();

        let undone = snaps.undo().unwrap().unwrap();
        assert_eq!(undone.strand, "mine");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");

        // the sibling's journal line survives, and its blob is untouched
        let manifest = std::fs::read_to_string(snap_root.join("manifest.jsonl")).unwrap();
        assert!(
            manifest.lines().any(|l| l.contains("\"theirs\"")),
            "sibling entry must survive undo: {manifest}"
        );
        assert_eq!(
            std::fs::read_to_string(snap_root.join("1-999999.blob")).unwrap(),
            "sibling bytes"
        );
    }
}
