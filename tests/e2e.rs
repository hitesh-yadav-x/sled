//! Drives the real `sled` binary for hook and sweep, then reads the result over HTTP.
mod common;
use common::*;
use serde_json::Value;
use session_ledger::config::Config;
use session_ledger::db::Db;
use session_ledger::ingest::ingest_session;
use std::path::{Path, PathBuf};

const SID: &str = "e2e-session";

fn ts(base: chrono::DateTime<chrono::Utc>, offset_ms: i64) -> String {
    (base + chrono::Duration::milliseconds(offset_ms)).format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn transcript(env: &Env) -> PathBuf {
    env.claude.join("projects").join("-w-app").join(format!("{SID}.jsonl"))
}

fn hook(env: &Env, event: &str, path: &Path) {
    let payload = serde_json::json!({"session_id": SID, "transcript_path": path, "cwd": "/w/app",
                                     "hook_event_name": event});
    env.sled().arg("hook").write_stdin(payload.to_string()).assert().success().stdout("").stderr("");
}

fn sweep(env: &Env) {
    env.sled().arg("sweep").assert().success();
}

/// The lines Claude Code writes after the first prompt: one prompt, one reply 40 s later.
fn first_turn() -> Vec<String> {
    let now = chrono::Utc::now();
    vec![
        prompt_line(SID, &ts(now, 0), "/w/app", "main", "fix the login bug"),
        assistant_line(SID, &ts(now, 40_000), "msg_1", "req_1", "claude-x", usage(100, 50, 10, 20, 30)),
    ]
}

fn write_turn(path: &Path, lines: &[String]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(path, text).unwrap();
}

fn get(addr: &str, path: &str) -> Value {
    let mut r = agent().get(format!("http://{addr}{path}")).call().unwrap();
    assert_eq!(r.status().as_u16(), 200, "{path}");
    serde_json::from_str(&r.body_mut().read_to_string().unwrap()).unwrap()
}

fn fresh_active_ms(path: &Path) -> i64 {
    let env = Env::new();
    let mut db = Db::open(&env.paths().db_file()).unwrap();
    ingest_session(&mut db, path).unwrap();
    db.session(SID).unwrap().unwrap().active_ms
}

#[test]
fn session_swept_before_its_transcript_exists_is_resumable_once_written() {
    let env = Env::new();
    let path = transcript(&env);
    hook(&env, "SessionStart", &path);
    sweep(&env); // marks the not-yet-written transcript missing
    write_turn(&path, &first_turn());
    hook(&env, "Stop", &path);

    let addr = start(&env, Config::default());
    let s = get(&addr, &format!("/api/sessions/{SID}"));
    assert_eq!(s["resumable"], true, "{s}");
    assert!(s["resume_command"].is_string(), "{s}");
    assert_eq!(s["running"], true, "{s}");
}

#[test]
fn hook_sweep_and_server_agree_end_to_end() {
    let env = Env::new();
    let path = transcript(&env);
    hook(&env, "SessionStart", &path);
    sweep(&env);
    write_turn(&path, &first_turn());
    hook(&env, "Stop", &path);
    hook(&env, "Stop", &path); // nothing new

    let addr = start(&env, Config::default());
    let list = get(&addr, "/api/sessions");
    let rows = list["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{list}");
    let detail = get(&addr, &format!("/api/sessions/{SID}"));
    for s in [&rows[0], &detail] {
        assert_eq!(s["id"], SID);
        assert_eq!(s["tokens"]["input"], 100);
        assert_eq!(s["tokens"]["output"], 50);
        assert_eq!(s["tokens"]["total"], 210);
        assert_eq!(s["active_ms"], fresh_active_ms(&path));
        assert_eq!(s["active_ms"], 40_000);
        assert_eq!(s["resumable"], true);
        assert_eq!(s["running"], true);
    }
    assert_eq!(detail["first_prompt"], "fix the login bug");
    assert!(detail["resume_command"].is_string());

    hook(&env, "SessionEnd", &path);
    assert_eq!(get(&addr, &format!("/api/sessions/{SID}"))["running"], false);
    assert_eq!(get(&addr, "/api/sessions")["sessions"][0]["running"], false);
}
