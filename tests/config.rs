mod common;

use common::Env;
use session_ledger::config::{Config, CostMode};
use std::path::Path;

#[test]
fn defaults_when_file_missing() {
    let c = Config::load(Path::new("/nonexistent/config.toml")).unwrap();
    assert_eq!(c.port, 7878);
    assert_eq!(c.cost_mode, CostMode::List);
    assert_eq!(c.multiplier, 1.0);
    assert!(c.prices.is_empty());
}

#[test]
fn reads_overrides() {
    let env = Env::new();
    let file = env.data.join("config.toml");
    std::fs::write(&file, "port = 9000\ncost_mode = \"tokens\"\n").unwrap();
    let c = Config::load(&file).unwrap();
    assert_eq!(c.port, 9000);
    assert_eq!(c.cost_mode, CostMode::Tokens);
    assert_eq!(c.multiplier, 1.0);
}

#[test]
fn unknown_cost_mode_is_an_error() {
    let env = Env::new();
    let file = env.data.join("config.toml");
    std::fs::write(&file, "cost_mode = \"free\"\n").unwrap();
    assert!(Config::load(&file).is_err());
}

#[test]
fn paths_follow_env() {
    let env = Env::new();
    let paths = env.paths();
    assert_eq!(paths.db_file(), env.data.join("ledger.db"));
    assert_eq!(paths.projects_dir(), env.claude.join("projects"));
    assert_eq!(paths.settings_file(), env.claude.join("settings.json"));
    assert_eq!(paths.config_file(), env.data.join("config.toml"));
    assert_eq!(paths.hook_log(), env.data.join("hook.log"));

    // The binary reads both variables: the sweep writes its database under SLED_DATA_DIR.
    env.sled().arg("sweep").assert().success();
    assert!(env.data.join("ledger.db").is_file());
    assert!(!env.claude.join("session-ledger").exists());

    // Without SLED_DATA_DIR the data folder sits inside the Claude folder.
    let other = Env::new();
    other.sled().env_remove("SLED_DATA_DIR").arg("sweep").assert().success();
    assert!(other.claude.join("session-ledger").join("ledger.db").is_file());
    assert!(!other.data.join("ledger.db").exists());
}
