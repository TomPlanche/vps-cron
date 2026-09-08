// pm2 process file for vps-cron.
//
//   pm2 start ecosystem.config.cjs
//   pm2 logs vps-cron
//   pm2 save && pm2 startup      # survive a reboot
//
// The binary is the release build, so run `cargo build --release` first.
// Configuration comes from the `.env` next to `cwd`, which the process loads
// itself; the `env` block below only holds what pm2 needs to know about.

const path = require("node:path");

const root = __dirname;

module.exports = {
  apps: [
    {
      name: "vps-cron",
      script: path.join(root, "target/release/vps-cron"),
      // A compiled binary, not a script: pm2 must not put an interpreter in front of it.
      interpreter: "none",
      // vps-cron resolves `.env`, `jobs.toml` and `DATA_DIR` relative to the working directory.
      cwd: root,
      // The scheduler is a single stateful process holding the job locks; never fork it.
      instances: 1,
      exec_mode: "fork",
      autorestart: true,
      restart_delay: 5000,
      // A job that fails at startup should not turn into a restart loop.
      min_uptime: "30s",
      max_restarts: 10,
      watch: false,
      kill_timeout: 10000,
      merge_logs: true,
      time: true,
      out_file: path.join(root, "logs/vps-cron.out.log"),
      error_file: path.join(root, "logs/vps-cron.err.log"),
      env: {
        RUST_LOG: "info",
      },
      env_debug: {
        RUST_LOG: "debug",
      },
    },
  ],
};
