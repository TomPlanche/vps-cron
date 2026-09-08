//! The generic shell job: run any command line on a schedule.
//!
//! This is what makes the manager useful beyond the built-ins compiled into it.
//! Commands go through `sh -c`, so pipes, redirections and `&&` all work the
//! way they do in a crontab.

use anyhow::Context;
use async_trait::async_trait;
use tokio::process::Command;

use crate::job::{Job, JobContext, JobFailure, JobReport, JobResult};
use crate::jobs_file::{ShellCommand, ShellFileSpec};

/// Runs one configured command line.
pub struct ShellJob {
    spec: ShellCommand,
}

impl ShellJob {
    /// Builds a job from its jobs-file declaration.
    pub fn new(spec: ShellCommand) -> Self {
        Self { spec }
    }
}

/// Runs one script file directly, rather than an inline command line.
///
/// Where [`ShellJob`] always goes through `sh -c` (so a one-liner with `&&` and pipes
/// works the way it would in a crontab), this spawns the file itself, honouring its own
/// shebang. That's what lets a longer job be a proper `#!/usr/bin/env bash` script
/// instead of an escaped one-liner wedged into TOML, and lets it use anything other than
/// `/bin/sh` if it wants to. The file needs `chmod +x`; a job that isn't executable fails
/// with a clear error pointing at that, rather than a confusing "No such file or
/// directory" from the OS.
pub struct ShellFileJob {
    spec: ShellFileSpec,
}

impl ShellFileJob {
    /// Builds a job from its jobs-file declaration.
    pub fn new(spec: ShellFileSpec) -> Self {
        Self { spec }
    }
}

#[async_trait]
impl Job for ShellFileJob {
    async fn run(&self, ctx: &JobContext<'_>) -> JobResult {
        let path = self.spec.path();

        let mut command = Command::new(path);
        command.args(self.spec.args());

        command.env("VPS_CRON_JOB", ctx.job_name);
        command.env("VPS_CRON_STARTED_AT", ctx.started_at.to_rfc3339());
        command.kill_on_drop(true);

        if let Some(workdir) = self.spec.workdir() {
            command.current_dir(workdir);
        }

        if let Some(env) = self.spec.env() {
            for (key, value) in env {
                command.env(key, value);
            }
        }

        let output = command.output().await.with_context(|| {
            format!("Failed to run '{path}'. Is it executable? Try 'chmod +x {path}'.")
        })?;

        let captured = merge_streams(&output.stdout, &output.stderr);

        if output.status.success() {
            return Ok(JobReport::summary("Script exited 0").with_output(captured));
        }

        let status = match output.status.code() {
            Some(code) => format!("Script exited {code}"),
            None => "Script was killed by a signal".to_string(),
        };

        Err(JobFailure::new(status).with_output(captured))
    }
}

#[async_trait]
impl Job for ShellJob {
    async fn run(&self, ctx: &JobContext<'_>) -> JobResult {
        let command_line = self.spec.command();

        let mut command = Command::new("sh");
        command.arg("-c").arg(command_line);

        // Handy for a script that logs, names an output file, or reports
        // itself: it can tell which job it is without being told twice.
        command.env("VPS_CRON_JOB", ctx.job_name);
        command.env("VPS_CRON_STARTED_AT", ctx.started_at.to_rfc3339());

        // The scheduler enforces timeouts by dropping this future. Without
        // `kill_on_drop` a timed-out command would keep running unsupervised.
        command.kill_on_drop(true);

        if let Some(workdir) = self.spec.workdir() {
            command.current_dir(workdir);
        }

        if let Some(env) = self.spec.env() {
            for (key, value) in env {
                command.env(key, value);
            }
        }

        let output = command
            .output()
            .await
            .with_context(|| format!("Failed to spawn '{command_line}'"))?;

        let captured = merge_streams(&output.stdout, &output.stderr);

        if output.status.success() {
            return Ok(JobReport::summary("Command exited 0").with_output(captured));
        }

        let status = match output.status.code() {
            Some(code) => format!("Command exited {code}"),
            None => "Command was killed by a signal".to_string(),
        };

        Err(JobFailure::new(status).with_output(captured))
    }
}

/// Joins stdout and stderr into one captured block, labelling each stream.
///
/// Streams are labelled only when both carry content, so the common case of a
/// command that writes to just one of them stays clean.
fn merge_streams(stdout: &[u8], stderr: &[u8]) -> String {
    let out = String::from_utf8_lossy(stdout);
    let err = String::from_utf8_lossy(stderr);
    let (out, err) = (out.trim(), err.trim());

    match (out.is_empty(), err.is_empty()) {
        (true, true) => String::new(),
        (false, true) => out.to_string(),
        (true, false) => err.to_string(),
        (false, false) => format!("[stdout]\n{out}\n\n[stderr]\n{err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn context<'a>(args: &'a toml::Table) -> JobContext<'a> {
        JobContext {
            job_name: "test",
            args,
            started_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn captures_stdout_on_success() {
        let args = toml::Table::new();
        let job = ShellJob::new(ShellCommand::Line("echo hello".to_string()));
        let report = job
            .run(&context(&args))
            .await
            .expect("command should succeed");

        assert_eq!(report.summary, "Command exited 0");
        assert_eq!(report.output.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn reports_the_exit_code_and_keeps_stderr_on_failure() {
        let args = toml::Table::new();
        let job = ShellJob::new(ShellCommand::Line("echo boom >&2; exit 3".to_string()));
        let failure = job
            .run(&context(&args))
            .await
            .expect_err("command should fail");

        assert_eq!(failure.message, "Command exited 3");
        assert_eq!(failure.output.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn exposes_the_job_name_to_the_command() {
        let args = toml::Table::new();
        let job = ShellJob::new(ShellCommand::Line("echo $VPS_CRON_JOB".to_string()));
        let report = job
            .run(&context(&args))
            .await
            .expect("command should succeed");

        assert_eq!(report.output.as_deref(), Some("test"));
    }

    #[tokio::test]
    async fn honours_workdir_and_env() {
        let args = toml::Table::new();
        let job = ShellJob::new(ShellCommand::Detailed {
            command: "pwd; echo $VPS_CRON_TEST".to_string(),
            workdir: Some("/".to_string()),
            env: [("VPS_CRON_TEST".to_string(), "marker".to_string())]
                .into_iter()
                .collect(),
        });

        let report = job
            .run(&context(&args))
            .await
            .expect("command should succeed");
        let output = report.output.expect("output should be captured");
        assert!(
            output.contains("marker"),
            "env var should reach the command"
        );
        assert!(output.starts_with('/'), "workdir should be honoured");
    }

    /// Writes an executable script to a temp file for a shell_file test, returning its
    /// path. Not cleaned up automatically: each test removes its own when done.
    fn temp_script(name: &str, contents: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "vps-cron-test-{name}-{}-{:?}.sh",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test]
    async fn shell_file_captures_stdout_on_success() {
        let script = temp_script("stdout", "#!/bin/sh\necho hello\n");
        let args = toml::Table::new();
        let job = ShellFileJob::new(ShellFileSpec::Path(script.to_str().unwrap().to_string()));
        let report = job
            .run(&context(&args))
            .await
            .expect("script should succeed");

        assert_eq!(report.summary, "Script exited 0");
        assert_eq!(report.output.as_deref(), Some("hello"));
        std::fs::remove_file(&script).ok();
    }

    #[tokio::test]
    async fn shell_file_reports_the_exit_code_and_keeps_stderr_on_failure() {
        let script = temp_script("exit-code", "#!/bin/sh\necho boom >&2\nexit 3\n");
        let args = toml::Table::new();
        let job = ShellFileJob::new(ShellFileSpec::Path(script.to_str().unwrap().to_string()));
        let failure = job
            .run(&context(&args))
            .await
            .expect_err("script should fail");

        assert_eq!(failure.message, "Script exited 3");
        assert_eq!(failure.output.as_deref(), Some("boom"));
        std::fs::remove_file(&script).ok();
    }

    #[tokio::test]
    async fn shell_file_exposes_the_job_name_and_forwards_args() {
        let script = temp_script("args", "#!/bin/sh\necho $VPS_CRON_JOB $1\n");
        let args = toml::Table::new();
        let job = ShellFileJob::new(ShellFileSpec::Detailed {
            path: script.to_str().unwrap().to_string(),
            workdir: None,
            env: std::collections::BTreeMap::new(),
            args: vec!["marker-arg".to_string()],
        });
        let report = job
            .run(&context(&args))
            .await
            .expect("script should succeed");

        assert_eq!(report.output.as_deref(), Some("test marker-arg"));
        std::fs::remove_file(&script).ok();
    }

    #[tokio::test]
    async fn shell_file_honours_workdir_and_env() {
        let script = temp_script("workdir", "#!/bin/sh\npwd; echo $VPS_CRON_TEST\n");
        let args = toml::Table::new();
        let job = ShellFileJob::new(ShellFileSpec::Detailed {
            path: script.to_str().unwrap().to_string(),
            workdir: Some("/".to_string()),
            env: [("VPS_CRON_TEST".to_string(), "marker".to_string())]
                .into_iter()
                .collect(),
            args: Vec::new(),
        });

        let report = job
            .run(&context(&args))
            .await
            .expect("script should succeed");
        let output = report.output.expect("output should be captured");
        assert!(output.contains("marker"), "env var should reach the script");
        assert!(output.starts_with('/'), "workdir should be honoured");
        std::fs::remove_file(&script).ok();
    }

    #[tokio::test]
    async fn shell_file_explains_a_missing_executable_bit() {
        let path = std::env::temp_dir().join(format!(
            "vps-cron-test-noexec-{}-{:?}.sh",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, "#!/bin/sh\necho hi\n").unwrap();
        // Deliberately no chmod +x: the OS should refuse to spawn this at all.

        let args = toml::Table::new();
        let job = ShellFileJob::new(ShellFileSpec::Path(path.to_str().unwrap().to_string()));
        let failure = job
            .run(&context(&args))
            .await
            .expect_err("a non-executable file should fail to spawn");

        assert!(
            failure.message.contains("chmod +x"),
            "error should point at the fix, got: {}",
            failure.message
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn labels_streams_only_when_both_are_present() {
        assert_eq!(merge_streams(b"out", b""), "out");
        assert_eq!(merge_streams(b"", b"err"), "err");
        assert_eq!(
            merge_streams(b"out", b"err"),
            "[stdout]\nout\n\n[stderr]\nerr"
        );
        assert_eq!(merge_streams(b"", b""), "");
    }
}
