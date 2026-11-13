mod common;
use common::*;
use serde_json::{json, Value};
use session_ledger::transcript::{clip, parse_line, Kind};

const TS: &str = "2026-11-13T05:30:00.000Z";

fn kind(line: &str) -> Kind {
    parse_line(line).expect("should parse").kind
}

fn with(line: String, f: impl FnOnce(&mut Value)) -> String {
    let mut v: Value = serde_json::from_str(&line).unwrap();
    f(&mut v);
    v.to_string()
}

#[test]
fn title() {
    assert!(matches!(kind(&title_line("s", "Fix auth")), Kind::Title(t) if t == "Fix auth"));
}

#[test]
fn recap() {
    let l = parse_line(&recap_line("s", TS, "Did things.\n\nMore.")).unwrap();
    assert_eq!(l.ts_ms, Some(1794547800000));
    assert_eq!(l.entrypoint.as_deref(), Some("cli"));
    assert!(matches!(l.kind, Kind::Recap(t) if t == "Did things. More."));
}

#[test]
fn recap_under_500_chars_is_kept_whole_but_collapsed() {
    let text = format!("{}\n\n  end", "r".repeat(480));
    let Kind::Recap(r) = kind(&recap_line("s", TS, &text)) else { panic!("not recap") };
    assert_eq!(r, format!("{} end", "r".repeat(480)));
}

#[test]
fn recap_over_500_chars_is_cut_with_ellipsis() {
    let Kind::Recap(r) = kind(&recap_line("s", TS, &"é".repeat(700))) else { panic!("not recap") };
    assert_eq!(r, format!("{}…", "é".repeat(499)));
    assert_eq!(r.chars().count(), 500);
    let Kind::Recap(r) = kind(&recap_line("s", TS, &"x".repeat(500))) else { panic!("not recap") };
    assert_eq!(r, "x".repeat(500));
}

#[test]
fn title_is_clipped_to_300_chars() {
    let Kind::Title(t) = kind(&title_line("s", &format!("  a \n b{}", "x".repeat(400)))) else {
        panic!("not title")
    };
    assert_eq!(t, clip(&format!("a b{}", "x".repeat(400))));
    assert_eq!(t.chars().count(), 300);
    assert!(t.starts_with("a bx") && t.ends_with('…'));
}

#[test]
fn usage_full() {
    let l = parse_line(&assistant_line(
        "s", TS, "msg_1", "req_1", "claude-opus-4-1",
        usage(1000, 2000, 100000, 10000, 20000),
    ))
    .unwrap();
    let Kind::Usage(u) = l.kind else { panic!("not usage") };
    assert_eq!(u.key, "msg_1|req_1");
    assert_eq!(u.model, "claude-opus-4-1");
    assert_eq!(
        (u.input, u.output, u.cache_read, u.cache_write_5m, u.cache_write_1h),
        (1000, 2000, 100000, 10000, 20000)
    );
    assert_eq!(u.web_search, 0);
    assert_eq!(u.service_tier.as_deref(), Some("standard"));
    assert_eq!(u.speed.as_deref(), Some("standard"));
}

#[test]
fn usage_without_cache_breakdown_goes_to_5m() {
    let line = assistant_line("s", TS, "m", "r", "claude-opus-4-1", usage(1, 2, 3, 0, 0));
    let line = with(line, |v| {
        let u = &mut v["message"]["usage"];
        u.as_object_mut().unwrap().remove("cache_creation");
        u["cache_creation_input_tokens"] = json!(500);
    });
    let Kind::Usage(u) = kind(&line) else { panic!("not usage") };
    assert_eq!((u.cache_write_5m, u.cache_write_1h), (500, 0));
}

#[test]
fn cache_creation_object_without_ephemeral_keys_goes_to_5m() {
    let line = assistant_line("s", TS, "m", "r", "claude-opus-4-1", usage(1, 2, 3, 0, 0));
    let line = with(line, |v| {
        let u = &mut v["message"]["usage"];
        u["cache_creation"] = json!({});
        u["cache_creation_input_tokens"] = json!(500);
    });
    let Kind::Usage(u) = kind(&line) else { panic!("not usage") };
    assert_eq!((u.cache_write_5m, u.cache_write_1h), (500, 0));
}

#[test]
fn sidechain_assistant_line_is_marked_subagent() {
    let line = assistant_line("s", TS, "m", "r", "claude-opus-4-1", usage(1, 2, 3, 0, 0));
    let subagent = |line: &str| match kind(line) {
        Kind::Usage(u) => u.subagent,
        other => panic!("not usage: {other:?}"),
    };
    assert!(!subagent(&line), "absent");
    assert!(!subagent(&with(line.clone(), |v| v["isSidechain"] = json!(false))), "false");
    assert!(subagent(&with(line, |v| v["isSidechain"] = json!(true))), "true");
}

#[test]
fn synthetic_model_is_other() {
    let line = assistant_line("s", TS, "m", "r", "<synthetic>", usage(0, 0, 0, 0, 0));
    assert!(matches!(kind(&line), Kind::Other));
}

#[test]
fn prompt_string() {
    let l = parse_line(&prompt_line("s", TS, "/work/app", "main", "  hello \n world ")).unwrap();
    assert_eq!(l.cwd.as_deref(), Some("/work/app"));
    assert_eq!(l.git_branch.as_deref(), Some("main"));
    assert!(matches!(l.kind, Kind::Prompt(t) if t == "hello world"));
}

#[test]
fn prompt_text_block_with_image() {
    let line = with(prompt_line("s", TS, "/w", "main", ""), |v| {
        v["message"]["content"] = json!([
            {"type":"image","source":{"type":"base64","data":"AAAA"}},
            {"type":"text","text":"look at this"},
            {"type":"text","text":"second"}
        ]);
    });
    assert!(matches!(kind(&line), Kind::Prompt(t) if t == "look at this"));
}

#[test]
fn tool_result_is_other() {
    assert!(matches!(kind(&tool_result_line("s", TS)), Kind::Other));
}

#[test]
fn null_tool_use_result_counts_as_absent() {
    let line = with(prompt_line("s", TS, "/w", "main", "hi"), |v| v["toolUseResult"] = Value::Null);
    assert_eq!(kind(&line), Kind::Prompt("hi".into()));
}

#[test]
fn meta_sidechain_and_compact_summary_are_other() {
    for flag in ["isMeta", "isSidechain", "isCompactSummary"] {
        let line = with(prompt_line("s", TS, "/w", "main", "hi"), |v| v[flag] = json!(true));
        assert!(matches!(kind(&line), Kind::Other), "{flag}");
    }
}

fn prompt_text(text: &str) -> Kind {
    kind(&prompt_line("s", TS, "/w", "m", text))
}

#[test]
fn slash_command_with_args_is_a_prompt() {
    let text = "<command-name>/review</command-name>\n<command-message>review</command-message>\n\
                <command-args>the auth module</command-args>";
    assert_eq!(prompt_text(text), Kind::Prompt("/review the auth module".into()));
}

#[test]
fn slash_command_without_args_is_its_name() {
    let text = "<command-name>/clear</command-name>\n<command-message>clear</command-message>\n\
                <command-args></command-args>";
    assert_eq!(prompt_text(text), Kind::Prompt("/clear".into()));
    assert_eq!(prompt_text("<command-name>/clear</command-name>"), Kind::Prompt("/clear".into()));
    // A name without its leading slash gains exactly one.
    assert_eq!(prompt_text("<command-name>clear</command-name>"), Kind::Prompt("/clear".into()));
}

#[test]
fn empty_command_name_is_other() {
    for text in [
        "<command-name></command-name>",
        "<command-name>  </command-name><command-args>x</command-args>",
        "<command-name>/</command-name>",
    ] {
        assert!(matches!(prompt_text(text), Kind::Other), "{text}");
    }
}

#[test]
fn slash_command_tags_in_any_order() {
    let text = "<command-message>review</command-message>\n  <command-name>/review</command-name>\n\
                <command-args>\n  the   auth module \n</command-args>";
    assert_eq!(prompt_text(text), Kind::Prompt("/review the auth module".into()));
}

#[test]
fn slash_command_prompt_is_clipped() {
    let text = format!("<command-name>/review</command-name><command-args>{}</command-args>", "a".repeat(400));
    assert_eq!(prompt_text(&text), Kind::Prompt(clip(&format!("/review {}", "a".repeat(400)))));
}

#[test]
fn bash_mode_and_interrupt_lines_are_other() {
    for text in [
        "<bash-input>ls</bash-input>",
        "<bash-stdout>file</bash-stdout><bash-stderr></bash-stderr>",
        "<bash-stderr>oops</bash-stderr>",
        "[Request interrupted by user]",
        "[Request interrupted by user for tool use]",
    ] {
        assert!(matches!(prompt_text(text), Kind::Other), "{text}");
    }
}

#[test]
fn slash_command_wrapper_is_other() {
    for text in [
        "<local-command-stdout>x</local-command-stdout>",
        "<local-command-stdout><command-name>/x</command-name></local-command-stdout>",
        "  <task-notification>x",
        "<system-reminder>x",
    ] {
        assert!(matches!(kind(&prompt_line("s", TS, "/w", "m", text)), Kind::Other), "{text}");
    }
}

#[test]
fn clip_cuts_at_300_chars_not_bytes() {
    let out = clip(&"é".repeat(400));
    assert_eq!(out, format!("{}…", "é".repeat(299)));
    assert_eq!(out.chars().count(), 300);
    assert_eq!(clip(&"x".repeat(301)).chars().count(), 300);
    assert_eq!(clip("a \n\n b"), "a b");
    assert_eq!(clip(&"x".repeat(300)), "x".repeat(300));
}

#[test]
fn garbage_never_panics() {
    for s in ["", "   ", "not json", "[1,2]", "{\"type\":5}", "{\"type\":\"assistant\"}"] {
        match parse_line(s) {
            None => {}
            Some(l) => assert!(matches!(l.kind, Kind::Other), "{s}"),
        }
    }
    assert!(parse_line("").is_none());
    assert!(parse_line("[1,2]").is_none());
}
