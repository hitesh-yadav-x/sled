mod common;

use common::{agent, start, Env};
use session_ledger::config::Config;

fn fetch(addr: &str, path: &str) -> (u16, String, Vec<u8>) {
    let mut r = agent().get(format!("http://{addr}{path}")).call().unwrap();
    let status = r.status().as_u16();
    let ct = r
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let body = r.body_mut().read_to_vec().unwrap();
    (status, ct, body)
}

fn fetch_text(addr: &str, path: &str) -> String {
    let (status, _, body) = fetch(addr, path);
    assert_eq!(status, 200, "{path}");
    String::from_utf8(body).unwrap()
}

/// Removes every `open ... close` span from `s`.
fn strip_spans(s: &str, open: &str, close: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find(open) {
        out.push_str(&rest[..i]);
        rest = match rest[i + open.len()..].find(close) {
            Some(j) => &rest[i + open.len() + j + close.len()..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// Drops `//` line comments that start a line or follow whitespace (so `a://b` survives).
fn strip_line_comments(s: &str) -> String {
    s.lines()
        .map(|line| {
            let cut = line
                .match_indices("//")
                .find(|(i, _)| *i == 0 || line[..*i].ends_with(char::is_whitespace))
                .map(|(i, _)| i);
            match cut {
                Some(i) => &line[..i],
                None => line,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn page_has_no_external_references() {
    let env = Env::new();
    let addr = start(&env, Config::default());
    let html = strip_spans(&fetch_text(&addr, "/"), "<!--", "-->");
    let css = strip_spans(&fetch_text(&addr, "/app.css"), "/*", "*/");
    let js = strip_line_comments(&strip_spans(&fetch_text(&addr, "/app.js"), "/*", "*/"));
    for (name, body) in [("index.html", &html), ("app.css", &css), ("app.js", &js)] {
        assert!(!body.trim().is_empty(), "{name} is empty");
        for bad in ["http://", "https://", "googleapis", "gstatic"] {
            assert!(!body.contains(bad), "{name} contains {bad}");
        }
    }
    // The real page, not the placeholder: it loads its own assets and fonts.
    assert!(html.contains("/app.css") && html.contains("/app.js"));
    assert!(css.contains("/fonts/outfit.woff2") && css.contains("/fonts/geist-mono.woff2"));
    assert!(js.contains("/api/sessions"));
}

#[test]
fn fonts_are_served_as_woff2() {
    let env = Env::new();
    let addr = start(&env, Config::default());
    for path in ["/fonts/outfit.woff2", "/fonts/geist-mono.woff2"] {
        let (status, ct, body) = fetch(&addr, path);
        assert_eq!(status, 200, "{path}");
        assert_eq!(ct, "font/woff2", "{path}");
        assert!(body.starts_with(b"wOF2"), "{path} is not woff2");
    }
    for path in ["/fonts/missing.woff2", "/fonts/../Cargo.toml", "/fonts/OFL-Outfit.txt", "/fonts/"] {
        assert_eq!(fetch(&addr, path).0, 404, "{path}");
    }
}

#[test]
fn app_js_does_not_inject_session_text_as_html() {
    let js = include_str!("../assets/app.js");
    assert!(js.contains("textContent"), "app.js should render text with textContent");
    let fields = ["title", "description", "prompt", "note", "project_dir", "branch", "resume_command"];
    for (n, line) in js.lines().enumerate() {
        if line.contains("innerHTML") || line.contains("outerHTML") || line.contains("insertAdjacentHTML") {
            for f in fields {
                assert!(!line.contains(f), "app.js line {}: HTML injection near {f}: {line}", n + 1);
            }
        }
    }
}

const INDEX_HTML: &str = include_str!("../assets/index.html");
const APP_CSS: &str = include_str!("../assets/app.css");
const APP_JS: &str = include_str!("../assets/app.js");

/// Whether `s` calls `eval`: `eval` not preceded by an identifier character
/// (so `retrieval(` is fine, `window.eval (` is not), then optional whitespace and `(`.
fn calls_eval(s: &str) -> bool {
    let b = s.as_bytes();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
    s.match_indices("eval")
        .any(|(i, _)| (i == 0 || !ident(b[i - 1])) && s[i + 4..].trim_start().starts_with('('))
}

/// Whether a CSS `url(` is followed by optional spaces, an optional quote and `//`.
fn css_url_is_protocol_relative(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.match_indices("url(").any(|(i, _)| {
        let rest = lower[i + 4..].trim_start();
        let rest = rest.strip_prefix(['"', '\'']).unwrap_or(rest).trim_start();
        rest.starts_with("//")
    })
}

#[test]
fn eval_detector_matches_calls_only() {
    assert!(calls_eval("eval(x)"));
    assert!(calls_eval("window.eval (x)"));
    assert!(calls_eval("y = eval\t(x)"));
    assert!(!calls_eval("retrieval(x)"));
    assert!(!calls_eval("evaluate(x)"));
    assert!(!calls_eval("my_eval(x)"));
}

#[test]
fn css_url_detector_finds_protocol_relative_urls() {
    assert!(css_url_is_protocol_relative("a { background: url(//cdn.example/a.png) }"));
    assert!(css_url_is_protocol_relative("a { background: url(   //cdn.example/a.png) }"));
    assert!(css_url_is_protocol_relative("a { background: url( '//cdn.example/a.png') }"));
    assert!(css_url_is_protocol_relative("a { background: URL(//cdn.example/a.png) }"));
    assert!(!css_url_is_protocol_relative("src: url(\"/fonts/outfit.woff2\")"));
    assert!(!css_url_is_protocol_relative("src: url(/fonts/outfit.woff2)"));
}

#[test]
fn app_js_has_no_html_sinks_or_dynamic_code() {
    for bad in ["innerHTML", "outerHTML", "insertAdjacentHTML", "document.write", "DOMParser", "new Function"] {
        assert!(!APP_JS.contains(bad), "app.js contains {bad}");
    }
    assert!(!calls_eval(APP_JS), "app.js calls eval");
}

/// Byte offsets of protocol-relative URLs: a `//` straight after a quote, `(` or `=`
/// and followed by a host character. A `//` comment starts a line or follows whitespace.
fn protocol_relative_urls(s: &str) -> Vec<usize> {
    let b = s.as_bytes();
    s.match_indices("//")
        .map(|(i, _)| i)
        .filter(|&i| {
            let before = i > 0 && matches!(b[i - 1], b'"' | b'\'' | b'`' | b'(' | b'=');
            let host = b.get(i + 2).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'[');
            before && host
        })
        .collect()
}

#[test]
fn protocol_relative_detector_finds_urls_but_not_comments() {
    assert_eq!(protocol_relative_urls(r#"<script src="//cdn.example/x.js">"#).len(), 1);
    assert_eq!(protocol_relative_urls("a { background: url(//cdn.example/a.png) }").len(), 1);
    assert_eq!(protocol_relative_urls("<img src=//cdn.example/a.png>").len(), 1);
    assert!(protocol_relative_urls("// a comment\nlet x = 1; // another\nconst s = \"//\";").is_empty());
}

#[test]
fn assets_have_no_remote_urls_or_imports() {
    for (name, body) in [("index.html", INDEX_HTML), ("app.css", APP_CSS), ("app.js", APP_JS)] {
        for bad in ["http://", "https://", "ws://", "wss://", "@import"] {
            assert!(!body.contains(bad), "{name} contains {bad}");
        }
        assert!(!css_url_is_protocol_relative(body), "{name} has a protocol-relative url()");
        let found = protocol_relative_urls(body);
        assert!(found.is_empty(), "{name} has a protocol-relative URL at byte {found:?}");
    }
}

#[test]
fn page_has_the_copy_for_resume_states_copy_failure_and_cost() {
    let page = format!("{INDEX_HTML}\n{APP_JS}");
    for text in [
        "This folder no longer exists, so the command will stop at cd.",
        "Transcript removed, cannot resume. Claude Code deleted this session's file.",
        "No folder was recorded for this session, so there is no command to resume it.",
        "This session's id has unusual characters, so no command is offered.",
        "Could not load this session. Close the drawer and try again.",
        "Could not copy. The command is selected; press Cmd+C or Ctrl+C.",
        "Estimated cost",
        "Could not save. Try again.",
        "No folder",
        "Not updating. Is \"sled serve\" still running?",
        "This note changed elsewhere. Edit it to save your version.",
    ] {
        assert!(page.contains(text), "page is missing {text:?}");
    }
    // Unsaved notes are kept under this key prefix, and the blocked reasons come from the API.
    assert!(APP_JS.contains("\"sled.unsaved.\""));
    for reason in ["transcript_removed", "no_folder", "unsafe_folder", "unsafe_id", "dir_exists"] {
        assert!(APP_JS.contains(reason), "app.js does not read {reason}");
    }
}

/// Seeds a synthetic ledger for the manual browser check; never part of the suite.
/// Run: SLED_SEED_DIR=<empty temp dir> cargo test --test page -- --ignored seed_browser_check
/// It writes `<dir>/claude/projects/...` and `<dir>/data/ledger.db`.
#[test]
#[ignore]
fn seed_browser_check() {
    use common::{assistant_line, prompt_line, recap_line, title_line, usage};
    use session_ledger::db::Db;
    use session_ledger::ingest::ingest_session;

    let root = std::path::PathBuf::from(std::env::var("SLED_SEED_DIR").expect("set SLED_SEED_DIR"));
    let projects = root.join("claude/projects");
    let data = root.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let mut db = Db::open(&data.join("ledger.db")).unwrap();

    const MIN: i64 = 60_000;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    let ts = |ago: i64| {
        (chrono::Utc::now() - chrono::Duration::milliseconds(ago)).format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
    };
    let msg = |sid: &str, id: &str, model: &str, ago: i64, u: serde_json::Value| {
        assistant_line(sid, &ts(ago), id, &format!("req_{id}"), model, u)
    };
    let mut write = |dir: &str, sid: &str, lines: Vec<String>| -> std::path::PathBuf {
        let path = projects.join(dir).join(format!("{sid}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
        ingest_session(&mut db, &path).unwrap();
        path
    };
    let opus = "claude-opus-5-5";
    let sonnet = "claude-sonnet-5-5";

    // 1. HTML-looking title and prompt, project "web".
    let sid = "11111111-0000-4000-8000-000000000001";
    let (cwd, ago) = ("/Users/demo/work/web", 3 * HOUR);
    write("-Users-demo-work-web", sid, vec![
        title_line(sid, "<img src=x onerror=alert(1)>"),
        prompt_line(sid, &ts(ago + 40 * MIN), cwd, "main", "<script>alert(2)</script>"),
        msg(sid, "m1a", sonnet, ago + 30 * MIN, usage(9_000, 14_000, 410_000, 32_000, 0)),
        prompt_line(sid, &ts(ago), cwd, "main", "and <b>bold</b> & \"quotes\""),
        msg(sid, "m1b", sonnet, ago, usage(1_000, 2_000, 50_000, 0, 0)),
    ]);

    // 2. Folder with a space, project "my proj", with a recap.
    let sid = "11111111-0000-4000-8000-000000000002";
    let (cwd, ago) = ("/Users/demo/work/my proj", 5 * HOUR);
    write("-Users-demo-work-my-proj", sid, vec![
        title_line(sid, "Folder with a space"),
        prompt_line(sid, &ts(ago + 48 * MIN), cwd, "feat/pipeline", "set up the build pipeline"),
        msg(sid, "m2a", opus, ago + 40 * MIN, usage(18_000, 22_000, 1_100_000, 60_000, 0)),
        recap_line(sid, &ts(ago + 5 * MIN), "Wired the build stage. The deploy step fails because the task role is missing a permission."),
        prompt_line(sid, &ts(ago), cwd, "feat/pipeline", "why is the deploy stage stuck?"),
    ]);

    // 3. Deleted transcript, project "web", yesterday.
    let sid = "11111111-0000-4000-8000-000000000003";
    let (cwd, ago) = ("/Users/demo/work/web", DAY + 2 * HOUR);
    let gone = write("-Users-demo-work-web", sid, vec![
        title_line(sid, "Session whose file was deleted"),
        prompt_line(sid, &ts(ago + 25 * MIN), cwd, "fix/login", "the login form loses focus"),
        msg(sid, "m3a", sonnet, ago, usage(4_000, 6_000, 190_000, 10_000, 0)),
    ]);
    std::fs::remove_file(gone).unwrap();

    // 4. Unknown model, project "infra".
    let sid = "11111111-0000-4000-8000-000000000004";
    let (cwd, ago) = ("/Users/demo/work/infra", DAY + 5 * HOUR);
    write("-Users-demo-work-infra", sid, vec![
        title_line(sid, "Model with no known price"),
        prompt_line(sid, &ts(ago + 65 * MIN), cwd, "staging-vpc", "turn the staging vpc into a module"),
        msg(sid, "m4a", "claude-mystery-9", ago, usage(26_000, 31_000, 1_600_000, 90_000, 0)),
    ]);

    // 5. Active within the last minute, project "web".
    let sid = "11111111-0000-4000-8000-000000000005";
    let cwd = "/Users/demo/work/web";
    write("-Users-demo-work-web", sid, vec![
        title_line(sid, "Session that is running now"),
        prompt_line(sid, &ts(2 * HOUR + 10 * MIN), cwd, "main", "design the pricing page"),
        msg(sid, "m5a", opus, HOUR, usage(42_000, 61_000, 2_900_000, 110_000, 0)),
        recap_line(sid, &ts(10 * MIN), "Designing the pricing page. The plan table and the checkout form are done."),
        prompt_line(sid, &ts(30_000), cwd, "main", "tighten the spacing"),
    ]);

    // 6. Mixed priced and unpriced usage, project "infra", three days ago.
    let sid = "11111111-0000-4000-8000-000000000006";
    let (cwd, ago) = ("/Users/demo/work/infra", 3 * DAY);
    write("-Users-demo-work-infra", sid, vec![
        title_line(sid, "Rotate CI deploy keys"),
        prompt_line(sid, &ts(ago + 22 * MIN), cwd, "main", "rotate the ci deploy keys"),
        msg(sid, "m6a", sonnet, ago + 10 * MIN, usage(7_000, 8_000, 300_000, 18_000, 0)),
        msg(sid, "m6b", "claude-mystery-9", ago, usage(500, 400, 0, 0, 0)),
    ]);

    // 7. Continued session: shares a message with session 8, which came first.
    let first = "11111111-0000-4000-8000-000000000008";
    let (cwd, ago) = ("/Users/demo/work/infra", 4 * DAY);
    write("-Users-demo-work-infra", first, vec![
        title_line(first, "Original infra session"),
        prompt_line(first, &ts(ago + 30 * MIN), cwd, "main", "plan the subnet layout"),
        msg(first, "shared", opus, ago, usage(3_000, 4_000, 100_000, 0, 0)),
    ]);
    std::thread::sleep(std::time::Duration::from_millis(20));
    let sid = "11111111-0000-4000-8000-000000000007";
    write("-Users-demo-work-infra", sid, vec![
        title_line(sid, "Continued infra session"),
        msg(sid, "shared", opus, ago, usage(3_000, 4_000, 100_000, 0, 0)),
        prompt_line(sid, &ts(2 * DAY), cwd, "main", "continue the subnet work"),
        msg(sid, "m7b", opus, 2 * DAY - MIN, usage(2_000, 3_000, 80_000, 0, 0)),
    ]);

    // 9. Automated run (no prompt), project "web".
    let sid = "11111111-0000-4000-8000-000000000009";
    write("-Users-demo-work-web", sid, vec![
        msg(sid, "m9a", sonnet, 6 * HOUR, usage(100, 200, 0, 0, 0)),
    ]);

    // 10. Older than 14 days, project "web": only under "All time".
    let sid = "11111111-0000-4000-8000-000000000010";
    let (cwd, ago) = ("/Users/demo/work/web", 20 * DAY);
    write("-Users-demo-work-web", sid, vec![
        title_line(sid, "Old session from last month"),
        prompt_line(sid, &ts(ago + 12 * MIN), cwd, "main", "tidy the readme"),
        msg(sid, "m10a", sonnet, ago, usage(900, 700, 0, 0, 0)),
    ]);

    // 11. Two sub-agent files, one on an unknown model, project "web". Session 1 has none.
    let sid = "11111111-0000-4000-8000-000000000011";
    let (cwd, ago) = ("/Users/demo/work/web", 4 * HOUR);
    let agents = projects.join("-Users-demo-work-web").join(sid).join("subagents");
    std::fs::create_dir_all(&agents).unwrap();
    for (agent, id, model, u) in [
        ("agent-a1", "m11c", sonnet, usage(30_000, 45_000, 3_300_000, 125_000, 0)),
        ("agent-a2", "m11d", "claude-mystery-9", usage(40_000, 60_000, 3_800_000, 100_000, 0)),
    ] {
        let line = msg(sid, id, model, ago + 10 * MIN, u);
        std::fs::write(agents.join(format!("{agent}.jsonl")), format!("{line}\n")).unwrap();
    }
    write("-Users-demo-work-web", sid, vec![
        title_line(sid, "Session with two sub-agents"),
        prompt_line(sid, &ts(ago + 30 * MIN), cwd, "main", "survey the repo with sub-agents"),
        msg(sid, "m11a", opus, ago + 20 * MIN, usage(6_000, 10_000, 240_000, 10_000, 0)),
    ]);

    // 12. Folder that exists on disk, so the resume card has no "folder gone" line.
    let sid = "11111111-0000-4000-8000-000000000012";
    let real = root.join("work/real");
    std::fs::create_dir_all(&real).unwrap();
    let real = real.to_str().unwrap().to_string();
    write("-work-real", sid, vec![
        title_line(sid, "Folder that still exists"),
        prompt_line(sid, &ts(7 * HOUR), &real, "main", "check the folder line"),
        msg(sid, "m12a", sonnet, 7 * HOUR - MIN, usage(1_000, 1_000, 0, 0, 0)),
    ]);

    // 13. No folder recorded: the prompt carries no cwd. Lists under "No folder".
    let sid = "11111111-0000-4000-8000-000000000013";
    let mut no_cwd: serde_json::Value =
        serde_json::from_str(&prompt_line(sid, &ts(8 * HOUR), "", "", "a session with no folder")).unwrap();
    no_cwd.as_object_mut().unwrap().remove("cwd");
    no_cwd.as_object_mut().unwrap().remove("gitBranch");
    write("-no-folder", sid, vec![
        title_line(sid, "Session with no folder"),
        no_cwd.to_string(),
        msg(sid, "m13a", sonnet, 8 * HOUR - MIN, usage(2_000, 1_000, 0, 0, 0)),
    ]);

    // 14. Relative folder: resume is blocked as no_folder.
    let sid = "11111111-0000-4000-8000-000000000014";
    write("-relative", sid, vec![
        title_line(sid, "Session with a relative folder"),
        prompt_line(sid, &ts(9 * HOUR), "relative/place", "main", "relative folder"),
        msg(sid, "m14a", sonnet, 9 * HOUR - MIN, usage(1_500, 1_000, 0, 0, 0)),
    ]);

    // 15. Unsafe id (starts with a dash): resume is blocked as unsafe_id.
    let sid = "-unsafe-session-id";
    write("-Users-demo-work-web", sid, vec![
        title_line(sid, "Session with an unusual id"),
        prompt_line(sid, &ts(10 * HOUR), "/Users/demo/work/web", "main", "unusual id"),
        msg(sid, "m15a", sonnet, 10 * HOUR - MIN, usage(1_200, 1_000, 0, 0, 0)),
    ]);

    // 17. Root folder: its project label is the empty string, yet it has a folder,
    // so it must list apart from "No folder".
    let sid = "11111111-0000-4000-8000-000000000017";
    write("-", sid, vec![
        title_line(sid, "Session in the root folder"),
        prompt_line(sid, &ts(11 * HOUR), "/", "", "work at the root"),
        msg(sid, "m17a", sonnet, 11 * HOUR - MIN, usage(1_100, 1_000, 0, 0, 0)),
    ]);

    // 18. Over a billion tokens, in a long folder whose last component must stay visible.
    let sid = "11111111-0000-4000-8000-000000000018";
    write("-Users-demo-Documents-projects-my-app", sid, vec![
        title_line(sid, "Session with billions of tokens"),
        prompt_line(sid, &ts(12 * HOUR), "/Users/demo/Documents/projects/my-app", "feat/big-numbers", "big"),
        msg(sid, "m18a", sonnet, 12 * HOUR - MIN, usage(1_700_000, 0, 4_280_000_000, 0, 0)),
    ]);

    // 19. A last component wider than the column on its own.
    let sid = "11111111-0000-4000-8000-000000000019";
    let long = "/Users/demo/work/a-very-long-project-folder-name-that-is-wider-than-the-column-itself";
    write("-Users-demo-work-long", sid, vec![
        title_line(sid, "Session with a very long folder name"),
        prompt_line(sid, &ts(13 * HOUR), long, "main", "long"),
        msg(sid, "m19a", sonnet, 13 * HOUR - MIN, usage(999_950, 0, 0, 0, 0)),
    ]);

    // 20. A relative folder with no "/".
    let sid = "11111111-0000-4000-8000-000000000020";
    write("-solo", sid, vec![
        title_line(sid, "Session in a folder with no slash"),
        prompt_line(sid, &ts(14 * HOUR), "solo", "main", "solo"),
        msg(sid, "m20a", sonnet, 14 * HOUR - MIN, usage(1_000, 0, 0, 0, 0)),
    ]);

    eprintln!("seeded {}", root.display());
}
