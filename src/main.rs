//! vps-cron: a small cron manager for a single VPS.
//!
//! Jobs are declared in a TOML file, not in code. Each one is either a shell
//! command or a built-in compiled into the binary, and the scheduler gives all
//! of them the same treatment: overlap protection, optional timeouts,
//! structured logs and a row in the run history.

use std::sync::Arc;

use anyhow::{Context, Result};
use lastfm_client::LastFmClient;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

mod builtins;
mod config;
mod history;
mod http;
mod job;
mod jobs_file;
mod registry;
mod scheduler;
mod update_gist;

use builtins::lastfm::LastFm;
use config::Config;
use history::History;
use jobs_file::JobsFile;
use registry::Registry;
use scheduler::Scheduler;

/// How often to prune the run history.
const PRUNE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    init_tracing();

    let config = Config::from_env().context("Invalid configuration")?;
    config.ensure_data_dir()?;

    let jobs_file = JobsFile::load(&config.jobs_file)?;
    let registry = build_registry(&config)?;

    let history = History::open(&history::default_path(&config.data_dir))?;
    let scheduler = Scheduler::build(jobs_file, &registry, history.clone())?;

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

/// Builds the registry, adding the Last.fm built-ins when configured.
///
/// A host with no Last.fm credentials still gets a working manager; it just
/// has no Last.fm built-ins to reference.
fn build_registry(config: &Config) -> Result<Registry> {
    let Some(settings) = &config.lastfm else {
        info!("LAST_FM_USERNAME is unset, Last.fm builtins are not registered");
        return Ok(Registry::new());
    };

    settings.ensure_destination_folder()?;

    if settings.gist.is_none() {
        info!("GITHUB_TOKEN or GIST_ID is unset, the gist builtin will fail if scheduled");
    }

    let client = LastFmClient::new()
        .map_err(|e| anyhow::anyhow!("{e:?}"))
        .context("Failed to create the Last.fm client")?;

    let lastfm = Arc::new(LastFm::new(Arc::new(client), Arc::clone(settings)));

    Ok(Registry::new().with_lastfm(lastfm))
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
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,vps_cron=info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
