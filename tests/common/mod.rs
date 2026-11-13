#![allow(dead_code)]
use assert_cmd::Command;
use serde_json::{json, Value};
use session_ledger::config::Config;
use session_ledger::paths::Paths;
use session_ledger::server;
use std::path::{Path, PathBuf};

pub struct Env {
    pub tmp: tempfile::TempDir,
    pub claude: PathBuf,
    pub data: PathBuf,
}

impl Env {
    pub fn new() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join("claude");
        let data = tmp.path().join("data");
        std::fs::create_dir_all(claude.join("projects")).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        Env { tmp, claude, data }
    }

    /// The `sled` binary with both folders pointed at this temp environment.
    pub fn sled(&self) -> Command {
        let mut cmd = Command::cargo_bin("sled").unwrap();
        cmd.env("CLAUDE_CONFIG_DIR", &self.claude)
            .env("SLED_DATA_DIR", &self.data);
        cmd
    }

    pub fn paths(&self) -> Paths {
        Paths {
            claude_dir: self.claude.clone(),
            data_dir: self.data.clone(),
        }
    }
}

// ---- transcript line builders: each returns one JSON line, no trailing newline ----

pub fn title_line(sid: &str, title: &str) -> String {
    json!({"type":"ai-title","sessionId":sid,"aiTitle":title,"entrypoint":"cli"}).to_string()
}

pub fn recap_line(sid: &str, ts: &str, text: &str) -> String {
    json!({"type":"system","subtype":"away_summary","sessionId":sid,"timestamp":ts,
           "content":text,"entrypoint":"cli"})
    .to_string()
}

pub fn prompt_line(sid: &str, ts: &str, cwd: &str, branch: &str, text: &str) -> String {
    json!({"type":"user","sessionId":sid,"timestamp":ts,"cwd":cwd,"gitBranch":branch,
           "entrypoint":"cli","message":{"role":"user","content":text}})
    .to_string()
}

pub fn tool_result_line(sid: &str, ts: &str) -> String {
    json!({"type":"user","sessionId":sid,"timestamp":ts,"entrypoint":"cli",
           "toolUseResult":{"stdout":"ok"},
           "message":{"role":"user","content":[
               {"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]}})
    .to_string()
}

pub fn assistant_line(
    sid: &str,
    ts: &str,
    msg_id: &str,
    req_id: &str,
    model: &str,
    usage: Value,
) -> String {
    json!({"type":"assistant","sessionId":sid,"timestamp":ts,"requestId":req_id,
           "entrypoint":"cli",
           "message":{"id":msg_id,"role":"assistant","model":model,
                      "content":[{"type":"text","text":"hi"}],"usage":usage}})
    .to_string()
}

pub fn usage(input: u64, output: u64, cache_read: u64, w5m: u64, w1h: u64) -> Value {
    json!({"input_tokens":input,"output_tokens":output,
           "cache_read_input_tokens":cache_read,
           "cache_creation_input_tokens":w5m + w1h,
           "cache_creation":{"ephemeral_5m_input_tokens":w5m,"ephemeral_1h_input_tokens":w1h},
           "service_tier":"standard","speed":"standard",
           "server_tool_use":{"web_search_requests":0}})
}

// ---- session file helpers ----

impl Env {
    /// Writes `<claude>/projects/<project>/<sid>.jsonl`, a newline after each line.
    pub fn write_session(&self, project: &str, sid: &str, lines: &[String]) -> PathBuf {
        let path = self.claude.join("projects").join(project).join(format!("{sid}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, join_lines(lines)).unwrap();
        path
    }

    /// Writes `<claude>/projects/<project>/<sid>/subagents/<agent>.jsonl`.
    pub fn write_subagent(&self, project: &str, sid: &str, agent: &str, lines: &[String]) -> PathBuf {
        let path = self
            .claude
            .join("projects")
            .join(project)
            .join(sid)
            .join("subagents")
            .join(format!("{agent}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, join_lines(lines)).unwrap();
        path
    }

    /// Appends raw text to an existing file.
    pub fn append(path: &Path, text: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }
}

/// Sets a file's modification time to `ago_ms` milliseconds before now.
pub fn set_mtime_ago(path: &Path, ago_ms: u64) {
    let t = std::time::SystemTime::now() - std::time::Duration::from_millis(ago_ms);
    std::fs::File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
}

fn join_lines(lines: &[String]) -> String {
    lines.iter().map(|l| format!("{l}\n")).collect()
}

// ---- server helpers ----

/// Starts the server on a free port in a background thread; returns `127.0.0.1:<port>`.
pub fn start(env: &Env, cfg: Config) -> String {
    let server = server::bind(0).unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let paths = env.paths();
    std::thread::spawn(move || server::run(server, paths, cfg));
    format!("127.0.0.1:{port}")
}

/// An HTTP client that returns non-2xx responses instead of erroring.
pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().http_status_as_error(false).build().into()
}
