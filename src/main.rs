//! vps-cron: a small cron manager for a single VPS.
//!
//! Jobs are declared in a TOML file, not in code. Each one is either a shell
//! command or a built-in compiled into the binary, and the scheduler gives all
//! of them the same treatment: overlap protection, optional timeouts,
//! structured logs and a row in the run history.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use lastfm_client::LastFmClient;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

mod builtins;
mod cli;
mod config;
mod history;
mod http;
mod job;
mod jobs_file;
mod lock;
mod registry;
mod scheduler;
mod update_gist;

use builtins::lastfm::LastFm;
use cli::Command;
use config::Config;
use history::History;
use jobs_file::{JobSpec, JobsFile};
use lock::LockDir;
use registry::Registry;
use scheduler::Scheduler;

/// How often to prune the run history.
const PRUNE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

#[tokio::main]
async fn main() -> Result<()> {
    let command = match cli::parse(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("{error}\n\n{}", cli::USAGE);
            std::process::exit(2);
        }
    };

    if command == Command::Help {
        println!("{}", cli::USAGE);
        return Ok(());
    }

    dotenv::dotenv().ok();

    // A one-shot run prints its own report, so the log preamble that suits a
    // daemon would only be noise in front of it.
    init_tracing(matches!(command, Command::Serve));

    let config = Config::from_env().context("Invalid configuration")?;
    config.ensure_data_dir()?;
    let jobs_file = JobsFile::load(&config.jobs_file)?;

    match command {
        Command::Help => unreachable!("handled above"),
        Command::List => {
            list_jobs(&jobs_file);
            Ok(())
        }
        Command::Run { job } => run_one(&config, jobs_file, &job).await,
        Command::Serve => serve(config, jobs_file).await,
    }
}

/// Runs the scheduler until the process is stopped.
async fn serve(config: Config, jobs_file: JobsFile) -> Result<()> {
    let registry = build_registry(&config)?;
    let history = History::open(&history::default_path(&config.data_dir))?;
    let locks = LockDir::new(&config.data_dir)?;

    let scheduler = Scheduler::build(jobs_file, &registry, history.clone(), locks)?;

    if scheduler.enabled_count() == 0 {
        warn!(
            "No enabled jobs in '{}'. The manager will idle.",
            config.jobs_file.display()
        );
    }

    let status = scheduler.status();

    if let Some(addr) = config.http_addr.clone() {
        let (status, history) = (Arc::clone(&status), history.clone());
        tokio::spawn(async move {
            if let Err(error) = http::serve(&addr, status, history).await {
                error!("Status server failed: {error:#}");
            }
        });
    }

    spawn_pruner(history, config.history_days);

    info!(
        jobs = scheduler.enabled_count(),
        file = %config.jobs_file.display(),
        "vps-cron started"
    );

    scheduler.spawn();

    // Job tasks run forever, so the process lives until it is told to stop.
    tokio::signal::ctrl_c()
        .await
        .context("Failed to listen for shutdown")?;

    info!("Shutting down");
    Ok(())
}

/// Runs one job once, now, and reports what happened.
///
/// Disabled jobs are runnable this way on purpose: keeping a job in the file
/// with `enabled = false` and triggering it by hand is a reasonable way to
/// work.
async fn run_one(config: &Config, jobs_file: JobsFile, name: &str) -> Result<()> {
    let spec = find_job(&jobs_file, name)?;

    let registry = build_registry(config)?;
    let job = scheduler::resolve(spec, &registry)?;
    let history = History::open(&history::default_path(&config.data_dir))?;
    let locks = LockDir::new(&config.data_dir)?;

    // Same lock the daemon takes, so a manual run cannot collide with a
    // scheduled one already in flight.
    let Some(_lock) = locks.try_acquire(name)? else {
        bail!(
            "Job '{name}' is already running (the scheduler holds its lock). Try again once it finishes."
        );
    };

    if !spec.enabled {
        eprintln!("Note: '{name}' is disabled in the jobs file, running it anyway.");
    }

    let record = scheduler::execute_once(spec, job.as_ref()).await;

    println!("{}: {}", record.outcome, record.summary);
    println!("took {} ms", record.duration_ms);
    if let Some(output) = &record.output {
        println!("---");
        println!("{output}");
    }

    history.record(record.clone()).await?;

    // A failed job should fail the command, so this composes with && in a
    // shell and with any wrapper that checks exit codes.
    if record.outcome.is_problem() {
        std::process::exit(1);
    }

    Ok(())
}

/// Prints the configured jobs.
fn list_jobs(jobs_file: &JobsFile) {
    if jobs_file.jobs.is_empty() {
        println!("No jobs configured.");
        return;
    }

    let width = jobs_file
        .jobs
        .iter()
        .map(|job| job.name.len())
        .max()
        .unwrap_or(4);

    for job in &jobs_file.jobs {
        let state = if job.enabled { "enabled " } else { "disabled" };
        println!(
            "{:<width$}  {state}  {:<15}  {}",
            job.name,
            job.schedule,
            job.kind.label(),
            width = width
        );
    }
}

/// Finds a job by name, listing the alternatives when it is missing.
fn find_job<'a>(jobs_file: &'a JobsFile, name: &str) -> Result<&'a JobSpec> {
    jobs_file
        .jobs
        .iter()
        .find(|job| job.name == name)
        .with_context(|| {
            let names: Vec<&str> = jobs_file.jobs.iter().map(|j| j.name.as_str()).collect();
            format!("No job named '{name}'. Configured jobs: {}", names.join(", "))
        })
}

/// Builds the registry, adding the Last.fm built-ins when configured.
///
/// A host with no Last.fm credentials still gets a working manager; it just
/// has no Last.fm built-ins to reference.
fn build_registry(config: &Config) -> Result<Registry> {
    let mut registry = Registry::new();

    match &config.github {
        Some(github) => {
            github.ensure_destination_folder()?;
            if github.gist.is_none() {
                info!("GIST_ID is unset, the gist builtin will fail if scheduled");
            }
            registry = registry.with_github(Arc::clone(github))?;
        }
        None => info!("GITHUB_TOKEN is unset, GitHub builtins are not registered"),
    }

    let Some(settings) = &config.lastfm else {
        info!("LAST_FM_USERNAME is unset, Last.fm builtins are not registered");
        return Ok(registry);
    };

    settings.ensure_destination_folder()?;

    let client = LastFmClient::new()
        .map_err(|e| anyhow::anyhow!("{e:?}"))
        .context("Failed to create the Last.fm client")?;

    let lastfm = Arc::new(LastFm::new(
        Arc::new(client),
        Arc::clone(settings),
        config.github.clone(),
    ));

    Ok(registry.with_lastfm(lastfm))
}

/// Periodically trims the run history so it stays bounded.
fn spawn_pruner(history: History, keep_days: u32) {
    tokio::spawn(async move {
        loop {
            match history.prune(keep_days).await {
                Ok(0) => {}
                Ok(deleted) => info!(deleted, keep_days, "Pruned old runs from the history"),
                Err(error) => error!("Failed to prune the run history: {error:#}"),
            }
            tokio::time::sleep(PRUNE_INTERVAL).await;
        }
    });
}

/// Sets up structured logging, honouring `RUST_LOG`.
///
/// One-shot commands log warnings and errors only unless `RUST_LOG` says
/// otherwise, so their report is not buried under startup lines.
fn init_tracing(verbose: bool) {
    let default = if verbose { "info" } else { "warn" };

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
