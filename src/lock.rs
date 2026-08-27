//! Cross-process job locks.
//!
//! The in-process guard in the scheduler stops a job from overlapping itself
//! inside the daemon, but it cannot see a `vps-cron run` invoked from a
//! terminal. Without something shared, a manual run of `lastfm-scrobbles-db`
//! during a scheduled one would have two processes writing the same SQLite
//! file and the same JSON export.
//!
//! An advisory lock file per job closes that gap: the daemon and the CLI take
//! the same lock, and whoever arrives second backs off instead of racing.
//!
//! This uses `std::fs::File::try_lock`, stable since Rust 1.89, so it needs no
//! dependency.

use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// An acquired job lock, released when dropped.
///
/// The lock file itself is deliberately left behind: removing it would race
/// with another process opening it, and an empty file costs nothing.
#[derive(Debug)]
pub struct JobLock {
    _file: File,
}

/// Where job lock files live.
#[derive(Debug, Clone)]
pub struct LockDir {
    dir: PathBuf,
}

impl LockDir {
    /// Creates the lock directory under `data_dir`.
    pub fn new(data_dir: &str) -> Result<Self> {
        let dir = Path::new(data_dir).join("locks");

        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create the lock directory '{}'", dir.display()))?;

        Ok(Self { dir })
    }

    /// Takes the lock for `job`, or returns `None` if another process holds it.
    ///
    /// This never blocks: a held lock means the job is already running, which
    /// is an answer rather than something to wait for.
    pub fn try_acquire(&self, job: &str) -> Result<Option<JobLock>> {
        let path = self.dir.join(format!("{}.lock", sanitise(job)));

        let file = File::create(&path)
            .with_context(|| format!("Failed to open the lock file '{}'", path.display()))?;

        match file.try_lock() {
            Ok(()) => Ok(Some(JobLock { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("Failed to lock '{}'", path.display()))
            }
        }
    }
}

/// Makes a job name safe to use as a file name.
///
/// Job names are free text from the jobs file, so a name containing a slash
/// would otherwise point the lock at a different directory entirely.
fn sanitise(job: &str) -> String {
    job.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("vps-cron-lock-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.display().to_string()
    }

    #[test]
    fn a_second_acquire_is_refused_while_the_first_is_held() {
        let locks = LockDir::new(&temp_dir("held")).unwrap();

        let first = locks.try_acquire("backup").unwrap();
        assert!(first.is_some(), "the first acquire should succeed");

        // Same process, same file: the lock must still be visible.
        assert!(
            locks.try_acquire("backup").unwrap().is_none(),
            "a held lock should refuse a second acquire"
        );

        drop(first);
        assert!(
            locks.try_acquire("backup").unwrap().is_some(),
            "dropping the lock should release it"
        );
    }

    #[test]
    fn different_jobs_do_not_block_each_other() {
        let locks = LockDir::new(&temp_dir("distinct")).unwrap();

        let _a = locks.try_acquire("alpha").unwrap().unwrap();
        assert!(locks.try_acquire("beta").unwrap().is_some());
    }

    #[test]
    fn a_name_with_a_slash_cannot_escape_the_lock_directory() {
        assert_eq!(sanitise("../../etc/passwd"), "______etc_passwd");
        assert_eq!(sanitise("lastfm-recent_plays"), "lastfm-recent_plays");
    }
}
