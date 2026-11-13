mod common;
use common::*;
use serde_json::{json, Value};
use session_ledger::install::{add_hooks, confirm_retention, hook_command, remove_hooks};

const CMD: &str = "/usr/local/bin/sled hook";

fn ours(command: &str) -> Value {
    json!({"hooks":[{"type":"command","command":command,"timeout":10}]})
}

fn read_settings(env: &Env) -> Value {
    serde_json::from_str(&std::fs::read_to_string(env.paths().settings_file()).unwrap()).unwrap()
}

fn backups(env: &Env) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(&env.claude)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("settings.json.sled-backup-"))
        .collect()
}

#[test]
fn adds_three_events_to_empty_settings() {
    let mut s = json!({});
    assert!(add_hooks(&mut s, CMD));
    for ev in ["SessionStart", "Stop", "SessionEnd"] {
        assert_eq!(s["hooks"][ev], json!([ours(CMD)]), "{ev}");
    }
}

#[test]
fn second_install_changes_nothing() {
    let mut s = json!({});
    assert!(add_hooks(&mut s, CMD));
    let after_first = s.clone();
    assert!(!add_hooks(&mut s, CMD));
    assert_eq!(s, after_first);
    for ev in ["SessionStart", "Stop", "SessionEnd"] {
        assert_eq!(s["hooks"][ev].as_array().unwrap().len(), 1);
    }
}

#[test]
fn reinstall_from_new_path_updates_in_place() {
    let mut s = json!({});
    add_hooks(&mut s, CMD);
    let new = "\"/opt/my tools/sled\" hook";
    assert!(add_hooks(&mut s, new));
    for ev in ["SessionStart", "Stop", "SessionEnd"] {
        assert_eq!(s["hooks"][ev], json!([ours(new)]), "{ev}");
    }
}

#[test]
fn entry_written_carries_a_ten_second_timeout() {
    let mut s = json!({});
    add_hooks(&mut s, CMD);
    for ev in ["SessionStart", "Stop", "SessionEnd"] {
        let hook = &s["hooks"][ev][0]["hooks"][0];
        assert_eq!(hook, &json!({"type":"command","command":CMD,"timeout":10}), "{ev}");
        let keys: Vec<&String> = hook.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["type", "command", "timeout"], "{ev}");
    }
}

#[test]
fn our_entry_without_timeout_gains_it_on_reinstall() {
    let old = json!({"hooks":[{"type":"command","command":CMD}]});
    let mut s = json!({"hooks":{"SessionStart":[old.clone()],"Stop":[old.clone()],"SessionEnd":[old]}});
    assert!(add_hooks(&mut s, CMD));
    for ev in ["SessionStart", "Stop", "SessionEnd"] {
        assert_eq!(s["hooks"][ev], json!([ours(CMD)]), "{ev}");
    }
    assert!(!add_hooks(&mut s, CMD));
    assert!(remove_hooks(&mut s));
    assert!(s.get("hooks").is_none());
}

#[test]
fn failed_first_sweep_says_hooks_are_installed() {
    let env = Env::new();
    let not_a_dir = env.tmp.path().join("data-is-a-file");
    std::fs::write(&not_a_dir, "x").unwrap();
    let out = env.sled().env("SLED_DATA_DIR", &not_a_dir).arg("install").output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("hooks were installed"), "{err}");
    assert!(err.contains("sled sweep"), "{err}");
    assert!(read_settings(&env)["hooks"]["Stop"].is_array());
}

#[test]
fn lookalike_commands_are_not_ours() {
    let mut s = json!({"hooks":{"Stop":[ours("/x/sledge hook"), ours("/x/sled hookah")]}});
    let original = s.clone();
    assert!(add_hooks(&mut s, CMD));
    assert_eq!(s["hooks"]["Stop"].as_array().unwrap().len(), 3);
    assert!(remove_hooks(&mut s));
    assert_eq!(s["hooks"]["Stop"], original["hooks"]["Stop"]);
}

#[test]
fn other_hooks_and_keys_are_preserved() {
    let original = json!({
        "model": "opus",
        "statusLine": {"type":"command","command":"echo hi"},
        "hooks": {
            "Stop": [{"hooks":[{"type":"command","command":"notify-send done"}]}],
            "PreToolUse": [{"matcher":"Bash","hooks":[{"type":"command","command":"check.sh"}]}]
        },
        "env": {"A":"1"}
    });
    let mut s = original.clone();
    assert!(add_hooks(&mut s, CMD));
    assert!(remove_hooks(&mut s));
    assert_eq!(s, original);
    assert_eq!(
        serde_json::to_string(&s).unwrap(),
        serde_json::to_string(&original).unwrap(),
        "key order must match"
    );
}

#[test]
fn uninstall_removes_only_ours() {
    // An entry mixing our command with a foreign one keeps the foreign one.
    let mut s = json!({"hooks":{"Stop":[
        {"matcher":"x","hooks":[
            {"type":"command","command":"/a/sled hook"},
            {"type":"command","command":"other"}]},
        ours("\"/b c/sled\" hook")
    ]}});
    assert!(remove_hooks(&mut s));
    assert_eq!(
        s,
        json!({"hooks":{"Stop":[{"matcher":"x","hooks":[{"type":"command","command":"other"}]}]}})
    );
    assert!(!remove_hooks(&mut s));

    // When nothing else is left, the hooks object disappears.
    let mut t = json!({"model":"m"});
    add_hooks(&mut t, CMD);
    assert!(remove_hooks(&mut t));
    assert_eq!(t, json!({"model":"m"}));
}

#[test]
fn uninstall_command_removes_hooks_from_file() {
    let env = Env::new();
    env.sled().arg("install").assert().success();
    assert!(read_settings(&env)["hooks"]["Stop"].is_array());
    env.sled().arg("uninstall").assert().success();
    assert!(read_settings(&env).get("hooks").is_none());
}

#[test]
fn invalid_settings_file_is_untouched() {
    let env = Env::new();
    let file = env.paths().settings_file();
    std::fs::write(&file, "not json {").unwrap();
    env.sled().arg("install").assert().failure();
    assert_eq!(std::fs::read(&file).unwrap(), b"not json {");
    assert!(backups(&env).is_empty());

    std::fs::write(&file, "[1,2]").unwrap();
    env.sled().arg("install").assert().failure();
    assert_eq!(std::fs::read(&file).unwrap(), b"[1,2]");
    assert!(backups(&env).is_empty());
}

#[test]
fn backup_is_written_before_change() {
    let env = Env::new();
    let original = "{\n  \"model\": \"opus\"\n}\n";
    std::fs::write(env.paths().settings_file(), original).unwrap();
    let out = env.sled().arg("install").output().unwrap();
    assert!(out.status.success());
    let b = backups(&env);
    assert_eq!(b.len(), 1);
    assert_eq!(std::fs::read_to_string(&b[0]).unwrap(), original);
    assert!(String::from_utf8_lossy(&out.stdout).contains(&b[0].display().to_string()));
    let text = std::fs::read_to_string(env.paths().settings_file()).unwrap();
    assert!(text.ends_with("}\n"));
    assert!(text.contains("\n  \"model\": \"opus\""));
}

#[test]
fn no_change_means_no_backup() {
    let env = Env::new();
    std::fs::write(env.paths().settings_file(), "{}").unwrap();
    env.sled().arg("install").assert().success();
    assert_eq!(backups(&env).len(), 1);
    env.sled().arg("install").assert().success();
    assert_eq!(backups(&env).len(), 1);
}

#[test]
fn retention_set_writes_value_and_keep_does_not() {
    let env = Env::new();
    env.sled().args(["install", "--retention-days", "400"]).assert().success();
    assert_eq!(read_settings(&env)["cleanupPeriodDays"], json!(400));

    let env = Env::new();
    env.sled().args(["install", "--keep-retention"]).assert().success();
    assert!(read_settings(&env).get("cleanupPeriodDays").is_none());
}

#[test]
fn retention_not_lowered_when_already_high() {
    let env = Env::new();
    std::fs::write(env.paths().settings_file(), r#"{"cleanupPeriodDays":999}"#).unwrap();
    env.sled().args(["install", "--retention-days", "365"]).assert().success();
    assert_eq!(read_settings(&env)["cleanupPeriodDays"], json!(999));
}

#[test]
fn non_terminal_stdin_keeps_retention() {
    let env = Env::new();
    env.sled().arg("install").write_stdin("y\n").assert().success();
    assert!(read_settings(&env).get("cleanupPeriodDays").is_none());
}

#[test]
fn install_runs_first_sweep() {
    let env = Env::new();
    env.write_session("p", "s1", &[prompt_line("s1", "2026-01-01T00:00:00Z", "/w", "main", "hello")]);
    let out = env.sled().arg("install").output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Indexed 1 sessions (1 new)."), "{stdout}");
    assert!(stdout.contains("Run \"sled serve\" to open the ledger."), "{stdout}");
}

#[test]
fn retention_is_never_lowered() {
    let env = Env::new();
    std::fs::write(env.paths().settings_file(), r#"{"cleanupPeriodDays":200}"#).unwrap();
    let out = env.sled().args(["install", "--retention-days", "100"]).output().unwrap();
    assert!(out.status.success());
    assert_eq!(read_settings(&env)["cleanupPeriodDays"], json!(200));
    assert!(String::from_utf8_lossy(&out.stdout).contains("Retention left at 200 days."));

    for n in ["7", "0"] {
        let env = Env::new();
        let out = env.sled().args(["install", "--retention-days", n]).output().unwrap();
        assert!(out.status.success());
        assert!(read_settings(&env).get("cleanupPeriodDays").is_none(), "{n}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("Retention left at 30 (default) days."));
    }
}

#[cfg(unix)]
#[test]
fn symlinked_settings_stays_a_symlink_and_target_is_updated() {
    let env = Env::new();
    let real = env.tmp.path().join("dotfiles-settings.json");
    std::fs::write(&real, r#"{"model":"opus"}"#).unwrap();
    let link = env.paths().settings_file();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    env.sled().args(["install", "--keep-retention"]).assert().success();
    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&real).unwrap()).unwrap();
    assert_eq!(v["model"], json!("opus"));
    assert!(v["hooks"]["Stop"].is_array());
}

#[cfg(unix)]
#[test]
fn settings_file_mode_is_preserved() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    let file = env.paths().settings_file();
    std::fs::write(&file, "{}").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    env.sled().args(["install", "--keep-retention"]).assert().success();
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
}

#[test]
fn hooks_with_unexpected_shape_is_an_error_and_file_untouched() {
    for body in [r#"{"hooks":[]}"#, r#"{"hooks":{"Stop":{"a":1}}}"#] {
        let env = Env::new();
        let file = env.paths().settings_file();
        std::fs::write(&file, body).unwrap();
        let out = env.sled().arg("install").output().unwrap();
        assert!(!out.status.success(), "{body}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("settings.json") && (err.contains("hooks is not an object") || err.contains("hooks.Stop is not an array")), "{err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), body);
        assert!(backups(&env).is_empty());
    }
}

// ---- hardening ----

/// Backups sorted oldest first by (stamp, collision number).
fn backups_in_order(env: &Env) -> Vec<std::path::PathBuf> {
    let mut b = backups(env);
    b.sort_by_key(|p| {
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        let rest = name.trim_start_matches("settings.json.sled-backup-").to_string();
        let (stamp, n) = match rest.get(15..) {
            Some(suffix) if !suffix.is_empty() => (rest[..15].to_string(), suffix.trim_start_matches('-').parse::<u32>().unwrap()),
            _ => (rest.clone(), 1),
        };
        (stamp, n)
    });
    b
}

#[test]
fn backup_names_never_collide() {
    let env = Env::new();
    let file = env.paths().settings_file();
    let original = "{\n  \"model\": \"opus\"\n}\n";
    std::fs::write(&file, original).unwrap();
    // Decoy backups for every second around now, so both runs land on a taken name.
    let now = chrono::Local::now();
    let mut decoys = Vec::new();
    for s in -2..30 {
        let stamp = (now + chrono::Duration::seconds(s)).format("%Y%m%d-%H%M%S");
        let decoy = env.claude.join(format!("settings.json.sled-backup-{stamp}"));
        std::fs::write(&decoy, "decoy").unwrap();
        decoys.push(decoy);
    }
    env.sled().args(["install", "--keep-retention"]).assert().success();
    let installed = std::fs::read_to_string(&file).unwrap();
    env.sled().arg("uninstall").assert().success();

    for d in &decoys {
        assert_eq!(std::fs::read_to_string(d).unwrap(), "decoy", "{}", d.display());
    }
    let ours: Vec<_> = backups_in_order(&env).into_iter().filter(|p| !decoys.contains(p)).collect();
    assert_eq!(ours.len(), 2, "{ours:?}");
    assert!(ours.iter().all(|p| p.to_string_lossy().ends_with("-2") || p.to_string_lossy().ends_with("-3")), "{ours:?}");
    assert_eq!(std::fs::read_to_string(&ours[0]).unwrap(), original);
    assert_eq!(std::fs::read_to_string(&ours[1]).unwrap(), installed);
}

#[test]
fn quick_successive_changes_leave_distinct_backups() {
    let env = Env::new();
    let file = env.paths().settings_file();
    let original = "{\n  \"model\": \"opus\"\n}\n";
    std::fs::write(&file, original).unwrap();
    env.sled().args(["install", "--keep-retention"]).assert().success();
    env.sled().arg("uninstall").assert().success();
    let b = backups_in_order(&env);
    assert_eq!(b.len(), 2, "{b:?}");
    assert_ne!(b[0], b[1]);
    assert_eq!(std::fs::read_to_string(&b[0]).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn backup_keeps_the_settings_file_mode() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    let file = env.paths().settings_file();
    std::fs::write(&file, "{}").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    env.sled().args(["install", "--keep-retention"]).assert().success();
    let b = backups(&env);
    assert_eq!(b.len(), 1);
    assert_eq!(std::fs::metadata(&b[0]).unwrap().permissions().mode() & 0o777, 0o600);
}

const AWKWARD_DIRS: [&str; 7] =
    ["/opt/my tools", "/opt/a$b", "/opt/a(b", "/opt/a&b", "/opt/a\tb", "/opt/a\"b", "/opt/it's"];

#[test]
fn awkward_paths_round_trip() {
    for dir in AWKWARD_DIRS {
        let cmd = hook_command(&std::path::Path::new(dir).join("sled"));
        assert!(cmd.starts_with('\'') && cmd.ends_with("/sled' hook"), "{dir:?}: {cmd}");
        let mut s = json!({});
        assert!(add_hooks(&mut s, CMD));
        assert!(add_hooks(&mut s, &cmd), "{dir:?}");
        for ev in ["SessionStart", "Stop", "SessionEnd"] {
            assert_eq!(s["hooks"][ev], json!([ours(&cmd)]), "{dir:?} {ev}");
        }
        assert!(!add_hooks(&mut s, &cmd), "{dir:?}");
        assert!(add_hooks(&mut s, CMD), "{dir:?}");
        assert!(add_hooks(&mut s, &cmd), "{dir:?}");
        assert!(remove_hooks(&mut s), "{dir:?}");
        assert_eq!(s, json!({}), "{dir:?}");
    }
}

#[test]
fn plain_paths_stay_unquoted() {
    assert_eq!(hook_command(std::path::Path::new("/usr/local/bin/sled")), CMD);
    assert_eq!(hook_command(std::path::Path::new("/a/b_c-d.e/sled")), "/a/b_c-d.e/sled hook");
}

/// The quoted command, run by a real shell, starts the program at the awkward path with `hook`.
#[cfg(unix)]
#[test]
fn quoted_command_runs_under_a_shell() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    for (i, name) in ["my tools", "a$b", "a(b", "a&b", "a\tb", "a\"b", "it's"].into_iter().enumerate() {
        let dir = tmp.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("sled");
        std::fs::write(&fake, "#!/bin/sh\nprintf '%s|%s' \"$#\" \"$1\" > \"$OUT\"\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = tmp.path().join(format!("out{i}"));
        let cmd = hook_command(&fake);
        let status = std::process::Command::new("/bin/sh").arg("-c").arg(&cmd).env("OUT", &out).status().unwrap();
        assert!(status.success(), "{name:?}: {cmd}");
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "1|hook", "{name:?}: {cmd}");
    }
}

#[test]
fn quoted_forms_of_our_command_are_recognised() {
    for cmd in [
        "/usr/local/bin/sled hook",
        "\"/opt/my tools/sled\" hook",
        r"'/opt/it'\''s/sled' hook",
        "'/opt/a$b/sled'   hook  ",
        "'/opt/my tools/sled' hook",
        "/opt/x/sled\thook\t",
        "sled hook",
    ] {
        let mut s = json!({"hooks":{"Stop":[ours(cmd)]}});
        assert!(remove_hooks(&mut s), "{cmd}");
        assert_eq!(s, json!({}), "{cmd}");
    }
}

#[test]
fn more_lookalikes_are_not_ours() {
    for cmd in [
        "/x/sledge hook",
        "/x/sled hookah",
        "/x/sled hook; x",
        "'/x/sled' hook extra",
        "'/x/sled'hook",
        "sled",
        "'/x/sled",
        "/x/sled\nhook",
        "/x/sled\r\nhook",
        "/x/sled hook\n",
        "/x/sled/ hook",
        "/x/sled/. hook",
    ] {
        let mut s = json!({"hooks":{"Stop":[ours(cmd)]}});
        assert!(!remove_hooks(&mut s), "{cmd}");
        assert!(add_hooks(&mut s, CMD), "{cmd}");
        assert_eq!(s["hooks"]["Stop"][0], ours(cmd), "{cmd}");
        assert_eq!(s["hooks"]["Stop"].as_array().unwrap().len(), 2, "{cmd}");
    }
}

#[test]
fn duplicate_entries_of_ours_are_collapsed() {
    let mut s = json!({"hooks":{
        "Stop":[
            ours("/old/sled hook"),
            {"matcher":"x","hooks":[
                {"type":"command","command":"other"},
                {"type":"command","command":"/older/sled hook"}]},
            ours("'/b c/sled' hook")
        ],
        "SessionStart":[{"hooks":[
            {"type":"command","command":"/a/sled hook","timeout":10},
            {"type":"command","command":"/a/sled hook","timeout":10}]}],
        "SessionEnd":[ours(CMD)]
    }});
    assert!(add_hooks(&mut s, CMD));
    assert_eq!(
        s["hooks"]["Stop"],
        json!([ours(CMD), {"matcher":"x","hooks":[{"type":"command","command":"other"}]}])
    );
    assert_eq!(s["hooks"]["SessionStart"], json!([ours(CMD)]));
    assert_eq!(s["hooks"]["SessionEnd"], json!([ours(CMD)]));
    assert!(!add_hooks(&mut s, CMD));

    // Duplicates alone, already current, still count as a change.
    let mut t = json!({"hooks":{"SessionStart":[ours(CMD)],"Stop":[ours(CMD), ours(CMD)],"SessionEnd":[ours(CMD)]}});
    assert!(add_hooks(&mut t, CMD));
    assert_eq!(t["hooks"]["Stop"], json!([ours(CMD)]));
}

#[cfg(unix)]
#[test]
fn dangling_symlink_is_written_at_its_target() {
    let env = Env::new();
    let real = env.tmp.path().join("dotfiles").join("settings.json");
    std::fs::create_dir_all(real.parent().unwrap()).unwrap();
    let link = env.paths().settings_file();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    env.sled().args(["install", "--keep-retention"]).assert().success();
    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read_link(&link).unwrap(), real);
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&real).unwrap()).unwrap();
    assert!(v["hooks"]["Stop"].is_array());
}

#[cfg(unix)]
#[test]
fn relative_dangling_symlink_is_written_at_its_target() {
    let env = Env::new();
    std::fs::create_dir_all(env.claude.join("conf")).unwrap();
    let link = env.paths().settings_file();
    std::os::unix::fs::symlink("conf/settings.json", &link).unwrap();
    env.sled().args(["install", "--keep-retention"]).assert().success();
    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    let v: Value = serde_json::from_str(&std::fs::read_to_string(env.claude.join("conf/settings.json")).unwrap()).unwrap();
    assert!(v["hooks"]["Stop"].is_array());
}

#[cfg(unix)]
#[test]
fn dangling_symlink_into_missing_folder_is_an_error() {
    let env = Env::new();
    let real = env.tmp.path().join("missing").join("settings.json");
    let link = env.paths().settings_file();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let before: Vec<_> = std::fs::read_dir(&env.claude).unwrap().map(|e| e.unwrap().file_name()).collect();
    let out = env.sled().args(["install", "--keep-retention"]).output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains(&link.display().to_string()), "{err}");
    assert!(err.contains(&real.display().to_string()), "{err}");
    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read_link(&link).unwrap(), real);
    assert!(!real.parent().unwrap().exists());
    let after: Vec<_> = std::fs::read_dir(&env.claude).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(before, after);
}

fn ask(current: Option<u64>, input: &str) -> (bool, String) {
    let mut out = Vec::new();
    let mut inp = std::io::Cursor::new(input.as_bytes().to_vec());
    let yes = confirm_retention(current, &mut out, &mut inp);
    (yes, String::from_utf8(out).unwrap())
}

#[test]
fn retention_prompt_accepts_only_yes() {
    for (input, want) in [("y\n", true), ("YES\n", true), (" yes \n", true), ("n\n", false), ("\n", false), ("", false), ("yep\n", false)] {
        assert_eq!(ask(None, input).0, want, "{input:?}");
    }
}

#[test]
fn retention_prompt_text_is_unchanged() {
    assert_eq!(
        ask(None, "").1,
        "Claude Code deletes session files after 30 (default) days, after which a session cannot be resumed. \
         Keep them for 365 days instead? This uses more disk. [y/N] "
    );
    assert!(ask(Some(90), "n\n").1.starts_with("Claude Code deletes session files after 90 days, "));
}

#[test]
fn key_order_survives_install_and_uninstall() {
    let env = Env::new();
    let file = env.paths().settings_file();
    let original = r#"{
  "z": 1,
  "model": "x",
  "a": {
    "k": 2,
    "b": 1
  },
  "hooks": {
    "Stop": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "notify done"
          }
        ]
      }
    ]
  },
  "env": {
    "Z": "1",
    "A": "2"
  }
}
"#;
    std::fs::write(&file, original).unwrap();
    env.sled().args(["install", "--keep-retention"]).assert().success();
    assert_ne!(std::fs::read_to_string(&file).unwrap(), original);
    env.sled().arg("uninstall").assert().success();
    let text = std::fs::read_to_string(&file).unwrap();
    let (got, want): (Value, Value) = (serde_json::from_str(&text).unwrap(), serde_json::from_str(original).unwrap());
    assert_eq!(got, want);
    // Same text means the same key order at every level.
    assert_eq!(text, original);
}

const FOREIGN_ENDING_IN_SLED_HOOK: [&str; 6] = [
    "notify;/usr/local/bin/sled hook",
    "notify&&/usr/local/bin/sled hook",
    "a|/x/sled hook",
    "$(curl x)/sled hook",
    "`x`/sled hook",
    "\"$(x)/sled\" hook",
];

#[test]
fn foreign_commands_ending_in_sled_hook_are_not_ours() {
    for cmd in FOREIGN_ENDING_IN_SLED_HOOK {
        let mut s = json!({"hooks":{"Stop":[ours(cmd)]}});
        assert!(!remove_hooks(&mut s), "{cmd}");
        assert_eq!(s, json!({"hooks":{"Stop":[ours(cmd)]}}), "{cmd}");
        assert!(add_hooks(&mut s, CMD), "{cmd}");
        assert_eq!(s["hooks"]["Stop"], json!([ours(cmd), ours(CMD)]), "{cmd}");
        assert!(remove_hooks(&mut s), "{cmd}");
        assert_eq!(s, json!({"hooks":{"Stop":[ours(cmd)]}}), "{cmd}");
    }
}

#[cfg(unix)]
#[test]
fn new_settings_file_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    env.sled().args(["install", "--keep-retention"]).assert().success();
    let mode = std::fs::metadata(env.paths().settings_file()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}
