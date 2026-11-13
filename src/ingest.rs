//! Incremental ingest of a session transcript (and its sub-agent files) into the ledger.

use crate::db::{Db, SessionPatch};
use crate::transcript::{parse_line, Kind, MessageUsage};
use anyhow::Result;
use chrono::{Local, TimeZone};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Gaps longer than this between consecutive lines are idle time, not active time.
const MAX_ACTIVE_GAP_MS: i64 = 900_000;

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Main,
    SubAgent,
}

/// Ingests the main transcript, then every `*.jsonl` under `<transcript stem>/subagents/`.
/// A missing main file is `Ok(())`. The path is canonicalized first, so one file reached
/// under two spellings has one `files` row and is counted once.
pub fn ingest_session(db: &mut Db, transcript: &Path) -> Result<()> {
    ingest(db, transcript, false)
}

/// Like `ingest_session`, but a main transcript that cannot be opened is an error, so a
/// rebuild that cleared the session rolls back instead of committing it empty.
pub fn reingest_session(db: &mut Db, transcript: &Path) -> Result<()> {
    ingest(db, transcript, true)
}

fn ingest(db: &mut Db, transcript: &Path, require_main: bool) -> Result<()> {
    let canonical = std::fs::canonicalize(transcript).unwrap_or_else(|_| transcript.to_path_buf());
    let transcript = canonical.as_path();
    let Some(session_id) = transcript.file_stem().and_then(|s| s.to_str()) else {
        return Ok(());
    };
    let created_ms = match std::fs::metadata(transcript) {
        Ok(m) => created_ms(&m),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !require_main => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    ingest_file(db, session_id, transcript, transcript, created_ms, Role::Main, require_main)?;

    let dir = transcript.with_extension("").join("subagents");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(());
    };
    // A symlinked sub-agent file is used under its real path, so one real file is
    // never ingested under two paths.
    let mut files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .collect();
    files.sort();
    files.dedup();
    for f in files {
        ingest_file(db, session_id, transcript, &f, created_ms, Role::SubAgent, false)?;
    }
    Ok(())
}

fn created_ms(m: &std::fs::Metadata) -> i64 {
    m.created()
        .or_else(|_| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn day_of(ts_ms: i64) -> Option<String> {
    Local
        .timestamp_millis_opt(ts_ms)
        .single()
        .map(|d| d.format("%Y-%m-%d").to_string())
}

fn ingest_file(
    db: &mut Db,
    session_id: &str,
    main_path: &Path,
    file: &Path,
    file_created_ms: i64,
    role: Role,
    must_open: bool,
) -> Result<()> {
    let key = file.to_string_lossy().into_owned();
    let stored = db.file_state(&key)?.map(|s| s.offset).unwrap_or(0);

    let mut f = match File::open(file) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !must_open => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let len = f.metadata()?.len();
    let truncated = len < stored;
    let start = if truncated { 0 } else { stored };
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;

    // Only complete lines; the end is decided by the bytes actually read.
    let complete = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let reset = truncated && role == Role::Main;
    // A truncated file is recorded even with nothing complete to read, so its offset
    // starts over and later growth is read from the start.
    if complete == 0 && !truncated {
        return Ok(());
    }
    let new_offset = start + complete as u64;

    let mut patch = SessionPatch {
        session_id: session_id.to_owned(),
        transcript_path: main_path.to_string_lossy().into_owned(),
        file_created_ms,
        project_dir: None,
        git_branch: None,
        entrypoint: None,
        title: None,
        recap: None,
        first_prompt: None,
        last_prompt: None,
        first_ts_ms: None,
        last_ts_ms: None,
        add_active_ms: 0,
        reset_activity: reset,
    };
    let mut messages: Vec<(MessageUsage, String)> = Vec::new();
    // Gaps are measured between lines of this file only: a first read starts fresh even
    // when a hook already recorded the session, so it matches a single-pass ingest.
    // Each gap is measured from the latest timestamp seen so far, so lines that go
    // backwards in time give the same total in one pass as across several runs.
    let stored_last_active = db.last_active(session_id)?;
    let mut prev_active = if role == Role::SubAgent || start == 0 {
        None
    } else {
        stored_last_active
    };
    // Lines before the first timestamp of this run go on the session's last active day.
    let mut cur_day = stored_last_active
        .and_then(day_of)
        .unwrap_or_else(|| Local::now().format("%Y-%m-%d").to_string());

    for raw in buf[..complete].split(|&b| b == b'\n') {
        let Ok(text) = std::str::from_utf8(raw) else { continue };
        let Some(line) = parse_line(text) else { continue };
        if let Some(d) = line.ts_ms.and_then(day_of) {
            cur_day = d;
        }
        if role == Role::SubAgent {
            if let Kind::Usage(mut u) = line.kind {
                u.subagent = true;
                messages.push((u, cur_day.clone()));
            }
            continue;
        }
        if let Some(ts) = line.ts_ms {
            if let Some(p) = prev_active {
                let gap = ts - p;
                if gap > 0 && gap <= MAX_ACTIVE_GAP_MS {
                    patch.add_active_ms += gap;
                }
            }
            prev_active = Some(prev_active.map_or(ts, |p| p.max(ts)));
            patch.first_ts_ms = Some(patch.first_ts_ms.map_or(ts, |m| m.min(ts)));
            patch.last_ts_ms = Some(patch.last_ts_ms.map_or(ts, |m| m.max(ts)));
        }
        if patch.project_dir.is_none() {
            patch.project_dir = line.cwd;
        }
        if line.git_branch.is_some() {
            patch.git_branch = line.git_branch;
        }
        if patch.entrypoint.is_none() {
            patch.entrypoint = line.entrypoint;
        }
        match line.kind {
            Kind::Title(t) => patch.title = Some(t),
            Kind::Recap(r) => patch.recap = Some(r),
            Kind::Prompt(p) if !p.is_empty() => {
                if patch.first_prompt.is_none() {
                    patch.first_prompt = Some(p.clone());
                }
                patch.last_prompt = Some(p);
            }
            Kind::Prompt(_) => {}
            Kind::Usage(u) => messages.push((u, cur_day.clone())),
            Kind::Other => {}
        }
    }

    db.apply(&patch, &key, stored, new_offset, &messages)
}
