//! Built-in Last.fm jobs.
//!
//! These are the jobs this manager grew out of. They stay compiled in rather
//! than becoming shell commands so they can share one `LastFmClient` and keep
//! typed access to the API, but they are now ordinary registry entries: the
//! jobs file decides whether they run at all, and on what schedule.
//!
//! Every one of them is registered only when the Last.fm environment is
//! configured, so an install that never touches Last.fm is not obliged to
//! provide credentials.

use std::sync::Arc;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use chrono_tz::Tz;
use lastfm_client::{api::Period, prelude::*, LastFmClient};
use tracing::warn;

use crate::builtins::write_json;
use crate::config::{GitHubSettings, LastFmSettings};
use crate::job::{Job, JobContext, JobReport, JobResult};
use crate::update_gist::{format_top_tracks_markdown, update_gist};
use crate::update_readme::{
    format_recent_tracks_details, load_track_stats, update_readme_section, RepoFile,
};

/// Shared state handed to every Last.fm built-in.
pub struct LastFm {
    /// The API client, shared across all Last.fm jobs.
    pub client: Arc<LastFmClient>,
    /// Credentials and paths read from the environment.
    pub settings: Arc<LastFmSettings>,
    /// GitHub settings, needed only by the gist job.
    pub github: Option<Arc<GitHubSettings>>,
}

impl LastFm {
    /// Bundles a client and settings for the registry.
    pub fn new(
        client: Arc<LastFmClient>,
        settings: Arc<LastFmSettings>,
        github: Option<Arc<GitHubSettings>>,
    ) -> Self {
        Self {
            client,
            settings,
            github,
        }
    }
}

/// Writes the most recent plays to a JSON file.
///
/// Arguments: `limit` (default 100), `filename` (default
/// `recent_play_counts.json`).
pub struct RecentPlays(pub Arc<LastFm>);

#[async_trait]
impl Job for RecentPlays {
    async fn run(&self, ctx: &JobContext<'_>) -> JobResult {
        let limit = ctx.arg_u32("limit").unwrap_or(100);
        let filename = ctx.arg_str("filename").unwrap_or("recent_play_counts.json");

        let tracks = self
            .0
            .client
            .recent_tracks(&self.0.settings.username)
            .limit(limit)
            .fetch()
            .await
            .map_err(|e| anyhow!("{e:?}"))
            .context("Failed to fetch recent plays")?;

        let count = tracks.len();
        let path = write_json(&self.0.settings.destination_folder, filename, &tracks)?;

        Ok(JobReport::summary(format!(
            "Wrote {count} recent plays to {path}"
        )))
    }
}

/// Writes the currently playing track to a JSON file.
///
/// Arguments: `filename` (default `currently_listening.json`).
pub struct CurrentTrack(pub Arc<LastFm>);

#[async_trait]
impl Job for CurrentTrack {
    async fn run(&self, ctx: &JobContext<'_>) -> JobResult {
        let filename = ctx.arg_str("filename").unwrap_or("currently_listening.json");

        let tracks = self
            .0
            .client
            .recent_tracks(&self.0.settings.username)
            .limit(1)
            .fetch()
            .await
            .map_err(|e| anyhow!("{e:?}"))
            .context("Failed to fetch the current track")?;

        let path = write_json(&self.0.settings.destination_folder, filename, &tracks)?;

        let summary = match tracks.first() {
            Some(track) => format!("Now playing '{}' -> {path}", track.name),
            None => format!("Nothing playing, wrote {path}"),
        };

        Ok(JobReport::summary(summary))
    }
}

/// Appends new scrobbles to the SQLite listening history.
///
/// Arguments: `db_file` (defaults to the `LAST_FM_DB_FILE` environment value).
pub struct ScrobblesDb(pub Arc<LastFm>);

#[async_trait]
impl Job for ScrobblesDb {
    async fn run(&self, ctx: &JobContext<'_>) -> JobResult {
        let db_file = ctx
            .arg_str("db_file")
            .unwrap_or(&self.0.settings.db_file)
            .to_string();

        self.0
            .client
            .recent_tracks(&self.0.settings.username)
            .fetch_extended_and_update_sqlite(&db_file)
            .await
            .map_err(|e| anyhow!("{e:?}"))
            .context("Failed to update the scrobbles database")?;

        Ok(JobReport::summary(format!("Updated {db_file}")))
    }
}

/// Renders top tracks as Markdown and pushes them to a GitHub gist.
///
/// Arguments: `limit` (default 5), `period` (default `week`), `gist_id` and
/// `gist_filename` (default to the environment values).
pub struct TopTracksGist(pub Arc<LastFm>);

#[async_trait]
impl Job for TopTracksGist {
    async fn run(&self, ctx: &JobContext<'_>) -> JobResult {
        let settings = &self.0.settings;

        let github = self
            .0
            .github
            .as_ref()
            .context("GITHUB_TOKEN must be set to update a gist")?;

        let gist = github
            .gist
            .as_ref()
            .context("GIST_ID must be set to update a gist")?;

        let limit = ctx.arg_u32("limit").unwrap_or(5);
        let period = parse_period(ctx.arg_str("period").unwrap_or("week"))?;
        let gist_id = ctx.arg_str("gist_id").unwrap_or(&gist.id);
        let gist_filename = ctx.arg_str("gist_filename").unwrap_or(&gist.filename);

        let mut top_tracks = self
            .0
            .client
            .top_tracks(&settings.username)
            .limit(limit)
            .period(period)
            .fetch()
            .await
            .map_err(|e| anyhow!("{e:?}"))
            .context("Failed to fetch top tracks")?;

        top_tracks.sort_by_key(|t| std::cmp::Reverse(t.playcount));
        let content = format_top_tracks_markdown(&top_tracks);

        update_gist(&content, &github.token, gist_id, gist_filename)
            .await
            .map_err(|e| anyhow!("{e}"))
            .context("Failed to update the gist")?;

        Ok(JobReport::summary(format!(
            "Pushed {} tracks to gist {gist_id}/{gist_filename}",
            top_tracks.len()
        ))
        .with_output(content))
    }
}

/// Renders the last played tracks into a section of a repository file, such
/// as a GitHub profile README.
///
/// Arguments: `repo` (`owner/name`, required), `limit` (default 10), `path`
/// (default `README.md`), `branch` (default: the repository default branch),
/// `timezone` (default `Europe/Paris`).
///
/// Play counts and loved markers come from the `lastfm_scrobbles_db`
/// database. When it cannot be read, the list is published without them.
///
/// The file must contain the `<!-- LASTFM:START -->` and `<!-- LASTFM:END -->`
/// markers. A commit is only made when the section actually changed.
pub struct RecentTracksReadme(pub Arc<LastFm>);

#[async_trait]
impl Job for RecentTracksReadme {
    async fn run(&self, ctx: &JobContext<'_>) -> JobResult {
        let github = self
            .0
            .github
            .as_ref()
            .context("GITHUB_TOKEN must be set to update a README")?;

        let repo = ctx
            .arg_str("repo")
            .context("The 'repo' argument (owner/name) is required")?;
        let limit = ctx.arg_u32("limit").unwrap_or(10);
        let tz_name = ctx.arg_str("timezone").unwrap_or("Europe/Paris");
        let tz: Tz = tz_name
            .parse()
            .map_err(|_| anyhow!("Unknown timezone '{tz_name}'"))?;
        let target = RepoFile {
            repo,
            path: ctx.arg_str("path").unwrap_or("README.md"),
            branch: ctx.arg_str("branch"),
        };

        // One extra, because a currently playing track takes a slot and is dropped.
        let tracks = self
            .0
            .client
            .recent_tracks(&self.0.settings.username)
            .limit(limit + 1)
            .fetch()
            .await
            .map_err(|e| anyhow!("{e:?}"))
            .context("Failed to fetch recent plays")?;

        let played: Vec<_> = tracks
            .into_iter()
            .filter(|t| t.attr.is_none())
            .take(limit as usize)
            .collect();

        // Stats are a nice-to-have: a missing or locked database should not
        // stop the list itself from being published.
        let db_file = self.0.settings.db_file.clone();
        let lookup = played.clone();
        let stats = match tokio::task::spawn_blocking(move || load_track_stats(&db_file, &lookup))
            .await
            .context("The stats lookup panicked")?
        {
            Ok(stats) => Some(stats),
            Err(err) => {
                warn!("Publishing without play counts: {err:#}");
                None
            }
        };

        let section = format_recent_tracks_details(&played, stats.as_deref(), tz);
        let committed = update_readme_section(
            &section,
            &github.token,
            &target,
            "chore: update last played songs",
        )
        .await?;

        let summary = if committed {
            format!("Committed {} tracks to {repo}/{}", played.len(), target.path)
        } else {
            format!("{repo}/{} already up to date", target.path)
        };

        Ok(JobReport::summary(summary).with_output(section))
    }
}

/// Maps a jobs-file `period` argument onto the Last.fm period.
fn parse_period(raw: &str) -> anyhow::Result<Period> {
    match raw.to_ascii_lowercase().replace(['-', '_'], "").as_str() {
        "overall" | "all" => Ok(Period::Overall),
        "week" | "7day" => Ok(Period::Week),
        "month" | "30day" => Ok(Period::Month),
        "3month" | "threemonth" => Ok(Period::ThreeMonth),
        "6month" | "sixmonth" => Ok(Period::SixMonth),
        "12month" | "twelvemonth" | "year" => Ok(Period::TwelveMonth),
        other => Err(anyhow!(
            "Unknown period '{other}'. Use one of: overall, week, month, 3month, 6month, 12month"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Period` has no `PartialEq`, so compare the debug spelling instead.
    fn period_of(raw: &str) -> String {
        format!("{:?}", parse_period(raw).expect("should parse"))
    }

    #[test]
    fn accepts_every_documented_period_spelling() {
        assert_eq!(period_of("week"), "Week");
        assert_eq!(period_of("7day"), "Week");
        assert_eq!(period_of("3MONTH"), "ThreeMonth");
        assert_eq!(period_of("three_month"), "ThreeMonth");
        assert_eq!(period_of("twelve-month"), "TwelveMonth");
        assert_eq!(period_of("year"), "TwelveMonth");
        assert_eq!(period_of("overall"), "Overall");
        assert_eq!(period_of("month"), "Month");
        assert_eq!(period_of("6month"), "SixMonth");
    }

    #[test]
    fn rejects_an_unknown_period_with_a_helpful_message() {
        let err = parse_period("fortnight").expect_err("should be rejected");
        assert!(err.to_string().contains("Unknown period"));
        assert!(err.to_string().contains("overall, week, month"));
    }
}
