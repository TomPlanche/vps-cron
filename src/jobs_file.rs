//! Parsing and validation of the jobs file.
//!
//! The jobs file is the whole point of the manager: schedules live in data
//! rather than in code, so adding or retiming a shell job needs an edit and a
//! restart, not a rebuild.
//!
//! ```toml
//! [[jobs]]
//! name = "backup-db"
//! schedule = "0 0 3 * * *"
//! kind = { shell = "restic backup /srv" }
//! timeout = "10m"
//!
//! [[jobs]]
//! name = "lastfm-recent"
//! schedule = "0 0/1 * * * *"
//! kind = { builtin = "lastfm_recent_plays" }
//! ```

use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use cron::Schedule;
use serde::Deserialize;

/// The parsed contents of a jobs file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobsFile {
    /// Every job declared in the file, enabled or not.
    #[serde(default)]
    pub jobs: Vec<JobSpec>,
}

/// One job declaration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobSpec {
    /// Unique name, used in logs, history rows and HTTP responses.
    pub name: String,
    /// A six-field cron expression, seconds first.
    pub schedule: String,
    /// Set to `false` to keep a declaration around without running it.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// What to actually run.
    pub kind: JobKind,
    /// Kills the run if it lasts longer than this, e.g. `"10m"`.
    #[serde(default, with = "humantime_serde::option")]
    pub timeout: Option<Duration>,
    /// Runs the job once at startup instead of waiting for the first occurrence.
    #[serde(default)]
    pub run_on_start: bool,
    /// Free-form parameters handed to the job at run time.
    #[serde(default)]
    pub args: toml::Table,
}

/// What a job runs.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum JobKind {
    /// An arbitrary command line, run through `sh -c`.
    Shell(ShellCommand),
    /// A built-in job, resolved by name against the registry.
    Builtin(String),
}

impl JobKind {
    /// A short label for logs and HTTP responses.
    pub fn label(&self) -> String {
        match self {
            Self::Shell(cmd) => format!("shell: {}", cmd.command()),
            Self::Builtin(name) => format!("builtin: {name}"),
        }
    }
}

/// A shell command, written either as a bare string or as a table.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ShellCommand {
    /// `kind = { shell = "restic backup /srv" }`
    Line(String),
    /// `kind = { shell = { command = "...", workdir = "...", env = { ... } } }`
    Detailed {
        /// The command line passed to `sh -c`.
        command: String,
        /// Directory to run the command in.
        #[serde(default)]
        workdir: Option<String>,
        /// Extra environment variables for the command.
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
}

impl ShellCommand {
    /// The command line to execute.
    pub fn command(&self) -> &str {
        match self {
            Self::Line(command) | Self::Detailed { command, .. } => command,
        }
    }

    /// The working directory, if one was configured.
    pub fn workdir(&self) -> Option<&str> {
        match self {
            Self::Line(_) => None,
            Self::Detailed { workdir, .. } => workdir.as_deref(),
        }
    }

    /// Extra environment variables, if any were configured.
    pub fn env(&self) -> Option<&BTreeMap<String, String>> {
        match self {
            Self::Line(_) => None,
            Self::Detailed { env, .. } => Some(env),
        }
    }
}

impl JobsFile {
    /// Reads and validates a jobs file.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read jobs file '{}'", path.display()))?;

        let parsed: Self = toml::from_str(&raw)
            .with_context(|| format!("Failed to parse jobs file '{}'", path.display()))?;

        parsed
            .validate()
            .with_context(|| format!("Invalid jobs file '{}'", path.display()))?;

        Ok(parsed)
    }

    /// Rejects duplicate names, blank names and unparseable cron expressions.
    ///
    /// Validation covers disabled jobs too, so a typo cannot lie dormant until
    /// the day it gets switched on.
    fn validate(&self) -> Result<()> {
        let mut seen: BTreeMap<&str, ()> = BTreeMap::new();

        for job in &self.jobs {
            if job.name.trim().is_empty() {
                bail!("A job has a blank name");
            }

            if seen.insert(job.name.as_str(), ()).is_some() {
                bail!("Duplicate job name: '{}'", job.name);
            }

            job.parse_schedule()?;

            if let JobKind::Shell(cmd) = &job.kind {
                if cmd.command().trim().is_empty() {
                    bail!("Job '{}' has a blank shell command", job.name);
                }
            }
        }

        Ok(())
    }
}

impl JobSpec {
    /// Parses this job's cron expression.
    pub fn parse_schedule(&self) -> Result<Schedule> {
        Schedule::from_str(&self.schedule).with_context(|| {
            format!(
                "Job '{}' has an invalid cron expression '{}'",
                self.name, self.schedule
            )
        })
    }
}

/// Jobs are enabled unless the file says otherwise.
fn default_enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Result<JobsFile> {
        let parsed: JobsFile = toml::from_str(raw)?;
        parsed.validate()?;
        Ok(parsed)
    }

    #[test]
    fn parses_the_documented_shapes() {
        let file = parse(
            r#"
            [[jobs]]
            name = "backup"
            schedule = "0 0 3 * * *"
            kind = { shell = "restic backup /srv" }
            timeout = "10m"

            [[jobs]]
            name = "lastfm-recent"
            schedule = "0 0/1 * * * *"
            kind = { builtin = "lastfm_recent_plays" }
            run_on_start = true
            args = { limit = 100 }
            "#,
        )
        .expect("file should parse");

        assert_eq!(file.jobs.len(), 2);
        assert_eq!(file.jobs[0].timeout, Some(Duration::from_secs(600)));
        assert!(file.jobs[0].enabled);
        assert!(file.jobs[1].run_on_start);
        assert_eq!(file.jobs[1].args.get("limit").unwrap().as_integer(), Some(100));
    }

    #[test]
    fn parses_a_detailed_shell_command() {
        let file = parse(
            r#"
            [[jobs]]
            name = "deploy"
            schedule = "0 0 4 * * *"
            kind = { shell = { command = "make deploy", workdir = "/srv/app", env = { RUST_LOG = "info" } } }
            "#,
        )
        .expect("file should parse");

        let JobKind::Shell(cmd) = &file.jobs[0].kind else {
            panic!("expected a shell job");
        };
        assert_eq!(cmd.command(), "make deploy");
        assert_eq!(cmd.workdir(), Some("/srv/app"));
        assert_eq!(cmd.env().unwrap().get("RUST_LOG").unwrap(), "info");
    }

    #[test]
    fn the_committed_jobs_file_is_valid() {
        // Guards the file the service actually boots from: a typo here would
        // otherwise only show up as a failed start on the VPS.
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("jobs.toml");
        JobsFile::load(&path).expect("the committed jobs.toml should be valid");
    }

    #[test]
    fn rejects_duplicate_names() {
        let err = parse(
            r#"
            [[jobs]]
            name = "dupe"
            schedule = "0 0 3 * * *"
            kind = { shell = "true" }

            [[jobs]]
            name = "dupe"
            schedule = "0 0 4 * * *"
            kind = { shell = "true" }
            "#,
        )
        .expect_err("duplicates should be rejected");

        assert!(err.to_string().contains("Duplicate job name"));
    }

    #[test]
    fn rejects_a_bad_cron_expression_even_when_disabled() {
        let err = parse(
            r#"
            [[jobs]]
            name = "broken"
            schedule = "not a cron expression"
            enabled = false
            kind = { shell = "true" }
            "#,
        )
        .expect_err("bad schedules should be rejected");

        assert!(err.to_string().contains("invalid cron expression"));
    }

    #[test]
    fn rejects_a_blank_shell_command() {
        let err = parse(
            r#"
            [[jobs]]
            name = "empty"
            schedule = "0 0 3 * * *"
            kind = { shell = "   " }
            "#,
        )
        .expect_err("blank commands should be rejected");

        assert!(err.to_string().contains("blank shell command"));
    }
}
