//! SES-01: a small line diff. The workspace carries no diff crate and
//! spawning `git diff` for a session summary would be both a trust
//! smell and a portability tax, so ~100 lines of textbook Myers live
//! here: the greedy O(ND) shortest-edit algorithm with head/tail
//! pre-trim, a size guard that degrades huge middles to one
//! whole-block replace, and a unified-diff renderer.

/// One edit-script operation: keep the old line, remove an old line,
/// or add a new line (in walk order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Edit {
    /// Old and new line match; consume one of each.
    Match,
    /// Consume one old line (a `-` row).
    Delete,
    /// Consume one new line (a `+` row).
    Insert,
}

/// Line counts of a diff.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DiffStat {
    /// Inserted lines.
    pub added: usize,
    /// Deleted lines.
    pub removed: usize,
}

/// Upper bound on the Myers core's work cells (worst-case trace memory
/// is O(D × (N+M))). Middles above it collapse to a whole-block
/// replace — the diff stays correct in aggregate, just not minimal.
const MAX_DIFF_CELLS: usize = 8_000_000;

/// Myers edit script between two line slices. Common head/tail lines
/// are trimmed first (they dominate real file edits and keep D small);
/// the O(ND) core only sees the changed middle.
pub(crate) fn edits(old: &[&str], new: &[&str]) -> Vec<Edit> {
    let mut start = 0;
    while start < old.len() && start < new.len() && old[start] == new[start] {
        start += 1;
    }
    let mut end_old = old.len();
    let mut end_new = new.len();
    while end_old > start && end_new > start && old[end_old - 1] == new[end_new - 1] {
        end_old -= 1;
        end_new -= 1;
    }
    let mid_old = &old[start..end_old];
    let mid_new = &new[start..end_new];
    let mut out = Vec::with_capacity(old.len() + new.len());
    out.extend(std::iter::repeat_n(Edit::Match, start));
    let n = mid_old.len();
    let m = mid_new.len();
    let cells = n.checked_add(m).map(|t| t * (2 * t + 1)).unwrap_or(0);
    if cells <= MAX_DIFF_CELLS {
        out.extend(myers(mid_old, mid_new));
    } else {
        // too big to diff cheaply: one whole-block replace
        out.extend(std::iter::repeat_n(Edit::Delete, n));
        out.extend(std::iter::repeat_n(Edit::Insert, m));
    }
    out.extend(std::iter::repeat_n(Edit::Match, old.len() - end_old));
    out
}

/// The greedy Myers shortest-edit script (the "O(ND) diff" paper, fig.
/// 2): walk furthest-reaching D-paths per diagonal, snapshot the V
/// array per depth, then backtrack the snapshot stack into the edit
/// script. `old`/`new` should be pre-trimmed; the whole inputs work
/// too, just slower.
fn myers(old: &[&str], new: &[&str]) -> Vec<Edit> {
    let n = old.len() as isize;
    let m = new.len() as isize;
    if n == 0 && m == 0 {
        return Vec::new();
    }
    let max = (n + m) as usize;
    let offset = max as isize;
    let mut v = vec![0isize; 2 * max + 1];
    let mut trace: Vec<Vec<isize>> = Vec::new();
    for d in 0..=max as isize {
        // snapshot BEFORE this depth mutates v: backtrack reads round
        // d's decisions off the state left by rounds < d
        trace.push(v.clone());
        let mut found = false;
        for k in (-d..=d).rev().step_by(2) {
            let idx = (k + offset) as usize;
            let mut x = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) {
                v[idx + 1] // down: insert from new
            } else {
                v[idx - 1] + 1 // right: delete from old
            };
            let mut y = x - k;
            while x < n && y < m && old[x as usize] == new[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x >= n && y >= m {
                found = true;
                break;
            }
        }
        if found {
            return backtrack(old, new, &trace, offset);
        }
    }
    // unreachable: the full-diff path always terminates by d = n+m
    let mut out = std::iter::repeat_n(Edit::Delete, old.len()).collect::<Vec<_>>();
    out.extend(std::iter::repeat_n(Edit::Insert, new.len()));
    out
}

/// Reconstruct the edit script from the per-depth V snapshots: walk
/// from the endpoint back to the origin, diagonal runs are matches and
/// each depth step is one delete or one insert.
fn backtrack(old: &[&str], new: &[&str], trace: &[Vec<isize>], offset: isize) -> Vec<Edit> {
    let mut x = old.len() as isize;
    let mut y = new.len() as isize;
    let mut script = Vec::new();
    for (d, v) in trace.iter().enumerate().rev() {
        let d = d as isize;
        let k = x - y;
        let prev_k =
            if k == -d || (k != d && v[(k - 1 + offset) as usize] < v[(k + 1 + offset) as usize]) {
                k + 1
            } else {
                k - 1
            };
        let prev_x = v[(prev_k + offset) as usize];
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            script.push(Edit::Match);
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            // x unchanged → the move came down (insert); else right (delete)
            script.push(if x == prev_x {
                Edit::Insert
            } else {
                Edit::Delete
            });
        }
        x = prev_x;
        y = prev_y;
    }
    script.reverse();
    script
}

/// `+`/`-` line counts between two texts.
pub(crate) fn stat(old: &str, new: &str) -> DiffStat {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let mut s = DiffStat::default();
    for e in edits(&old_lines, &new_lines) {
        match e {
            Edit::Match => {}
            Edit::Delete => s.removed += 1,
            Edit::Insert => s.added += 1,
        }
    }
    s
}

/// Unified diff of two texts against one display path (`--- a/path` /
/// `+++ b/path` / `@@` hunks with `context` context lines). Empty when
/// the texts are line-identical.
pub(crate) fn unified(path: &str, old: &str, new: &str, context: usize) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let script = edits(&old_lines, &new_lines);
    if script.iter().all(|&e| e == Edit::Match) {
        return String::new();
    }
    // flat row list with the 0-based old/new position each row sits at
    enum Row<'a> {
        Ctx(&'a str),
        Del(&'a str),
        Ins(&'a str),
    }
    let mut rows: Vec<Row> = Vec::new();
    let mut row_a: Vec<usize> = Vec::new();
    let mut row_b: Vec<usize> = Vec::new();
    let mut changed: Vec<usize> = Vec::new();
    let (mut ai, mut bi) = (0usize, 0usize);
    for e in &script {
        let (row, a, b) = match e {
            Edit::Match => {
                let r = Row::Ctx(old_lines[ai]);
                ai += 1;
                bi += 1;
                (r, ai - 1, bi - 1)
            }
            Edit::Delete => {
                let r = Row::Del(old_lines[ai]);
                ai += 1;
                changed.push(rows.len());
                (r, ai - 1, bi)
            }
            Edit::Insert => {
                let r = Row::Ins(new_lines[bi]);
                bi += 1;
                changed.push(rows.len());
                (r, ai, bi - 1)
            }
        };
        rows.push(row);
        row_a.push(a);
        row_b.push(b);
    }
    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    // cluster change rows whose context windows would overlap
    let mut clusters: Vec<&[usize]> = Vec::new();
    let mut start = 0;
    for i in 1..=changed.len() {
        let split = i == changed.len() || changed[i] - changed[i - 1] > 2 * context + 1;
        if split {
            clusters.push(&changed[start..i]);
            start = i;
        }
    }
    for cluster in clusters {
        let lo = cluster[0].saturating_sub(context);
        let hi = (cluster[cluster.len() - 1] + context + 1).min(rows.len());
        let a_len = (lo..hi)
            .filter(|&r| !matches!(rows[r], Row::Ins(_)))
            .count();
        let b_len = (lo..hi)
            .filter(|&r| !matches!(rows[r], Row::Del(_)))
            .count();
        // unified convention: an empty side points at the line the
        // hunk would insert before (0-based position, unadjusted)
        let a_show = if a_len > 0 { row_a[lo] + 1 } else { row_a[lo] };
        let b_show = if b_len > 0 { row_b[lo] + 1 } else { row_b[lo] };
        out.push_str(&format!("@@ -{a_show},{a_len} +{b_show},{b_len} @@\n"));
        for row in &rows[lo..hi] {
            match row {
                Row::Ctx(line) => out.push_str(&format!(" {line}\n")),
                Row::Del(line) => out.push_str(&format!("-{line}\n")),
                Row::Ins(line) => out.push_str(&format!("+{line}\n")),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn lines(s: &str) -> Vec<&str> {
        s.lines().collect()
    }

    #[test]
    fn identical_inputs_diff_to_nothing() {
        let t = "a\nb\nc\n";
        assert!(stat(t, t) == DiffStat::default());
        assert!(unified("f.txt", t, t, 3).is_empty());
    }

    #[test]
    fn pure_insert_and_delete_count() {
        // "b" exists only in old: any script deletes it, then adds x,y
        let old = "a\nb\nc\n";
        let new = "a\nx\ny\nc\n";
        let s = stat(old, new);
        assert_eq!((s.added, s.removed), (2, 1));
        let s = stat(new, old);
        assert_eq!((s.added, s.removed), (1, 2));
    }

    #[test]
    fn replacement_counts_both_sides() {
        let old = "one\ntwo\nthree\nfour\nfive\n";
        let new = "one\ntwo\nTWO\nfour\nfive\n";
        let s = stat(old, new);
        assert_eq!((s.added, s.removed), (1, 1));
    }

    #[test]
    fn empty_to_content_is_all_inserts() {
        let s = stat("", "a\nb\n");
        assert_eq!((s.added, s.removed), (2, 0));
        let s = stat("a\nb\n", "");
        assert_eq!((s.added, s.removed), (0, 2));
    }

    #[test]
    fn myers_finds_the_short_script() {
        // classic: abcab → cbabc — the LCS ("babc") is 4 long, so the
        // minimal script is 1 deletion + 1 insertion
        let old = lines("abcab");
        let new = lines("cbabc");
        let script = edits(&old, &new);
        assert_eq!(script.iter().filter(|&&e| e == Edit::Delete).count(), 1);
        assert_eq!(script.iter().filter(|&&e| e == Edit::Insert).count(), 1);
        // walking the script must reconstruct both texts
        let mut o = Vec::new();
        let mut n = Vec::new();
        let (mut i, mut j) = (0usize, 0usize);
        for e in &script {
            match e {
                Edit::Match => {
                    o.push(old[i]);
                    n.push(new[j]);
                    i += 1;
                    j += 1;
                }
                Edit::Delete => {
                    o.push(old[i]);
                    i += 1;
                }
                Edit::Insert => {
                    n.push(new[j]);
                    j += 1;
                }
            }
        }
        assert_eq!(o, old);
        assert_eq!(n, new);
    }

    #[test]
    fn unified_hunks_carry_context_and_headers() {
        let old = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n";
        let new = "l1\nl2\nl3\nl4\nCHANGED\nl6\nl7\nl8\n";
        let out = unified("src/x.rs", old, new, 3);
        assert!(out.starts_with("--- a/src/x.rs\n+++ b/src/x.rs\n"), "{out}");
        assert!(out.contains("@@ -2,7 +2,7 @@"), "{out}");
        assert!(out.contains("-l5\n+CHANGED\n"), "{out}");
        assert!(out.contains(" l2\n"), "{out}");
    }

    #[test]
    fn distant_changes_make_two_hunks() {
        let mut new = String::new();
        for i in 0..40 {
            new.push_str(&format!("line{i}\n"));
            if i == 2 {
                new.push_str("A\n");
            }
            if i == 36 {
                new.push_str("B\n");
            }
        }
        let mut old = String::new();
        for i in 0..40 {
            old.push_str(&format!("line{i}\n"));
        }
        let out = unified("f", &old, &new, 3);
        assert_eq!(out.matches("@@").count(), 4, "two hunks × 2 @@: {out}");
    }

    #[test]
    fn huge_middles_degrade_to_block_replace() {
        let old = "x\n".repeat(30_000);
        let mut new = String::new();
        for i in 0..30_000 {
            new.push_str(&format!("y{i}\n"));
        }
        // must terminate fast and stay coherent: every old line removed,
        // every new line added
        let s = stat(&old, &new);
        assert_eq!(s.removed, 30_000);
        assert_eq!(s.added, 30_000);
    }

    #[test]
    fn insertion_at_top_reports_zero_start() {
        let out = unified("f", "", "a\nb\nc\n", 3);
        assert!(out.contains("@@ -0,0 +1,3 @@"), "{out}");
    }
}
