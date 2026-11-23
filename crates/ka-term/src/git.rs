//! Local git surface (GIT-04) and the `/pr` review preset (GIT-01).
//!
//! Two things live here, both deliberately thin over git itself:
//!
//! * `/git` — the staging & commit panel. It parses
//!   `git status --porcelain`, builds the stage/unstage/commit/diff
//!   commands, and runs them against the session cwd. The panel never
//!   pushes, never forces, never rebases, never hard-resets, never
//!   touches the stash: stage, unstage, commit — everything else
//!   belongs to the user's shell or the model's bash hand under its
//!   own permission rules.
//! * `/pr <n|url|branch> [fix]` — a preset prompt FOR THE MODEL. `gh`
//!   is not a ka-term dependency: the surface only composes the
//!   instructions; the model runs `gh` through its bash hand.

use crossterm::event::KeyCode;
use std::path::Path;

/// One `git status --porcelain` row: the index (staged) status `x`,
/// the worktree status `y`, and the path — the rename/copy *target*
/// when `orig` carries the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFile {
    /// Index status (`' '` when nothing is staged for this path).
    pub x: char,
    /// Worktree status (`' '` when the worktree matches the index).
    pub y: char,
    /// Working-tree path (rename target when [`GitFile::orig`] is set).
    pub path: String,
    /// Rename/copy source (`old` of `R  old -> new`).
    pub orig: Option<String>,
}

impl GitFile {
    /// Something is already in the index for this path.
    pub fn staged(&self) -> bool {
        self.x != ' ' && self.x != '?'
    }

    /// Untracked (`??`): never in HEAD, so `git diff HEAD` shows
    /// nothing for it and the panel diffs against /dev/null instead.
    pub fn untracked(&self) -> bool {
        self.x == '?' && self.y == '?'
    }

    /// The two-char state tag shown in the list (`M `, ` M`, `MM`,
    /// `A `, `D `, `R `, `??`) — porcelain's own spelling, so the row
    /// reads the same as `git status -s`.
    pub fn tag(&self) -> String {
        format!("{}{}", self.x, self.y)
    }

    /// The list-row label: `TAG path` (renames show `old -> new`).
    pub fn row(&self) -> String {
        match &self.orig {
            Some(orig) => format!("{} {} -> {}", self.tag(), orig, self.path),
            None => format!("{} {}", self.tag(), self.path),
        }
    }
}

/// Parse `git status --porcelain` output (v1, line-oriented). Blank
/// lines and truncated rows are skipped; `old -> new` rename rows
/// split into `orig`/`path`.
pub fn parse_porcelain(out: &str) -> Vec<GitFile> {
    out.lines()
        .filter_map(|line| {
            let line = line.trim_end_matches('\r');
            let mut chars = line.chars();
            let x = chars.next()?;
            let y = chars.next()?;
            let rest = chars.as_str();
            let path = rest.strip_prefix(' ')?;
            if path.is_empty() {
                return None;
            }
            // git quotes paths containing specials; the panel shows the
            // unquoted form (best effort — C-escapes stay literal)
            let path = path
                .strip_prefix('"')
                .and_then(|p| p.strip_suffix('"'))
                .unwrap_or(path);
            let (orig, path) = match path.rsplit_once(" -> ") {
                Some((old, new)) => (Some(old.to_string()), new.to_string()),
                None => (None, path.to_string()),
            };
            Some(GitFile { x, y, path, orig })
        })
        .collect()
}

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| (*p).to_string()).collect()
}

/// `git status --porcelain -uall` (untracked files listed individually).
pub fn status_args() -> Vec<String> {
    args(&["status", "--porcelain", "-uall"])
}

/// Stage one path (`git add -- <path>`; also stages deletions).
pub fn stage_args(path: &str) -> Vec<String> {
    args(&["add", "--", path])
}

/// Stage the whole working tree (`git add -A`).
pub fn stage_all_args() -> Vec<String> {
    args(&["add", "-A"])
}

/// Unstage one path (`git reset HEAD -- <path>`): index only — the
/// worktree is never touched, so no hard reset anywhere in the panel.
pub fn unstage_args(path: &str) -> Vec<String> {
    args(&["reset", "HEAD", "--", path])
}

/// Commit what is staged (`git commit -m <message>`).
pub fn commit_args(message: &str) -> Vec<String> {
    args(&["commit", "-m", message])
}

/// The diff command for one file. Tracked: against HEAD (staged +
/// unstaged in one view). Untracked: no-index against /dev/null, the
/// cheapest way to show what `git add` would bring in.
pub fn diff_args(path: &str, untracked: bool) -> Vec<String> {
    if untracked {
        args(&["diff", "--no-index", "--", "/dev/null", path])
    } else {
        args(&["diff", "HEAD", "--", path])
    }
}

/// Current branch (`git symbolic-ref --short HEAD`) — the panel
/// header. `None` outside a repo or on a detached HEAD.
pub fn branch(cwd: &Path) -> Option<String> {
    let out = run_git(cwd, &args(&["symbolic-ref", "--short", "HEAD"])).ok()?;
    let name = out.trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// One git invocation's raw result (spawn errors are the only hard
/// `Err`): `code` is `None` when the process was killed by a signal.
struct GitOutput {
    ok: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_git_raw(cwd: &Path, git_args: &[String]) -> Result<GitOutput, String> {
    let out = std::process::Command::new("git")
        .current_dir(cwd)
        .args(git_args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    Ok(GitOutput {
        ok: out.status.success(),
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

/// Run one git command in `cwd`; stdout on success, the (trimmed)
/// stderr — or a generic exit note when stderr is empty — on failure.
pub fn run_git(cwd: &Path, git_args: &[String]) -> Result<String, String> {
    let out = run_git_raw(cwd, git_args)?;
    if out.ok {
        return Ok(out.stdout);
    }
    Err(if out.stderr.is_empty() {
        format!(
            "git {} failed ({})",
            git_args.first().map(String::as_str).unwrap_or("?"),
            out.code
                .map(|c| format!("exit {c}"))
                .unwrap_or_else(|| "signalled".into())
        )
    } else {
        out.stderr
    })
}

/// The diff text for one status row. Tracked: against HEAD, falling
/// back to the worktree-vs-index diff in a repo without commits.
/// Untracked: `--no-index` against /dev/null, whose exit code 1 means
/// "differences found" — the diff itself rides in stdout.
pub fn diff_for(cwd: &Path, file: &GitFile) -> Result<String, String> {
    let untracked = file.untracked();
    if untracked {
        let out = run_git_raw(cwd, &diff_args(&file.path, true))?;
        return match (out.ok, out.code) {
            (true, _) | (false, Some(1)) => Ok(out.stdout),
            _ => Err(if out.stderr.is_empty() {
                format!("git diff --no-index failed on {}", file.path)
            } else {
                out.stderr
            }),
        };
    }
    match run_git(cwd, &diff_args(&file.path, false)) {
        Ok(text) => Ok(text),
        Err(_) => run_git(cwd, &args(&["diff", "--", &file.path])),
    }
}

/// The `[branch abc1234]` line `git commit` prints, reduced to the
/// short hash — `None` when the output does not carry one (hooks may
/// silence it; the caller then shows a generic confirmation).
pub fn commit_summary(stdout: &str) -> Option<String> {
    let line = stdout.lines().find(|l| l.starts_with('['))?;
    let inner = line.trim_start_matches('[').split(']').next()?;
    inner.split_whitespace().nth(1).map(str::to_string)
}

/// Which sub-view the git panel shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pane {
    /// File list — the landing view.
    List,
    /// `git diff` of one file (the text is loaded when the pane opens).
    Diff { path: String, text: String },
    /// Commit-message input over the staged set.
    Commit,
}

/// What a key press means to the panel — pure, so the keymap is
/// testable without the TUI loop. The app loop applies the action to
/// its modal state (and runs the git commands).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitAction {
    /// Unlisted key: nothing.
    None,
    /// Esc on the landing view: close the panel.
    Close,
    /// Esc on a sub-view: back to the list.
    Back,
    /// ↑/↓ on the list: the new selected row.
    Select(usize),
    /// ⏎ on the list: open the selected file's diff.
    OpenDiff,
    /// s: stage the selected file — or unstage it when it already
    /// carries staged changes.
    Stage,
    /// a: stage the whole working tree.
    StageAll,
    /// c: open the commit-message input.
    CommitOpen,
    /// ⏎ in the message input: commit the staged set.
    CommitSubmit,
    /// ?: toggle the key-help overlay (landing view).
    ToggleKeys,
    /// Diff scroll: lines down (positive) or up (negative).
    Scroll(i32),
    /// Diff jump: top.
    Top,
    /// Diff jump: tail.
    Tail,
    /// A printable character for the commit message.
    Type(char),
    /// Backspace in the commit message.
    Erase,
}

/// The panel's keymap. `len`/`selected` are the list's size and row
/// (clamping happens here); `keys_shown` is the landing view's help
/// overlay, which swallows everything except its own close keys.
pub fn handle_key(
    code: KeyCode,
    pane: &Pane,
    keys_shown: bool,
    len: usize,
    selected: usize,
) -> GitAction {
    use KeyCode::*;
    match pane {
        Pane::List if keys_shown => match code {
            Esc | Char('?') => GitAction::ToggleKeys,
            _ => GitAction::None,
        },
        Pane::List => match code {
            Esc => GitAction::Close,
            Up => GitAction::Select(selected.saturating_sub(1)),
            Down => GitAction::Select((selected + 1).min(len.saturating_sub(1))),
            Enter => GitAction::OpenDiff,
            Char('s') => GitAction::Stage,
            Char('a') => GitAction::StageAll,
            Char('c') => GitAction::CommitOpen,
            Char('?') => GitAction::ToggleKeys,
            _ => GitAction::None,
        },
        Pane::Diff { .. } => match code {
            Esc => GitAction::Back,
            Up => GitAction::Scroll(-1),
            Down => GitAction::Scroll(1),
            PageUp => GitAction::Scroll(-10),
            PageDown => GitAction::Scroll(10),
            Home => GitAction::Top,
            End => GitAction::Tail,
            _ => GitAction::None,
        },
        Pane::Commit => match code {
            Esc => GitAction::Back,
            Enter => GitAction::CommitSubmit,
            Backspace => GitAction::Erase,
            Char(c) => GitAction::Type(c),
            _ => GitAction::None,
        },
    }
}

/// GIT-01: the `/pr` argument parse. `<ref>` plus an optional `fix`
/// word; anything else is a usage error (refused, not guessed).
pub fn parse_pr_invocation(rest: Option<&str>) -> Option<(String, bool)> {
    let rest = rest?;
    let mut parts = rest.split_whitespace();
    let reference = parts.next()?.to_string();
    let fix = match parts.next() {
        None => false,
        Some("fix") => true,
        Some(_) => return None,
    };
    (parts.next().is_none()).then_some((reference, fix))
}

/// The transcript label for a PR reference: bare numbers shorten to
/// `#123`; branches and URLs pass through as typed.
pub fn pr_label(reference: &str) -> String {
    if reference.chars().all(|c| c.is_ascii_digit()) && !reference.is_empty() {
        format!("#{reference}")
    } else {
        reference.to_string()
    }
}

/// GIT-01: the review preset. Directs the MODEL at the GitHub CLI —
/// ka-term never runs `gh`. Readonly reviews and fix runs share the
/// discovery/diff/comments steps; `fix` adds implement-commit-push
/// with the panel's own taboos (no force, no rebase) spelled out.
pub fn pr_review_prompt(reference: &str, fix: bool) -> String {
    let mut text = format!(
        "Review pull request {reference} as a strict senior engineer, using the GitHub CLI \
(`gh` — run it via the bash tool; never modify files or branches yet). Steps: \
(1) locate the PR: `gh pr view {reference} --json number,title,url,state,author,headRefName,\
baseRefName` (a bare number resolves against this repo's origin; a branch name or PR URL \
works in the same command); \
(2) read the change: `gh pr diff {reference}` — `gh pr checkout {reference}` only if files \
must be read or tests run locally; \
(3) read the discussion: `gh api repos/<owner>/<repo>/pulls/<number>/comments --paginate` \
for the positioned review comments (resolve <owner>/<repo> and <number> from the view \
output) and `gh pr view {reference} --comments` for review summaries. \
Verify every finding against the actual code (read files for context; run the test suite \
only when it is cheap). Produce: \
(1) a one-line verdict — approve / request changes / blocked; \
(2) findings ordered blocker > major > minor > nit, each as `file:line — issue — concrete fix`; \
(3) every open review thread: whether HEAD already addresses it, and if not, the exact fix."
    );
    if fix {
        text.push_str(
            "\n\nThis is a FIX run: after the review, implement every blocker and major \
finding (minors only when trivial) on the PR's branch — `gh pr checkout <ref>` first if it \
is not checked out — commit the fixes with clear messages, and push to the PR's head branch. \
Never force-push, never rebase, never touch files outside the PR's scope. Finish with a \
per-finding fix summary and the resulting commit hashes.",
        );
    }
    text
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn porcelain_parse_reads_state_tags_paths_and_renames() {
        let files = parse_porcelain(
            " M src/tui.rs\nM  docs/x.md\nA  new.rs\nD  gone.rs\n?? scratch\n\
             R  old.rs -> new.rs\nMM both.rs\n\r\n",
        );
        assert_eq!(files.len(), 7);
        assert_eq!(files[0].tag(), " M");
        assert!(!files[0].staged());
        assert_eq!(files[1].tag(), "M ");
        assert!(files[1].staged());
        assert_eq!(files[4].path, "scratch");
        assert!(files[4].untracked());
        assert_eq!(files[5].orig.as_deref(), Some("old.rs"));
        assert_eq!(files[5].path, "new.rs");
        assert!(files[6].staged() && !files[6].untracked());
        assert_eq!(files[6].row(), "MM both.rs");
        assert_eq!(files[5].row(), "R  old.rs -> new.rs");
    }

    #[test]
    fn porcelain_parse_skips_garbage() {
        assert!(parse_porcelain("").is_empty());
        assert!(parse_porcelain("\n\n").is_empty());
        assert!(parse_porcelain("X").is_empty());
        assert!(parse_porcelain("M \n").is_empty());
        assert!(parse_porcelain("M  \n").is_empty());
    }

    #[test]
    fn command_builders_match_the_documented_git_calls() {
        assert_eq!(status_args(), ["status", "--porcelain", "-uall"]);
        assert_eq!(stage_args("a b.rs"), ["add", "--", "a b.rs"]);
        assert_eq!(stage_all_args(), ["add", "-A"]);
        assert_eq!(
            unstage_args("a.rs"),
            ["reset", "HEAD", "--", "a.rs"],
            "unstage is index-only: reset HEAD -- path, never --hard"
        );
        assert_eq!(commit_args("hi"), ["commit", "-m", "hi"]);
        assert_eq!(diff_args("a.rs", false), ["diff", "HEAD", "--", "a.rs"]);
        assert_eq!(
            diff_args("a.rs", true),
            ["diff", "--no-index", "--", "/dev/null", "a.rs"]
        );
    }

    #[test]
    fn commit_summary_extracts_the_hash() {
        let out = "[main 1a2b3c4] feat: x\n 1 file changed, 2 insertions(+)\n";
        assert_eq!(commit_summary(out).as_deref(), Some("1a2b3c4"));
        assert_eq!(commit_summary("nothing here"), None);
        assert_eq!(commit_summary(""), None);
    }

    #[test]
    fn keymap_answers_every_documented_key() {
        use KeyCode::*;
        // landing view
        assert_eq!(handle_key(Esc, &Pane::List, false, 3, 0), GitAction::Close);
        assert_eq!(
            handle_key(Down, &Pane::List, false, 3, 1),
            GitAction::Select(2)
        );
        assert_eq!(
            handle_key(Down, &Pane::List, false, 3, 2),
            GitAction::Select(2),
            "selection clamps at the last row"
        );
        assert_eq!(
            handle_key(Up, &Pane::List, false, 3, 0),
            GitAction::Select(0)
        );
        assert_eq!(
            handle_key(Enter, &Pane::List, false, 3, 1),
            GitAction::OpenDiff
        );
        assert_eq!(
            handle_key(Char('s'), &Pane::List, false, 3, 0),
            GitAction::Stage
        );
        assert_eq!(
            handle_key(Char('a'), &Pane::List, false, 3, 0),
            GitAction::StageAll
        );
        assert_eq!(
            handle_key(Char('c'), &Pane::List, false, 3, 0),
            GitAction::CommitOpen
        );
        assert_eq!(
            handle_key(Char('?'), &Pane::List, false, 3, 0),
            GitAction::ToggleKeys
        );
        assert_eq!(
            handle_key(Char('x'), &Pane::List, false, 3, 0),
            GitAction::None
        );
        // help overlay swallows everything but its close keys
        assert_eq!(
            handle_key(Char('s'), &Pane::List, true, 3, 0),
            GitAction::None
        );
        assert_eq!(
            handle_key(Esc, &Pane::List, true, 3, 0),
            GitAction::ToggleKeys
        );
        // diff view: back + scroll, nothing else
        let diff = Pane::Diff {
            path: "a.rs".into(),
            text: String::new(),
        };
        assert_eq!(handle_key(Esc, &diff, false, 3, 0), GitAction::Back);
        assert_eq!(
            handle_key(PageUp, &diff, false, 3, 0),
            GitAction::Scroll(-10)
        );
        assert_eq!(
            handle_key(PageDown, &diff, false, 3, 0),
            GitAction::Scroll(10)
        );
        assert_eq!(handle_key(End, &diff, false, 3, 0), GitAction::Tail);
        assert_eq!(handle_key(Char('s'), &diff, false, 3, 0), GitAction::None);
        // commit input
        assert_eq!(handle_key(Esc, &Pane::Commit, false, 3, 0), GitAction::Back);
        assert_eq!(
            handle_key(Enter, &Pane::Commit, false, 3, 0),
            GitAction::CommitSubmit
        );
        assert_eq!(
            handle_key(Backspace, &Pane::Commit, false, 3, 0),
            GitAction::Erase
        );
        assert_eq!(
            handle_key(Char('x'), &Pane::Commit, false, 3, 0),
            GitAction::Type('x')
        );
    }

    #[test]
    fn pr_invocation_parses_ref_and_fix_flag() {
        assert_eq!(
            parse_pr_invocation(Some("123")),
            Some(("123".into(), false))
        );
        assert_eq!(
            parse_pr_invocation(Some("123 fix")),
            Some(("123".into(), true))
        );
        assert_eq!(
            parse_pr_invocation(Some("https://github.com/o/r/pull/42")),
            Some(("https://github.com/o/r/pull/42".into(), false))
        );
        assert_eq!(
            parse_pr_invocation(Some("feat/x fix")),
            Some(("feat/x".into(), true))
        );
        // anything unparsable is a usage error, never a guess; a bare
        // `fix` in the ref slot is a branch named fix (the flag is
        // only recognized as the second token)
        assert_eq!(parse_pr_invocation(None), None);
        assert_eq!(parse_pr_invocation(Some("")), None);
        assert_eq!(
            parse_pr_invocation(Some("fix")),
            Some(("fix".into(), false))
        );
        assert_eq!(parse_pr_invocation(Some("123 fix extra")), None);
        assert_eq!(parse_pr_invocation(Some("123 FIX")), None);
        // the ref slot takes any token (branches may look like anything)
        assert_eq!(
            parse_pr_invocation(Some("123Fix")),
            Some(("123Fix".into(), false))
        );
    }

    #[test]
    fn pr_label_hashes_bare_numbers_only() {
        assert_eq!(pr_label("123"), "#123");
        assert_eq!(pr_label("main"), "main");
        assert_eq!(
            pr_label("https://github.com/o/r/pull/9"),
            "https://github.com/o/r/pull/9"
        );
    }

    #[test]
    fn pr_prompt_covers_locate_diff_comments_and_fix_taboos() {
        let ro = pr_review_prompt("123", false);
        assert!(ro.contains("gh pr view 123"), "{ro}");
        assert!(ro.contains("gh pr diff 123"), "{ro}");
        assert!(
            ro.contains("gh api repos/<owner>/<repo>/pulls/<number>/comments"),
            "{ro}"
        );
        assert!(ro.contains("file:line — issue — concrete fix"), "{ro}");
        assert!(ro.contains("never modify files or branches"), "{ro}");
        assert!(!ro.contains("FIX run"), "{ro}");
        assert!(!ro.contains("push"));

        let fix = pr_review_prompt("feat/x", true);
        assert!(fix.contains("gh pr view feat/x"), "{fix}");
        assert!(fix.contains("FIX run"), "{fix}");
        assert!(fix.contains("Never force-push"), "{fix}");
        assert!(fix.contains("push to the PR's head branch"), "{fix}");
    }
}
