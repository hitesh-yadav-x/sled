//! The `sled hook` entry point: ingest one session, never fail, never print.

use crate::db::Db;
use crate::ingest::ingest_session;
use crate::paths::{create_private_dir, make_private, private_file_options, Paths};
use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::io::Write;
use std::path::Path;

#[derive(Deserialize)]
struct Payload {
    session_id: String,
    transcript_path: String,
    #[serde(default)]
    hook_event_name: String,
}

/// Handles one hook event. Every error is appended to the hook log and swallowed.
pub fn run(paths: &Paths, stdin: &str) {
    if let Err(e) = handle(paths, stdin) {
        log(paths, &format!("{e:#}"));
    }
}

fn handle(paths: &Paths, stdin: &str) -> Result<()> {
    let p: Payload = serde_json::from_str(stdin).map_err(|e| anyhow!("unparseable hook input: {e}"))?;
    let mut db = Db::open(&paths.db_file())?;
    let transcript = Path::new(&p.transcript_path);
    ingest_session(&mut db, transcript)?;

    // A session gets a row only once its transcript has content, when ingest creates it.
    if db.session(&p.session_id)?.is_none() {
        return Ok(());
    }

    match p.hook_event_name.as_str() {
        "SessionStart" => db.set_ended(&p.session_id, false)?,
        "SessionEnd" => db.set_ended(&p.session_id, true)?,
        _ => {}
    }
    Ok(())
}

/// Appends `<RFC 3339 time> <message>`; failure to log is ignored.
fn log(paths: &Paths, message: &str) {
    let _ = (|| -> std::io::Result<()> {
        create_private_dir(&paths.data_dir)?;
        // Cap the log: past 256 KB the old file becomes hook.log.1.
        if std::fs::metadata(paths.hook_log()).is_ok_and(|m| m.len() > 262_144) {
            // Removed first: rename does not replace an existing file everywhere.
            let old = paths.data_dir.join("hook.log.1");
            let _ = std::fs::remove_file(&old);
            if std::fs::rename(paths.hook_log(), &old).is_ok() {
                make_private(&old);
            }
        }
        let mut f = private_file_options().create(true).append(true).open(paths.hook_log())?;
        let line = message.replace('\n', " ");
        writeln!(f, "{} {}", chrono::Local::now().to_rfc3339(), line)
    })();
}
