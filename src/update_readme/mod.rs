//! Utilities to render recent Last.fm plays into a section of a repository file.
//!
//! The target file (typically a GitHub profile `README.md`) carries two HTML
//! comment markers. Everything between them is owned by this module and gets
//! replaced on each run; everything outside them is left untouched:
//!
//! ```markdown
//! <!-- LASTFM:START -->
//! <!-- LASTFM:END -->
//! ```
//!
//! The file is read and written through the GitHub Contents API, so no local
//! clone is needed. A commit is only created when the rendered section differs
//! from what is already there, which keeps the repository history quiet when
//! nothing was scrobbled since the last run.

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chrono::DateTime;
use chrono_tz::Tz;
use lastfm_client::types::RecentTrack;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use rusqlite::{params, Connection, OpenFlags};
use serde::Deserialize;

/// Opening marker of the managed section.
pub const START_MARKER: &str = "<!-- LASTFM:START -->";
/// Closing marker of the managed section.
pub const END_MARKER: &str = "<!-- LASTFM:END -->";

/// The subset of the Contents API response this module needs.
#[derive(Deserialize)]
struct ContentsResponse {
    /// Base64 file content, wrapped with newlines by GitHub.
    content: String,
    /// Blob SHA, required by the API to overwrite the file.
    sha: String,
}

/// Where the managed file lives.
pub struct RepoFile<'a> {
    /// `owner/name` of the repository.
    pub repo: &'a str,
    /// Path of the file inside the repository.
    pub path: &'a str,
    /// Branch to read from and commit to, or `None` for the default branch.
    pub branch: Option<&'a str>,
}

/// Replaces the managed section of a repository file with `section`.
///
/// # Returns
/// * `Ok(true)` when a commit was created, `Ok(false)` when the file already
///   held this exact content.
///
/// # Errors
/// Fails when the file cannot be fetched, does not contain both markers, or
/// the update is rejected by GitHub.
pub async fn update_readme_section(
    section: &str,
    github_token: &str,
    target: &RepoFile<'_>,
    commit_message: &str,
) -> Result<bool> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static("vps-cron"));
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {github_token}"))
            .context("GITHUB_TOKEN contains characters that cannot go in a header")?,
    );

    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .context("Failed to build the GitHub HTTP client")?;

    let url = format!(
        "https://api.github.com/repos/{}/contents/{}",
        target.repo, target.path
    );

    let mut request = client.get(&url);
    if let Some(branch) = target.branch {
        request = request.query(&[("ref", branch)]);
    }

    let resp = request.send().await.context("Failed to fetch the file")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!(
            "Failed to fetch {}/{}: {status} - {text}",
            target.repo,
            target.path
        );
    }

    let file: ContentsResponse = resp
        .json()
        .await
        .context("Unexpected Contents API response")?;

    let encoded: String = file.content.split_whitespace().collect();
    let bytes = STANDARD
        .decode(encoded)
        .context("File content is not valid base64")?;
    let current = String::from_utf8(bytes).context("File is not valid UTF-8")?;

    let updated = splice_section(&current, section)?;
    if updated == current {
        return Ok(false);
    }

    let mut body = serde_json::json!({
        "message": commit_message,
        "content": STANDARD.encode(updated.as_bytes()),
        "sha": file.sha,
    });
    if let Some(branch) = target.branch {
        body["branch"] = serde_json::Value::from(branch);
    }

    let resp = client
        .put(&url)
        .json(&body)
        .send()
        .await
        .context("Failed to update the file")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!(
            "Failed to update {}/{}: {status} - {text}",
            target.repo,
            target.path
        );
    }

    Ok(true)
}

/// Replaces whatever sits between the markers in `document` with `section`.
///
/// The markers themselves are kept, each on its own line around the section.
fn splice_section(document: &str, section: &str) -> Result<String> {
    let start = document
        .find(START_MARKER)
        .with_context(|| format!("Marker '{START_MARKER}' not found in the file"))?;
    let content_start = start + START_MARKER.len();

    let end = document[content_start..]
        .find(END_MARKER)
        .map(|offset| content_start + offset)
        .with_context(|| format!("Marker '{END_MARKER}' not found after '{START_MARKER}'"))?;

    Ok(format!(
        "{}\n{}\n{}",
        &document[..content_start],
        section.trim_end(),
        &document[end..]
    ))
}

/// Escapes characters that would break a Markdown link label or open HTML.
fn escape_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\\' | '[' | ']' | '*' | '_' | '`' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Listening stats for one track, read from the scrobble database.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TrackStats {
    /// How many times the track was scrobbled in total.
    pub plays: u32,
    /// Whether the track is loved, as of its latest scrobble.
    pub loved: bool,
}

/// Looks up play counts and loved state for `tracks` in the scrobble database
/// written by the `lastfm_scrobbles_db` job.
///
/// This is synchronous, so callers on the runtime should wrap it in
/// `spawn_blocking`. The database is opened read-only: the scrobbles job owns
/// it and this must never be the one to create it.
///
/// # Returns
/// One entry per track, in the same order as `tracks`.
pub fn load_track_stats(db_file: &str, tracks: &[RecentTrack]) -> Result<Vec<TrackStats>> {
    let conn = Connection::open_with_flags(db_file, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("Failed to open the scrobble database '{db_file}'"))?;

    // `loved` is a snapshot taken at scrobble time, so the latest row is the
    // one that reflects the current state.
    let mut stmt = conn
        .prepare(
            "SELECT COUNT(*),
                    COALESCE((SELECT loved FROM recent_tracks_extended
                              WHERE name = ?1 AND artist = ?2
                              ORDER BY date_uts DESC LIMIT 1), 0)
             FROM recent_tracks_extended
             WHERE name = ?1 AND artist = ?2",
        )
        .context("Failed to query the scrobble database")?;

    tracks
        .iter()
        .map(|track| {
            stmt.query_row(params![track.name, track.artist.text], |row| {
                Ok(TrackStats {
                    plays: row.get(0)?,
                    loved: row.get::<_, i64>(1)? != 0,
                })
            })
            .with_context(|| format!("Failed to read stats for '{}'", track.name))
        })
        .collect()
}

/// Renders played tracks as a collapsible `<details>` block.
///
/// Each line reads `[Title - Artist](url) · 29/09 07:01 · x42 ♥`, with the
/// time shown in `tz`. `stats`, when given, must line up with `tracks`;
/// without it the play count and loved marker are left out.
///
/// `tracks` should already exclude the currently playing track, since it has
/// no play date and would churn the file on every run.
pub fn format_recent_tracks_details(
    tracks: &[RecentTrack],
    stats: Option<&[TrackStats]>,
    tz: Tz,
) -> String {
    let mut out = String::new();
    out.push_str("<details>\n");
    out.push_str(&format!(
        "  <summary>My last {} songs</summary>\n\n",
        tracks.len()
    ));

    if tracks.is_empty() {
        out.push_str("No tracks found.\n");
    }

    for (i, track) in tracks.iter().enumerate() {
        let mut parts = vec![format!(
            "[{} - {}]({})",
            escape_markdown(&track.name),
            escape_markdown(&track.artist.text),
            track.url
        )];

        let played_at = track
            .date
            .as_ref()
            .and_then(|date| DateTime::from_timestamp(i64::from(date.uts), 0));
        if let Some(played_at) = played_at {
            parts.push(
                played_at
                    .with_timezone(&tz)
                    .format("%d/%m %H:%M")
                    .to_string(),
            );
        }

        if let Some(stat) = stats.and_then(|s| s.get(i)) {
            // A zero count only means the scrobbles job has not caught up yet.
            let marks: Vec<String> = [
                (stat.plays > 0).then(|| format!("x{}", stat.plays)),
                stat.loved.then(|| "\u{2665}".to_string()),
            ]
            .into_iter()
            .flatten()
            .collect();

            if !marks.is_empty() {
                parts.push(marks.join(" "));
            }
        }

        out.push_str(&format!("- {}\n", parts.join(" \u{b7} ")));
    }

    out.push_str("\n</details>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_only_the_managed_section() {
        let doc = format!("before\n{START_MARKER}\nold\nstuff\n{END_MARKER}\nafter\n");
        let out = splice_section(&doc, "new").expect("should splice");
        assert_eq!(
            out,
            format!("before\n{START_MARKER}\nnew\n{END_MARKER}\nafter\n")
        );
    }

    #[test]
    fn splicing_is_idempotent() {
        let doc = format!("a\n{START_MARKER}\n{END_MARKER}\nb");
        let once = splice_section(&doc, "x").expect("should splice");
        let twice = splice_section(&once, "x").expect("should splice");
        assert_eq!(once, twice);
    }

    #[test]
    fn missing_markers_are_reported() {
        let err = splice_section("no markers", "x").expect_err("should fail");
        assert!(err.to_string().contains(START_MARKER));

        let err = splice_section(&format!("{START_MARKER} only"), "x").expect_err("should fail");
        assert!(err.to_string().contains(END_MARKER));
    }

    /// `RecentTrack` is `#[non_exhaustive]`, so build one the way the API would.
    fn track(name: &str, artist: &str, uts: u32) -> RecentTrack {
        serde_json::from_value(serde_json::json!({
            "artist": { "mbid": "", "#text": artist },
            "streamable": "0",
            "image": [],
            "album": { "mbid": "", "#text": "" },
            "date": { "uts": uts.to_string(), "#text": "" },
            "name": name,
            "mbid": "",
            "url": "https://www.last.fm/music/x",
        }))
        .expect("valid recent track")
    }

    #[test]
    fn renders_short_local_time_and_stats() {
        // 2026-09-29 05:01 UTC is 07:01 in Paris (CEST).
        let tracks = [track("Song", "Band", 1_790_658_060)];
        let stats = [TrackStats {
            plays: 42,
            loved: true,
        }];

        let out = format_recent_tracks_details(&tracks, Some(&stats), chrono_tz::Europe::Paris);

        assert!(out.contains("<summary>My last 1 songs</summary>"));
        assert!(out.contains(
            "- [Song - Band](https://www.last.fm/music/x) \u{b7} 29/09 07:01 \u{b7} x42 \u{2665}\n"
        ));
    }

    #[test]
    fn leaves_stats_out_when_unavailable() {
        let tracks = [track("Song", "Band", 1_790_658_060)];
        let out = format_recent_tracks_details(&tracks, None, chrono_tz::UTC);
        assert!(out.contains("- [Song - Band](https://www.last.fm/music/x) \u{b7} 29/09 05:01\n"));
    }

    #[test]
    fn hides_a_zero_play_count() {
        let tracks = [
            track("Song", "Band", 1_790_658_060),
            track("Other", "Band", 1_790_658_060),
        ];
        let stats = [
            TrackStats {
                plays: 0,
                loved: false,
            },
            TrackStats {
                plays: 0,
                loved: true,
            },
        ];

        let out = format_recent_tracks_details(&tracks, Some(&stats), chrono_tz::UTC);

        assert!(out.contains("- [Song - Band](https://www.last.fm/music/x) \u{b7} 29/09 05:01\n"));
        assert!(out.contains(
            "- [Other - Band](https://www.last.fm/music/x) \u{b7} 29/09 05:01 \u{b7} \u{2665}\n"
        ));
        assert!(!out.contains("x0"));
    }

    #[test]
    fn counts_plays_and_takes_the_latest_loved_state() {
        let dir = std::env::temp_dir().join(format!("vps-cron-stats-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("scrobbles.db");
        let _ = std::fs::remove_file(&db);

        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE recent_tracks_extended (name TEXT, artist TEXT, date_uts INTEGER, loved INTEGER);
             INSERT INTO recent_tracks_extended VALUES
               ('Song', 'Band', 1, 1),
               ('Song', 'Band', 2, 0),
               ('Song', 'Band', 3, 1),
               ('Song', 'Other', 4, 0);",
        )
        .unwrap();
        drop(conn);

        let stats = load_track_stats(
            db.to_str().unwrap(),
            &[track("Song", "Band", 3), track("Unknown", "Band", 5)],
        )
        .expect("should query");

        assert_eq!(
            stats,
            vec![
                TrackStats {
                    plays: 3,
                    loved: true
                },
                TrackStats {
                    plays: 0,
                    loved: false
                },
            ]
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn escapes_link_breaking_characters() {
        assert_eq!(
            escape_markdown("[Live] <Remix>"),
            "\\[Live\\] &lt;Remix&gt;"
        );
    }
}
