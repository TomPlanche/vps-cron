//! Persistent run history, stored in its own SQLite database.
//!
//! Every finished run is appended here, including skips and timeouts, so
//! "did the backup actually run last night?" is a query rather than a scroll
//! through journald.
//!
//! `rusqlite` is synchronous, so all database work happens on the blocking
//! pool. Write volume is one row per run, which is far too low to justify a
//! connection pool.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rusqlite::{params, Connection};

use crate::job::{Outcome, RunRecord};

/// Handle to the run-history database.
#[derive(Clone)]
pub struct History {
    conn: Arc<Mutex<Connection>>,
}

impl History {
    /// Opens (and if needed creates) the history database at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("Failed to create history directory '{}'", parent.display())
                })?;
            }
        }

        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open history database '{}'", path.display()))?;

        // WAL keeps the HTTP status reads from blocking the scheduler's writes.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS runs (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 job          TEXT    NOT NULL,
                 started_at   TEXT    NOT NULL,
                 finished_at  TEXT    NOT NULL,
                 duration_ms  INTEGER NOT NULL,
                 outcome      TEXT    NOT NULL,
                 summary      TEXT    NOT NULL,
                 output       TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_runs_job_started ON runs (job, id DESC);
             CREATE INDEX IF NOT EXISTS idx_runs_started ON runs (id DESC);",
        )
        .context("Failed to initialise the history schema")?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Appends one finished run.
    pub async fn record(&self, record: RunRecord) -> Result<()> {
        let conn = Arc::clone(&self.conn);

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("history mutex poisoned");
            conn.execute(
                "INSERT INTO runs (job, started_at, finished_at, duration_ms, outcome, summary, output)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    record.job,
                    record.started_at.to_rfc3339(),
                    record.finished_at.to_rfc3339(),
                    record.duration_ms,
                    record.outcome.as_str(),
                    record.summary,
                    record.output,
                ],
            )?;
            Ok::<_, rusqlite::Error>(())
        })
        .await
        .context("History write task panicked")?
        .context("Failed to write a run to the history")?;

        Ok(())
    }

    /// Returns the most recent runs, newest first.
    ///
    /// Passing a `job` name restricts the listing to that job.
    pub async fn recent(&self, job: Option<String>, limit: u32) -> Result<Vec<RunRecord>> {
        let conn = Arc::clone(&self.conn);

        let rows = tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("history mutex poisoned");
            let (sql, args): (&str, Vec<Box<dyn rusqlite::ToSql>>) = match &job {
                Some(name) => (
                    "SELECT job, started_at, finished_at, duration_ms, outcome, summary, output
                     FROM runs WHERE job = ?1 ORDER BY id DESC LIMIT ?2",
                    vec![Box::new(name.clone()), Box::new(limit)],
                ),
                None => (
                    "SELECT job, started_at, finished_at, duration_ms, outcome, summary, output
                     FROM runs ORDER BY id DESC LIMIT ?1",
                    vec![Box::new(limit)],
                ),
            };

            let mut stmt = conn.prepare(sql)?;
            let param_refs: Vec<&dyn rusqlite::ToSql> =
                args.iter().map(std::convert::AsRef::as_ref).collect();

            let records = stmt
                .query_map(param_refs.as_slice(), |row| {
                    Ok(RunRecord {
                        job: row.get(0)?,
                        started_at: parse_timestamp(&row.get::<_, String>(1)?),
                        finished_at: parse_timestamp(&row.get::<_, String>(2)?),
                        duration_ms: row.get(3)?,
                        outcome: Outcome::from_str_lossy(&row.get::<_, String>(4)?),
                        summary: row.get(5)?,
                        output: row.get(6)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;

            Ok::<_, rusqlite::Error>(records)
        })
        .await
        .context("History read task panicked")?
        .context("Failed to read the run history")?;

        Ok(rows)
    }

    /// Deletes runs older than `keep_days`, returning how many rows went.
    ///
    /// A job on a five-second schedule writes over half a million rows a month,
    /// so an unattended manager needs this to keep the database bounded.
    pub async fn prune(&self, keep_days: u32) -> Result<usize> {
        let conn = Arc::clone(&self.conn);
        let cutoff = (Utc::now() - ChronoDuration::days(i64::from(keep_days))).to_rfc3339();

        let deleted = tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("history mutex poisoned");
            conn.execute("DELETE FROM runs WHERE started_at < ?1", params![cutoff])
        })
        .await
        .context("History prune task panicked")?
        .context("Failed to prune the run history")?;

        Ok(deleted)
    }
}

/// Parses a stored RFC 3339 timestamp.
///
/// A row that somehow holds an unparseable timestamp falls back to the Unix
/// epoch rather than failing the whole listing.
fn parse_timestamp(raw: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| DateTime::UNIX_EPOCH)
}

/// Resolves the history database path, defaulting inside `data_dir`.
pub fn default_path(data_dir: &str) -> PathBuf {
    Path::new(data_dir).join("vps-cron-history.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(job: &str, outcome: Outcome) -> RunRecord {
        let now = Utc::now();
        RunRecord {
            job: job.to_string(),
            started_at: now,
            finished_at: now,
            duration_ms: 12,
            outcome,
            summary: format!("{job} {outcome}"),
            output: None,
        }
    }

    #[tokio::test]
    async fn records_and_reads_back_runs() {
        let dir = std::env::temp_dir().join(format!("vps-cron-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history-roundtrip.db");
        let _ = std::fs::remove_file(&path);

        let history = History::open(&path).unwrap();
        history.record(record("alpha", Outcome::Success)).await.unwrap();
        history.record(record("beta", Outcome::Failure)).await.unwrap();
        history.record(record("alpha", Outcome::Timeout)).await.unwrap();

        let all = history.recent(None, 10).await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].job, "alpha", "newest run should come first");
        assert_eq!(all[0].outcome, Outcome::Timeout);

        let alpha = history.recent(Some("alpha".to_string()), 10).await.unwrap();
        assert_eq!(alpha.len(), 2);
        assert!(alpha.iter().all(|r| r.job == "alpha"));

        let limited = history.recent(None, 1).await.unwrap();
        assert_eq!(limited.len(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn prune_keeps_recent_runs() {
        let dir = std::env::temp_dir().join(format!("vps-cron-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history-prune.db");
        let _ = std::fs::remove_file(&path);

        let history = History::open(&path).unwrap();

        let mut old = record("old", Outcome::Success);
        old.started_at = Utc::now() - ChronoDuration::days(90);
        history.record(old).await.unwrap();
        history.record(record("fresh", Outcome::Success)).await.unwrap();

        let deleted = history.prune(30).await.unwrap();
        assert_eq!(deleted, 1);

        let remaining = history.recent(None, 10).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].job, "fresh");

        let _ = std::fs::remove_file(&path);
    }
}
