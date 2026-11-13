//! Local HTTP server: embedded page assets and a small JSON API over the ledger.
//! Binds 127.0.0.1 only. Four workers share the listener; each opens its own database
//! connection per request.

use crate::config::{Config, CostMode};
use crate::db::{Db, SessionRow};
use crate::demo::Mask;
use crate::paths::Paths;
use crate::pricing::PriceTable;
use crate::transcript::MessageUsage;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tiny_http::{Header, Method, Request, Response, Server};

const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_NOTE_CHARS: usize = 4000;
const RUNNING_WINDOW_MS: i64 = 600_000;
const DAY_MS: i64 = 86_400_000;
/// Threads answering requests, so one stalled client cannot hold up the page.
const WORKERS: usize = 4;
/// The page loads only its own files, cannot be framed, and cannot post a form anywhere.
const CSP: &str = "default-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

const INDEX_HTML: &str = include_str!("../assets/index.html");
const APP_CSS: &str = include_str!("../assets/app.css");
const APP_JS: &str = include_str!("../assets/app.js");
const OUTFIT_WOFF2: &[u8] = include_bytes!("../assets/fonts/outfit.woff2");
const GEIST_MONO_WOFF2: &[u8] = include_bytes!("../assets/fonts/geist-mono.woff2");

/// Binds `127.0.0.1:<port>`; port 0 picks a free port.
pub fn bind(port: u16) -> Result<Server> {
    Server::http(("127.0.0.1", port)).map_err(|e| {
        let in_use = e
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::AddrInUse);
        if in_use {
            anyhow!("port {port} is already in use; pick another with --port <N>")
        } else {
            anyhow!("cannot listen on 127.0.0.1:{port}: {e}")
        }
    })
}

/// Serves requests on four worker threads, forever. A panic while handling a request is
/// contained to that request. Should a worker stop anyway, the process exits with status 1.
pub fn run(server: Server, paths: Paths, cfg: Config) -> ! {
    let port = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(0);
    let shared = Arc::new((server, paths, cfg));
    let (stopped_tx, stopped_rx) = std::sync::mpsc::channel::<()>();
    for _ in 0..WORKERS {
        let shared = Arc::clone(&shared);
        let stopped = WorkerStopped(stopped_tx.clone());
        std::thread::spawn(move || {
            let _stopped = stopped;
            let (server, paths, cfg) = &*shared;
            serve_requests(server, port, paths, cfg);
        });
    }
    drop(stopped_tx);
    let _ = stopped_rx.recv();
    eprintln!("sled: a server worker stopped unexpectedly; the server is shutting down");
    std::process::exit(1)
}

/// Reports, when dropped, that its worker has ended, however it ended.
struct WorkerStopped(std::sync::mpsc::Sender<()>);

impl Drop for WorkerStopped {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

/// Answers requests from the shared listener until it stops.
fn serve_requests(server: &Server, port: u16, paths: &Paths, cfg: &Config) {
    while let Ok(mut request) = server.recv() {
        let routed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            route(&mut request, port, paths, cfg)
        }));
        let reply = routed.unwrap_or_else(|panic| {
            let what = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            eprintln!("sled: a request handler panicked: {}", what.replace('\n', " "));
            error(500, "internal error")
        });
        let mut response = Response::from_data(reply.body).with_status_code(reply.status);
        for (name, value) in [
            ("Content-Type", reply.content_type),
            ("Cache-Control", "no-store"),
            ("X-Content-Type-Options", "nosniff"),
            ("X-Frame-Options", "DENY"),
            ("Referrer-Policy", "no-referrer"),
            ("Content-Security-Policy", CSP),
        ] {
            if let Ok(h) = Header::from_bytes(name, value) {
                response.add_header(h);
            }
        }
        let _ = request.respond(response);
    }
}

/// `s` unchanged when it is made only of `A-Za-z0-9_./~-`, else single-quoted.
pub use crate::quote::shell_quote;
use crate::quote::shell_quote_path;

/// `cd <dir> && claude --resume <id>`, each part quoted for a POSIX shell. A leading `~/` is
/// left unquoted so the shell expands it; the rest of the folder is quoted, `~` included.
pub fn resume_command(dir: &str, session_id: &str) -> String {
    let dir = match dir.strip_prefix("~/") {
        Some(rest) => format!("~/{}", shell_quote_path(rest)),
        None => shell_quote_path(dir),
    };
    format!("cd {dir} && claude --resume {}", shell_quote(session_id))
}

/// Why no resume command is offered, checked in this order: the transcript is gone, the
/// folder is unknown or not absolute (`/…` or `~/…`), the folder has a character that is
/// unsafe to paste (see `is_safe_folder`), or the id is not `[A-Za-z0-9][A-Za-z0-9_-]*`.
fn resume_blocked(row: &SessionRow, transcript_missing: bool) -> Option<&'static str> {
    let absolute = row
        .project_dir
        .as_deref()
        .is_some_and(|d| d.starts_with('/') || d.starts_with("~/"));
    if transcript_missing {
        Some("transcript_removed")
    } else if !absolute {
        Some("no_folder")
    } else if !row.project_dir.as_deref().is_some_and(is_safe_folder) {
        Some("unsafe_folder")
    } else if !is_safe_session_id(&row.session_id) {
        Some("unsafe_id")
    } else {
        None
    }
}

/// Quoting protects the shell, not the terminal: a pasted control character acts as a key
/// press (Ctrl-C abandons the line, and what follows runs as a new command), and invisible
/// or direction-changing characters hide what is being pasted. A backslash is refused too,
/// because shells that are not POSIX (fish) treat it as an escape inside single quotes.
fn is_safe_folder(dir: &str) -> bool {
    !dir.chars().any(|c| {
        c.is_control()
            || c == '\\'
            || matches!(c,
                '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
    })
}

fn is_safe_session_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Whether the stored folder is a directory now. A leading `~/` is expanded with `$HOME`;
/// a folder that is still not absolute (or `~/…` without `HOME`) counts as missing.
fn dir_exists(dir: Option<&str>) -> bool {
    let Some(dir) = dir else { return false };
    let path = match dir.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME").filter(|h| !h.is_empty()) {
            Some(home) => PathBuf::from(home).join(rest),
            None => return false,
        },
        None => PathBuf::from(dir),
    };
    path.is_absolute() && path.is_dir()
}

struct Reply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

fn text(status: u16, content_type: &'static str, body: &str) -> Reply {
    Reply { status, content_type, body: body.as_bytes().to_vec() }
}

fn json_reply(status: u16, v: Value) -> Reply {
    text(status, "application/json", &v.to_string())
}

fn error(status: u16, message: &str) -> Reply {
    json_reply(status, json!({ "error": message }))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn header(request: &Request, name: &'static str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str().to_string())
}

fn route(request: &mut Request, port: u16, paths: &Paths, cfg: &Config) -> Reply {
    let host_ok = header(request, "Host").is_some_and(|h| {
        let h = h.to_ascii_lowercase();
        h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}")
    });
    if !host_ok {
        return error(403, "forbidden host");
    }
    let url = request.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let method = request.method().clone();

    // Test-only: lets the tests check that a panicking handler is contained. Not in release builds.
    #[cfg(debug_assertions)]
    if method == Method::Get && path == "/api/debug/panic" {
        panic!("test panic requested");
    }

    if method == Method::Get {
        match path {
            "/" => return text(200, "text/html; charset=utf-8", INDEX_HTML),
            "/app.css" => return text(200, "text/css; charset=utf-8", APP_CSS),
            "/app.js" => return text(200, "text/javascript; charset=utf-8", APP_JS),
            "/api/sessions" => return api(|| list_sessions(query, paths, cfg)),
            _ => {}
        }
        if let Some(name) = path.strip_prefix("/fonts/") {
            return match font(name) {
                Some((ct, bytes)) => Reply { status: 200, content_type: ct, body: bytes.to_vec() },
                None => error(404, "not found"),
            };
        }
        if let Some(id) = path.strip_prefix("/api/sessions/") {
            if !id.is_empty() && !id.contains('/') {
                return api(|| session_detail(id, paths, cfg));
            }
        }
    } else if method == Method::Put {
        // A browser names the page a request comes from; only this page may change a note.
        let foreign = header(request, "Origin").is_some_and(|o| {
            let o = o.to_ascii_lowercase();
            o != format!("http://127.0.0.1:{port}") && o != format!("http://localhost:{port}")
        });
        if foreign {
            return error(403, "forbidden origin");
        }
        if let Some(id) = path
            .strip_prefix("/api/sessions/")
            .and_then(|r| r.strip_suffix("/note"))
        {
            if !id.is_empty() && !id.contains('/') {
                if cfg.demo {
                    return error(403, "notes are read-only in demo mode");
                }
                return put_note(request, id, paths);
            }
        }
    }
    error(404, "not found")
}

/// The two embedded font files, by exact name; anything else is `None`.
fn font(name: &str) -> Option<(&'static str, &'static [u8])> {
    match name {
        "outfit.woff2" => Some(("font/woff2", OUTFIT_WOFF2)),
        "geist-mono.woff2" => Some(("font/woff2", GEIST_MONO_WOFF2)),
        _ => None,
    }
}

/// Runs a handler. An error is written in full to stderr; the client gets a fixed 500 body.
fn api(f: impl FnOnce() -> Result<Reply>) -> Reply {
    f().unwrap_or_else(|e| {
        eprintln!("sled: {e:#}");
        error(500, "internal error")
    })
}

fn open_db(paths: &Paths) -> Result<Db> {
    Db::open(&paths.db_file())
}

fn list_sessions(query: &str, paths: &Paths, cfg: &Config) -> Result<Reply> {
    let mut period = "14";
    let mut automated = "0";
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        match pair.split_once('=').unwrap_or((pair, "")) {
            ("period", v) => period = v,
            ("automated", v) => automated = v,
            _ => {}
        }
    }
    let now = now_ms();
    let since = match period {
        "7" => Some(now - 7 * DAY_MS),
        "14" => Some(now - 14 * DAY_MS),
        "all" => None,
        _ => return Ok(error(400, "period must be 7, 14 or all")),
    };
    let include_automated = match automated {
        "0" => false,
        "1" => true,
        _ => return Ok(error(400, "automated must be 0 or 1")),
    };
    let db = open_db(paths)?;
    let rows = db.sessions(since, include_automated)?;
    let mut by_session: BTreeMap<String, Vec<MessageUsage>> = BTreeMap::new();
    let ids: Vec<String> = rows.iter().map(|r| r.session_id.clone()).collect();
    for (sid, u) in db.usage_for(&ids)? {
        by_session.entry(sid).or_default().push(u);
    }
    let prices = PriceTable::for_config(cfg);
    let labels = project_labels_by_id(&db)?;
    let mask = demo_mask(&db, cfg)?;
    let sessions: Vec<Value> = rows
        .iter()
        .map(|row| {
            let usage = by_session.get(&row.session_id).map(Vec::as_slice).unwrap_or(&[]);
            let mut v = session_json(row, usage, &prices, cfg, now, label_of(&labels, row));
            if let Some(mask) = &mask {
                mask.apply(&mut v);
            }
            v
        })
        .collect();
    Ok(json_reply(
        200,
        json!({
            "cost_mode": cost_mode_name(cfg.cost_mode),
            "cost_label": cost_label(cfg),
            "prices_as_of": prices.as_of(),
            "demo": cfg.demo,
            "sessions": sessions,
        }),
    ))
}

fn session_detail(id: &str, paths: &Paths, cfg: &Config) -> Result<Reply> {
    let db = open_db(paths)?;
    let Some(row) = db.session(id)? else {
        return Ok(error(404, "no such session"));
    };
    let usage: Vec<MessageUsage> = db.usage(Some(id))?.into_iter().map(|(_, u)| u).collect();
    let prices = PriceTable::for_config(cfg);
    let now = now_ms();
    let project = label_of(&project_labels_by_id(&db)?, &row);
    let mut v = session_json(&row, &usage, &prices, cfg, now, project);

    // A model name can be a private deployment name, so demo mode shows only known ones.
    let models: BTreeSet<&str> = usage
        .iter()
        .map(|u| u.model.as_str())
        .map(|m| if cfg.demo && prices.rates_for(m).is_none() { "custom model" } else { m })
        .collect();
    let continued_from = match row.continued_from.as_deref() {
        Some(other) => db
            .session(other)?
            .map(|o| json!({ "id": o.session_id, "title": title_of(&o) })),
        None => None,
    };
    let blocked = resume_blocked(&row, transcript_mtime_ms(&row).is_none());
    let resume = match (blocked, row.project_dir.as_deref()) {
        (None, Some(d)) => Some(resume_command(d, &row.session_id)),
        _ => None,
    };
    let obj = v.as_object_mut().expect("session json is an object");
    obj.insert("first_prompt".into(), json!(row.first_prompt.clone().unwrap_or_default()));
    obj.insert("last_prompt".into(), json!(row.last_prompt.clone().unwrap_or_default()));
    obj.insert("models".into(), json!(models));
    obj.insert("resume_command".into(), json!(resume));
    obj.insert("resume_blocked".into(), json!(blocked));
    obj.insert("dir_exists".into(), json!(dir_exists(row.project_dir.as_deref())));
    obj.insert("continued_from".into(), json!(continued_from));
    obj.insert("split".into(), split_json(&usage, db.subagent_count(id)?, &prices, cfg));
    if let Some(mask) = demo_mask(&db, cfg)? {
        mask.apply(&mut v);
    }
    Ok(json_reply(200, v))
}

/// In demo mode, the stand-ins for every session in the ledger; otherwise `None`.
fn demo_mask(db: &Db, cfg: &Config) -> Result<Option<Mask>> {
    if !cfg.demo {
        return Ok(None);
    }
    Ok(Some(Mask::new(&db.sessions(None, true)?)))
}

fn put_note(request: &mut Request, id: &str, paths: &Paths) -> Reply {
    let json_ct = header(request, "Content-Type").is_some_and(|c| {
        c.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("application/json")
    });
    if !json_ct {
        return error(415, "Content-Type must be application/json");
    }
    if request.body_length().is_some_and(|n| n > MAX_BODY_BYTES) {
        return error(413, "request body too large");
    }
    let mut body = Vec::new();
    let read = request
        .as_reader()
        .take(MAX_BODY_BYTES as u64 + 1)
        .read_to_end(&mut body);
    if read.is_err() {
        return error(400, "cannot read request body");
    }
    if body.len() > MAX_BODY_BYTES {
        return error(413, "request body too large");
    }
    let note = match serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("note").and_then(Value::as_str).map(str::to_string))
    {
        Some(n) => n,
        None => return error(400, "body must be {\"note\": \"...\"}"),
    };
    if note.chars().count() > MAX_NOTE_CHARS {
        return error(400, "note is longer than 4000 characters");
    }
    api(|| {
        let found = open_db(paths)?.set_note(id, &note)?;
        Ok(if found { json_reply(200, json!({ "ok": true })) } else { error(404, "no such session") })
    })
}

fn cost_mode_name(m: CostMode) -> &'static str {
    match m {
        CostMode::List => "list",
        CostMode::Custom => "custom",
        CostMode::Tokens => "tokens",
    }
}

fn cost_label(cfg: &Config) -> &'static str {
    match cfg.cost_mode {
        CostMode::List if !cfg.prices.is_empty() => "API list price, with your overrides",
        CostMode::List => "API list price",
        CostMode::Custom => "Custom rates",
        CostMode::Tokens => "",
    }
}

/// The transcript file as it is now: `None` when it is missing, else its modification
/// time in ms (0 if unknown). The stored `transcript_missing` flag can be stale, so it is
/// not consulted.
fn transcript_mtime_ms(row: &SessionRow) -> Option<i64> {
    let m = std::fs::metadata(Path::new(&row.transcript_path)).ok()?;
    Some(
        m.modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_millis() as i64),
    )
}

fn is_resumable(row: &SessionRow, missing: bool) -> bool {
    !missing && row.project_dir.is_some()
}

fn title_of(row: &SessionRow) -> String {
    non_empty(row.title.as_deref())
        .or_else(|| non_empty(row.first_prompt.as_deref()))
        .unwrap_or("Untitled session")
        .to_string()
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.filter(|s| !s.is_empty())
}

/// The last `n` components of `project_dir`, joined with `/`.
fn project_tail(row: &SessionRow, n: usize) -> String {
    let dir = row.project_dir.as_deref().unwrap_or("");
    let parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    parts[parts.len().saturating_sub(n)..].join("/")
}

/// Every session's project label, by session id, worked out over every session in the
/// ledger, so the list (whatever its filters) and the detail view always agree.
fn project_labels_by_id(db: &Db) -> Result<BTreeMap<String, String>> {
    let all = db.sessions(None, true)?;
    let labels = project_labels(&all);
    Ok(all.into_iter().map(|r| r.session_id).zip(labels).collect())
}

fn label_of(labels: &BTreeMap<String, String>, row: &SessionRow) -> String {
    labels.get(&row.session_id).cloned().unwrap_or_else(|| project_tail(row, 1))
}

/// One label per row: the last component, or the last two when different
/// directories in this set share the same last component.
fn project_labels(rows: &[SessionRow]) -> Vec<String> {
    let mut dirs: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    for r in rows {
        dirs.entry(project_tail(r, 1))
            .or_default()
            .insert(r.project_dir.as_deref().unwrap_or(""));
    }
    rows.iter()
        .map(|r| {
            let short = project_tail(r, 1);
            if dirs.get(&short).is_some_and(|s| s.len() > 1) {
                project_tail(r, 2)
            } else {
                short
            }
        })
        .collect()
}

/// Token totals by kind (input, output, cache read, cache write 5m, cache write 1h)
/// and the cost of a set of messages under the configured cost mode.
struct Tally {
    tokens: [u64; 5],
    /// `None` in tokens mode, or when there are messages and none of them is priced.
    cost: Option<f64>,
    /// Some messages are priced and some are not; `cost` covers only the priced ones.
    cost_partial: bool,
}

fn tally<'a>(usage: impl Iterator<Item = &'a MessageUsage>, prices: &PriceTable, cfg: &Config) -> Tally {
    let mut t = [0u64; 5];
    let (mut priced, mut unpriced, mut sum) = (0usize, 0usize, 0.0f64);
    for u in usage {
        for (sum, n) in t.iter_mut().zip([u.input, u.output, u.cache_read, u.cache_write_5m, u.cache_write_1h]) {
            *sum = sum.saturating_add(n);
        }
        match prices.cost(u) {
            Some(c) => {
                priced += 1;
                sum += c;
            }
            None => unpriced += 1,
        }
    }
    let cost = match cfg.cost_mode {
        CostMode::Tokens => None,
        _ if priced == 0 && unpriced > 0 => None,
        CostMode::Custom => Some(sum * cfg.multiplier),
        CostMode::List => Some(sum),
    };
    Tally {
        tokens: t,
        cost,
        cost_partial: cfg.cost_mode != CostMode::Tokens && priced > 0 && unpriced > 0,
    }
}

fn saturating_sum(counts: &[u64]) -> u64 {
    counts.iter().fold(0, |a, &b| a.saturating_add(b))
}

/// The detail view's split of usage between the main conversation and its sub-agents.
fn split_json(usage: &[MessageUsage], subagent_files: usize, prices: &PriceTable, cfg: &Config) -> Value {
    let group = |subagent: bool| {
        let t = tally(usage.iter().filter(|u| u.subagent == subagent), prices, cfg);
        json!({
            "tokens": saturating_sum(&t.tokens),
            "cost": t.cost,
            "cost_partial": t.cost_partial,
        })
    };
    let mut subagents = group(true);
    subagents["count"] = json!(subagent_files);
    json!({ "main": group(false), "subagents": subagents })
}

fn session_json(
    row: &SessionRow,
    usage: &[MessageUsage],
    prices: &PriceTable,
    cfg: &Config,
    now: i64,
    project: String,
) -> Value {
    let mtime = transcript_mtime_ms(row);
    let missing = mtime.is_none();
    let (description, kind) = match (non_empty(row.recap.as_deref()), non_empty(row.first_prompt.as_deref())) {
        (Some(r), _) => (r, "recap"),
        (None, Some(p)) => (p, "first_prompt"),
        _ => ("", "none"),
    };
    let Tally { tokens: t, cost, cost_partial } = tally(usage.iter(), prices, cfg);
    let last_active = row.last_active_at;
    // A long turn writes to the file without firing a hook, so the file's mtime counts too.
    let latest = last_active.max(mtime);
    json!({
        "id": row.session_id,
        "title": title_of(row),
        "description": description,
        "description_kind": kind,
        "project": project,
        "project_dir": row.project_dir.clone().unwrap_or_default(),
        "branch": row.git_branch.clone().unwrap_or_default(),
        "started_at": row.started_at,
        "last_active_at": last_active,
        "active_ms": row.active_ms,
        "running": !row.ended && !missing && latest.is_some_and(|l| now - l < RUNNING_WINDOW_MS),
        "resumable": is_resumable(row, missing),
        "automated": row.automated,
        "tokens": {
            "input": t[0], "output": t[1], "cache_read": t[2],
            "cache_write_5m": t[3], "cache_write_1h": t[4],
            "total": saturating_sum(&t),
        },
        "cost": cost,
        "cost_partial": cost_partial,
        "note": row.note,
    })
}
