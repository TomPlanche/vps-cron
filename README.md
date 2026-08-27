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

### Built-in jobs

Built-ins are compiled in, so they can share one API client and keep typed access to the API. The Last.fm ones are registered only when `LAST_FM_USERNAME` is set.

| Builtin | Arguments | What it does |
| --- | --- | --- |
| `lastfm_recent_plays` | `limit` (100), `filename` (`recent_play_counts.json`) | Writes recent plays as JSON |
| `lastfm_current_track` | `filename` (`currently_listening.json`) | Writes the currently playing track as JSON |
| `lastfm_scrobbles_db` | `db_file` (`LAST_FM_DB_FILE`) | Appends new scrobbles to the SQLite listening history |
| `lastfm_top_tracks_gist` | `limit` (5), `period` (`week`), `gist_id`, `gist_filename` | Renders top tracks as Markdown and pushes them to a GitHub gist |

`period` accepts `overall`, `week`, `month`, `3month`, `6month` and `12month`.

Referencing a builtin that is not registered fails at startup with the list of names that are, which is usually the clue that a credential is missing.

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
| `GITHUB_TOKEN` | unset | Token with the `gist` scope |
| `GIST_ID` | unset | Target gist |
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

## Running it

```
cp .env.example .env    # then fill it in
cargo build --release
./target/release/vps-cron
```

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

Logs go to stdout and stderr, so `journalctl -u vps-cron -f` shows them. `RUST_LOG=debug` turns up the detail.

## Adding a builtin

1. Implement `Job` in `src/builtins/`. The trait has one method, `run`, taking a `JobContext` (the job name, its `args` and the run's start time) and returning a summary or a failure.
2. Register it in `src/registry.rs` under the name the jobs file will use.
3. Reference it from `jobs.toml` with `kind = { builtin = "your_name" }`.

Anything that does not need typed API access is better off as a shell job, which needs no code at all.

## Layout

| Path | Contents |
| --- | --- |
| `src/job.rs` | The `Job` trait and the run record types |
| `src/jobs_file.rs` | Parsing and validation of `jobs.toml` |
| `src/scheduler.rs` | The cron loop, overlap guard and timeouts |
| `src/registry.rs` | Built-in lookup by name |
| `src/history.rs` | SQLite run history |
| `src/http.rs` | Status server |
| `src/builtins/` | Job implementations |
| `src/config.rs` | Environment configuration |
