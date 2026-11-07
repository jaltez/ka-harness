//! Strand contracts: append-only sessions, resume-by-id, fork tree,
//! interrupted-turn synthesis, and markdown export — the durability
//! layer every surface (TUI, headless, serve, ACP) shares.

#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::sync::Mutex;

use ka_strand::{IdMatch, Record, Role, StrandFile};

/// ka-strand has no tokio; the data-dir global is process-wide, so
/// data-dir-dependent tests serialize on this lock.
static DATA_DIR_LOCK: Mutex<()> = Mutex::new(());

fn temp_data_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ka-strand-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    ka_strand::set_data_dir_for_tests(dir.clone());
    dir
}

/// A strand persists what was appended: reopening the file replays the
/// same records (the resume contract).
#[test]
fn append_reopen_replays_records() {
    let dir = temp_data_dir("reopen");
    let cwd = dir.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut strand = StrandFile::create(&cwd, None).unwrap();
    strand
        .append(Record::Message {
            images: Vec::new(),
            id: ka_strand::new_record_id(),
            role: Role::User,
            content: "hello world".into(),
            calls: Vec::new(),
            results: Vec::new(),
            thinking: None,
        })
        .unwrap();
    let path = strand.path().unwrap().to_path_buf();
    drop(strand);

    let reopened = StrandFile::open(&path).unwrap();
    let messages: Vec<&Record> = reopened
        .records()
        .iter()
        .filter(|r| matches!(r, Record::Message { .. }))
        .collect();
    assert_eq!(messages.len(), 1);
    match messages[0] {
        Record::Message { role, content, .. } => {
            assert_eq!(*role, Role::User);
            assert_eq!(content, "hello world");
        }
        _ => unreachable!(),
    }
}

/// Markdown export: user/assistant content lands in a readable form —
/// the `ka export` contract.
#[test]
fn render_markdown_includes_roles_and_content() {
    let records = vec![
        Record::Message {
            id: ka_strand::new_record_id(),
            images: Vec::new(),
            role: Role::User,
            content: "what is this repo".into(),
            calls: Vec::new(),
            results: Vec::new(),
            thinking: None,
        },
        Record::Message {
            id: ka_strand::new_record_id(),
            images: Vec::new(),
            role: Role::Assistant,
            content: "a rust workspace".into(),
            calls: Vec::new(),
            results: Vec::new(),
            thinking: None,
        },
    ];
    let md = ka_strand::render_markdown(&records);
    assert!(md.contains("what is this repo"), "{md:?}");
    assert!(md.contains("a rust workspace"), "{md:?}");
    assert!(md.contains("you"), "{md:?}");
    assert!(md.contains("ka"), "{md:?}");
}

/// Resume by id: unique prefixes resolve; ambiguous prefixes report the
/// candidates; unknown ids report none — the `ka --session <prefix>`
/// contract.
#[test]
fn resolve_id_unique_ambiguous_none() {
    let _guard = DATA_DIR_LOCK.lock().unwrap();
    let dir = temp_data_dir("resolve");
    let cwd = dir.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();

    // strands materialize on first append — empty creates are not listed
    let mut a = StrandFile::create(&cwd, None).unwrap();
    let mut b = StrandFile::create(&cwd, None).unwrap();
    for strand in [&mut a, &mut b] {
        strand
            .append(Record::Message {
                images: Vec::new(),
                id: ka_strand::new_record_id(),
                role: Role::User,
                content: "materialize".into(),
                calls: Vec::new(),
                results: Vec::new(),
                thinking: None,
            })
            .unwrap();
    }
    drop(a);
    drop(b);
    let summaries = ka_strand::list(&cwd).unwrap();
    assert_eq!(summaries.len(), 2, "{summaries:?}");
    let id_a = summaries[0].id.clone();
    let id_b = summaries[1].id.clone();

    // full ids resolve uniquely
    assert!(matches!(
        ka_strand::resolve_id(&cwd, &id_a),
        Ok(IdMatch::Unique(_))
    ));
    assert!(matches!(
        ka_strand::resolve_id(&cwd, &id_b),
        Ok(IdMatch::Unique(_))
    ));
    // unknown ids say so
    assert!(matches!(
        ka_strand::resolve_id(&cwd, "sdeadbeef"),
        Ok(IdMatch::None)
    ));
    // a shared short prefix reports both candidates
    let shared: String = id_a
        .chars()
        .zip(id_b.chars())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x)
        .collect();
    if shared.len() >= 2 {
        match ka_strand::resolve_id(&cwd, &shared) {
            Ok(IdMatch::Ambiguous(candidates)) => {
                assert_eq!(candidates.len(), 2, "{candidates:?}");
            }
            other => panic!("shared prefix {shared:?} must be ambiguous, got {other:?}"),
        }
    }
}

/// Interrupted turns synthesize on reopen: a dangling turn is marked
/// aborted so the next resume never re-sends a half-finished exchange
/// as if it were complete.
#[test]
fn synthesize_aborted_marks_dangling_turn() {
    let dir = temp_data_dir("dangling");
    let cwd = dir.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut strand = StrandFile::create(&cwd, None).unwrap();
    strand
        .append(Record::Message {
            images: Vec::new(),
            id: ka_strand::new_record_id(),
            role: Role::User,
            content: "start something".into(),
            calls: Vec::new(),
            results: Vec::new(),
            thinking: None,
        })
        .unwrap();
    let path = strand.path().unwrap().to_path_buf();
    drop(strand);

    // reopen: the settings flag the dangling turn (fresh append-only
    // file with an unfinished exchange)
    let mut resumed = StrandFile::open(&path).unwrap();
    resumed.synthesize_aborted().unwrap();
    // idempotent: a second pass is a no-op
    assert!(!resumed.synthesize_aborted().unwrap());

    // the synthesized record is an assistant turn marked aborted
    let has_synthesized = resumed.records().iter().any(|r| match r {
        Record::Message { role, content, .. } => {
            *role == Role::Assistant && content.contains("(turn interrupted)")
        }
        _ => false,
    });
    assert!(
        has_synthesized,
        "dangling turn must synthesize an aborted assistant record: {:?}",
        resumed.records()
    );
}

/// Forks carry a parent pointer: `ka_strand::list` shows the tree edge
/// and `set_parent` records it (the /fork + /tree contract).
#[test]
fn fork_parent_pointer_survives_listing() {
    let _guard = DATA_DIR_LOCK.lock().unwrap();
    let dir = temp_data_dir("fork");
    let cwd = dir.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();

    let mut parent = StrandFile::create(&cwd, None).unwrap();
    parent
        .append(Record::Message {
            images: Vec::new(),
            id: ka_strand::new_record_id(),
            role: Role::User,
            content: "trunk conversation".into(),
            calls: Vec::new(),
            results: Vec::new(),
            thinking: None,
        })
        .unwrap();
    drop(parent);

    // fork: a fresh strand pointing at the parent (id from the listing)
    let parent_id = ka_strand::list(&cwd).unwrap()[0].id.clone();
    let mut child = StrandFile::create(&cwd, None).unwrap();
    child.set_parent(&parent_id);
    child
        .append(Record::Message {
            images: Vec::new(),
            id: ka_strand::new_record_id(),
            role: Role::User,
            content: "offshoot".into(),
            calls: Vec::new(),
            results: Vec::new(),
            thinking: None,
        })
        .unwrap();
    drop(child);
    let child_id = ka_strand::list(&cwd)
        .unwrap()
        .iter()
        .find(|s| s.parent.as_deref() == Some(parent_id.as_str()))
        .expect("child listed")
        .id
        .clone();

    let summaries = ka_strand::list(&cwd).unwrap();
    let child_summary = summaries
        .iter()
        .find(|s| s.id == child_id)
        .expect("child listed");
    assert_eq!(
        child_summary.parent.as_deref(),
        Some(parent_id.as_str()),
        "the fork edge must survive a listing"
    );
}
