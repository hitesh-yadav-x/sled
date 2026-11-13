use session_ledger::db::{Db, SessionPatch};
use session_ledger::transcript::MessageUsage;
use tempfile::TempDir;

fn open(dir: &TempDir) -> Db {
    Db::open(&dir.path().join("sub").join("ledger.db")).unwrap()
}

fn usage(key: &str, output: u64) -> MessageUsage {
    MessageUsage {
        key: key.into(),
        model: "claude-test".into(),
        input: 1,
        output,
        cache_read: 2,
        cache_write_5m: 3,
        cache_write_1h: 4,
        web_search: 5,
        service_tier: Some("standard".into()),
        speed: None,
        subagent: false,
    }
}

fn sub(mut u: MessageUsage) -> MessageUsage {
    u.subagent = true;
    u
}

fn patch(id: &str, created: i64) -> SessionPatch {
    SessionPatch {
        session_id: id.into(),
        transcript_path: format!("/nonexistent/{id}.jsonl"),
        file_created_ms: created,
        project_dir: None,
        git_branch: None,
        entrypoint: None,
        title: None,
        recap: None,
        first_prompt: Some("hello".into()),
        last_prompt: None,
        first_ts_ms: None,
        last_ts_ms: None,
        add_active_ms: 0,
        reset_activity: false,
    }
}

fn day(u: MessageUsage) -> (MessageUsage, String) {
    (u, "2026-01-02".into())
}

#[test]
fn apply_creates_session_and_messages() {
    let dir = TempDir::new().unwrap();
    let mut db = open(&dir);
    let mut p = patch("s1", 100);
    p.title = Some("T".into());
    p.recap = Some("R".into());
    p.entrypoint = Some("cli".into());
    db.apply(&p, "/f/s1.jsonl", 0, 10, &[day(usage("m1", 7)), day(usage("m2", 8))]).unwrap();
    let s = db.session("s1").unwrap().unwrap();
    assert_eq!(s.transcript_path, "/nonexistent/s1.jsonl");
    assert_eq!(s.title.as_deref(), Some("T"));
    assert_eq!(s.recap.as_deref(), Some("R"));
    assert_eq!(s.entrypoint.as_deref(), Some("cli"));
    assert!(!s.ended);
    assert_eq!(s.note, "");
    assert_eq!(db.session_count().unwrap(), 1);
    let u = db.usage(Some("s1")).unwrap();
    assert_eq!(u.len(), 2);
    assert!(u.iter().all(|(sid, _)| sid == "s1"));
    assert!(u.iter().any(|(_, m)| m.key == "m1" && m.output == 7 && m.cache_write_1h == 4));
    assert_eq!(db.usage(None).unwrap().len(), 2);
    assert!(db.session("nope").unwrap().is_none());
}

#[test]
fn same_key_same_session_last_wins() {
    let dir = TempDir::new().unwrap();
    let mut db = open(&dir);
    db.apply(&patch("s1", 100), "/f", 0, 1, &[day(usage("msg_1", 10))]).unwrap();
    db.apply(&patch("s1", 100), "/f", 1, 2, &[day(usage("msg_1", 99))]).unwrap();
    let u = db.usage(None).unwrap();
    assert_eq!(u.len(), 1);
    assert_eq!(u[0].1.output, 99);
}

#[test]
fn subagent_flag_round_trips_and_last_line_wins() {
    let dir = TempDir::new().unwrap();
    let mut db = open(&dir);
    let flags = |db: &Db| -> Vec<(String, bool)> {
        db.usage(Some("s1")).unwrap().into_iter().map(|(_, u)| (u.key, u.subagent)).collect()
    };
    db.apply(&patch("s1", 100), "/p/s1.jsonl", 0, 1, &[day(usage("m1", 1)), day(sub(usage("m2", 1)))])
        .unwrap();
    assert_eq!(flags(&db), [("m1".to_string(), false), ("m2".to_string(), true)]);
    // The same key seen later in another file takes that file's flag, both ways.
    db.apply(&patch("s1", 100), "/p/s1/subagents/a.jsonl", 0, 1, &[day(sub(usage("m1", 2)))]).unwrap();
    db.apply(&patch("s1", 100), "/p/s1.jsonl", 1, 2, &[day(usage("m2", 2))]).unwrap();
    assert_eq!(flags(&db), [("m1".to_string(), true), ("m2".to_string(), false)]);
}

#[test]
fn subagent_count_counts_subagent_files_only() {
    let dir = TempDir::new().unwrap();
    let mut db = open(&dir);
    let at = |sid: &str| {
        let mut p = patch(sid, 100);
        p.transcript_path = format!("/p/{sid}.jsonl");
        p
    };
    for (sid, path) in [
        ("s1", "/p/s1.jsonl"),
        ("s1", "/p/s1/subagents/agent-a.jsonl"),
        ("s1", "/p/s1/subagents/agent-b.jsonl"),
        ("s2", "/p/s2.jsonl"),
        ("s2", "/p/s2/subagents/agent-c.jsonl"),
        ("s3", "/p/s3.jsonl"),
    ] {
        db.apply(&at(sid), path, 0, 1, &[]).unwrap();
    }
    assert_eq!(db.subagent_count("s1").unwrap(), 2);
    assert_eq!(db.subagent_count("s2").unwrap(), 1);
    assert_eq!(db.subagent_count("s3").unwrap(), 0);
    assert_eq!(db.subagent_count("nope").unwrap(), 0);
}

#[test]
fn subagent_count_ignores_a_subagents_folder_above_the_claude_folder() {
    let dir = TempDir::new().unwrap();
    let mut db = open(&dir);
    let root = "/home/u/subagents/claude/projects/p";
    let mut p = patch("s1", 100);
    p.transcript_path = format!("{root}/s1.jsonl");
    for path in [
        format!("{root}/s1.jsonl"),
        format!("{root}/s1/subagents/agent-a.jsonl"),
        format!("{root}/s1-other/subagents/agent-x.jsonl"),
    ] {
        db.apply(&p, &path, 0, 1, &[]).unwrap();
    }
    assert_eq!(db.subagent_count("s1").unwrap(), 1);
}

#[test]
fn file_created_ms_keeps_earliest_so_ownership_does_not_move() {
    let dir = TempDir::new().unwrap();
    let mut db = open(&dir);
    let m = [day(usage("msg_x", 5))];
    db.apply(&patch("A", 100), "/fa", 0, 1, &m).unwrap();
    db.apply(&patch("B", 200), "/fb", 0, 1, &m).unwrap();
    assert_eq!(db.usage(None).unwrap()[0].0, "A");
    // A's file now reports a later creation time than B's.
    db.apply(&patch("A", 300), "/fa", 1, 2, &[]).unwrap();
    db.apply(&patch("B", 200), "/fb", 1, 2, &m).unwrap();
    let u = db.usage(None).unwrap();
    assert_eq!(u.len(), 1);
    assert_eq!(u[0].0, "A");
}

#[test]
fn token_counts_saturate_at_i64_max() {
    let dir = TempDir::new().unwrap();
    let mut db = open(&dir);
    let mut u = usage("big", u64::MAX);
    u.input = u64::MAX;
    u.cache_read = u64::MAX;
    u.cache_write_5m = u64::MAX;
    u.cache_write_1h = u64::MAX;
    u.web_search = u64::MAX;
    db.apply(&patch("s1", 1), "/f", 0, 1, &[day(u)]).unwrap();
    let got = &db.usage(None).unwrap()[0].1;
    let max = i64::MAX as u64;
    assert_eq!(
        (got.input, got.output, got.cache_read, got.cache_write_5m, got.cache_write_1h, got.web_search),
        (max, max, max, max, max, max)
    );
    let conn = rusqlite::Connection::open(dir.path().join("sub").join("ledger.db")).unwrap();
    let stored: i64 = conn.query_row("SELECT input FROM messages", [], |r| r.get(0)).unwrap();
    assert_eq!(stored, i64::MAX);
}

#[test]
fn usage_for_returns_only_the_listed_sessions_in_chunks() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    drop(db);
    let mut conn = rusqlite::Connection::open(dir.path().join("sub").join("ledger.db")).unwrap();
    let tx = conn.transaction().unwrap();
    for i in 0..1_300 {
        tx.execute(
            "INSERT INTO messages(key, session_id, day, model, input, output, cache_read,
               cache_write_5m, cache_write_1h, web_search, subagent)
             VALUES(?1, ?2, '2026-01-02', 'm', ?3, 1, 0, 0, 0, 0, 0)",
            rusqlite::params![format!("k{i:05}"), format!("s{i:05}"), i as i64],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    let db = open(&dir);
    let ids: Vec<String> = (0..1_200).map(|i| format!("s{i:05}")).collect();
    let got = db.usage_for(&ids).unwrap();
    assert_eq!(got.len(), 1_200);
    let mut seen: Vec<&str> = got.iter().map(|(sid, _)| sid.as_str()).collect();
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 1_200);
    assert!(got.iter().all(|(sid, u)| u.input as usize == sid[1..].parse::<usize>().unwrap()));
    assert!(got.iter().all(|(sid, _)| sid.as_str() < "s01200"));
    assert!(db.usage_for(&[]).unwrap().is_empty());
    assert_eq!(db.usage_for(&["s01250".to_string(), "nope".to_string()]).unwrap().len(), 1);
}

#[test]
fn first_cwd_and_first_prompt_stick_branch_and_last_prompt_follow() {
    let dir = TempDir::new().unwrap();
    let mut db = open(&dir);
    let mut a = patch("s1", 100);
    a.project_dir = Some("/p1".into());
    a.git_branch = Some("b1".into());
    a.first_prompt = Some("first".into());
    a.last_prompt = Some("last1".into());
    a.entrypoint = Some("cli".into());
    db.apply(&a, "/f", 0, 1, &[]).unwrap();
    let mut b = patch("s1", 100);
    b.project_dir = Some("/p2".into());
    b.git_branch = Some("b2".into());
    b.first_prompt = Some("second".into());
    b.last_prompt = Some("last2".into());
    b.entrypoint = Some("sdk".into());
    db.apply(&b, "/f", 1, 2, &[]).unwrap();
    // None leaves values unchanged
    let mut c = patch("s1", 100);
    c.first_prompt = None;
    db.apply(&c, "/f", 2, 3, &[]).unwrap();
    let s = db.session("s1").unwrap().unwrap();
    assert_eq!(s.project_dir.as_deref(), Some("/p1"));
    assert_eq!(s.first_prompt.as_deref(), Some("first"));
    assert_eq!(s.entrypoint.as_deref(), Some("cli"));
    assert_eq!(s.git_branch.as_deref(), Some("b2"));
    assert_eq!(s.last_prompt.as_deref(), Some("last2"));
}

#[test]
fn started_is_min_last_active_is_max_active_ms_accumulates() {
    let dir = TempDir::new().unwrap();
    let mut db = open(&dir);
    let mut a = patch("s1", 100);
    a.first_ts_ms = Some(500);
    a.last_ts_ms = Some(900);
    a.add_active_ms = 10;
    db.apply(&a, "/f", 0, 1, &[]).unwrap();
    let mut b = patch("s1", 100);
    b.first_ts_ms = Some(300);
    b.last_ts_ms = Some(700);
    b.add_active_ms = 5;
    db.apply(&b, "/f", 1, 2, &[]).unwrap();
    let s = db.session("s1").unwrap().unwrap();
    assert_eq!(s.started_at, Some(300));
    assert_eq!(s.last_active_at, Some(900));
    assert_eq!(s.active_ms, 15);
    assert_eq!(db.last_active("s1").unwrap(), Some(900));
    assert_eq!(db.last_active("nope").unwrap(), None);
}

#[test]
fn reset_activity_zeroes_active_ms() {
    let dir = TempDir::new().unwrap();
    let mut db = open(&dir);
    let mut a = patch("s1", 100);
    a.first_ts_ms = Some(500);
    a.last_ts_ms = Some(900);
    a.add_active_ms = 50;
    db.apply(&a, "/f", 0, 1, &[]).unwrap();
    let mut b = patch("s1", 100);
    b.first_ts_ms = Some(500);
    b.last_ts_ms = Some(600);
    b.add_active_ms = 7;
    b.reset_activity = true;
    db.apply(&b, "/f", 1, 2, &[]).unwrap();
    let s = db.session("s1").unwrap().unwrap();
    assert_eq!(s.active_ms, 7);
    assert_eq!(s.last_active_at, Some(600));
}

#[test]
fn duplicate_key_belongs_to_older_file_regardless_of_order() {
    for a_first in [true, false] {
        let dir = TempDir::new().unwrap();
        let mut db = open(&dir);
        let pa = patch("A", 100);
        let pb = patch("B", 200);
        let m = [day(usage("msg_x", 5))];
        if a_first {
            db.apply(&pa, "/fa", 0, 1, &m).unwrap();
            db.apply(&pb, "/fb", 0, 1, &m).unwrap();
        } else {
            db.apply(&pb, "/fb", 0, 1, &m).unwrap();
            db.apply(&pa, "/fa", 0, 1, &m).unwrap();
        }
        let u = db.usage(None).unwrap();
        assert_eq!(u.len(), 1, "a_first={a_first}");
        assert_eq!(u[0].0, "A", "a_first={a_first}");
        assert_eq!(db.session("B").unwrap().unwrap().continued_from.as_deref(), Some("A"));
        assert_eq!(db.session("A").unwrap().unwrap().continued_from, None);
        assert_eq!(db.usage(Some("B")).unwrap().len(), 0);
    }
}

#[test]
fn equal_creation_time_breaks_tie_by_session_id() {
    for a_first in [true, false] {
        let dir = TempDir::new().unwrap();
        let mut db = open(&dir);
        let m = [day(usage("msg_x", 5))];
        if a_first {
            db.apply(&patch("A", 100), "/fa", 0, 1, &m).unwrap();
            db.apply(&patch("B", 100), "/fb", 0, 1, &m).unwrap();
        } else {
            db.apply(&patch("B", 100), "/fb", 0, 1, &m).unwrap();
            db.apply(&patch("A", 100), "/fa", 0, 1, &m).unwrap();
        }
        assert_eq!(db.usage(None).unwrap()[0].0, "A");
    }
}

#[test]
fn note_survives_reapply() {
    let dir = TempDir::new().unwrap();
    let mut db = open(&dir);
    db.apply(&patch("s1", 100), "/f", 0, 1, &[]).unwrap();
    assert!(db.set_note("s1", "keep me").unwrap());
    let mut p = patch("s1", 100);
    p.reset_activity = true;
    db.apply(&p, "/f", 1, 0, &[day(usage("m", 1))]).unwrap();
    assert_eq!(db.session("s1").unwrap().unwrap().note, "keep me");
    assert_eq!(db.sessions(None, true).unwrap()[0].note, "keep me");
}

#[test]
fn set_note_unknown_session_is_false() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    assert!(!db.set_note("ghost", "x").unwrap());
}

#[test]
fn automated_hidden_by_default() {
    let dir = TempDir::new().unwrap();
    let mut db = open(&dir);
    let mut human = patch("human", 1);
    human.entrypoint = Some("cli".into());
    let mut sdk = patch("sdk", 2);
    sdk.entrypoint = Some("sdk-cli".into());
    let mut none = patch("noprompt", 3);
    none.first_prompt = None;
    for p in [&human, &sdk, &none] {
        db.apply(p, &format!("/f/{}", p.session_id), 0, 1, &[]).unwrap();
    }
    let ids = |all| -> Vec<String> {
        db.sessions(None, all).unwrap().into_iter().map(|s| s.session_id).collect()
    };
    assert_eq!(ids(false), vec!["human"]);
    let mut all = ids(true);
    all.sort();
    assert_eq!(all, vec!["human", "noprompt", "sdk"]);
}

#[test]
fn since_filters_on_last_active() {
    let dir = TempDir::new().unwrap();
    let mut db = open(&dir);
    for (id, ts) in [("old", 100), ("mid", 200), ("new", 300)] {
        let mut p = patch(id, 1);
        p.last_ts_ms = Some(ts);
        db.apply(&p, &format!("/f/{id}"), 0, 1, &[]).unwrap();
    }
    let ids = |since| -> Vec<String> {
        db.sessions(since, false).unwrap().into_iter().map(|s| s.session_id).collect()
    };
    assert_eq!(ids(None), vec!["new", "mid", "old"]);
    assert_eq!(ids(Some(200)), vec!["new", "mid"]);
    assert_eq!(ids(Some(301)), Vec::<String>::new());
}

#[test]
fn refresh_missing_marks_but_keeps_row_and_usage() {
    let dir = TempDir::new().unwrap();
    let mut db = open(&dir);
    let real = dir.path().join("real.jsonl");
    std::fs::write(&real, "x").unwrap();
    let mut present = patch("present", 1);
    present.transcript_path = real.to_string_lossy().into_owned();
    db.apply(&present, "/f1", 0, 1, &[day(usage("m1", 1))]).unwrap();
    db.apply(&patch("gone", 2), "/f2", 0, 1, &[day(usage("m2", 2))]).unwrap();
    // Only "gone" changes: it starts unflagged.
    assert_eq!(db.refresh_missing().unwrap(), 1);
    assert!(!db.session("present").unwrap().unwrap().transcript_missing);
    assert!(db.session("gone").unwrap().unwrap().transcript_missing);
    assert_eq!(db.session_count().unwrap(), 2);
    assert_eq!(db.usage(None).unwrap().len(), 2);
    // Nothing changed on disk, so nothing is updated.
    assert_eq!(db.refresh_missing().unwrap(), 0);
    // The present file is deleted: it is flagged, and still kept.
    std::fs::remove_file(&real).unwrap();
    assert_eq!(db.refresh_missing().unwrap(), 1);
    assert!(db.session("present").unwrap().unwrap().transcript_missing);
    assert_eq!(db.session_count().unwrap(), 2);
    // The file reappears: the flag clears.
    std::fs::write(&real, "x").unwrap();
    assert_eq!(db.refresh_missing().unwrap(), 1);
    assert!(!db.session("present").unwrap().unwrap().transcript_missing);
    assert!(db.session("gone").unwrap().unwrap().transcript_missing);
}

#[test]
fn set_ended_round_trips() {
    let dir = TempDir::new().unwrap();
    let mut db = open(&dir);
    db.apply(&patch("s1", 1), "/f", 0, 1, &[]).unwrap();
    db.set_ended("s1", true).unwrap();
    assert!(db.session("s1").unwrap().unwrap().ended);
    db.set_ended("s1", false).unwrap();
    assert!(!db.session("s1").unwrap().unwrap().ended);
}

#[test]
fn file_state_round_trips_offset() {
    let dir = TempDir::new().unwrap();
    let mut db = open(&dir);
    assert!(db.file_state("/f").unwrap().is_none());
    db.apply(&patch("s1", 1), "/f", 0, 1234, &[]).unwrap();
    assert_eq!(db.file_state("/f").unwrap().unwrap().offset, 1234);
    db.apply(&patch("s1", 1), "/f", 1234, 99, &[]).unwrap();
    assert_eq!(db.file_state("/f").unwrap().unwrap().offset, 99);
}

#[test]
fn apply_with_stale_expected_offset_changes_nothing() {
    let dir = TempDir::new().unwrap();
    let mut db = open(&dir);
    let mut a = patch("s1", 100);
    a.first_ts_ms = Some(500);
    a.last_ts_ms = Some(900);
    a.add_active_ms = 10;
    db.apply(&a, "/f", 0, 50, &[day(usage("m1", 1))]).unwrap();
    let row = db.session("s1").unwrap().unwrap();

    // A second writer that also started from offset 0: its bytes were already applied.
    let mut b = patch("s1", 100);
    b.title = Some("late".into());
    b.last_ts_ms = Some(2000);
    b.add_active_ms = 10;
    db.apply(&b, "/f", 0, 50, &[day(usage("m2", 2))]).unwrap();
    assert_eq!(db.session("s1").unwrap().unwrap(), row);
    assert_eq!(db.usage(None).unwrap().len(), 1);
    assert_eq!(db.file_state("/f").unwrap().unwrap().offset, 50);

    // Expecting a stored offset for a file with no row is stale too.
    db.apply(&patch("s2", 100), "/g", 7, 9, &[]).unwrap();
    assert!(db.session("s2").unwrap().is_none());
    assert!(db.file_state("/g").unwrap().is_none());

    // The matching offset applies.
    db.apply(&b, "/f", 50, 80, &[day(usage("m2", 2))]).unwrap();
    let s = db.session("s1").unwrap().unwrap();
    assert_eq!((s.title.as_deref(), s.active_ms, s.last_active_at), (Some("late"), 20, Some(2000)));
    assert_eq!(db.file_state("/f").unwrap().unwrap().offset, 80);
}

#[test]
fn open_existing_db_is_fast_while_a_writer_holds_the_lock() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ledger.db");
    drop(Db::open(&path).unwrap());
    let raw = rusqlite::Connection::open(&path).unwrap();
    raw.execute_batch("BEGIN IMMEDIATE").unwrap();
    let t = std::time::Instant::now();
    let r = Db::open(&path);
    assert!(r.is_ok(), "{:?}", r.err());
    assert!(t.elapsed() < std::time::Duration::from_millis(500), "took {:?}", t.elapsed());
    raw.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn open_does_not_wait_10s_when_setup_is_blocked() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ledger.db");
    let raw = rusqlite::Connection::open(&path).unwrap();
    raw.execute_batch("BEGIN IMMEDIATE; CREATE TABLE blocker(x);").unwrap();
    let t = std::time::Instant::now();
    let _ = Db::open(&path);
    assert!(t.elapsed() < std::time::Duration::from_secs(4), "took {:?}", t.elapsed());
    raw.execute_batch("ROLLBACK").unwrap();
}
