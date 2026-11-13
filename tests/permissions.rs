#![cfg(unix)]
mod common;

use common::Env;
use session_ledger::db::Db;
use session_ledger::paths::Paths;
use session_ledger::{hook, sweep};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn set_mode(path: &Path, m: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(m)).unwrap();
}

/// Paths whose data folder does not exist yet.
fn fresh(env: &Env) -> Paths {
    Paths { claude_dir: env.claude.clone(), data_dir: env.tmp.path().join("new").join("data") }
}

#[test]
fn database_and_its_folder_are_created_private() {
    let env = Env::new();
    let paths = fresh(&env);
    Db::open(&paths.db_file()).unwrap();
    assert_eq!(mode(&paths.data_dir), 0o700);
    assert_eq!(mode(&paths.db_file()), 0o600);
    // The WAL side files follow the database file's mode.
    for side in ["ledger.db-wal", "ledger.db-shm"] {
        let p = paths.data_dir.join(side);
        if p.exists() {
            assert_eq!(mode(&p), 0o600, "{side}");
        }
    }
}

#[test]
fn hook_log_is_created_private() {
    let env = Env::new();
    let paths = fresh(&env);
    hook::run(&paths, "not json");
    assert_eq!(mode(&paths.data_dir), 0o700);
    assert_eq!(mode(&paths.hook_log()), 0o600);
}

#[test]
fn rotated_hook_log_is_private() {
    let env = Env::new();
    let paths = env.paths();
    std::fs::write(paths.hook_log(), "x".repeat(262_145)).unwrap();
    set_mode(&paths.hook_log(), 0o644);
    hook::run(&paths, "not json");
    assert_eq!(mode(&env.data.join("hook.log.1")), 0o600);
    assert_eq!(mode(&paths.hook_log()), 0o600);
}

/// A data folder at 0755 holding a database at 0644, as an older version left them.
fn wide_open(env: &Env) -> Paths {
    let paths = env.paths();
    Db::open(&paths.db_file()).unwrap();
    set_mode(&paths.data_dir, 0o755);
    set_mode(&paths.db_file(), 0o644);
    paths
}

#[test]
fn sweep_tightens_existing_permissions() {
    let env = Env::new();
    let paths = wide_open(&env);
    sweep::run(&paths).unwrap();
    assert_eq!(mode(&paths.data_dir), 0o700);
    assert_eq!(mode(&paths.db_file()), 0o600);
}

#[test]
fn sweep_command_tightens_existing_permissions() {
    let env = Env::new();
    let paths = wide_open(&env);
    env.sled().arg("sweep").assert().success();
    assert_eq!(mode(&paths.data_dir), 0o700);
    assert_eq!(mode(&paths.db_file()), 0o600);
}

#[test]
fn tightening_never_widens() {
    let env = Env::new();
    let paths = env.paths();
    Db::open(&paths.db_file()).unwrap();
    set_mode(&paths.db_file(), 0o400);
    paths.tighten_permissions();
    assert_eq!(mode(&paths.db_file()), 0o400);
    set_mode(&paths.db_file(), 0o600);
}

#[test]
fn serve_tightens_existing_permissions() {
    let env = Env::new();
    let paths = wide_open(&env);
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_sled"))
        .args(["serve", "--port", "0"])
        .env("CLAUDE_CONFIG_DIR", &env.claude)
        .env("SLED_DATA_DIR", &env.data)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let (dir, db) = (mode(&paths.data_dir), mode(&paths.db_file()));
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(line.starts_with("Session ledger is running"), "{line}");
    assert_eq!((dir, db), (0o700, 0o600));
}

#[test]
fn hook_does_not_tighten_existing_permissions() {
    let env = Env::new();
    let paths = wide_open(&env);
    hook::run(&paths, r#"{"session_id":"x","transcript_path":"/nonexistent/x.jsonl","hook_event_name":"Stop"}"#);
    assert_eq!(mode(&paths.data_dir), 0o755);
    assert_eq!(mode(&paths.db_file()), 0o644);
}

#[test]
fn tightening_skips_a_symlinked_database() {
    let env = Env::new();
    let outside = env.tmp.path().join("elsewhere.db");
    Db::open(&outside).unwrap();
    set_mode(&outside, 0o644);
    std::os::unix::fs::symlink(&outside, env.data.join("ledger.db")).unwrap();
    sweep::run(&env.paths()).unwrap();
    assert_eq!(mode(&outside), 0o644);
}

#[test]
fn sweep_command_tightens_existing_hook_logs() {
    let env = Env::new();
    for name in ["hook.log", "hook.log.1"] {
        std::fs::write(env.data.join(name), "old line\n").unwrap();
        set_mode(&env.data.join(name), 0o644);
    }
    env.sled().arg("sweep").assert().success();
    assert_eq!(mode(&env.data.join("hook.log")), 0o600);
    assert_eq!(mode(&env.data.join("hook.log.1")), 0o600);
}
