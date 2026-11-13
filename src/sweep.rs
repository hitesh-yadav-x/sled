//! `sled sweep`: ingest every transcript under the projects folder.
//! `sled sweep --rebuild` first re-reads every session whose file is still there.

use crate::db::Db;
use crate::ingest::{ingest_session, reingest_session};
use crate::paths::Paths;
use anyhow::Result;
use std::path::Path;

pub struct SweepReport {
    pub sessions: usize,
    pub new: usize,
}

pub struct RebuildReport {
    pub rebuilt: usize,
    pub kept: usize,
    /// Sessions whose rebuild failed and was rolled back; each is reported on stderr.
    pub failed: usize,
}

/// Sweeps, failing if any step fails, including the final missing-file refresh.
pub fn run(paths: &Paths) -> Result<SweepReport> {
    let (report, refreshed) = run_keeping_counts(paths)?;
    refreshed?;
    Ok(report)
}

/// Sweeps and returns the counts even when the final missing-file refresh fails;
/// that failure is returned beside them.
pub fn run_keeping_counts(paths: &Paths) -> Result<(SweepReport, Result<()>)> {
    let mut db = Db::open(&paths.db_file())?;
    // Install runs this sweep too, so both commands tighten an older, wider data folder.
    paths.tighten_permissions();
    let before = db.session_count()?;

    let mut files = Vec::new();
    if let Ok(projects) = std::fs::read_dir(paths.projects_dir()) {
        for project in projects.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()) {
            let Ok(entries) = std::fs::read_dir(&project) else { continue };
            files.extend(
                entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.is_file() && p.extension().and_then(|x| x.to_str()) == Some("jsonl")),
            );
        }
    }
    files.sort();
    for f in &files {
        if let Err(e) = ingest_session(&mut db, f) {
            eprintln!("sled: skipping {}: {e:#}", f.display());
        }
    }
    let refreshed = db.refresh_missing().map(|_| ());

    let sessions = db.session_count()?;
    Ok((SweepReport { sessions, new: sessions.saturating_sub(before) }, refreshed))
}

/// Re-reads, from offset 0, every session whose transcript file still exists, each in its
/// own transaction, oldest file first. A session whose file is gone is left exactly as it
/// is, and no stored usage is ever removed.
pub fn rebuild(paths: &Paths) -> Result<RebuildReport> {
    let mut db = Db::open(&paths.db_file())?;
    let mut report = RebuildReport { rebuilt: 0, kept: 0, failed: 0 };
    for (session_id, path) in db.sessions_oldest_first()? {
        let transcript = Path::new(&path);
        // Ingest names the session after the file, so only a matching file is re-read.
        let same_session = transcript.file_stem().and_then(|s| s.to_str()) == Some(&session_id);
        if !transcript.is_file() || !same_session {
            report.kept += 1;
            continue;
        }
        match db.rebuild_session(&session_id, |db| reingest_session(db, transcript)) {
            Ok(()) => report.rebuilt += 1,
            Err(e) => {
                report.failed += 1;
                eprintln!("sled: could not rebuild {session_id}: {e:#}");
            }
        }
    }
    Ok(report)
}
