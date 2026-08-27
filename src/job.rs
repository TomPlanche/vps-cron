//! The core job abstraction shared by every scheduled unit of work.
//!
//! A [`Job`] is anything the scheduler can run on a cron expression: a shell
//! command, or a built-in written in Rust. The scheduler owns the schedule, the
//! overlap guard and the timeout, so an implementation only has to describe how
//! to do the work once.

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Longest captured output kept per run, in characters.
///
/// Anything longer is truncated from the front so the most recent (and usually
/// most relevant) output survives.
const MAX_OUTPUT_CHARS: usize = 4_000;

/// What a job produced when it finished successfully.
#[derive(Debug)]
pub struct JobReport {
    /// Short single-line summary recorded in the run history.
    pub summary: String,
    /// Captured output, if the job produced any.
    pub output: Option<String>,
}

impl JobReport {
    /// Builds a report with a summary and no captured output.
    pub fn summary(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            output: None,
        }
    }

    /// Attaches captured output, truncated to the last [`MAX_OUTPUT_CHARS`].
    pub fn with_output(mut self, output: impl Into<String>) -> Self {
        let output = output.into();
        let trimmed = output.trim();
        if trimmed.is_empty() {
            self.output = None;
        } else {
            self.output = Some(truncate_tail(trimmed));
        }
        self
    }
}

/// Everything a job is told about the run it is currently performing.
pub struct JobContext<'a> {
    /// The job name as written in the jobs file.
    pub job_name: &'a str,
    /// Free-form per-job parameters from the jobs file `args` table.
    pub args: &'a toml::Table,
    /// When the scheduler started this run.
    pub started_at: DateTime<Utc>,
}

impl JobContext<'_> {
    /// Reads a string argument from the job's `args` table.
    pub fn arg_str(&self, key: &str) -> Option<&str> {
        self.args.get(key).and_then(toml::Value::as_str)
    }

    /// Reads an integer argument from the job's `args` table.
    pub fn arg_u32(&self, key: &str) -> Option<u32> {
        self.args
            .get(key)
            .and_then(toml::Value::as_integer)
            .and_then(|v| u32::try_from(v).ok())
    }
}

/// A failed run.
///
/// This is a type of its own rather than a plain [`anyhow::Error`] so a failure
/// can carry captured output: the moment a backup breaks is exactly the moment
/// its stderr is worth keeping.
#[derive(Debug)]
pub struct JobFailure {
    /// Single-line description of what went wrong.
    pub message: String,
    /// Captured output, if the job produced any before failing.
    pub output: Option<String>,
}

impl JobFailure {
    /// Builds a failure from a message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            output: None,
        }
    }

    /// Attaches captured output, truncated to the last [`MAX_OUTPUT_CHARS`].
    pub fn with_output(mut self, output: impl Into<String>) -> Self {
        let output = output.into();
        let trimmed = output.trim();
        self.output = if trimmed.is_empty() {
            None
        } else {
            Some(truncate_tail(trimmed))
        };
        self
    }
}

/// Flattens the whole `anyhow` context chain onto one line, so a history
/// summary stays readable in a table.
impl From<anyhow::Error> for JobFailure {
    fn from(error: anyhow::Error) -> Self {
        Self::new(format!("{error:#}"))
    }
}

/// What a job hands back to the scheduler.
pub type JobResult = std::result::Result<JobReport, JobFailure>;

/// A unit of work the scheduler can run.
#[async_trait]
pub trait Job: Send + Sync {
    /// Performs the work. Returning `Err` marks the run as failed; the
    /// scheduler logs it and carries on to the next occurrence.
    ///
    /// Implementations should produce their errors through `anyhow` `.context()`
    /// so `?` converts them into a [`JobFailure`] automatically.
    async fn run(&self, ctx: &JobContext<'_>) -> JobResult;
}

/// How a single run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The job ran to completion without error.
    Success,
    /// The job returned an error.
    Failure,
    /// The job exceeded its configured timeout and was cancelled.
    Timeout,
    /// The previous run was still going, so this occurrence was skipped.
    Skipped,
}

impl Outcome {
    /// The wire and database representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Timeout => "timeout",
            Self::Skipped => "skipped",
        }
    }

    /// Parses the representation produced by [`Outcome::as_str`].
    ///
    /// Unrecognised values become [`Outcome::Failure`] so a row written by an
    /// older build can never break a history read.
    pub fn from_str_lossy(raw: &str) -> Self {
        match raw {
            "success" => Self::Success,
            "timeout" => Self::Timeout,
            "skipped" => Self::Skipped,
            _ => Self::Failure,
        }
    }

    /// Whether this outcome should be reported as a problem.
    pub fn is_problem(self) -> bool {
        matches!(self, Self::Failure | Self::Timeout)
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A completed run, as stored in the history and served over HTTP.
#[derive(Debug, Clone, Serialize)]
pub struct RunRecord {
    /// Name of the job that ran.
    pub job: String,
    /// When the run started.
    pub started_at: DateTime<Utc>,
    /// When the run finished.
    pub finished_at: DateTime<Utc>,
    /// How long the run took.
    pub duration_ms: u64,
    /// How the run ended.
    pub outcome: Outcome,
    /// Single-line summary, or the error message for a failed run.
    pub summary: String,
    /// Captured output tail, if any.
    pub output: Option<String>,
}

/// Truncates to the last [`MAX_OUTPUT_CHARS`] characters, marking the cut.
fn truncate_tail(text: &str) -> String {
    let char_count = text.chars().count();
    if char_count <= MAX_OUTPUT_CHARS {
        return text.to_string();
    }

    let skipped = char_count - MAX_OUTPUT_CHARS;
    let tail: String = text.chars().skip(skipped).collect();
    format!("[truncated {skipped} characters]\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_output_is_kept_verbatim() {
        let report = JobReport::summary("done").with_output("all good");
        assert_eq!(report.output.as_deref(), Some("all good"));
    }

    #[test]
    fn blank_output_becomes_none() {
        let report = JobReport::summary("done").with_output("  \n ");
        assert!(report.output.is_none());
    }

    #[test]
    fn long_output_keeps_the_tail() {
        let long = "x".repeat(MAX_OUTPUT_CHARS + 10);
        let report = JobReport::summary("done").with_output(long);
        let output = report.output.expect("output should be kept");
        assert!(output.starts_with("[truncated 10 characters]"));
        assert!(output.ends_with("xxx"));
    }

    #[test]
    fn unknown_outcome_reads_back_as_failure() {
        assert_eq!(Outcome::from_str_lossy("nonsense"), Outcome::Failure);
        assert_eq!(Outcome::from_str_lossy("skipped"), Outcome::Skipped);
    }
}
