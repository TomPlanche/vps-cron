#!/usr/bin/env bash
# atkuzu-daily.sh: generates tomorrow's Takuzu daily puzzle, rebuilds the site so the
# fresh archive file is actually served, and restarts it under pm2. Run by vps-cron (see
# jobs.toml's "atkuzu-daily" job) at Europe/Paris midnight, DST included (that's the
# job's tz field, not anything in this script).
#
# Expects its cwd to already be the atkuzu repo (see the job's workdir): `pnpm run ...`
# and generate-daily.ts's own ATKUZU_DAILY_SEED lookup (dotenv) are both relative to that.
set -euo pipefail

date=$(TZ=Europe/Paris date +%Y-%m-%d)
timestamp=$(date -u -d "${date}T12:00:00" +%s)

pnpm run daily:generate "$timestamp"

# adapter-node serves static/ from whatever was copied into build/client at the last
# `vite build`, not read live from disk, so the fresh file above stays invisible to
# players until the site is rebuilt and pm2 picks up that new build on restart.
pnpm run build
pm2 restart atkuzu
