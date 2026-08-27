//! Process-level configuration, read from the environment.
//!
//! The split is deliberate: schedules and job definitions live in the jobs file
//! where they can be edited without a rebuild, while secrets and paths stay in
//! the environment where they can be kept out of version control.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

/// The default jobs file, relative to the working directory.
const DEFAULT_JOBS_FILE: &str = "jobs.toml";
/// The default directory for generated data and the run history.
const DEFAULT_DATA_DIR: &str = "./data";
/// The default address for the status server.
const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:8787";
/// The default retention window for run history.
const DEFAULT_HISTORY_DAYS: u32 = 30;

/// Everything the manager itself needs to start.
pub struct Config {
    /// Path to the jobs file.
    pub jobs_file: PathBuf,
    /// Directory for generated files and the run history database.
    pub data_dir: String,
    /// Address the status server binds to, or `None` when it is disabled.
    pub http_addr: Option<String>,
    /// How many days of run history to keep.
    pub history_days: u32,
    /// GitHub settings, present only when a token is configured.
    pub github: Option<Arc<GitHubSettings>>,
    /// Last.fm settings, present only when the environment supplies them.
    ///
    /// Shared behind an `Arc` because every Last.fm built-in holds onto it.
    pub lastfm: Option<Arc<LastFmSettings>>,
}

/// Credentials and paths for the GitHub built-ins.
///
/// The token lives here rather than under Last.fm because it is a
/// GitHub-wide concern: the activity job needs it without any gist involved.
pub struct GitHubSettings {
    /// A GitHub personal access token.
    pub token: String,
    /// Where the GitHub exports are written.
    pub destination_folder: String,
    /// Gist target, present only when a gist ID is configured.
    pub gist: Option<GistTarget>,
}

/// Where the gist built-in publishes.
pub struct GistTarget {
    /// The target gist ID.
    pub id: String,
    /// The file within the gist to overwrite.
    pub filename: String,
}

/// Credentials and paths for the Last.fm built-ins.
pub struct LastFmSettings {
    /// The Last.fm account to read.
    pub username: String,
    /// Where the JSON exports are written.
    pub destination_folder: String,
    /// Path to the scrobble history database.
    pub db_file: String,
}

impl Config {
    /// Reads configuration from the environment.
    ///
    /// Only the manager's own settings are required. Last.fm is optional so the
    /// binary runs as a plain cron manager on a host with no Last.fm at all.
    pub fn from_env() -> Result<Self> {
        let data_dir = env_or(DEFAULT_DATA_DIR, "DATA_DIR");

        let jobs_file = std::env::var("JOBS_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_JOBS_FILE));

        let http_addr = match std::env::var("HTTP_ADDR") {
            // An explicit empty value is the documented way to turn the server off.
            Ok(addr) if addr.trim().is_empty() => None,
            Ok(addr) => Some(addr),
            Err(_) => Some(DEFAULT_HTTP_ADDR.to_string()),
        };

        let history_days = match std::env::var("HISTORY_RETENTION_DAYS") {
            Ok(raw) => raw
                .trim()
                .parse()
                .with_context(|| format!("HISTORY_RETENTION_DAYS is not a number: '{raw}'"))?,
            Err(_) => DEFAULT_HISTORY_DAYS,
        };

        Ok(Self {
            jobs_file,
            github: GitHubSettings::from_env(&data_dir).map(Arc::new),
            lastfm: LastFmSettings::from_env(&data_dir).map(Arc::new),
            data_dir,
            http_addr,
            history_days,
        })
    }

    /// Creates the data directory, failing if the path is taken by a file.
    pub fn ensure_data_dir(&self) -> Result<()> {
        ensure_dir(&self.data_dir)
    }
}

impl LastFmSettings {
    /// Reads the Last.fm settings, returning `None` when the account is unset.
    ///
    /// `LAST_FM_USERNAME` is the switch: without it there is nothing to fetch,
    /// so the Last.fm built-ins simply do not get registered.
    fn from_env(data_dir: &str) -> Option<Self> {
        let username = non_empty_env("LAST_FM_USERNAME")?;

        let destination_folder = env_or(data_dir, "DESTINATION_FOLDER");

        let db_file = std::env::var("LAST_FM_DB_FILE")
            .or_else(|_| std::env::var("DB_FILE"))
            .unwrap_or_else(|_| {
                Path::new(data_dir)
                    .join("scrobbles.db")
                    .display()
                    .to_string()
            });

        Some(Self {
            username,
            destination_folder,
            db_file,
        })
    }

    /// Creates the folder the JSON exports are written to.
    pub fn ensure_destination_folder(&self) -> Result<()> {
        ensure_dir(&self.destination_folder)
    }
}

impl GitHubSettings {
    /// Reads the GitHub settings, returning `None` when no token is set.
    ///
    /// `GITHUB_TOKEN` is the switch. The gist target is separate: a token with
    /// no `GIST_ID` still powers the activity job.
    fn from_env(data_dir: &str) -> Option<Self> {
        Some(Self {
            token: non_empty_env("GITHUB_TOKEN")?,
            destination_folder: env_or(data_dir, "GITHUB_DESTINATION_FOLDER"),
            gist: non_empty_env("GIST_ID").map(|id| GistTarget {
                id,
                filename: non_empty_env("GIST_FILENAME")
                    .unwrap_or_else(|| "top-tracks.md".to_string()),
            }),
        })
    }

    /// Creates the folder the GitHub exports are written to.
    pub fn ensure_destination_folder(&self) -> Result<()> {
        ensure_dir(&self.destination_folder)
    }
}

/// Reads an environment variable, treating blank values as unset.
fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Reads an environment variable, falling back to `default`.
fn env_or(default: &str, key: &str) -> String {
    non_empty_env(key).unwrap_or_else(|| default.to_string())
}

/// Creates `dir` if needed, rejecting a path already taken by a file.
fn ensure_dir(dir: &str) -> Result<()> {
    let path = Path::new(dir);

    if path.is_dir() {
        return Ok(());
    }

    anyhow::ensure!(
        !path.exists(),
        "Path exists but is not a directory: '{dir}'"
    );

    std::fs::create_dir_all(path).with_context(|| format!("Failed to create directory '{dir}'"))
}
