//! The cron scheduler.
//!
//! One tokio task per job owns that job's schedule and does nothing but wait
//! for the next occurrence. The work itself is spawned onto a separate task
//! holding an overlap guard, so a run that overruns its window never delays the
//! ticks behind it: the scheduler notices the guard is taken and records a skip
//! instead.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use cron::Schedule;
use serde::Serialize;
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::builtins::shell::ShellJob;
use crate::history::History;
use crate::job::{Job, JobContext, JobFailure, JobReport, JobResult, Outcome, RunRecord};
use crate::jobs_file::{JobKind, JobSpec, JobsFile};
use crate::registry::Registry;

/// Live status for every configured job, shared with the status server.
pub type SharedStatus = Arc<RwLock<BTreeMap<String, JobStatus>>>;

/// What the status server reports about one job.
#[derive(Debug, Clone, Serialize)]
pub struct JobStatus {
    /// The job name from the jobs file.
    pub name: String,
    /// Its cron expression.
    pub schedule: String,
    /// A short description of what it runs.
    pub kind: String,
    /// Whether the scheduler is running it at all.
    pub enabled: bool,
    /// Whether a run is in flight right now.
    pub running: bool,
    /// When the next occurrence is due, for enabled jobs.
    pub next_run: Option<DateTime<Utc>>,
    /// The most recent finished run.
    pub last_run: Option<RunRecord>,
    /// How many runs have actually executed since startup.
    pub total_runs: u64,
    /// How many of those failed or timed out.
    pub total_failures: u64,
    /// How many occurrences were skipped because a run was still going.
    pub total_skipped: u64,
}

impl JobStatus {
    /// Builds the pre-run status for a configured job.
    fn new(spec: &JobSpec) -> Self {
        Self {
            name: spec.name.clone(),
            schedule: spec.schedule.clone(),
            kind: spec.kind.label(),
            enabled: spec.enabled,
            running: false,
            next_run: None,
            last_run: None,
            total_runs: 0,
            total_failures: 0,
            total_skipped: 0,
        }
    }
}

/// A job resolved from the jobs file and ready to run.
struct ScheduledJob {
    spec: JobSpec,
    schedule: Schedule,
    job: Arc<dyn Job>,
    /// Held for the duration of a run; a taken guard means "still running".
    guard: Arc<Mutex<()>>,
}

/// Owns the resolved jobs and the shared state they report into.
pub struct Scheduler {
    jobs: Vec<ScheduledJob>,
    status: SharedStatus,
    history: History,
    /// Counts runs that finished, so tests and logs can observe progress.
    completed: Arc<AtomicU64>,
}

impl Scheduler {
    /// Resolves every job in the file against the registry.
    ///
    /// Resolution happens up front so an unknown builtin or an unusable
    /// schedule stops the process at startup, rather than at 3am on the first
    /// occurrence.
    pub fn build(file: JobsFile, registry: &Registry, history: History) -> Result<Self> {
        let mut jobs = Vec::new();
        let mut status = BTreeMap::new();

        for spec in file.jobs {
            status.insert(spec.name.clone(), JobStatus::new(&spec));

            if !spec.enabled {
                info!(job = %spec.name, "Job is disabled, skipping");
                continue;
            }

            let schedule = spec.parse_schedule()?;

            let job: Arc<dyn Job> = match &spec.kind {
                JobKind::Shell(command) => Arc::new(ShellJob::new(command.clone())),
                JobKind::Builtin(name) => registry
                    .resolve(name)
                    .with_context(|| format!("Job '{}' could not be resolved", spec.name))?,
            };

            jobs.push(ScheduledJob {
                spec,
                schedule,
                job,
                guard: Arc::new(Mutex::new(())),
            });
        }

        Ok(Self {
            jobs,
            status: Arc::new(RwLock::new(status)),
            history,
            completed: Arc::new(AtomicU64::new(0)),
        })
    }

    /// A handle to the live status map, for the status server.
    pub fn status(&self) -> SharedStatus {
        Arc::clone(&self.status)
    }

    /// How many jobs are enabled and will actually be scheduled.
    pub fn enabled_count(&self) -> usize {
        self.jobs.len()
    }

    /// Spawns one task per enabled job. The tasks run until the process exits.
    pub fn spawn(self) -> Vec<JoinHandle<()>> {
        let Self {
            jobs,
            status,
            history,
            completed,
        } = self;

        jobs.into_iter()
            .map(|job| {
                let runner = Runner {
                    status: Arc::clone(&status),
                    history: history.clone(),
                    completed: Arc::clone(&completed),
                };
                tokio::spawn(async move { runner.drive(job).await })
            })
            .collect()
    }
}

/// The shared machinery each job task needs to report its runs.
#[derive(Clone)]
struct Runner {
    status: SharedStatus,
    history: History,
    completed: Arc<AtomicU64>,
}

impl Runner {
    /// Drives one job forever: wait for the next occurrence, then dispatch it.
    async fn drive(self, job: ScheduledJob) {
        let name = job.spec.name.clone();
        info!(job = %name, schedule = %job.spec.schedule, kind = %job.spec.kind.label(), "Scheduling job");

        let job = Arc::new(job);

        if job.spec.run_on_start {
            self.dispatch(Arc::clone(&job)).await;
        }

        loop {
            let now = Utc::now();

            let Some(delay) = next_delay(&job.schedule, now) else {
                // A schedule with nothing left ahead of it (a fixed date in the
                // past, say) would otherwise spin. Stop driving it instead.
                warn!(job = %name, "Schedule has no further occurrences, stopping this job");
                self.set_next_run(&name, None).await;
                return;
            };

            self.set_next_run(&name, Some(now + chrono::Duration::from_std(delay).unwrap_or_default()))
                .await;

            tokio::time::sleep(delay).await;
            self.dispatch(Arc::clone(&job)).await;
        }
    }

    /// Starts one run, or records a skip when the previous one is still going.
    ///
    /// The run is spawned rather than awaited so the schedule keeps ticking on
    /// time no matter how long the work takes.
    async fn dispatch(&self, job: Arc<ScheduledJob>) {
        let name = job.spec.name.clone();

        let Ok(guard) = Arc::clone(&job.guard).try_lock_owned() else {
            warn!(job = %name, "Previous run is still going, skipping this occurrence");
            let now = Utc::now();
            self.finish(
                &name,
                RunRecord {
                    job: name.clone(),
                    started_at: now,
                    finished_at: now,
                    duration_ms: 0,
                    outcome: Outcome::Skipped,
                    summary: "Skipped: the previous run was still going".to_string(),
                    output: None,
                },
            )
            .await;
            return;
        };

        let runner = self.clone();
        tokio::spawn(async move {
            // Holding the guard for the whole run is what makes the next
            // occurrence skip rather than pile up.
            let _guard = guard;
            runner.execute(&job).await;
        });
    }

    /// Runs the job, applies its timeout, and records the outcome.
    async fn execute(&self, job: &ScheduledJob) {
        let name = &job.spec.name;
        let started_at = Utc::now();
        let start = std::time::Instant::now();

        self.set_running(name, true).await;

        let ctx = JobContext {
            job_name: name,
            args: &job.spec.args,
            started_at,
        };

        // Timeouts are tracked as a flag rather than sniffed out of the error
        // message later, so a job that legitimately reports "timed out" itself
        // is not miscategorised.
        let (outcome, timed_out) = match job.spec.timeout {
            Some(limit) => match tokio::time::timeout(limit, job.job.run(&ctx)).await {
                Ok(result) => (result, false),
                Err(_) => (
                    Err(JobFailure::new(format!(
                        "Timed out after {}",
                        humantime::format_duration(limit)
                    ))),
                    true,
                ),
            },
            None => (job.job.run(&ctx).await, false),
        };

        let record = Self::to_record(name, started_at, start.elapsed(), outcome, timed_out);

        match record.outcome {
            Outcome::Success => {
                info!(job = %name, duration_ms = record.duration_ms, "{}", record.summary);
            }
            _ => {
                error!(job = %name, duration_ms = record.duration_ms, "{}", record.summary);
            }
        }

        self.set_running(name, false).await;
        self.finish(name, record).await;
    }

    /// Turns a job result into the record written to the history.
    fn to_record(
        name: &str,
        started_at: DateTime<Utc>,
        elapsed: Duration,
        outcome: JobResult,
        timed_out: bool,
    ) -> RunRecord {
        let (result, summary, output) = match outcome {
            Ok(JobReport { summary, output }) => (Outcome::Success, summary, output),
            Err(failure) if timed_out => (Outcome::Timeout, failure.message, failure.output),
            Err(failure) => (Outcome::Failure, failure.message, failure.output),
        };

        RunRecord {
            job: name.to_string(),
            started_at,
            finished_at: Utc::now(),
            duration_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            outcome: result,
            summary,
            output,
        }
    }

    /// Publishes a finished run to the status map and the history database.
    async fn finish(&self, name: &str, record: RunRecord) {
        {
            let mut status = self.status.write().await;
            if let Some(entry) = status.get_mut(name) {
                // A skipped occurrence is recorded, but it is not a run: counting
                // it as one would overstate how often the job actually executed.
                match record.outcome {
                    Outcome::Skipped => entry.total_skipped += 1,
                    outcome => {
                        entry.total_runs += 1;
                        if outcome.is_problem() {
                            entry.total_failures += 1;
                        }
                    }
                }
                entry.last_run = Some(record.clone());
            }
        }

        if let Err(error) = self.history.record(record).await {
            // A history write failing must never take the scheduler down: the
            // job itself already did its work.
            error!(job = %name, "Failed to record the run: {error:#}");
        }

        self.completed.fetch_add(1, Ordering::Relaxed);
    }

    /// Marks whether a run is currently in flight.
    async fn set_running(&self, name: &str, running: bool) {
        let mut status = self.status.write().await;
        if let Some(entry) = status.get_mut(name) {
            entry.running = running;
        }
    }

    /// Publishes when the next occurrence is due.
    async fn set_next_run(&self, name: &str, next: Option<DateTime<Utc>>) {
        let mut status = self.status.write().await;
        if let Some(entry) = status.get_mut(name) {
            entry.next_run = next;
        }
    }
}

/// How long to wait for the next occurrence of `schedule` after `now`.
///
/// Sub-second precision matters here: truncating to whole seconds makes a
/// short schedule (every five seconds, say) wake early and spin.
fn next_delay(schedule: &Schedule, now: DateTime<Utc>) -> Option<Duration> {
    let next = schedule.after(&now).next()?;
    Some((next - now).to_std().unwrap_or(Duration::ZERO))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn sub_second_delays_survive() {
        let schedule = Schedule::from_str("0/5 * * * * *").unwrap();
        let now = DateTime::parse_from_rfc3339("2026-01-01T00:00:00.100Z")
            .unwrap()
            .with_timezone(&Utc);

        let delay = next_delay(&schedule, now).expect("a 5s schedule always has a next tick");

        // Truncating to whole seconds would give 4s here and wake 900ms early.
        assert_eq!(delay, Duration::from_millis(4_900));
    }

    #[test]
    fn a_schedule_with_no_future_occurrence_yields_none() {
        // A fixed date that has already passed.
        let schedule = Schedule::from_str("0 0 0 1 1 * 2020").unwrap();
        let now = Utc::now();

        assert!(next_delay(&schedule, now).is_none());
    }

    #[test]
    fn hourly_schedule_lands_on_the_hour() {
        let schedule = Schedule::from_str("0 0 * * * *").unwrap();
        let now = DateTime::parse_from_rfc3339("2026-01-01T10:30:00Z")
            .unwrap()
            .with_timezone(&Utc);

        assert_eq!(next_delay(&schedule, now), Some(Duration::from_secs(1_800)));
    }
}
