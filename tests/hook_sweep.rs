mod common;
use common::*;
use session_ledger::db::Db;

const S: &str = "2026-01-01T10:00:00.000Z";
const T: &str = "2026-01-01T10:01:00.000Z";

fn open(env: &Env) -> Db {
    Db::open(&env.data.join("ledger.db")).unwrap()
}

fn lines(sid: &str, cwd: &str) -> Vec<String> {
    vec![
        prompt_line(sid, S, cwd, "main", "hello"),
        assistant_line(sid, T, &format!("msg_{sid}"), &format!("req_{sid}"), "claude-x", usage(10, 5, 1, 2, 3)),
    ]
}

fn payload(event: &str, sid: &str, path: &std::path::Path, cwd: &str) -> String {
    serde_json::json!({"session_id": sid, "transcript_path": path, "cwd": cwd,
                       "hook_event_name": event}).to_string()
}

#[test]
fn hook_indexes_session_silently() {
    let env = Env::new();
    let p = env.write_session("proj", "s1", &lines("s1", "/work/a"));
    env.sled().arg("hook").write_stdin(payload("Stop", "s1", &p, "/work/a"))
        .assert().success().stdout("").stderr("");
    let db = open(&env);
    let s = db.session("s1").unwrap().unwrap();
    assert_eq!(s.project_dir.as_deref(), Some("/work/a"));
    assert!(!s.ended);
    assert_eq!(db.usage(Some("s1")).unwrap().len(), 1);
}

#[test]
fn session_start_before_file_exists_creates_no_row() {
    let env = Env::new();
    let p = env.claude.join("projects/proj/s2.jsonl");
    for event in ["SessionStart", "Stop", "SessionEnd"] {
        env.sled().arg("hook").write_stdin(payload(event, "s2", &p, "/work/b"))
            .assert().success().stdout("").stderr("");
    }
    let db = open(&env);
    assert!(db.session("s2").unwrap().is_none());
    assert_eq!(db.session_count().unwrap(), 0);

    // The first Stop after the file is written creates the row, not ended.
    env.write_session("proj", "s2", &lines("s2", "/work/b"));
    env.sled().arg("hook").write_stdin(payload("Stop", "s2", &p, "/work/b"))
        .assert().success().stdout("").stderr("");
    let s = open(&env).session("s2").unwrap().unwrap();
    assert_eq!(s.project_dir.as_deref(), Some("/work/b"));
    assert!(!s.ended);
}

#[test]
fn hook_exits_zero_when_home_and_config_dir_are_unset() {
    let env = Env::new();
    let p = env.write_session("proj", "s9", &lines("s9", "/w"));
    env.sled()
        .env_remove("HOME")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("SLED_DATA_DIR")
        .arg("hook")
        .write_stdin(payload("Stop", "s9", &p, "/w"))
        .assert()
        .success()
        .stdout("")
        .stderr("");
    assert!(!env.data.join("ledger.db").exists());
}

#[test]
fn session_end_marks_ended_and_start_reopens() {
    let env = Env::new();
    let p = env.write_session("proj", "s3", &lines("s3", "/work/c"));
    let run = |ev: &str| {
        env.sled().arg("hook").write_stdin(payload(ev, "s3", &p, "/work/c"))
            .assert().success().stdout("").stderr("");
    };
    run("SessionEnd");
    assert!(open(&env).session("s3").unwrap().unwrap().ended);
    run("Stop");
    assert!(open(&env).session("s3").unwrap().unwrap().ended);
    run("SessionStart");
    assert!(!open(&env).session("s3").unwrap().unwrap().ended);
}

#[test]
fn bad_stdin_exits_zero_and_logs() {
    let env = Env::new();
    for input in ["not json", ""] {
        env.sled().arg("hook").write_stdin(input).assert().success().stdout("").stderr("");
    }
    let log = std::fs::read_to_string(env.data.join("hook.log")).unwrap();
    assert!(!log.trim().is_empty());
    assert!(chrono::DateTime::parse_from_rfc3339(log.split_whitespace().next().unwrap()).is_ok());
}

#[test]
fn unwritable_data_dir_still_exits_zero() {
    let env = Env::new();
    let p = env.write_session("proj", "s4", &lines("s4", "/w"));
    env.sled().env("SLED_DATA_DIR", "/dev/null/x").arg("hook")
        .write_stdin(payload("Stop", "s4", &p, "/w"))
        .assert().success().stdout("").stderr("");
}

#[test]
fn hook_with_unexpected_args_is_silent_and_exits_zero() {
    let env = Env::new();
    for args in [vec!["hook", "--bogus"], vec!["hook", "extra", "args"]] {
        env.sled().args(args).write_stdin("{}").assert().success().stdout("").stderr("");
    }
}

#[test]
fn concurrent_hooks_all_succeed_and_ledger_catches_up() {
    let env = Env::new();
    let mut children = Vec::new();
    for i in 0..8 {
        let sid = format!("c{i}");
        let p = env.write_session("proj", &sid, &lines(&sid, "/w"));
        let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("sled"));
        cmd.arg("hook")
            .env("CLAUDE_CONFIG_DIR", &env.claude)
            .env("SLED_DATA_DIR", &env.data)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(payload("Stop", &sid, &p, "/w").as_bytes()).unwrap();
        children.push(child);
    }
    for c in children {
        let out = c.wait_with_output().unwrap();
        assert!(out.status.success());
        assert!(out.stdout.is_empty() && out.stderr.is_empty());
    }
    env.sled().arg("sweep").assert().success();
    let db = open(&env);
    assert_eq!(db.session_count().unwrap(), 8);
    for i in 0..8 {
        assert_eq!(db.usage(Some(&format!("c{i}"))).unwrap().len(), 1, "session c{i}");
    }
}

fn three_sessions(env: &Env) -> Vec<std::path::PathBuf> {
    vec![
        env.write_session("p1", "a", &lines("a", "/w/1")),
        env.write_session("p1", "b", &lines("b", "/w/1")),
        env.write_session("p2", "c", &lines("c", "/w/2")),
    ]
}

#[test]
fn sweep_indexes_everything_and_is_repeatable() {
    let env = Env::new();
    three_sessions(&env);
    env.sled().arg("sweep").assert().success().stdout("Indexed 3 sessions (3 new).\n");
    let first = open(&env).usage(None).unwrap();
    env.sled().arg("sweep").assert().success().stdout("Indexed 3 sessions (0 new).\n");
    let second = open(&env).usage(None).unwrap();
    assert_eq!(first.len(), 3);
    assert_eq!(first, second);
}

#[test]
fn sweep_marks_deleted_transcripts_but_keeps_rows() {
    let env = Env::new();
    let files = three_sessions(&env);
    env.sled().arg("sweep").assert().success();
    let before = open(&env).usage(Some("b")).unwrap();
    std::fs::remove_file(&files[1]).unwrap();
    env.sled().arg("sweep").assert().success();
    let db = open(&env);
    let s = db.session("b").unwrap().unwrap();
    assert!(s.transcript_missing);
    assert!(!db.session("a").unwrap().unwrap().transcript_missing);
    assert_eq!(db.usage(Some("b")).unwrap(), before);
    assert_eq!(db.session_count().unwrap(), 3);
}

#[test]
fn sweep_preserves_notes() {
    let env = Env::new();
    three_sessions(&env);
    env.sled().arg("sweep").assert().success();
    assert!(open(&env).set_note("a", "keep me").unwrap());
    env.sled().arg("sweep").assert().success();
    assert_eq!(open(&env).session("a").unwrap().unwrap().note, "keep me");
}

#[test]
#[ignore]
fn writes_large_fixture() {
    let env = Env::new();
    let mut ls = vec![prompt_line("big", S, "/w", "main", "hello")];
    for i in 0..20_000 {
        let ts = format!("2026-01-01T10:{:02}:{:02}.000Z", (i / 60) % 60, i % 60);
        ls.push(assistant_line("big", &ts, &format!("m{i}"), &format!("r{i}"), "claude-x", usage(10, 5, 1, 2, 3)));
    }
    let p = env.write_session("proj", "big", &ls);
    let keep = env.tmp.keep();
    println!("FIXTURE_CLAUDE={}", keep.join("claude").display());
    println!("FIXTURE_PATH={}", p.display());
}

#[test]
fn hook_log_is_rotated_when_large() {
    let env = Env::new();
    let log = env.data.join("hook.log");
    std::fs::write(&log, vec![b'x'; 262_145]).unwrap();
    env.sled().arg("hook").write_stdin("not json").assert().success().stdout("").stderr("");
    assert_eq!(std::fs::metadata(env.data.join("hook.log.1")).unwrap().len(), 262_145);
    let n = std::fs::metadata(&log).unwrap().len();
    assert!(n > 0 && n < 1024, "hook.log is {n} bytes");
}

#[test]
fn concurrent_sweeps_on_a_fresh_data_folder_all_succeed() {
    let env = Env::new();
    three_sessions(&env);
    let children: Vec<_> = (0..8)
        .map(|_| {
            std::process::Command::new(assert_cmd::cargo::cargo_bin("sled"))
                .arg("sweep")
                .env("CLAUDE_CONFIG_DIR", &env.claude)
                .env("SLED_DATA_DIR", &env.data)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for c in children {
        let out = c.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    assert_eq!(open(&env).session_count().unwrap(), 3);
}

#[test]
fn sweep_reports_refresh_failure_after_its_summary() {
    let env = Env::new();
    let files = three_sessions(&env);
    env.sled().arg("sweep").assert().success();
    let conn = rusqlite::Connection::open(env.data.join("ledger.db")).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER no_flag BEFORE UPDATE OF transcript_missing ON sessions
         BEGIN SELECT RAISE(ABORT, 'flag update refused'); END;",
    )
    .unwrap();
    std::fs::remove_file(&files[1]).unwrap();
    let out = env.sled().arg("sweep").assert().failure().stdout("Indexed 3 sessions (0 new).\n");
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(stderr.contains("flag update refused"), "{stderr}");
}

// ---- sled sweep --rebuild ----

fn raw(env: &Env) -> rusqlite::Connection {
    rusqlite::Connection::open(env.data.join("ledger.db")).unwrap()
}

fn rebuild(env: &Env) -> String {
    let out = env.sled().args(["sweep", "--rebuild"]).assert().success().get_output().stdout.clone();
    String::from_utf8(out).unwrap()
}

type Snapshot = (Vec<session_ledger::db::SessionRow>, Vec<(String, session_ledger::transcript::MessageUsage)>);

fn snapshot(db: &Db) -> Snapshot {
    (db.sessions(None, true).unwrap(), db.usage(None).unwrap())
}

/// A forked pair sharing one message, and a session with a sub-agent file.
fn forked_and_subagent(env: &Env) {
    let shared = assistant_line("fa", T, "msg_shared", "req_shared", "claude-x", usage(7, 7, 7, 7, 7));
    env.write_session("p1", "fa", &[
        prompt_line("fa", S, "/w/1", "main", "fork base"),
        shared.clone(),
    ]);
    std::thread::sleep(std::time::Duration::from_millis(20));
    env.write_session("p1", "fb", &[
        prompt_line("fb", S, "/w/1", "main", "fork base"),
        shared,
        prompt_line("fb", "2026-01-01T10:03:00.000Z", "/w/1", "main", "after fork"),
        assistant_line("fb", "2026-01-01T10:04:00.000Z", "msg_fb", "req_fb", "claude-x", usage(1, 2, 3, 4, 5)),
    ]);
    env.write_session("p2", "sx", &lines("sx", "/w/2"));
    env.write_subagent("p2", "sx", "agent-a", &[
        assistant_line("sx", "2026-01-01T10:02:00.000Z", "msg_sub", "req_sub", "claude-x", usage(40, 4, 0, 0, 0)),
    ]);
}

#[test]
fn rebuild_reapplies_reading_rules() {
    let env = Env::new();
    three_sessions(&env);
    env.sled().arg("sweep").assert().success();
    raw(&env).execute("UPDATE sessions SET first_prompt = 'stale', active_ms = 1 WHERE session_id = 'a'", []).unwrap();
    env.sled().arg("sweep").assert().success();
    assert_eq!(open(&env).session("a").unwrap().unwrap().first_prompt.as_deref(), Some("stale"));

    assert_eq!(rebuild(&env), "Rebuilt 3 sessions, kept 0 whose files are gone.\nIndexed 3 sessions (0 new).\n");
    let s = open(&env).session("a").unwrap().unwrap();
    assert_eq!(s.first_prompt.as_deref(), Some("hello"));
    assert_eq!(s.active_ms, 60_000);
}

#[test]
fn rebuild_keeps_sessions_whose_file_is_gone() {
    let env = Env::new();
    let files = three_sessions(&env);
    env.sled().arg("sweep").assert().success();
    assert!(open(&env).set_note("b", "only copy").unwrap());
    raw(&env).execute("UPDATE sessions SET first_prompt = 'old rules', title = 'T' WHERE session_id = 'b'", []).unwrap();
    std::fs::remove_file(&files[1]).unwrap();
    env.sled().arg("sweep").assert().success();
    let db = open(&env);
    let before = (db.session("b").unwrap().unwrap(), db.usage(Some("b")).unwrap());
    assert!(before.0.transcript_missing);
    assert_eq!(before.1.len(), 1);

    assert_eq!(rebuild(&env), "Rebuilt 2 sessions, kept 1 whose files are gone.\nIndexed 3 sessions (0 new).\n");
    let db = open(&env);
    assert_eq!((db.session("b").unwrap().unwrap(), db.usage(Some("b")).unwrap()), before);
}

#[test]
fn rebuild_preserves_notes_and_ended() {
    let env = Env::new();
    three_sessions(&env);
    env.sled().arg("sweep").assert().success();
    let db = open(&env);
    assert!(db.set_note("a", "keep me").unwrap());
    db.set_ended("a", true).unwrap();
    rebuild(&env);
    let db = open(&env);
    let a = db.session("a").unwrap().unwrap();
    assert_eq!(a.note, "keep me");
    assert!(a.ended);
    assert!(!db.session("c").unwrap().unwrap().ended);
    assert_eq!(db.session_count().unwrap(), 3);
}

#[test]
fn rebuild_twice_is_identical() {
    let env = Env::new();
    let files = three_sessions(&env);
    forked_and_subagent(&env);
    env.sled().arg("sweep").assert().success();
    std::fs::remove_file(&files[2]).unwrap();
    rebuild(&env);
    let first = snapshot(&open(&env));
    rebuild(&env);
    assert_eq!(snapshot(&open(&env)), first);
}

#[test]
fn rebuild_totals_equal_fresh_sweep() {
    let env = Env::new();
    three_sessions(&env);
    forked_and_subagent(&env);
    env.sled().arg("sweep").assert().success();
    // Stale state that a plain sweep would not repair.
    raw(&env)
        .execute_batch(
            "UPDATE sessions SET active_ms = active_ms + 999, first_prompt = 'stale', continued_from = NULL;
             UPDATE messages SET session_id = 'fb' WHERE key = 'msg_shared|req_shared';
             DELETE FROM messages WHERE subagent = 1;",
        )
        .unwrap();
    rebuild(&env);

    let fresh_data = env.tmp.path().join("fresh-data");
    env.sled().env("SLED_DATA_DIR", &fresh_data).arg("sweep").assert().success();
    let fresh = Db::open(&fresh_data.join("ledger.db")).unwrap();
    let rebuilt = open(&env);

    let totals = |db: &Db| -> Vec<(String, i64, u64)> {
        db.sessions(None, true)
            .unwrap()
            .into_iter()
            .map(|s| {
                let tokens = db
                    .usage(Some(&s.session_id))
                    .unwrap()
                    .iter()
                    .map(|(_, u)| u.input + u.output + u.cache_read + u.cache_write_5m + u.cache_write_1h)
                    .sum();
                (s.session_id, s.active_ms, tokens)
            })
            .collect()
    };
    assert_eq!(totals(&rebuilt), totals(&fresh));
    assert_eq!(rebuilt.usage(None).unwrap(), fresh.usage(None).unwrap());
    assert_eq!(snapshot(&rebuilt), snapshot(&fresh));
    // The fork's shared message has one owner, and the sub-agent message is back.
    let owners: Vec<String> = rebuilt
        .usage(None)
        .unwrap()
        .into_iter()
        .filter(|(_, u)| u.key == "msg_shared|req_shared" || u.key == "msg_sub|req_sub")
        .map(|(sid, _)| sid)
        .collect();
    assert_eq!(owners, ["fa", "sx"]);
}

#[test]
fn rebuild_keeps_usage_when_a_subagent_folder_is_gone() {
    let env = Env::new();
    forked_and_subagent(&env);
    env.sled().arg("sweep").assert().success();
    let before = open(&env).usage(Some("sx")).unwrap();
    assert_eq!(before.len(), 2);
    assert!(before.iter().any(|(_, u)| u.key == "msg_sub|req_sub" && u.subagent));
    std::fs::remove_dir_all(env.claude.join("projects/p2/sx/subagents")).unwrap();
    rebuild(&env);
    assert_eq!(open(&env).usage(Some("sx")).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn rebuild_keeps_usage_when_a_subagent_symlink_is_dead() {
    let env = Env::new();
    env.write_session("p2", "sx", &lines("sx", "/w/2"));
    let target = env.tmp.path().join("elsewhere.jsonl");
    std::fs::write(
        &target,
        format!("{}\n", assistant_line("sx", T, "msg_sub", "req_sub", "claude-x", usage(40, 4, 0, 0, 0))),
    )
    .unwrap();
    let dir = env.claude.join("projects/p2/sx/subagents");
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(&target, dir.join("agent-a.jsonl")).unwrap();
    env.sled().arg("sweep").assert().success();
    let before = open(&env).usage(Some("sx")).unwrap();
    assert_eq!(before.len(), 2);
    std::fs::remove_file(&target).unwrap();
    rebuild(&env);
    assert_eq!(open(&env).usage(Some("sx")).unwrap(), before);
}

#[test]
fn rebuild_never_drops_a_fork_shared_message() {
    let env = Env::new();
    forked_and_subagent(&env);
    env.sled().arg("sweep").assert().success();
    let shared = |db: &Db| -> Vec<String> {
        db.usage(None)
            .unwrap()
            .into_iter()
            .filter(|(_, u)| u.key == "msg_shared|req_shared")
            .map(|(sid, _)| sid)
            .collect()
    };
    assert_eq!(shared(&open(&env)), ["fa"]);
    // The owner's file no longer yields the shared message.
    env.write_session("p1", "fa", &[prompt_line("fa", S, "/w/1", "main", "fork base")]);
    rebuild(&env);
    assert_eq!(shared(&open(&env)).len(), 1);
}

#[cfg(unix)]
#[test]
fn rebuild_reports_failure_and_exits_non_zero_but_finishes_others() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    let files = three_sessions(&env);
    env.sled().arg("sweep").assert().success();
    raw(&env).execute("UPDATE sessions SET first_prompt = 'stale' WHERE session_id IN ('a', 'b')", []).unwrap();
    let db = open(&env);
    let before = (db.session("a").unwrap().unwrap(), db.usage(Some("a")).unwrap());
    std::fs::set_permissions(&files[0], std::fs::Permissions::from_mode(0o000)).unwrap();

    let out = env.sled().args(["sweep", "--rebuild"]).assert().failure();
    std::fs::set_permissions(&files[0], std::fs::Permissions::from_mode(0o644)).unwrap();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert_eq!(stdout, "Rebuilt 2 sessions, kept 0 whose files are gone.\nIndexed 3 sessions (0 new).\n");
    assert!(stderr.contains("could not rebuild a"), "{stderr}");

    let db = open(&env);
    assert_eq!((db.session("a").unwrap().unwrap(), db.usage(Some("a")).unwrap()), before);
    assert_eq!(db.session("b").unwrap().unwrap().first_prompt.as_deref(), Some("hello"));
}

/// `sweep::rebuild` checks the main file exists, then re-reads it inside
/// `Db::rebuild_session`. If the file vanishes between the two steps, the re-read fails and
/// the session is rolled back untouched. The gap cannot be hit reliably through the binary,
/// so this drives the same call `sweep::rebuild` makes, after deleting the file.
#[test]
fn rebuild_rolls_back_when_the_main_file_vanishes_mid_run() {
    let env = Env::new();
    let files = three_sessions(&env);
    env.write_subagent("p1", "a", "agent-a", &[
        assistant_line("a", "2026-01-01T10:02:00.000Z", "msg_sub", "req_sub", "claude-x", usage(40, 4, 0, 0, 0)),
    ]);
    env.sled().arg("sweep").assert().success();
    let files_rows = |env: &Env| -> Vec<(String, i64)> {
        let conn = raw(env);
        let mut st = conn.prepare("SELECT path, offset FROM files WHERE session_id = 'a' ORDER BY path").unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect()
    };
    let state = |env: &Env| {
        let db = open(env);
        (db.session("a").unwrap().unwrap(), files_rows(env), db.usage(Some("a")).unwrap())
    };
    let before = state(&env);
    assert_eq!(before.1.len(), 2, "main and sub-agent files are tracked");
    assert_eq!(before.2.len(), 2, "main and sub-agent usage is stored");

    std::fs::remove_file(&files[0]).unwrap();
    let mut db = open(&env);
    let result = db.rebuild_session("a", |db| session_ledger::ingest::reingest_session(db, &files[0]));
    assert!(result.is_err());
    drop(db);
    assert_eq!(state(&env), before);
}
