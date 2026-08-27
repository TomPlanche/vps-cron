# vps-cron

A small cron manager for a single VPS. Jobs are declared in a TOML file rather than in code: each one is either a shell command or a built-in compiled into the binary, and the scheduler treats them all the same way.

It grew out of a Last.fm data fetcher, so the Last.fm jobs are still here as built-ins. They are now optional: without Last.fm credentials in the environment the binary is a plain cron manager.

## Why not crontab

- Overlap protection: a run that overruns its window never starts a second copy of itself. The occurrence is skipped and recorded.
- Timeouts: a hung job is killed instead of blocking the schedule forever.
- Run history: every run lands in SQLite with its duration, outcome and captured output, so "did last night's backup run?" is a query.
- Status over HTTP: one request tells you what is scheduled, what is running, and what is failing.
- Second-level schedules: six-field cron expressions, so "every 5 seconds" is expressible.

## Configuring jobs

Jobs live in `jobs.toml`. Schedules are six-field cron expressions: `second minute hour day month weekday`. Edit the file and restart the service; no rebuild is needed.

```toml
[[jobs]]
name = "nightly-backup"
schedule = "0 0 3 * * *"        # 03:00 daily
timeout = "1h"
kind = { shell = "restic backup /srv" }

[[jobs]]
name = "lastfm-recent-plays"
schedule = "0 0/1 * * * *"      # every minute
kind = { builtin = "lastfm_recent_plays" }
args = { limit = 100 }
```

Every job accepts:

| Key | Default | Meaning |
| --- | --- | --- |
| `name` | required | Unique name, used in logs, history and the HTTP API |
| `schedule` | required | Six-field cron expression |
| `kind` | required | `{ shell = "..." }` or `{ builtin = "..." }` |
| `enabled` | `true` | Set to `false` to keep a declaration without running it |
| `timeout` | none | Kill the run after this long, e.g. `"30s"`, `"10m"`, `"1h"` |
| `run_on_start` | `false` | Run once at startup instead of waiting for the first occurrence |
| `args` | `{}` | Parameters passed to the job |

The whole file is validated at startup, including disabled jobs, so a typo cannot lie dormant until the day you switch it on. An unknown builtin or an unparseable cron expression stops the process immediately rather than at 3am.

### Shell jobs

Commands run through `sh -c`, so pipes, redirections and `&&` work as they do in a crontab. Both stdout and stderr are captured into the run history, on success and on failure.

```toml
kind = { shell = "certbot renew --quiet && systemctl reload nginx" }
```

The longer form takes a working directory and extra environment variables:

```toml
kind = { shell = { command = "make deploy", workdir = "/srv/app", env = { RUST_LOG = "info" } } }
```

Two variables are always set for the command: `VPS_CRON_JOB` holds the job name and `VPS_CRON_STARTED_AT` holds the run's start time in RFC 3339.

### The `filename` argument

Built-ins that write a file take a `filename` argument, joined onto the job's destination folder. Missing parent directories are created, so `exports/today.json` works out of the box.

Two consequences of that join are worth knowing. A relative path escapes the folder: `../public/now.json` resolves against the folder and lands outside it, which is a fine way to write into a directory served by nginx. An absolute path replaces the folder entirely, so `/var/www/html/now.json` ignores the configured destination altogether. Both are deliberate.

A leading `~` is rejected with an explicit error. Shells expand it, this does not, and creating a directory literally named `~` is never what anyone meant.

### Built-in jobs

Built-ins are compiled in, so they can share one API client and keep typed access to the API. The Last.fm ones are registered only when `LAST_FM_USERNAME` is set.

| Builtin | Arguments | What it does |
| --- | --- | --- |
| `lastfm_recent_plays` | `limit` (100), `filename` (`recent_play_counts.json`) | Writes recent plays as JSON |
| `lastfm_current_track` | `filename` (`currently_listening.json`) | Writes the currently playing track as JSON |
| `lastfm_scrobbles_db` | `db_file` (`LAST_FM_DB_FILE`) | Appends new scrobbles to the SQLite listening history |
| `lastfm_top_tracks_gist` | `limit` (5), `period` (`week`), `gist_id`, `gist_filename` | Renders top tracks as Markdown and pushes them to a GitHub gist |
| `github_activity` | `filename` (`github_activity.json`), `issues_limit` (20), `prs_limit` (20), `starred_limit` (30), `repos_limit` (100), `review_requests_limit` (20) | Snapshots your GitHub activity as typed JSON |

`period` accepts `overall`, `week`, `month`, `3month`, `6month` and `12month`.

The GitHub built-ins need `GITHUB_TOKEN`. `github_activity` reads public data only, so a token with no scopes at all is enough; `lastfm_top_tracks_gist` additionally needs the `gist` scope.

Referencing a builtin that is not registered fails at startup with the list of names that are, which is usually the clue that a credential is missing.

### GitHub activity

`github_activity` writes one JSON snapshot per run, overwriting the previous one. Everything comes from a single GraphQL request, so it costs one point of your 5000/hour budget.

```json
{
  "fetched_at": "2026-08-27T15:15:00Z",
  "login": "your-login",
  "issues": [
    { "number": 7, "title": "...", "url": "...", "state": "OPEN",
      "repository": "owner/repo", "comments": 3,
      "created_at": "...", "updated_at": "..." }
  ],
  "pull_requests": [
    { "number": 12, "title": "...", "url": "...", "state": "MERGED",
      "repository": "owner/repo", "additions": 40, "deletions": 5,
      "created_at": "...", "updated_at": "...", "merged_at": "..." }
  ],
  "review_requests": [
    { "number": 3, "title": "...", "url": "...",
      "repository": "owner/repo", "author": "someone", "created_at": "..." }
  ],
  "starred": [
    { "repository": "rust-lang/rust", "url": "...", "description": null,
      "stars": 99000, "language": "Rust", "starred_at": "..." }
  ],
  "repositories": {
    "total": 12, "counted": 12, "stars_received": 137,
    "items": [
      { "repository": "owner/repo", "url": "...", "description": "...",
        "stars": 100, "forks": 4, "language": "Rust" }
    ]
  },
  "rate_limit": { "remaining": 4987, "resets_at": "..." }
}
```

Things worth knowing about the shape:

- `state` is GitHub's own spelling: `OPEN` or `CLOSED` for issues, and `OPEN`, `CLOSED` or `MERGED` for pull requests.
- `repositories.stars_received` sums only the `counted` repositories. If you own more than `repos_limit`, it is a lower bound, though the list is ordered by stars so the ones that matter are included first.
- `author` is `null` when the account behind a review request has been deleted. `description` and `language` are `null` whenever GitHub has none.
- Private data is filtered out in code, not merely left out by the token's scopes. Granting the token `repo` access later for some other reason will not start leaking private repository names into this file.

## Environment

Copy `.env.example` to `.env` and fill in what you need. Only the manager's own settings apply to every install; the rest are for the Last.fm built-ins.

| Variable | Default | Meaning |
| --- | --- | --- |
| `JOBS_FILE` | `./jobs.toml` | Path to the jobs file |
| `DATA_DIR` | `./data` | Generated files and the run history database |
| `HTTP_ADDR` | `127.0.0.1:8787` | Status server address; set to an empty string to disable it |
| `HISTORY_RETENTION_DAYS` | `30` | How long to keep run history |
| `RUST_LOG` | `info` | Log level |
| `LAST_FM_USERNAME` | unset | Enables the Last.fm built-ins |
| `LAST_FM_API_KEY` | unset | Last.fm API key |
| `DESTINATION_FOLDER` | `DATA_DIR` | Where the Last.fm JSON exports are written |
| `LAST_FM_DB_FILE` | `DATA_DIR/scrobbles.db` | Scrobble history database |
| `GITHUB_TOKEN` | unset | Enables the GitHub built-ins |
| `GITHUB_DESTINATION_FOLDER` | `DATA_DIR` | Where the GitHub exports are written |
| `GIST_ID` | unset | Target gist, needed only by the gist job |
| `GIST_FILENAME` | `top-tracks.md` | File within the gist to overwrite |

## Status server

Read-only, bound to localhost by default. Reach it over an SSH tunnel rather than exposing it.

- `GET /health`: liveness, plus the names of any jobs whose last run failed
- `GET /jobs`: every configured job with its next run and last result
- `GET /jobs/{name}`: one job
- `GET /runs?job={name}&limit={n}`: recent runs from the history, newest first

```
$ curl -s localhost:8787/health
{"enabled":4,"failing":[],"jobs":5,"running":0,"status":"ok"}
```

## Run history

Runs are appended to `DATA_DIR/vps-cron-history.db`. Each row holds the job name, start and finish times, duration, outcome (`success`, `failure`, `timeout` or `skipped`), a one-line summary and the captured output tail. Rows older than `HISTORY_RETENTION_DAYS` are pruned every six hours, which matters because a job on a five-second schedule writes over half a million rows a month.

```
sqlite3 data/vps-cron-history.db \
  "SELECT started_at, job, outcome, duration_ms FROM runs ORDER BY id DESC LIMIT 10;"
```

## Running a job immediately

```
vps-cron run nightly-backup
```

It runs the job once, now, with its configured timeout, prints the result and exits. The run is recorded in the history exactly like a scheduled one, so it shows up in `GET /runs` alongside the rest.

The exit code is `0` on success and `1` on failure or timeout, so it composes with `&&` and with anything that checks exit codes.

```
$ vps-cron run hello
success: Command exited 0
took 4 ms
---
bonjour depuis hello
```

Disabled jobs are runnable this way on purpose. Keeping a job in the file with `enabled = false` and triggering it by hand is a reasonable way to work, and the command says so rather than silently ignoring the flag.

`vps-cron list` prints every configured job with its state, schedule and what it runs.

### It is safe to use while the service is running

The scheduler's in-process guard cannot see a `vps-cron run` typed in a terminal, so a manual run of a job the daemon was already running would otherwise have two processes writing the same files. Both take the same advisory lock file under `DATA_DIR/locks`, so whichever arrives second backs off:

```
$ vps-cron run slow
Error: Job 'slow' is already running (the scheduler holds its lock). Try again once it finishes.
```

In the other direction the daemon logs a skip and records it, rather than piling a scheduled run on top of your manual one. Locks are advisory and released when the process exits, including on a crash, so a killed run never leaves a job wedged.

## Running it

```
cp .env.example .env    # then fill it in
cargo build --release
./target/release/vps-cron
```

With no arguments it runs the scheduler until stopped. `--help` lists the commands.

As a systemd unit:

```ini
[Unit]
Description=vps-cron
After=network-online.target

[Service]
Type=simple
WorkingDirectory=/srv/vps-cron
ExecStart=/srv/vps-cron/vps-cron
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

Logs go to stdout and stderr, so `journalctl -u vps-cron -f` shows them. `RUST_LOG=debug` turns up the detail. One-shot commands log warnings and errors only unless `RUST_LOG` says otherwise, so their report is not buried under startup lines.

## Adding a builtin

1. Implement `Job` in `src/builtins/`. The trait has one method, `run`, taking a `JobContext` (the job name, its `args` and the run's start time) and returning a summary or a failure.
2. Register it in `src/registry.rs` under the name the jobs file will use.
3. Reference it from `jobs.toml` with `kind = { builtin = "your_name" }`.

Anything that does not need typed API access is better off as a shell job, which needs no code at all.

## Layout

| Path | Contents |
| --- | --- |
| `src/cli.rs` | Command line parsing |
| `src/job.rs` | The `Job` trait and the run record types |
| `src/lock.rs` | Advisory job locks shared by the daemon and the CLI |
| `src/jobs_file.rs` | Parsing and validation of `jobs.toml` |
| `src/scheduler.rs` | The cron loop, overlap guard and timeouts |
| `src/registry.rs` | Built-in lookup by name |
| `src/history.rs` | SQLite run history |
| `src/http.rs` | Status server |
| `src/builtins/` | Job implementations, plus the shared JSON writer |
| `src/config.rs` | Environment configuration |
