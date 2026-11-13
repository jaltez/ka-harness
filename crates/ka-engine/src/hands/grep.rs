//! The grep hand: Rust-regex content search over gitignore-aware walks,
//! with instructive errors for unsupported constructs.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

use super::{Hand, HandContext, HandDef, ToolOutput};

/// Maximum matching lines returned per file.
pub const PER_FILE_CAP: usize = 20;
/// Maximum matching lines returned in total.
pub const TOTAL_CAP: usize = 200;
/// Per-line character cap.
pub const LINE_CAP: usize = 512;
/// Maximum files scanned.
pub const FILE_CAP: usize = 2_000;
/// Files larger than this are skipped with a note (whole-file reads).
pub const MAX_SCAN_BYTES: u64 = 8 * 1024 * 1024;
/// Below this many candidate files the walk scans sequentially — thread
/// handoff costs more than it saves.
const PARALLEL_THRESHOLD: usize = 16;
/// Upper bound on scan worker threads.
const MAX_SCAN_WORKERS: usize = 8;

/// The grep tool.
pub struct GrepHand;

impl Hand for GrepHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "grep".to_string(),
            description: "Search file contents with a regular expression (Rust regex syntax: \
                no lookaround/backreferences). Returns `path:line:text`, capped. Supports an \
                optional glob filter on file names."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex to search for" },
                    "path": { "type": "string", "description": "Root directory or file (default: cwd)" },
                    "glob": { "type": "string", "description": "Only search files matching this glob (e.g. *.rs)" }
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
                return ToolOutput::err("grep: missing required 'pattern'");
            };
            let re = match regex::Regex::new(pattern) {
                Ok(r) => r,
                Err(e) => {
                    return ToolOutput::err(format!(
                        "grep: invalid regex: {e}\nhint: lookarounds ((?=..)) and backreferences (\\1) are not supported; restructure the pattern"
                    ));
                }
            };
            let root = args
                .get("path")
                .and_then(Value::as_str)
                .map(|p| super::read::resolve(ctx, p))
                .unwrap_or_else(|| ctx.cwd.clone());
            let glob_filter = args
                .get("glob")
                .and_then(Value::as_str)
                .and_then(name_regex);

            // The walk + per-file reads are blocking filesystem work: run
            // them on the blocking pool so the async runtime (ka runs a
            // single-threaded executor) keeps pumping events — TUI redraw,
            // bash previews, interrupts — while a repo-wide search works.
            let joined =
                tokio::task::spawn_blocking(move || search(&root, &re, glob_filter.as_ref()))
                    .await
                    .map_err(|e| format!("grep: scan task failed: {e}"));
            match joined {
                Ok(out) if out.is_empty() => ToolOutput::ok("no matches"),
                Ok(out) => ToolOutput::ok(out),
                Err(msg) => ToolOutput::err(msg),
            }
        })
    }
}

/// Blocking worker: one full search, returning the rendered output
/// (empty = no matches). Output order is the walk order — deterministic
/// whether the scan ran sequentially or across threads.
fn search(root: &std::path::Path, re: &regex::Regex, glob_filter: Option<&regex::Regex>) -> String {
    if root.is_file() {
        let hits = scan_one(root, root, re);
        return assemble(root, &[(root.to_path_buf(), hits)], false);
    }

    let (files, truncated) = collect_candidates(root, glob_filter);
    let results = if files.len() < PARALLEL_THRESHOLD {
        scan_sequential(&files, root, re)
    } else {
        scan_parallel(&files, root, re)
    };
    assemble(root, &results, truncated)
}

/// Walk the root collecting candidate files (metadata + gitignore
/// checks only — no contents). Stopping at FILE_CAP with entries left
/// over means the results were capped.
fn collect_candidates(
    root: &std::path::Path,
    glob_filter: Option<&regex::Regex>,
) -> (Vec<PathBuf>, bool) {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut truncated = false;
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .build();
    for entry in walker.flatten() {
        if files.len() >= FILE_CAP {
            truncated = true;
            break;
        }
        let path = entry.path();
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if let Some(filter) = glob_filter {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !filter.is_match(&name) {
                continue;
            }
        }
        files.push(path.to_path_buf());
    }
    (files, truncated)
}

/// In-order scan of every candidate (small searches: thread handoff
/// costs more than it saves).
fn scan_sequential(
    files: &[PathBuf],
    root: &std::path::Path,
    re: &regex::Regex,
) -> Vec<(PathBuf, FileHits)> {
    files
        .iter()
        .map(|path| (path.clone(), scan_one(path, root, re)))
        .collect()
}

/// Scoped worker threads sharing an index cursor over the candidate
/// list. Results land back in walk order, so assembly is byte-identical
/// to the sequential scan.
fn scan_parallel(
    files: &[PathBuf],
    root: &std::path::Path,
    re: &regex::Regex,
) -> Vec<(PathBuf, FileHits)> {
    let mut results: Vec<(PathBuf, FileHits)> = Vec::with_capacity(files.len());
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(MAX_SCAN_WORKERS)
        .min(files.len())
        .max(1);
    let cursor = AtomicUsize::new(0);
    let (tx, rx) = std::sync::mpsc::channel::<(usize, FileHits)>();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let tx = tx.clone();
            let cursor = &cursor;
            scope.spawn(move || {
                loop {
                    let i = cursor.fetch_add(1, Ordering::Relaxed);
                    if i >= files.len() {
                        break;
                    }
                    let hits = scan_one(&files[i], root, re);
                    if tx.send((i, hits)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        let mut slots: Vec<Option<FileHits>> = files.iter().map(|_| None).collect();
        for (i, hits) in rx {
            slots[i] = Some(hits);
        }
        for (path, slot) in files.iter().zip(slots) {
            results.push((path.clone(), slot.unwrap_or_else(FileHits::none)));
        }
    });
    results
}

/// Matching lines of one file, pre-rendered as `rel:line:text` rows
/// (LINE_CAP applied). `capped` records that PER_FILE_CAP hits stopped
/// the scan — the marker row does not consume the total budget.
struct FileHits {
    lines: Vec<String>,
    capped: bool,
}

impl FileHits {
    fn none() -> Self {
        Self {
            lines: Vec::new(),
            capped: false,
        }
    }
}

/// Scan one file (binary, unreadable, or over MAX_SCAN_BYTES → note or
/// no hits).
fn scan_one(path: &std::path::Path, root: &std::path::Path, re: &regex::Regex) -> FileHits {
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > MAX_SCAN_BYTES {
            let rel = path.strip_prefix(root).unwrap_or(path);
            return FileHits {
                lines: vec![format!(
                    "{}:[...file is {} bytes, over the {}-byte scan cap; skipped]\n",
                    rel.display(),
                    meta.len(),
                    MAX_SCAN_BYTES
                )],
                capped: false,
            };
        }
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return FileHits::none(); // binary or unreadable: skip silently
    };
    let rel = path.strip_prefix(root).unwrap_or(path);
    let mut hits = FileHits {
        lines: Vec::new(),
        capped: false,
    };
    for (i, line) in text.lines().enumerate() {
        if re.is_match(line) {
            let capped_line: &str = match line.char_indices().nth(LINE_CAP) {
                Some((idx, _)) => &line[..idx],
                None => line,
            };
            hits.lines
                .push(format!("{}:{}:{}\n", rel.display(), i + 1, capped_line));
            if hits.lines.len() >= PER_FILE_CAP {
                hits.capped = true;
                break;
            }
        }
    }
    hits
}

/// Render scanned files in walk order under the shared TOTAL_CAP budget
/// — the single assembly point for both scan paths.
fn assemble(root: &std::path::Path, results: &[(PathBuf, FileHits)], truncated: bool) -> String {
    let mut out = String::new();
    let mut total = 0usize;
    for (path, hits) in results {
        let rel = path.strip_prefix(root).unwrap_or(path);
        // all of the file's lines fit the budget? (a mid-file budget
        // stop leaves no marker, matching the sequential scan)
        let mut emitted_all = true;
        for line in &hits.lines {
            if total >= TOTAL_CAP {
                emitted_all = false;
                break;
            }
            out.push_str(line);
            total += 1;
        }
        if emitted_all && hits.capped {
            out.push_str(&format!("{}:[...per-file cap]\n", rel.display()));
        }
        if total >= TOTAL_CAP {
            break;
        }
    }
    if total >= TOTAL_CAP || truncated {
        out.push_str("[...results capped]\n");
    }
    out
}

fn name_regex(glob: &str) -> Option<regex::Regex> {
    let mut re = String::from("(?s)^");
    for c in glob.chars() {
        match c {
            '*' => re.push_str("[^/]*"),
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
        }
    }

    #[tokio::test]
    async fn finds_matches_with_locations() {
        let dir = std::env::temp_dir().join(format!("ka-grep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn one() {}\nfn two() {}\n").unwrap();
        std::fs::write(dir.join("src/b.txt"), "nothing here\n").unwrap();

        let ctx = ctx_for(&dir);
        let out = GrepHand
            .execute(&json!({"pattern": "fn \\w+\\(\\)"}), &ctx)
            .await;
        assert!(
            out.content.contains("src/a.rs:1:fn one() {}"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("src/a.rs:2:fn two() {}"),
            "{}",
            out.content
        );
        assert_eq!(out.content.matches("b.txt").count(), 0);

        let out = GrepHand
            .execute(&json!({"pattern": "fn", "glob": "*.txt"}), &ctx)
            .await;
        assert_eq!(out.content.trim(), "no matches");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn grep_skips_oversized_files() {
        let dir = std::env::temp_dir().join(format!("ka-grep-big-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // just over MAX_SCAN_BYTES
        let big = dir.join("big.log");
        let line = "needle in a very long haystack line\n";
        let reps = (MAX_SCAN_BYTES as usize) / line.len() + 2;
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&big).unwrap();
            for _ in 0..reps {
                f.write_all(line.as_bytes()).unwrap();
            }
        }
        assert!(std::fs::metadata(&big).unwrap().len() > MAX_SCAN_BYTES);
        let ctx = ctx_for(&dir);
        let out = GrepHand.execute(&json!({"pattern": "needle"}), &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("over the") && out.content.contains("scan cap; skipped"),
            "oversize note missing: {}",
            out.content
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn instructive_error_on_lookaround() {
        let ctx = ctx_for(std::path::Path::new("/tmp"));
        let out = GrepHand.execute(&json!({"pattern": "(?=foo)"}), &ctx).await;
        assert!(out.is_error);
        assert!(out.content.contains("not supported"), "{}", out.content);
    }

    /// A corpus wide enough to take the PARALLEL scan path must render
    /// byte-identical output to the sequential scan — walk order, caps,
    /// markers — or the tool's output would depend on thread scheduling.
    /// Walk order is readdir order (unsorted, filesystem-dependent), so
    /// every assertion below is order-independent.
    #[test]
    fn parallel_scan_is_byte_identical_to_sequential() {
        let base = std::env::temp_dir().join(format!("ka-grep-par-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        // corpus A: per-file cap, long-line cap, binary skip, miss —
        // match total under TOTAL_CAP so every file lands in budget
        let dir = base.join("a");
        std::fs::create_dir_all(&dir).unwrap();
        for f in 0..30 {
            let mut body = String::new();
            for l in 0..200 {
                body.push_str(&format!("line {f}:{l} ordinary text\n"));
                if l % 100 == 0 {
                    body.push_str("line with NEEDLE ka-par here\n");
                }
            }
            std::fs::write(dir.join(format!("f{f:03}.txt")), body).unwrap();
        }
        let mut capped = String::new();
        for m in 0..PER_FILE_CAP + 5 {
            capped.push_str(&format!("match {m} NEEDLE ka-par\n"));
        }
        std::fs::write(dir.join("capped.txt"), capped).unwrap();
        let long: String = "x".repeat(LINE_CAP + 100);
        std::fs::write(dir.join("long.txt"), format!("NEEDLE {long}\n")).unwrap();
        std::fs::write(dir.join("bin.dat"), [0u8, 159, 146, 150, 0, 7]).unwrap();
        std::fs::write(dir.join("miss.txt"), "nothing at all\n").unwrap();
        // gitignore only applies inside git repos for the `ignore` crate
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&base)
            .output()
            .unwrap();

        let re = regex::Regex::new("NEEDLE").unwrap();
        let (files, truncated) = collect_candidates(&dir, None);
        assert!(
            files.len() > PARALLEL_THRESHOLD,
            "corpus must take the parallel path"
        );
        let seq = assemble(&dir, &scan_sequential(&files, &dir, &re), truncated);
        let par = assemble(&dir, &scan_parallel(&files, &dir, &re), truncated);
        assert_eq!(seq, par, "parallel scan output must be byte-identical");
        assert!(
            seq.contains("capped.txt:20:match 19 NEEDLE ka-par"),
            "{seq}"
        );
        assert!(seq.contains("[...per-file cap]"), "{seq}");
        assert!(!seq.contains("[...results capped]"), "{seq}");
        assert!(!seq.contains("miss.txt"), "{seq}");
        // LINE_CAP: the long line is truncated, not passed through
        let long_row = seq.lines().find(|l| l.starts_with("long.txt:1:")).unwrap();
        let text = long_row.split_once(":1:").unwrap().1;
        assert_eq!(text.chars().count(), LINE_CAP);
        assert!(text.starts_with("NEEDLE "));

        // corpus B: 25 × 10 = 250 hits > TOTAL_CAP — the budget cut is
        // order-independent: exactly TOTAL_CAP rows plus the trailer
        let dir = base.join("b");
        std::fs::create_dir_all(&dir).unwrap();
        for f in 0..25 {
            let mut body = String::new();
            for m in 0..10 {
                body.push_str(&format!("hit {f}:{m} NEEDLE ka-par\n"));
            }
            body.push_str("filler\n");
            std::fs::write(dir.join(format!("g{f:03}.txt")), body).unwrap();
        }
        let (files, truncated) = collect_candidates(&dir, None);
        let seq = assemble(&dir, &scan_sequential(&files, &dir, &re), truncated);
        let par = assemble(&dir, &scan_parallel(&files, &dir, &re), truncated);
        assert_eq!(seq, par, "parallel scan output must be byte-identical");
        let rows = seq.lines().filter(|l| l.contains("NEEDLE")).count();
        assert_eq!(rows, TOTAL_CAP, "{seq}");
        assert!(seq.ends_with("[...results capped]\n"), "{seq}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Throughput benchmark (not a contract): `cargo test --release -p
    /// ka-engine --lib grep_bench -- --ignored --nocapture` prints
    /// sequential vs parallel wall time over the same candidate list.
    #[test]
    #[ignore = "perf benchmark: run explicitly with --release"]
    fn grep_bench_sequential_vs_parallel() {
        let dir = std::env::temp_dir().join(format!("ka-grep-bench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let body = "plain benchmark filler line\n".repeat(1_200);
        for f in 0..1_800 {
            let mut b = body.clone();
            if f % 10 == 0 {
                b.push_str("the grep_bench needle row\n");
            }
            std::fs::write(dir.join(format!("b{f:04}.txt")), b).unwrap();
        }
        let re = regex::Regex::new("grep_bench needle").unwrap();
        let (files, truncated) = collect_candidates(&dir, None);
        assert_eq!(files.len(), 1_800);

        let mut seq_times = Vec::new();
        let mut par_times = Vec::new();
        for round in 0..3 {
            let t = std::time::Instant::now();
            let seq = assemble(&dir, &scan_sequential(&files, &dir, &re), truncated);
            seq_times.push(t.elapsed());
            let t = std::time::Instant::now();
            let par = assemble(&dir, &scan_parallel(&files, &dir, &re), truncated);
            par_times.push(t.elapsed());
            assert_eq!(seq, par, "round {round}");
        }
        let seq_best = *seq_times.iter().min().unwrap();
        let par_best = *par_times.iter().min().unwrap();
        println!(
            "grep bench ({} files, {} MB): sequential best {:?}, parallel best {:?} ({:.1}x)",
            files.len(),
            1800 * body.len() / 1_000_000,
            seq_best,
            par_best,
            seq_best.as_secs_f64() / par_best.as_secs_f64(),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Responsiveness contract: ka runs a single-threaded executor, so
    /// grep's filesystem work MUST run on the blocking pool. If the
    /// scan ever executes inline in the hand future again, it stalls
    /// the whole runtime (TUI redraws, bash previews, interrupts) for
    /// the whole walk — and this test fails with zero ticks.
    #[tokio::test]
    async fn grep_does_not_block_the_runtime() {
        let dir = std::env::temp_dir().join(format!("ka-grep-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let filler = "filler line for the grep responsiveness corpus\n".repeat(700);
        for f in 0..1_200 {
            let mut body = filler.clone();
            if f % 400 == 0 {
                body.push_str("the responsiveness NEEDLE row\n");
            }
            std::fs::write(dir.join(format!("n{f:04}.txt")), body).unwrap();
        }
        let ctx = ctx_for(&dir);
        let args = json!({"pattern": "responsiveness NEEDLE"});

        // Calibrate: how long does one full grep take on this machine?
        let t0 = std::time::Instant::now();
        let out = GrepHand.execute(&args, &ctx).await;
        let scan_ms = t0.elapsed().as_millis() as u64;
        assert!(out.content.contains("NEEDLE row"), "{}", out.content);
        assert!(
            scan_ms >= 8,
            "corpus too small to prove non-blocking ({scan_ms} ms) — grow it"
        );

        // Re-run with a timer a fraction of the scan time: ticks must
        // fire WHILE the grep future is still pending. An inline
        // (runtime-blocking) scan lets zero ticks through.
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<()>();
        let ctx2 = ctx_for(&dir);
        let args2 = json!({"pattern": "responsiveness NEEDLE"});
        let task = tokio::spawn(async move {
            let out = GrepHand.execute(&args2, &ctx2).await;
            let _ = done_tx.send(());
            out
        });
        let mut interval =
            tokio::time::interval(std::time::Duration::from_millis((scan_ms / 8).max(1)));
        let mut ticks = 0usize;
        let raced = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                tokio::select! {
                    biased;
                    _ = &mut done_rx => break,
                    _ = interval.tick() => {
                        ticks += 1;
                        if ticks > 64 { break; }
                    }
                }
            }
        })
        .await;
        assert!(raced.is_ok(), "grep + timer race timed out");
        assert!(
            ticks >= 2,
            "runtime starved during grep: {ticks} ticks in a {scan_ms} ms scan — \
             the walk is blocking the executor again"
        );
        let out = task.await.unwrap();
        assert!(out.content.contains("NEEDLE row"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The unicode-table trim: `\p{...}` categories/scripts are outside
    /// the compiled feature set, so the pattern must fail INSTRUCTIVELY
    /// (a clean tool error naming the problem) — never silently match.
    #[tokio::test]
    async fn unicode_category_patterns_error_cleanly() {
        let ctx = ctx_for(&std::env::temp_dir());
        let out = GrepHand
            .execute(&json!({"pattern": "\\p{Greek}"}), &ctx)
            .await;
        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.contains("invalid regex"),
            "the pattern error is surfaced: {}",
            out.content
        );
        // core unicode semantics SURVIVE the trim: perl classes and
        // case-insensitive folding stay Unicode-aware
        let out = GrepHand
            .execute(&json!({"pattern": "(?i)CAFÉ", "path": "*"}), &ctx)
            .await;
        assert!(
            !out.is_error,
            "unicode-perl/case patterns compile: {}",
            out.content
        );
    }
}
