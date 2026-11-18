//! SQLite transcript + command log (v1 schema plus job-ownership columns). Batched writes, rolling retention.

use rusqlite::{Connection, OpenFlags, params};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct StoredSegment {
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JobRecord {
    pub id: String,
    pub status: String,
    pub boot_id: Option<String>,
    pub guard_pid: Option<i64>,
    pub guard_start: Option<i64>,
    pub job_pgid: Option<i64>,
    pub gate_start: Option<i64>,
}

/// One voice command and its job, as the `commands` table keeps it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CommandRow {
    pub id: String,
    pub captured_at: f64,
    pub text: String,
    pub harness: String,
    pub status: String,
    pub exit_code: Option<i64>,
    pub finished_at: Option<f64>,
    pub summary: Option<String>,
}

pub struct Store {
    db: Connection,
    pending: Vec<(i64, f64, f64, String, String)>,
    last_flush: Instant,
    failure: Option<String>,
}

const NON_FINAL: &str = "('queued','starting','running')";

fn wall_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

impl Store {
    pub fn open(path: &Path, read_only: bool) -> Result<Store, String> {
        let db = if read_only {
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|e| e.to_string())?
        } else {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                set_mode(dir, 0o700);
            }
            let db = Connection::open(path).map_err(|e| e.to_string())?;
            set_mode(path, 0o600);
            db.execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=NORMAL;
                 PRAGMA wal_autocheckpoint=256;
                 PRAGMA secure_delete=ON;
                 CREATE TABLE IF NOT EXISTS segments(
                   id INTEGER PRIMARY KEY, session INTEGER NOT NULL, start_wall REAL NOT NULL, end_wall REAL NOT NULL,
                   text TEXT NOT NULL, kind TEXT NOT NULL);
                 CREATE INDEX IF NOT EXISTS segments_end ON segments(end_wall);
                 CREATE TABLE IF NOT EXISTS commands(
                   id TEXT PRIMARY KEY, captured_wall REAL NOT NULL, text TEXT NOT NULL, harness TEXT NOT NULL,
                   status TEXT NOT NULL, exit_code INTEGER, finished_wall REAL, summary TEXT);
                 CREATE INDEX IF NOT EXISTS commands_captured ON commands(captured_wall);",
            )
            .map_err(|e| e.to_string())?;
            for col in [
                "boot_id TEXT",
                "guard_pid INTEGER",
                "guard_start INTEGER",
                "job_pgid INTEGER",
                "gate_start INTEGER",
            ] {
                let name = col.split(' ').next().unwrap_or_default();
                let exists: bool = db
                    .prepare("SELECT 1 FROM pragma_table_info('commands') WHERE name = ?1")
                    .and_then(|mut s| s.exists([name]))
                    .map_err(|e| e.to_string())?;
                if !exists {
                    db.execute_batch(&format!("ALTER TABLE commands ADD COLUMN {col}"))
                        .map_err(|e| e.to_string())?;
                }
            }
            for side in ["-wal", "-shm"] {
                set_mode(Path::new(&format!("{}{side}", path.display())), 0o600);
            }
            db
        };
        Ok(Store {
            db,
            pending: Vec::new(),
            last_flush: Instant::now(),
            failure: None,
        })
    }

    pub fn append(&mut self, session: u64, start_wall: f64, end_wall: f64, text: &str, kind: &str) {
        self.pending.push((
            session as i64,
            start_wall,
            end_wall,
            text.to_string(),
            kind.to_string(),
        ));
        if self.pending.len() >= 32 {
            let _ = self.flush();
        }
    }

    /// Writes pending rows in one transaction. On failure the batch is kept (bounded) for the next attempt and
    /// the error is returned as well as recorded.
    pub fn flush(&mut self) -> Result<(), String> {
        self.last_flush = Instant::now();
        if self.pending.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(&mut self.pending);
        let result = (|| -> rusqlite::Result<()> {
            let tx = self.db.transaction()?;
            {
                let mut stmt = tx.prepare_cached("INSERT INTO segments(session,start_wall,end_wall,text,kind) VALUES(?1,?2,?3,?4,?5)")?;
                for r in &rows {
                    stmt.execute(params![r.0, r.1, r.2, r.3, r.4])?;
                }
            }
            tx.commit()
        })();
        match result {
            Ok(()) => Ok(()),
            Err(e) => {
                let msg = format!("transcript store flush failed: {e}");
                self.failure = Some(msg.clone());
                // Keep the batch for a retry, but never grow without bound.
                let mut kept = rows;
                kept.extend(std::mem::take(&mut self.pending));
                let excess = kept.len().saturating_sub(4096);
                kept.drain(..excess);
                self.pending = kept;
                Err(msg)
            }
        }
    }

    pub fn flush_if_due(&mut self) {
        if self.last_flush.elapsed() >= Duration::from_secs(2) {
            let _ = self.flush();
        }
    }

    fn run(&mut self, op: &str, sql: &str, p: impl rusqlite::Params) {
        if let Err(e) = self.db.execute(sql, p) {
            self.failure = Some(format!("transcript store {op} failed: {e}"));
        }
    }

    pub fn retag_dictation(&mut self, from_wall: f64, to_wall: f64) {
        let _ = self.flush();
        self.run(
            "retag",
            "UPDATE segments SET kind='dictation' WHERE end_wall >= ?1 AND start_wall <= ?2",
            params![from_wall, to_wall],
        );
    }

    pub fn record_command(&mut self, id: &str, text: &str, harness: &str, status: &str) {
        self.run(
            "record",
            "INSERT OR REPLACE INTO commands(id,captured_wall,text,harness,status) VALUES(?1,?2,?3,?4,?5)",
            params![id, wall_now(), text, harness, status],
        );
    }

    /// Durable job ownership; must succeed before the harness is allowed to execute.
    pub fn set_job_owner(
        &mut self,
        id: &str,
        boot_id: &str,
        guard_pid: i64,
        guard_start: i64,
        pgid: i64,
        gate_start: i64,
    ) -> Result<(), String> {
        let n = self
            .db
            .execute(
                "UPDATE commands SET status='starting', boot_id=?2, guard_pid=?3, guard_start=?4, job_pgid=?5, gate_start=?6 WHERE id=?1",
                params![id, boot_id, guard_pid, guard_start, pgid, gate_start],
            )
            .map_err(|e| e.to_string())?;
        if n == 1 {
            Ok(())
        } else {
            Err(format!("job {id} not found"))
        }
    }

    pub fn set_status(&mut self, id: &str, status: &str) {
        self.run(
            "status",
            "UPDATE commands SET status=?2 WHERE id=?1",
            params![id, status],
        );
    }

    pub fn finish_command(
        &mut self,
        id: &str,
        status: &str,
        exit_code: Option<i32>,
        summary: &str,
    ) {
        self.run(
            "finish",
            "UPDATE commands SET status=?2, exit_code=?3, finished_wall=?4, summary=?5 WHERE id=?1",
            params![id, status, exit_code, wall_now(), summary],
        );
    }

    pub fn non_final_jobs(&self) -> Result<Vec<JobRecord>, String> {
        let mut stmt = self
            .db
            .prepare(&format!(
                "SELECT id,status,boot_id,guard_pid,guard_start,job_pgid,gate_start FROM commands WHERE status IN {NON_FINAL} OR status='ambiguous'"
            ))
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                Ok(JobRecord {
                    id: r.get(0)?,
                    status: r.get(1)?,
                    boot_id: r.get(2)?,
                    guard_pid: r.get(3)?,
                    guard_start: r.get(4)?,
                    job_pgid: r.get(5)?,
                    gate_start: r.get(6)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn maintenance(&mut self, retention_secs: f64) {
        let _ = self.flush();
        let cutoff = wall_now() - retention_secs;
        self.run(
            "prune",
            "DELETE FROM segments WHERE end_wall < ?1",
            params![cutoff],
        );
        self.run("prune", &format!("DELETE FROM commands WHERE captured_wall < ?1 AND status NOT IN {NON_FINAL} AND status != 'ambiguous'"), params![cutoff]);
        let _ = self.db.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
    }

    pub fn segments_since(&mut self, since_wall: f64) -> Vec<StoredSegment> {
        let _ = self.flush();
        let Ok(mut stmt) = self.db.prepare("SELECT start_wall,end_wall,text,kind FROM segments WHERE end_wall >= ?1 ORDER BY start_wall") else {
            return vec![];
        };
        stmt.query_map([since_wall], |r| {
            Ok(StoredSegment {
                start: r.get(0)?,
                end: r.get(1)?,
                text: r.get(2)?,
                kind: r.get(3)?,
            })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    /// The newest `limit` segments that ended at or after `since_wall`, oldest first. Read-only: no flush.
    pub fn last_segments(
        &self,
        since_wall: f64,
        limit: usize,
    ) -> Result<Vec<StoredSegment>, String> {
        let mut stmt = self
            .db
            .prepare(
                "SELECT start_wall,end_wall,text,kind FROM segments WHERE end_wall >= ?1 \
                 ORDER BY start_wall DESC, id DESC LIMIT ?2",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![since_wall, limit as i64], |r| {
                Ok(StoredSegment {
                    start: r.get(0)?,
                    end: r.get(1)?,
                    text: r.get(2)?,
                    kind: r.get(3)?,
                })
            })
            .map_err(|e| e.to_string())?;
        let mut out = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        out.reverse();
        Ok(out)
    }

    pub fn commands_since(&self, since_wall: f64) -> Vec<(String, String, String, Option<String>)> {
        let Ok(mut stmt) = self.db.prepare("SELECT id,text,status,summary FROM commands WHERE captured_wall >= ?1 ORDER BY captured_wall") else {
            return vec![];
        };
        stmt.query_map([since_wall], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    /// Commands captured since `since_wall`, plus every unfinished or ambiguous one whatever its age; oldest first.
    pub fn command_rows(&self, since_wall: f64) -> Result<Vec<CommandRow>, String> {
        let mut stmt = self
            .db
            .prepare(&format!(
                "SELECT id,captured_wall,text,harness,status,exit_code,finished_wall,summary FROM commands \
                 WHERE captured_wall >= ?1 OR status IN {NON_FINAL} OR status='ambiguous' ORDER BY captured_wall"
            ))
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([since_wall], |r| {
                Ok(CommandRow {
                    id: r.get(0)?,
                    captured_at: r.get(1)?,
                    text: r.get(2)?,
                    harness: r.get(3)?,
                    status: r.get(4)?,
                    exit_code: r.get(5)?,
                    finished_at: r.get(6)?,
                    summary: r.get(7)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn count(&self, table: &str) -> i64 {
        if !matches!(table, "segments" | "commands") {
            return -1;
        }
        self.db
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap_or(-1)
    }

    /// Returns (and clears) the last write failure so the caller can surface it.
    pub fn take_failure(&mut self) -> Option<String> {
        self.failure.take()
    }
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}
