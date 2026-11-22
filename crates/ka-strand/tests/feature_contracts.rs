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

/// JSONL export: the render re-parses through the same reader and
/// replays the same strand — same records, same tip — so exported
/// output is a strand file again (the `ka export --jsonl` contract).
#[test]
fn render_jsonl_reparses_to_the_same_records_and_tip() {
    let records = vec![
        Record::Header {
            id: ka_strand::new_strand_id(),
            ts: "2026-11-22T00:00:00Z".into(),
            cwd: "/tmp/proj".into(),
            version: 1,
            repo: None,
            parent: Some("s-origin".into()),
        },
        Record::Message {
            id: ka_strand::new_record_id(),
            images: Vec::new(),
            role: Role::User,
            content: "export me".into(),
            calls: Vec::new(),
            results: Vec::new(),
            thinking: None,
        },
        Record::Message {
            id: ka_strand::new_record_id(),
            images: Vec::new(),
            role: Role::Assistant,
            content: "rendered back".into(),
            calls: Vec::new(),
            results: Vec::new(),
            thinking: None,
        },
        Record::Change {
            id: ka_strand::new_record_id(),
            model: Some("ka-model-2".into()),
            effort: None,
            mode: None,
            features: None,
            sandbox: None,
        },
        Record::Title {
            id: ka_strand::new_record_id(),
            title: "export me (fork)".into(),
        },
    ];
    let rendered = ka_strand::render_jsonl(&records).unwrap();
    // one line per record, header fork carried
    assert_eq!(rendered.lines().count(), records.len());
    assert!(rendered.contains("\"parent\":\"s-origin\""), "{rendered}");

    let path = std::env::temp_dir().join(format!("ka-strand-jsonl-{}.jsonl", std::process::id()));
    std::fs::write(&path, &rendered).unwrap();
    let reparsed = ka_strand::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(reparsed, records);
    assert_eq!(reparsed.last(), records.last());
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

/// `[history] persistence = "none"` keeps the strand in memory only
/// (the file never materializes); `max_bytes` stops persisting once the
/// on-disk ceiling is reached while the session continues in memory.
#[test]
fn history_persistence_and_ceiling_honor_the_policy() {
    let dir = temp_data_dir("history-policy");
    let cwd = dir.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();

    // persistence off: nothing ever lands on disk
    let mut strand = StrandFile::create(&cwd, None).unwrap();
    strand.set_persist(false);
    strand
        .append(Record::Message {
            images: Vec::new(),
            id: ka_strand::new_record_id(),
            role: Role::User,
            content: "unpersisted".into(),
            calls: Vec::new(),
            results: Vec::new(),
            thinking: None,
        })
        .unwrap();
    assert!(
        strand.path().is_none(),
        "a memory-only strand must not materialize"
    );
    assert_eq!(strand.records().len(), 2, "records stay in memory");

    // ceiling: the first append materializes, the next one is skipped
    let mut capped = StrandFile::create(&cwd, None).unwrap();
    capped.set_max_bytes(Some(1));
    capped
        .append(Record::Message {
            images: Vec::new(),
            id: ka_strand::new_record_id(),
            role: Role::User,
            content: "fits".into(),
            calls: Vec::new(),
            results: Vec::new(),
            thinking: None,
        })
        .unwrap();
    let path = capped
        .path()
        .expect("first append materializes")
        .to_path_buf();
    capped
        .append(Record::Message {
            images: Vec::new(),
            id: ka_strand::new_record_id(),
            role: Role::User,
            content: "over the ceiling".into(),
            calls: Vec::new(),
            results: Vec::new(),
            thinking: None,
        })
        .unwrap();
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(on_disk.contains("fits"), "{on_disk:?}");
    assert!(
        !on_disk.contains("over the ceiling"),
        "ceiling reached: further records must not persist: {on_disk:?}"
    );
    assert_eq!(capped.records().len(), 3, "the session continues in memory");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The header strand id of a materialized strand (test helper).
fn header_id(strand: &StrandFile) -> String {
    match strand.records().first() {
        Some(Record::Header { id, .. }) => id.0.clone(),
        _ => unreachable!("every strand starts with a header"),
    }
}

/// A user message record (test helper).
fn user_msg(content: &str) -> Record {
    Record::Message {
        images: Vec::new(),
        id: ka_strand::new_record_id(),
        role: Role::User,
        content: content.into(),
        calls: Vec::new(),
        results: Vec::new(),
        thinking: None,
    }
}

/// Retitle appends a Title record: the next read/listing reports the
/// new display title while the memorable Name alias stays intact (the
/// `ka session rename` contract).
#[test]
fn retitle_updates_title_and_keeps_name_alias() {
    let _guard = DATA_DIR_LOCK.lock().unwrap();
    let dir = temp_data_dir("retitle");
    let cwd = dir.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut strand = StrandFile::create(&cwd, None).unwrap();
    strand.append(user_msg("original topic")).unwrap();
    strand
        .append(Record::Name {
            id: ka_strand::new_record_id(),
            name: "my-session".into(),
        })
        .unwrap();
    let path = strand.path().unwrap().to_path_buf();
    let id = header_id(&strand);
    drop(strand);

    ka_strand::retitle(&path, "renamed topic").unwrap();

    // re-legible: replay reports the appended title
    let reopened = StrandFile::open(&path).unwrap();
    assert_eq!(
        ka_strand::title_of(reopened.records()),
        "renamed topic",
        "the appended Title must become the effective title"
    );
    // listing agrees, and the alias survives
    let summary = ka_strand::list(&cwd)
        .unwrap()
        .into_iter()
        .find(|s| s.id == id)
        .expect("retitled strand listed");
    assert_eq!(summary.title, "renamed topic");
    assert_eq!(
        summary.name.as_deref(),
        Some("my-session"),
        "the Name alias must survive a retitle"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Cascade delete removes the strand and every descendant whose parent
/// chain reaches it — counting all removed files — while strands
/// without an edge into the doomed set survive, even forked ones (the
/// `ka session delete` contract).
#[test]
fn delete_cascade_removes_tree_leaves_unrelated() {
    let _guard = DATA_DIR_LOCK.lock().unwrap();
    let dir = temp_data_dir("delete-cascade");
    let cwd = dir.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();

    let mut parent = StrandFile::create(&cwd, None).unwrap();
    parent.append(user_msg("trunk")).unwrap();
    let parent_path = parent.path().unwrap().to_path_buf();
    let parent_id = header_id(&parent);
    drop(parent);

    let mut child = StrandFile::create(&cwd, None).unwrap();
    child.set_parent(&parent_id);
    child.append(user_msg("offshoot")).unwrap();
    let child_path = child.path().unwrap().to_path_buf();
    let child_id = header_id(&child);
    drop(child);

    let mut grandchild = StrandFile::create(&cwd, None).unwrap();
    grandchild.set_parent(&child_id);
    grandchild.append(user_msg("off-offshoot")).unwrap();
    let grandchild_path = grandchild.path().unwrap().to_path_buf();
    drop(grandchild);

    // an unrelated trunk with its own fork: a parent pointer alone must
    // not doom a strand — only an edge INTO the doomed set does
    let mut unrelated = StrandFile::create(&cwd, None).unwrap();
    unrelated.append(user_msg("unrelated trunk")).unwrap();
    let unrelated_path = unrelated.path().unwrap().to_path_buf();
    let unrelated_id = header_id(&unrelated);
    drop(unrelated);

    let mut unrelated_child = StrandFile::create(&cwd, None).unwrap();
    unrelated_child.set_parent(&unrelated_id);
    unrelated_child.append(user_msg("unrelated fork")).unwrap();
    let unrelated_child_path = unrelated_child.path().unwrap().to_path_buf();
    drop(unrelated_child);

    let removed = ka_strand::delete_cascade(&parent_path).unwrap();
    assert_eq!(removed, 3, "parent → child → grandchild = 3 files");
    for gone in [&parent_path, &child_path, &grandchild_path] {
        assert!(!gone.exists(), "{gone:?} must be deleted");
    }
    for kept in [&unrelated_path, &unrelated_child_path] {
        assert!(kept.exists(), "{kept:?} must survive the cascade");
    }
    let survivors = ka_strand::list(&cwd).unwrap();
    assert_eq!(survivors.len(), 2, "{survivors:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Archive files a strand out of the live tree — `list` and `latest` no
/// longer see it, `list_archived` does — and unarchive restores it to
/// its original store path (the `ka session archive` contract).
#[test]
fn archive_hides_from_list_and_unarchive_restores() {
    let _guard = DATA_DIR_LOCK.lock().unwrap();
    let dir = temp_data_dir("archive");
    let cwd = dir.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut strand = StrandFile::create(&cwd, None).unwrap();
    strand.append(user_msg("archivable session")).unwrap();
    let path = strand.path().unwrap().to_path_buf();
    let id = header_id(&strand);
    drop(strand);

    assert_eq!(ka_strand::list(&cwd).unwrap().len(), 1);
    assert!(ka_strand::list_archived(&cwd).is_empty());

    let archived_path = ka_strand::archive(&path, true).unwrap();
    assert!(!path.exists(), "the live file must move away");
    assert!(archived_path.exists());
    assert_ne!(archived_path, path);
    // same cwd encoding, parallel tree
    assert!(
        archived_path.starts_with(dir.join("strands-archived")),
        "{archived_path:?} must live in the archive tree"
    );
    // the live listing goes blind to it
    assert!(ka_strand::list(&cwd).unwrap().is_empty());
    assert!(ka_strand::latest(&cwd).unwrap().is_none());
    let archived = ka_strand::list_archived(&cwd);
    assert_eq!(archived.len(), 1, "{archived:?}");
    assert_eq!(archived[0].id, id);
    assert_eq!(archived[0].title, "archivable session");

    // unarchive restores the original store path and the live listing
    let restored = ka_strand::archive(&archived_path, false).unwrap();
    assert_eq!(
        restored, path,
        "unarchive must return the strand to its original store path"
    );
    assert!(restored.exists());
    assert!(!archived_path.exists());
    assert_eq!(ka_strand::list(&cwd).unwrap().len(), 1);
    assert!(ka_strand::list_archived(&cwd).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Session management refuses non-strands: missing files, files without
/// a strand header, and cascade/archive/retitle all error instead of
/// touching anything (the CLI's session-file guard rails).
#[test]
fn session_management_refuses_missing_or_non_strand_paths() {
    let _guard = DATA_DIR_LOCK.lock().unwrap();
    let dir = temp_data_dir("refuse");
    let cwd = dir.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();

    let missing = cwd.join("nope.jsonl");
    assert!(ka_strand::delete_cascade(&missing).is_err());
    assert!(ka_strand::retitle(&missing, "x").is_err());
    assert!(ka_strand::archive(&missing, true).is_err());

    // a .jsonl without a strand header record is not a session
    let junk = cwd.join("junk.jsonl");
    std::fs::write(
        &junk,
        "{\"record\":\"message\",\"id\":\"r1\",\"role\":\"user\",\"content\":\"hi\"}\n",
    )
    .unwrap();
    assert!(ka_strand::delete_cascade(&junk).is_err());
    assert!(ka_strand::retitle(&junk, "x").is_err());
    assert!(ka_strand::archive(&junk, true).is_err());
    assert!(junk.exists(), "refused operations must not touch the file");
    let _ = std::fs::remove_dir_all(&dir);
}
