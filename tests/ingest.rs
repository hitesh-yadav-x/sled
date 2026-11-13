mod common;
use common::*;
use session_ledger::db::Db;
use session_ledger::ingest::ingest_session;

fn ms(ts: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(ts).unwrap().timestamp_millis()
}

fn day(ts: &str) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_millis_opt(ms(ts)).unwrap().format("%Y-%m-%d").to_string()
}

fn open(env: &Env) -> Db {
    Db::open(&env.data.join("ledger.db")).unwrap()
}

fn base_lines(sid: &str) -> Vec<String> {
    vec![
        title_line(sid, "My title"),
        prompt_line(sid, "2026-01-01T10:00:00.000Z", "/work/proj", "main", "first prompt"),
        assistant_line(sid, "2026-01-01T10:01:00.000Z", "msg_1", "req_1", "claude-x", usage(100, 50, 10, 20, 30)),
        prompt_line(sid, "2026-01-01T10:03:00.000Z", "/work/proj", "dev", "second prompt"),
        assistant_line(sid, "2026-01-01T10:04:00.000Z", "msg_2", "req_2", "claude-x", usage(1, 2, 3, 4, 5)),
        recap_line(sid, "2026-01-01T10:05:00.000Z", "the recap"),
    ]
}

#[test]
fn ingests_full_session() {
    let env = Env::new();
    let p = env.write_session("proj", "s1", &base_lines("s1"));
    let mut db = open(&env);
    ingest_session(&mut db, &p).unwrap();

    let s = db.session("s1").unwrap().unwrap();
    assert_eq!(s.transcript_path, std::fs::canonicalize(&p).unwrap().to_string_lossy());
    assert_eq!(s.project_dir.as_deref(), Some("/work/proj"));
    assert_eq!(s.git_branch.as_deref(), Some("dev"));
    assert_eq!(s.entrypoint.as_deref(), Some("cli"));
    assert_eq!(s.title.as_deref(), Some("My title"));
    assert_eq!(s.recap.as_deref(), Some("the recap"));
    assert_eq!(s.first_prompt.as_deref(), Some("first prompt"));
    assert_eq!(s.last_prompt.as_deref(), Some("second prompt"));
    assert_eq!(s.started_at, Some(ms("2026-01-01T10:00:00.000Z")));
    assert_eq!(s.last_active_at, Some(ms("2026-01-01T10:05:00.000Z")));
    assert_eq!(s.active_ms, 300_000);

    let u = db.usage(Some("s1")).unwrap();
    assert_eq!(u.len(), 2);
    let (sid, m1) = &u[0];
    assert_eq!(sid, "s1");
    assert_eq!(m1.key, "msg_1|req_1");
    assert_eq!(m1.model, "claude-x");
    assert_eq!((m1.input, m1.output, m1.cache_read), (100, 50, 10));
    assert_eq!((m1.cache_write_5m, m1.cache_write_1h), (20, 30));
    assert_eq!(u[1].1.key, "msg_2|req_2");
    assert_eq!((u[1].1.input, u[1].1.output), (1, 2));
}

#[test]
fn messages_carry_local_day_of_their_line() {
    let env = Env::new();
    let lines = vec![
        assistant_line("s1", "2026-01-01T10:01:00.000Z", "msg_1", "r", "m", usage(1, 1, 0, 0, 0)),
        assistant_line("s1", "2026-03-05T10:01:00.000Z", "msg_2", "r", "m", usage(1, 1, 0, 0, 0)),
    ];
    let p = env.write_session("proj", "s1", &lines);
    let mut db = open(&env);
    ingest_session(&mut db, &p).unwrap();
    // The db API does not expose `day`; check through a second connection.
    let conn = rusqlite::Connection::open(env.data.join("ledger.db")).unwrap();
    let d: String = conn
        .query_row("SELECT day FROM messages WHERE key = 'msg_1|r'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(d, day("2026-01-01T10:01:00.000Z"));
    let d: String = conn
        .query_row("SELECT day FROM messages WHERE key = 'msg_2|r'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(d, day("2026-03-05T10:01:00.000Z"));
}

#[test]
fn repeated_lines_count_once() {
    let env = Env::new();
    let a = assistant_line("s1", "2026-01-01T10:00:00.000Z", "msg_1", "req_1", "m", usage(5, 5, 0, 0, 0));
    let b = assistant_line("s1", "2026-01-01T10:00:01.000Z", "msg_2", "req_2", "m", usage(7, 7, 0, 0, 0));
    let p = env.write_session("proj", "s1", &[a.clone(), a.clone(), a, b]);
    let mut db = open(&env);
    ingest_session(&mut db, &p).unwrap();
    assert_eq!(db.usage(None).unwrap().len(), 2);
}

#[test]
fn second_run_reads_only_new_lines() {
    let env = Env::new();
    let p = env.write_session("proj", "s1", &base_lines("s1"));
    let mut db = open(&env);
    ingest_session(&mut db, &p).unwrap();
    let before = db.session("s1").unwrap().unwrap();
    let usage_before = db.usage(None).unwrap();

    Env::append(
        &p,
        &format!(
            "{}\n{}\n",
            prompt_line("s1", "2026-01-01T10:06:00.000Z", "/work/proj", "dev", "third prompt"),
            assistant_line("s1", "2026-01-01T10:07:00.000Z", "msg_3", "req_3", "claude-x", usage(9, 9, 0, 0, 0)),
        ),
    );
    ingest_session(&mut db, &p).unwrap();

    let after = db.session("s1").unwrap().unwrap();
    assert_eq!(after.last_prompt.as_deref(), Some("third prompt"));
    assert_eq!(after.first_prompt, before.first_prompt);
    assert_eq!(after.title, before.title);
    assert_eq!(after.started_at, before.started_at);
    assert_eq!(after.last_active_at, Some(ms("2026-01-01T10:07:00.000Z")));
    // 10:05 -> 10:06 and 10:06 -> 10:07 add two minutes.
    assert_eq!(after.active_ms, before.active_ms + 120_000);
    let usage_after = db.usage(None).unwrap();
    assert_eq!(usage_after.len(), usage_before.len() + 1);
    assert_eq!(&usage_after[..2], &usage_before[..]);
}

#[test]
fn rerun_without_change_is_a_no_op() {
    let env = Env::new();
    let p = env.write_session("proj", "s1", &base_lines("s1"));
    let mut db = open(&env);
    ingest_session(&mut db, &p).unwrap();
    let row = db.session("s1").unwrap().unwrap();
    let usage1 = db.usage(None).unwrap();
    ingest_session(&mut db, &p).unwrap();
    assert_eq!(db.session("s1").unwrap().unwrap(), row);
    assert_eq!(db.usage(None).unwrap(), usage1);
    assert_eq!(db.session("s1").unwrap().unwrap().active_ms, 300_000);
}

#[test]
fn partial_last_line_waits_for_next_run() {
    let env = Env::new();
    let l1 = assistant_line("s1", "2026-01-01T10:00:00.000Z", "msg_1", "r1", "m", usage(1, 1, 0, 0, 0));
    let l2 = assistant_line("s1", "2026-01-01T10:01:00.000Z", "msg_2", "r2", "m", usage(2, 2, 0, 0, 0));
    let l3 = assistant_line("s1", "2026-01-01T10:02:00.000Z", "msg_3", "r3", "m", usage(3, 3, 0, 0, 0));
    let p = env.write_session("proj", "s1", &[l1, l2]);
    let (h1, h2) = l3.split_at(l3.len() / 2);
    Env::append(&p, h1);
    let mut db = open(&env);
    ingest_session(&mut db, &p).unwrap();
    assert_eq!(db.usage(None).unwrap().len(), 2);
    assert_eq!(db.session("s1").unwrap().unwrap().active_ms, 60_000);

    Env::append(&p, &format!("{h2}\n"));
    ingest_session(&mut db, &p).unwrap();
    assert_eq!(db.usage(None).unwrap().len(), 3);
    assert_eq!(db.session("s1").unwrap().unwrap().active_ms, 120_000);
    ingest_session(&mut db, &p).unwrap();
    assert_eq!(db.usage(None).unwrap().len(), 3);
}

#[test]
fn truncated_file_restarts_cleanly() {
    let env = Env::new();
    let p = env.write_session("proj", "s1", &base_lines("s1"));
    let mut db = open(&env);
    ingest_session(&mut db, &p).unwrap();

    let short = vec![
        prompt_line("s1", "2026-01-01T11:00:00.000Z", "/work/proj", "main", "p"),
        assistant_line("s1", "2026-01-01T11:02:00.000Z", "msg_9", "r9", "m", usage(1, 1, 0, 0, 0)),
    ];
    env.write_session("proj", "s1", &short);
    ingest_session(&mut db, &p).unwrap();

    let fresh_env = Env::new();
    let fp = fresh_env.write_session("proj", "s1", &short);
    let mut fresh = open(&fresh_env);
    ingest_session(&mut fresh, &fp).unwrap();

    let got = db.session("s1").unwrap().unwrap();
    assert_eq!(got.active_ms, 120_000);
    assert_eq!(got.active_ms, fresh.session("s1").unwrap().unwrap().active_ms);
    assert_eq!(got.last_active_at, Some(ms("2026-01-01T11:02:00.000Z")));
}

#[test]
fn active_time_skips_long_gaps() {
    let env = Env::new();
    let mk = |ts: &str, n: &str| assistant_line("s1", ts, n, "r", "m", usage(1, 1, 0, 0, 0));
    let p = env.write_session(
        "proj",
        "s1",
        &[
            mk("2026-01-01T10:00:00.000Z", "a"),
            mk("2026-01-01T10:05:00.000Z", "b"),
            mk("2026-01-01T11:05:00.000Z", "c"),
            mk("2026-01-01T11:10:00.000Z", "d"),
        ],
    );
    let mut db = open(&env);
    ingest_session(&mut db, &p).unwrap();
    assert_eq!(db.session("s1").unwrap().unwrap().active_ms, 600_000);
}

#[test]
fn subagent_usage_rolls_up_to_parent() {
    let env = Env::new();
    let p = env.write_session("proj", "s1", &base_lines("s1"));
    env.write_subagent(
        "proj",
        "s1",
        "agent-a",
        &[
            prompt_line("s1", "2026-01-01T12:00:00.000Z", "/other", "x", "sub prompt"),
            assistant_line("s1", "2026-01-01T12:01:00.000Z", "msg_sub", "req_sub", "m", usage(40, 4, 0, 0, 0)),
        ],
    );
    let mut db = open(&env);
    ingest_session(&mut db, &p).unwrap();

    assert_eq!(db.session_count().unwrap(), 1);
    let u = db.usage(Some("s1")).unwrap();
    assert_eq!(u.len(), 3);
    assert!(u.iter().any(|(sid, m)| sid == "s1" && m.key == "msg_sub|req_sub" && m.input == 40));
    let s = db.session("s1").unwrap().unwrap();
    assert_eq!(s.last_prompt.as_deref(), Some("second prompt"));
    assert_eq!(s.project_dir.as_deref(), Some("/work/proj"));
    assert_eq!(s.last_active_at, Some(ms("2026-01-01T10:05:00.000Z")));
    assert_eq!(s.active_ms, 300_000);
    // Re-run: sub-agent file is also incremental.
    ingest_session(&mut db, &p).unwrap();
    assert_eq!(db.usage(Some("s1")).unwrap().len(), 3);
}

#[test]
fn messages_from_subagent_files_are_flagged() {
    let env = Env::new();
    let p = env.write_session("proj", "s1", &base_lines("s1"));
    // The sub-agent line carries no isSidechain flag; its folder alone marks it.
    env.write_subagent(
        "proj",
        "s1",
        "agent-a",
        &[assistant_line("s1", "2026-01-01T12:01:00.000Z", "msg_sub", "req_sub", "m", usage(40, 4, 0, 0, 0))],
    );
    let mut db = open(&env);
    ingest_session(&mut db, &p).unwrap();
    let flags: Vec<(String, bool)> =
        db.usage(Some("s1")).unwrap().into_iter().map(|(_, u)| (u.key, u.subagent)).collect();
    assert_eq!(
        flags,
        [
            ("msg_1|req_1".to_string(), false),
            ("msg_2|req_2".to_string(), false),
            ("msg_sub|req_sub".to_string(), true),
        ]
    );
    assert_eq!(db.subagent_count("s1").unwrap(), 1);
}

#[test]
fn malformed_lines_are_skipped() {
    let env = Env::new();
    let a = assistant_line("s1", "2026-01-01T10:00:00.000Z", "msg_1", "r1", "m", usage(1, 1, 0, 0, 0));
    let b = assistant_line("s1", "2026-01-01T10:01:00.000Z", "msg_2", "r2", "m", usage(2, 2, 0, 0, 0));
    let p = env.write_session("proj", "s1", &[a, "garbage".to_string(), b]);
    Env::append(&p, "\u{0}\n");
    // A line that is not valid UTF-8.
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(&[0xff, 0xfe, b'\n']).unwrap();
    }
    let mut db = open(&env);
    ingest_session(&mut db, &p).unwrap();
    assert_eq!(db.usage(None).unwrap().len(), 2);
}

#[test]
fn missing_file_is_ok() {
    let env = Env::new();
    let mut db = open(&env);
    let p = env.claude.join("projects/proj/nope.jsonl");
    ingest_session(&mut db, &p).unwrap();
    assert_eq!(db.session_count().unwrap(), 0);
}

#[test]
fn empty_prompt_does_not_overwrite_last_prompt() {
    let env = Env::new();
    let lines = vec![
        prompt_line("s1", "2026-01-01T10:00:00.000Z", "/w", "main", "real prompt"),
        prompt_line("s1", "2026-01-01T10:01:00.000Z", "/w", "main", "   "),
    ];
    let p = env.write_session("proj", "s1", &lines);
    let mut db = open(&env);
    ingest_session(&mut db, &p).unwrap();
    let s = db.session("s1").unwrap().unwrap();
    assert_eq!(s.first_prompt.as_deref(), Some("real prompt"));
    assert_eq!(s.last_prompt.as_deref(), Some("real prompt"));

    // A session whose only prompt is empty gets no first_prompt.
    let p2 = env.write_session("proj", "s2", &[prompt_line("s2", "2026-01-01T10:00:00.000Z", "/w", "main", "")]);
    ingest_session(&mut db, &p2).unwrap();
    let s2 = db.session("s2").unwrap().unwrap();
    assert_eq!(s2.first_prompt, None);
    assert_eq!(s2.last_prompt, None);
}

#[cfg(unix)]
#[test]
fn same_file_via_two_path_spellings_counts_once() {
    let env = Env::new();
    let p = env.write_session("proj", "s1", &base_lines("s1"));
    env.write_subagent(
        "proj",
        "s1",
        "agent-a",
        &[assistant_line("s1", "2026-01-01T12:01:00.000Z", "msg_sub", "req_sub", "m", usage(40, 4, 0, 0, 0))],
    );
    let link = env.tmp.path().join("linked-projects");
    std::os::unix::fs::symlink(env.claude.join("projects"), &link).unwrap();
    let via_link = link.join("proj").join("s1.jsonl");

    let mut db = open(&env);
    ingest_session(&mut db, &p).unwrap();
    ingest_session(&mut db, &via_link).unwrap();

    let fresh_env = Env::new();
    let fp = fresh_env.write_session("proj", "s1", &base_lines("s1"));
    let mut fresh = open(&fresh_env);
    ingest_session(&mut fresh, &fp).unwrap();

    let s = db.session("s1").unwrap().unwrap();
    assert_eq!(s.active_ms, 300_000);
    assert_eq!(s.active_ms, fresh.session("s1").unwrap().unwrap().active_ms);
    assert_eq!(db.usage(Some("s1")).unwrap().len(), 3);
    let canonical = std::fs::canonicalize(&p).unwrap();
    assert_eq!(s.transcript_path, canonical.to_string_lossy());

    // One files row per real file: the main transcript and one sub-agent.
    let conn = rusqlite::Connection::open(env.data.join("ledger.db")).unwrap();
    let paths: Vec<String> = conn
        .prepare("SELECT path FROM files ORDER BY path")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let canonical_sub = std::fs::canonicalize(env.claude.join("projects/proj/s1/subagents/agent-a.jsonl")).unwrap();
    assert_eq!(paths, vec![canonical.to_string_lossy().into_owned(), canonical_sub.to_string_lossy().into_owned()]);

    // Appending and ingesting through the other spelling still adds only the new gap.
    Env::append(&via_link, &format!("{}\n", prompt_line("s1", "2026-01-01T10:06:00.000Z", "/work/proj", "dev", "more")));
    ingest_session(&mut db, &p).unwrap();
    ingest_session(&mut db, &via_link).unwrap();
    assert_eq!(db.session("s1").unwrap().unwrap().active_ms, 360_000);
}

#[test]
fn active_ms_same_in_one_pass_and_two_halves_with_backwards_timestamp() {
    let mk = |ts: &str, n: &str| assistant_line("s1", ts, n, "r", "m", usage(1, 1, 0, 0, 0));
    let first = [mk("2026-01-01T10:00:00.000Z", "a"), mk("2026-01-01T10:05:00.000Z", "b"), mk("2026-01-01T10:02:00.000Z", "c")];
    let second = [mk("2026-01-01T10:06:00.000Z", "d")];

    let one = Env::new();
    let all: Vec<String> = first.iter().chain(second.iter()).cloned().collect();
    let p1 = one.write_session("proj", "s1", &all);
    let mut db1 = open(&one);
    ingest_session(&mut db1, &p1).unwrap();

    let two = Env::new();
    let p2 = two.write_session("proj", "s1", &first);
    let mut db2 = open(&two);
    ingest_session(&mut db2, &p2).unwrap();
    Env::append(&p2, &format!("{}\n", second[0]));
    ingest_session(&mut db2, &p2).unwrap();

    let a = db1.session("s1").unwrap().unwrap().active_ms;
    let b = db2.session("s1").unwrap().unwrap().active_ms;
    // 10:00 -> 10:05, then 10:05 (the latest so far) -> 10:06.
    assert_eq!(a, 360_000);
    assert_eq!(b, a);
}

#[test]
fn truncated_subagent_file_without_a_complete_line_resets_offset() {
    let env = Env::new();
    let p = env.write_session("proj", "s1", &base_lines("s1"));
    let sub = env.write_subagent(
        "proj",
        "s1",
        "agent-a",
        &[
            assistant_line("s1", "2026-01-01T12:01:00.000Z", "msg_s1", "r", "m", usage(1, 1, 0, 0, 0)),
            assistant_line("s1", "2026-01-01T12:02:00.000Z", "msg_s2", "r", "m", usage(1, 1, 0, 0, 0)),
        ],
    );
    let mut db = open(&env);
    ingest_session(&mut db, &p).unwrap();
    let key = std::fs::canonicalize(&sub).unwrap().to_string_lossy().into_owned();
    assert!(db.file_state(&key).unwrap().unwrap().offset > 0);

    // Truncated to a partial line: nothing complete to read, but the offset starts over.
    std::fs::write(&sub, "{\"type\":\"assist").unwrap();
    ingest_session(&mut db, &p).unwrap();
    assert_eq!(db.file_state(&key).unwrap().unwrap().offset, 0);

    // Growth past the old length is read from the start.
    let new_lines = [
        assistant_line("s1", "2026-01-01T13:01:00.000Z", "msg_n1", "r", "m", usage(1, 1, 0, 0, 0)),
        assistant_line("s1", "2026-01-01T13:02:00.000Z", "msg_n2", "r", "m", usage(1, 1, 0, 0, 0)),
        assistant_line("s1", "2026-01-01T13:03:00.000Z", "msg_n3", "r", "m", usage(1, 1, 0, 0, 0)),
    ];
    std::fs::write(&sub, new_lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
    ingest_session(&mut db, &p).unwrap();
    let keys: Vec<String> = db.usage(Some("s1")).unwrap().into_iter().map(|(_, u)| u.key).collect();
    for k in ["msg_n1|r", "msg_n2|r", "msg_n3|r"] {
        assert!(keys.contains(&k.to_string()), "{k} missing from {keys:?}");
    }
}

#[test]
fn line_without_timestamp_uses_stored_last_active_day() {
    let env = Env::new();
    let p = env.write_session("proj", "s1", &base_lines("s1"));
    let mut db = open(&env);
    ingest_session(&mut db, &p).unwrap();
    let untimed = |sid: &str, msg: &str| {
        let mut v: serde_json::Value = serde_json::from_str(&assistant_line(
            sid, "2026-01-01T10:09:00.000Z", msg, "r", "m", usage(1, 1, 0, 0, 0),
        ))
        .unwrap();
        v.as_object_mut().unwrap().remove("timestamp");
        v.to_string()
    };
    Env::append(&p, &format!("{}\n", untimed("s1", "msg_nots")));
    ingest_session(&mut db, &p).unwrap();
    let conn = rusqlite::Connection::open(env.data.join("ledger.db")).unwrap();
    let d: String = conn
        .query_row("SELECT day FROM messages WHERE key = 'msg_nots|r'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(d, day("2026-01-01T10:05:00.000Z"));

    // A session with no stored activity falls back to today.
    let p2 = env.write_session("proj", "s2", &[untimed("s2", "msg_nots2")]);
    ingest_session(&mut db, &p2).unwrap();
    let d: String = conn
        .query_row("SELECT day FROM messages WHERE session_id = 's2'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(d, chrono::Local::now().format("%Y-%m-%d").to_string());
}

#[cfg(unix)]
#[test]
fn symlinked_subagent_file_is_ingested_under_its_real_path_once() {
    let env = Env::new();
    let p = env.write_session("proj", "s1", &base_lines("s1"));
    let real = env.write_subagent(
        "proj",
        "s1",
        "agent-a",
        &[assistant_line("s1", "2026-01-01T12:01:00.000Z", "msg_sub", "req_sub", "m", usage(40, 4, 0, 0, 0))],
    );
    let link = real.with_file_name("agent-b.jsonl");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let mut db = open(&env);
    ingest_session(&mut db, &p).unwrap();
    let conn = rusqlite::Connection::open(env.data.join("ledger.db")).unwrap();
    let paths: Vec<String> = conn
        .prepare("SELECT path FROM files ORDER BY path")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let canonical = std::fs::canonicalize(&p).unwrap().to_string_lossy().into_owned();
    let canonical_sub = std::fs::canonicalize(&real).unwrap().to_string_lossy().into_owned();
    assert_eq!(paths, vec![canonical, canonical_sub]);
    assert_eq!(db.subagent_count("s1").unwrap(), 1);
}
