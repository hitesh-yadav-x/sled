//! Pure parser for one line of a Claude Code session transcript (JSONL).
//! Works on `serde_json::Value` lookups so unknown shapes fall through to `Kind::Other`.

use serde_json::Value;

const CLIP_CHARS: usize = 300;
const RECAP_CHARS: usize = 500;

/// User text with these prefixes is wrapper output, not something the user typed.
const NOT_PROMPT_PREFIXES: [&str; 7] = [
    "<local-command-",
    "<task-notification",
    "<system-reminder",
    "<bash-input>",
    "<bash-stdout>",
    "<bash-stderr>",
    "[Request interrupted by user",
];

#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub ts_ms: Option<i64>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub entrypoint: Option<String>,
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Title(String),
    Recap(String),
    Prompt(String),
    Usage(MessageUsage),
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MessageUsage {
    pub key: String,
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub web_search: u64,
    pub service_tier: Option<String>,
    pub speed: Option<String>,
    /// From a sub-agent: the line's `isSidechain`, or any line of a file under `subagents/`.
    pub subagent: bool,
}

/// Collapse whitespace runs to one space, trim, and return at most 300 chars in total:
/// when cut, the first 299 followed by '…'.
pub fn clip(text: &str) -> String {
    clip_to(text, CLIP_CHARS)
}

/// Like `clip`, returning at most `max_chars` chars, the ellipsis included.
fn clip_to(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        return collapsed;
    }
    if max_chars == 0 {
        return String::new();
    }
    let head: String = collapsed.chars().take(max_chars - 1).collect();
    format!("{head}…")
}

/// Returns `None` only for blank or non-JSON-object lines.
pub fn parse_line(line: &str) -> Option<Line> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(line).ok()?;
    if !v.is_object() {
        return None;
    }
    let str_field = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let ts_ms = v
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis());
    Some(Line {
        ts_ms,
        cwd: str_field("cwd"),
        git_branch: str_field("gitBranch"),
        entrypoint: str_field("entrypoint"),
        kind: classify(&v).unwrap_or(Kind::Other),
    })
}

fn classify(v: &Value) -> Option<Kind> {
    match v.get("type")?.as_str()? {
        "ai-title" => Some(Kind::Title(clip(v.get("aiTitle")?.as_str()?))),
        "system" if v.get("subtype").and_then(Value::as_str) == Some("away_summary") => {
            Some(Kind::Recap(clip_to(v.get("content")?.as_str()?, RECAP_CHARS)))
        }
        "assistant" => usage_of(v).map(Kind::Usage),
        "user" => prompt_of(v).map(Kind::Prompt),
        _ => None,
    }
}

fn u64_at(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

fn usage_of(v: &Value) -> Option<MessageUsage> {
    let msg = v.get("message")?;
    let id = msg.get("id")?.as_str()?;
    let u = msg.get("usage")?;
    if !u.is_object() {
        return None;
    }
    let model = msg.get("model").and_then(Value::as_str).unwrap_or("");
    if model == "<synthetic>" {
        return None;
    }
    let req = v.get("requestId").and_then(Value::as_str).unwrap_or("");
    let has_breakdown = |c: &&Value| {
        c.get("ephemeral_5m_input_tokens").is_some() || c.get("ephemeral_1h_input_tokens").is_some()
    };
    let (w5m, w1h) = match u.get("cache_creation").filter(|c| c.is_object()).filter(has_breakdown) {
        Some(c) => (
            u64_at(c, "ephemeral_5m_input_tokens"),
            u64_at(c, "ephemeral_1h_input_tokens"),
        ),
        None => (u64_at(u, "cache_creation_input_tokens"), 0),
    };
    let opt = |k: &str| u.get(k).and_then(Value::as_str).map(str::to_owned);
    Some(MessageUsage {
        key: format!("{id}|{req}"),
        model: model.to_owned(),
        input: u64_at(u, "input_tokens"),
        output: u64_at(u, "output_tokens"),
        cache_read: u64_at(u, "cache_read_input_tokens"),
        cache_write_5m: w5m,
        cache_write_1h: w1h,
        web_search: u
            .get("server_tool_use")
            .map(|s| u64_at(s, "web_search_requests"))
            .unwrap_or(0),
        service_tier: opt("service_tier"),
        speed: opt("speed"),
        subagent: v.get("isSidechain").and_then(Value::as_bool) == Some(true),
    })
}

fn prompt_of(v: &Value) -> Option<String> {
    let flagged = |k: &str| v.get(k).and_then(Value::as_bool) == Some(true);
    if flagged("isMeta") || flagged("isSidechain") || flagged("isCompactSummary") {
        return None;
    }
    if v.get("toolUseResult").is_some_and(|r| !r.is_null()) {
        return None;
    }
    let content = v.get("message")?.get("content")?;
    let text = match content {
        Value::String(s) => s.as_str(),
        Value::Array(blocks) => blocks
            .iter()
            .find(|b| b.get("type").and_then(Value::as_str) == Some("text"))?
            .get("text")?
            .as_str()?,
        _ => return None,
    };
    let t = text.trim_start();
    if NOT_PROMPT_PREFIXES.iter().any(|p| t.starts_with(p)) {
        return None;
    }
    if let Some(name) = tag_content(t, "command-name") {
        let name = name.trim().trim_start_matches('/');
        if name.is_empty() {
            return None;
        }
        let args = tag_content(t, "command-args").unwrap_or("").trim();
        return Some(if args.is_empty() {
            clip(&format!("/{name}"))
        } else {
            clip(&format!("/{name} {args}"))
        });
    }
    if t.starts_with("<command-") {
        return None;
    }
    Some(clip(text))
}

/// The text between `<tag>` and the next `</tag>`, if both are present.
fn tag_content<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let len = text[start..].find(&close)?;
    Some(&text[start..start + len])
}
