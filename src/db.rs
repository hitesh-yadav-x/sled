//! SQLite store for sessions, per-message usage, file offsets and notes.
//! Nothing in this module deletes a session row.

use crate::paths;
use crate::transcript::MessageUsage;
use anyhow::Result;
use rusqlite::{params, Connection, ErrorCode, OptionalExtension, Transaction, TransactionBehavior};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct Db {
    conn: Connection,
}

pub struct FileState {
    pub offset: u64,
}

pub struct SessionPatch {
    pub session_id: String,
    pub transcript_path: String,
    pub file_created_ms: i64,
    pub project_dir: Option<String>,
    pub git_branch: Option<String>,
    pub entrypoint: Option<String>,
    pub title: Option<String>,
    pub recap: Option<String>,
    pub first_prompt: Option<String>,
    pub last_prompt: Option<String>,
    pub first_ts_ms: Option<i64>,
    pub last_ts_ms: Option<i64>,
    pub add_active_ms: i64,
    pub reset_activity: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    pub session_id: String,
    pub transcript_path: String,
    pub project_dir: Option<String>,
    pub git_branch: Option<String>,
    pub entrypoint: Option<String>,
    pub title: Option<String>,
    pub recap: Option<String>,
    pub first_prompt: Option<String>,
    pub last_prompt: Option<String>,
    pub started_at: Option<i64>,
    pub last_active_at: Option<i64>,
    pub active_ms: i64,
    pub ended: bool,
    pub transcript_missing: bool,
    pub continued_from: Option<String>,
    pub note: String,
    /// Started by the SDK or never prompted; see `automated_sql!`.
    pub automated: bool,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions(
  session_id TEXT PRIMARY KEY,
  transcript_path TEXT NOT NULL,
  file_created_ms INTEGER NOT NULL,
  project_dir TEXT, git_branch TEXT, entrypoint TEXT,
  title TEXT, recap TEXT, first_prompt TEXT, last_prompt TEXT,
  started_at INTEGER, last_active_at INTEGER,
  active_ms INTEGER NOT NULL DEFAULT 0,
  ended INTEGER NOT NULL DEFAULT 0,
  transcript_missing INTEGER NOT NULL DEFAULT 0,
  continued_from TEXT
);
CREATE TABLE IF NOT EXISTS notes(
  session_id TEXT PRIMARY KEY, note TEXT NOT NULL, updated_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS files(
  path TEXT PRIMARY KEY, session_id TEXT NOT NULL, offset INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS messages(
  key TEXT PRIMARY KEY, session_id TEXT NOT NULL, day TEXT NOT NULL, model TEXT NOT NULL,
  input INTEGER NOT NULL, output INTEGER NOT NULL, cache_read INTEGER NOT NULL,
  cache_write_5m INTEGER NOT NULL, cache_write_1h INTEGER NOT NULL, web_search INTEGER NOT NULL,
  service_tier TEXT, speed TEXT, subagent INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS messages_session ON messages(session_id);
";

/// The one definition of an automated session: started by the SDK, or with no prompt.
/// `sessions` filters on it and every `SessionRow` carries it as `automated`.
macro_rules! automated_sql {
    () => {
        "(COALESCE(substr(s.entrypoint, 1, 3), '') = 'sdk' OR s.first_prompt IS NULL)"
    };
}

const SESSION_COLS: &str = concat!(
    "s.session_id, s.transcript_path, s.project_dir, s.git_branch, \
    s.entrypoint, s.title, s.recap, s.first_prompt, s.last_prompt, s.started_at, \
    s.last_active_at, s.active_ms, s.ended, s.transcript_missing, s.continued_from, \
    COALESCE(n.note, ''), ",
    automated_sql!()
);

const SESSION_FROM: &str = "FROM sessions s LEFT JOIN notes n ON n.session_id = s.session_id";

const USAGE_COLS: &str = "session_id, key, model, input, output, cache_read, cache_write_5m, \
    cache_write_1h, web_search, service_tier, speed, subagent";

/// Bound parameters per statement in `usage_for`, well under SQLite's limit.
const MAX_PARAMS: usize = 500;

const AUTOMATED: &str = automated_sql!();

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy) | Some(ErrorCode::DatabaseLocked)
    )
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn row_to_session(r: &rusqlite::Row) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        session_id: r.get(0)?,
        transcript_path: r.get(1)?,
        project_dir: r.get(2)?,
        git_branch: r.get(3)?,
        entrypoint: r.get(4)?,
        title: r.get(5)?,
        recap: r.get(6)?,
        first_prompt: r.get(7)?,
        last_prompt: r.get(8)?,
        started_at: r.get(9)?,
        last_active_at: r.get(10)?,
        active_ms: r.get(11)?,
        ended: r.get::<_, i64>(12)? != 0,
        transcript_missing: r.get::<_, i64>(13)? != 0,
        continued_from: r.get(14)?,
        note: r.get(15)?,
        automated: r.get(16)?,
    })
}

fn row_to_usage(r: &rusqlite::Row) -> rusqlite::Result<(String, MessageUsage)> {
    Ok((
        r.get::<_, String>(0)?,
        MessageUsage {
            key: r.get(1)?,
            model: r.get(2)?,
            input: r.get::<_, i64>(3)? as u64,
            output: r.get::<_, i64>(4)? as u64,
            cache_read: r.get::<_, i64>(5)? as u64,
            cache_write_5m: r.get::<_, i64>(6)? as u64,
            cache_write_1h: r.get::<_, i64>(7)? as u64,
            web_search: r.get::<_, i64>(8)? as u64,
            service_tier: r.get(9)?,
            speed: r.get(10)?,
            subagent: r.get::<_, i64>(11)? != 0,
        },
    ))
}

/// A token count as SQLite stores it: values past `i64::MAX` saturate instead of wrapping.
fn count(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn user_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
}

impl Db {
    /// First open of a fresh file only. Bounded retry (about 2 s) so that
    /// processes racing on a new database both succeed; fails promptly otherwise.
    fn first_time_setup(conn: &mut Connection) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let attempt = (|| -> rusqlite::Result<()> {
                conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0))?;
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                if user_version(&tx)? == 0 {
                    tx.execute_batch(SCHEMA)?;
                    tx.execute_batch("PRAGMA user_version = 1")?;
                }
                tx.commit()
            })();
            match attempt {
                Ok(()) => return Ok(()),
                Err(e) if is_busy(&e) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Opens the database, creating its folder (mode 0700) and file (mode 0600) when missing.
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(parent) = path.parent() {
            paths::create_private_dir(parent)?;
        }
        // SQLite would create the file 0644; an empty file is a valid new database, and the
        // WAL side files take this file's mode. Any failure is left to the open below.
        let _ = paths::private_file_options().write(true).create_new(true).open(path);
        let conn = Connection::open(path)?;
        // Before any statement that can take a lock.
        conn.busy_timeout(Duration::from_millis(200))?;
        let mut conn = conn;
        // Hot path: schema already there. Reading user_version takes no write lock.
        if user_version(&conn)? == 0 {
            Self::first_time_setup(&mut conn)?;
        }
        Ok(Db { conn })
    }

    pub fn file_state(&self, file_path: &str) -> Result<Option<FileState>> {
        let off: Option<i64> = self
            .conn
            .query_row("SELECT offset FROM files WHERE path = ?1", [file_path], |r| r.get(0))
            .optional()?;
        Ok(off.map(|o| FileState { offset: o as u64 }))
    }

    /// One transaction: upsert session, upsert messages, store the file's new offset.
    /// `expected_offset` is the stored offset the caller read before reading the file
    /// (0 when there was none). If another writer moved it since, this changes nothing,
    /// so overlapping ingests of the same bytes are applied once.
    /// Inside `rebuild_session` it runs as a savepoint of that transaction instead.
    pub fn apply(
        &mut self,
        patch: &SessionPatch,
        file_path: &str,
        expected_offset: u64,
        new_offset: u64,
        messages: &[(MessageUsage, String)],
    ) -> Result<()> {
        if self.conn.is_autocommit() {
            // IMMEDIATE takes the write lock up front, so the busy timeout applies
            // instead of a failing read-to-write upgrade.
            let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if apply_in(&tx, patch, file_path, expected_offset, new_offset, messages)? {
                tx.commit()?;
            }
        } else {
            let sp = self.conn.savepoint()?;
            if apply_in(&sp, patch, file_path, expected_offset, new_offset, messages)? {
                sp.commit()?;
            }
        }
        Ok(())
    }

    /// Re-reads one session from scratch, all in one transaction: clears its derived
    /// fields, deletes its `files` rows, then runs `reingest` (which reads its files from
    /// offset 0). If any step fails the session is left as it was. No `messages` row is
    /// ever deleted: re-reading upserts each message again. The session row, its note,
    /// `ended`, `project_dir` and `file_created_ms` are kept.
    pub fn rebuild_session(
        &mut self,
        session_id: &str,
        reingest: impl FnOnce(&mut Db) -> Result<()>,
    ) -> Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = self
            .clear_derived(session_id)
            .and_then(|()| reingest(self))
            .and_then(|()| Ok(self.conn.execute_batch("COMMIT")?));
        if result.is_err() && !self.conn.is_autocommit() {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
        result
    }

    fn clear_derived(&self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET title = NULL, recap = NULL, first_prompt = NULL,
               last_prompt = NULL, git_branch = NULL, started_at = NULL, last_active_at = NULL,
               active_ms = 0, entrypoint = NULL, continued_from = NULL
             WHERE session_id = ?1",
            [session_id],
        )?;
        self.conn.execute("DELETE FROM files WHERE session_id = ?1", [session_id])?;
        Ok(())
    }

    /// Every session as `(session_id, transcript_path)`, oldest file first: ascending
    /// `(file_created_ms, session_id)`, the order in which shared messages are owned.
    pub fn sessions_oldest_first(&self) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare(
            "SELECT session_id, transcript_path FROM sessions ORDER BY file_created_ms, session_id",
        )?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn last_active(&self, session_id: &str) -> Result<Option<i64>> {
        let v: Option<Option<i64>> = self
            .conn
            .query_row(
                "SELECT last_active_at FROM sessions WHERE session_id = ?1",
                [session_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.flatten())
    }

    pub fn set_ended(&self, session_id: &str, ended: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET ended = ?2 WHERE session_id = ?1",
            params![session_id, ended as i64],
        )?;
        Ok(())
    }

    /// Sets each session's `transcript_missing` flag from its file, in one transaction,
    /// writing only rows whose flag changes. Returns how many changed.
    pub fn refresh_missing(&self) -> Result<usize> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let rows: Vec<(String, String, bool)> = {
            let mut st =
                tx.prepare("SELECT session_id, transcript_path, transcript_missing FROM sessions")?;
            let it = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0)))?;
            it.collect::<rusqlite::Result<_>>()?
        };
        let mut changed = 0;
        for (id, path, was_missing) in rows {
            let missing = !Path::new(&path).exists();
            if missing != was_missing {
                changed += tx.execute(
                    "UPDATE sessions SET transcript_missing = ?2 WHERE session_id = ?1",
                    params![id, missing as i64],
                )?;
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Stores the note if the session exists; the check and the write share one transaction.
    pub fn set_note(&self, session_id: &str, note: &str) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id = ?1)",
            [session_id],
            |r| r.get(0),
        )?;
        if !exists {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO notes(session_id, note, updated_ms) VALUES(?1,?2,?3)
             ON CONFLICT(session_id) DO UPDATE SET note = excluded.note, updated_ms = excluded.updated_ms",
            params![session_id, note, now_ms()],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn sessions(&self, since_ms: Option<i64>, include_automated: bool) -> Result<Vec<SessionRow>> {
        let mut sql = format!("SELECT {SESSION_COLS} {SESSION_FROM} WHERE 1=1");
        if since_ms.is_some() {
            sql.push_str(" AND s.last_active_at >= ?1");
        }
        if !include_automated {
            sql.push_str(&format!(" AND NOT {AUTOMATED}"));
        }
        sql.push_str(" ORDER BY s.last_active_at IS NULL, s.last_active_at DESC, s.session_id");
        let mut st = self.conn.prepare(&sql)?;
        let rows = match since_ms {
            Some(s) => st.query_map([s], row_to_session)?.collect::<rusqlite::Result<Vec<_>>>()?,
            None => st.query_map([], row_to_session)?.collect::<rusqlite::Result<Vec<_>>>()?,
        };
        Ok(rows)
    }

    pub fn session(&self, session_id: &str) -> Result<Option<SessionRow>> {
        let sql = format!("SELECT {SESSION_COLS} {SESSION_FROM} WHERE s.session_id = ?1");
        Ok(self.conn.query_row(&sql, [session_id], row_to_session).optional()?)
    }

    pub fn usage(&self, session_id: Option<&str>) -> Result<Vec<(String, MessageUsage)>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {USAGE_COLS} FROM messages WHERE (?1 IS NULL OR session_id = ?1) ORDER BY key"
        ))?;
        let rows = st.query_map([session_id], row_to_usage)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Usage rows of the given sessions, ordered by key like `usage`. The ids are sent in
    /// chunks of at most 500 bound parameters, all read in one snapshot.
    pub fn usage_for(&self, session_ids: &[String]) -> Result<Vec<(String, MessageUsage)>> {
        let tx = self.conn.unchecked_transaction()?;
        let mut out = Vec::new();
        for chunk in session_ids.chunks(MAX_PARAMS) {
            let marks = vec!["?"; chunk.len()].join(",");
            let mut st = tx.prepare(&format!(
                "SELECT {USAGE_COLS} FROM messages WHERE session_id IN ({marks})"
            ))?;
            let rows = st.query_map(rusqlite::params_from_iter(chunk), row_to_usage)?;
            for row in rows {
                out.push(row?);
            }
        }
        out.sort_by(|a, b| a.1.key.cmp(&b.1.key));
        Ok(out)
    }

    /// Number of this session's files under its own `<transcript stem>/subagents/` folder.
    pub fn subagent_count(&self, session_id: &str) -> Result<usize> {
        let transcript: Option<String> = self
            .conn
            .query_row(
                "SELECT transcript_path FROM sessions WHERE session_id = ?1",
                [session_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(transcript) = transcript else {
            return Ok(0);
        };
        let prefix = format!("{}/subagents/", Path::new(&transcript).with_extension("").display());
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM files
             WHERE session_id = ?1 AND substr(path, 1, length(?2)) = ?2",
            params![session_id, prefix],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    pub fn session_count(&self) -> Result<usize> {
        let n: i64 = self.conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
        Ok(n as usize)
    }
}

/// The body of `Db::apply` on an open transaction or savepoint. Returns false, having
/// written nothing, when the stored offset is not `expected_offset`.
fn apply_in(
    tx: &Connection,
    patch: &SessionPatch,
    file_path: &str,
    expected_offset: u64,
    new_offset: u64,
    messages: &[(MessageUsage, String)],
) -> Result<bool> {
    let stored: Option<i64> = tx
        .query_row("SELECT offset FROM files WHERE path = ?1", [file_path], |r| r.get(0))
        .optional()?;
    if stored.unwrap_or(0) as u64 != expected_offset {
        return Ok(false);
    }
    let sid = patch.session_id.as_str();
    tx.execute(
        "INSERT INTO sessions(session_id, transcript_path, file_created_ms, project_dir, git_branch,
           entrypoint, title, recap, first_prompt, last_prompt, started_at, last_active_at, active_ms)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,0)
         ON CONFLICT(session_id) DO UPDATE SET
           transcript_path = excluded.transcript_path,
           file_created_ms = MIN(sessions.file_created_ms, excluded.file_created_ms),
           project_dir = COALESCE(sessions.project_dir, excluded.project_dir),
           git_branch = COALESCE(excluded.git_branch, sessions.git_branch),
           entrypoint = COALESCE(sessions.entrypoint, excluded.entrypoint),
           title = COALESCE(excluded.title, sessions.title),
           recap = COALESCE(excluded.recap, sessions.recap),
           first_prompt = COALESCE(sessions.first_prompt, excluded.first_prompt),
           last_prompt = COALESCE(excluded.last_prompt, sessions.last_prompt)",
        params![
            sid,
            patch.transcript_path,
            patch.file_created_ms,
            patch.project_dir,
            patch.git_branch,
            patch.entrypoint,
            patch.title,
            patch.recap,
            patch.first_prompt,
            patch.last_prompt,
            Option::<i64>::None,
            Option::<i64>::None,
        ],
    )?;
    if patch.reset_activity {
        tx.execute(
            "UPDATE sessions SET active_ms = 0, last_active_at = NULL WHERE session_id = ?1",
            [sid],
        )?;
    }
    tx.execute(
        "UPDATE sessions SET
           started_at = CASE WHEN ?2 IS NULL THEN started_at
                             WHEN started_at IS NULL OR ?2 < started_at THEN ?2
                             ELSE started_at END,
           last_active_at = CASE WHEN ?3 IS NULL THEN last_active_at
                                 WHEN last_active_at IS NULL OR ?3 > last_active_at THEN ?3
                                 ELSE last_active_at END,
           active_ms = active_ms + ?4
         WHERE session_id = ?1",
        params![sid, patch.first_ts_ms, patch.last_ts_ms, patch.add_active_ms],
    )?;

    for (m, day) in messages {
        let owner: Option<String> = tx
            .query_row("SELECT session_id FROM messages WHERE key = ?1", [&m.key], |r| r.get(0))
            .optional()?;
        let write = match owner {
            None => true,
            Some(ref o) if o == sid => true,
            Some(ref o) => {
                let theirs: Option<i64> = tx
                    .query_row(
                        "SELECT file_created_ms FROM sessions WHERE session_id = ?1",
                        [o],
                        |r| r.get(0),
                    )
                    .optional()?;
                let incoming_wins = match theirs {
                    None => true,
                    Some(t) => (patch.file_created_ms, sid) < (t, o.as_str()),
                };
                if incoming_wins {
                    tx.execute(
                        "UPDATE sessions SET continued_from = ?2
                         WHERE session_id = ?1 AND continued_from IS NULL",
                        params![o, sid],
                    )?;
                } else {
                    tx.execute(
                        "UPDATE sessions SET continued_from = ?2
                         WHERE session_id = ?1 AND continued_from IS NULL",
                        params![sid, o],
                    )?;
                }
                incoming_wins
            }
        };
        if !write {
            continue;
        }
        tx.execute(
            "INSERT OR REPLACE INTO messages(key, session_id, day, model, input, output,
               cache_read, cache_write_5m, cache_write_1h, web_search, service_tier, speed,
               subagent)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                m.key,
                sid,
                day,
                m.model,
                count(m.input),
                count(m.output),
                count(m.cache_read),
                count(m.cache_write_5m),
                count(m.cache_write_1h),
                count(m.web_search),
                m.service_tier,
                m.speed,
                m.subagent as i64
            ],
        )?;
    }

    tx.execute(
        "INSERT INTO files(path, session_id, offset) VALUES(?1,?2,?3)
         ON CONFLICT(path) DO UPDATE SET session_id = excluded.session_id, offset = excluded.offset",
        params![file_path, sid, new_offset as i64],
    )?;
    Ok(true)
}
