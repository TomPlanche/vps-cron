//! Command line parsing.
//!
//! Three commands is not enough to justify a parser dependency, so this is
//! hand-rolled and tested rather than pulled in.

use anyhow::{bail, Result};

/// What the user asked the binary to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Run the scheduler until stopped. The default.
    Serve,
    /// Run one job once, now, then exit.
    Run {
        /// The job name from the jobs file.
        job: String,
    },
    /// Print the configured jobs and exit.
    List,
    /// Print usage and exit.
    Help,
}

/// Usage text, printed by `--help` and on a parse error.
pub const USAGE: &str = "\
vps-cron - a small cron manager

USAGE:
    vps-cron               Run the scheduler until stopped
    vps-cron run <job>     Run one job once, now, then exit
    vps-cron list          List the configured jobs
    vps-cron --help        Print this message

Configuration comes from the environment and the jobs file. See .env.example.";

/// Parses the arguments, excluding the program name.
pub fn parse<I>(args: I) -> Result<Command>
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = args.into_iter().collect();

    let Some(first) = args.first() else {
        return Ok(Command::Serve);
    };

    match first.as_str() {
        "-h" | "--help" | "help" => Ok(Command::Help),
        "list" => Ok(Command::List),
        "run" => match args.len() {
            1 => bail!("'run' needs a job name. Try 'vps-cron list' to see them."),
            2 => Ok(Command::Run {
                job: args[1].clone(),
            }),
            _ => bail!("'run' takes exactly one job name, got {}", args.len() - 1),
        },
        other => bail!("Unknown command '{other}'. Try 'vps-cron --help'."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<Command> {
        parse(args.iter().map(|s| (*s).to_string()))
    }

    #[test]
    fn no_arguments_means_serve() {
        assert_eq!(parse_args(&[]).unwrap(), Command::Serve);
    }

    #[test]
    fn run_takes_one_job_name() {
        assert_eq!(
            parse_args(&["run", "backup"]).unwrap(),
            Command::Run {
                job: "backup".to_string()
            }
        );
    }

    #[test]
    fn run_without_a_job_name_explains_itself() {
        let err = parse_args(&["run"]).unwrap_err();
        assert!(err.to_string().contains("needs a job name"));
    }

    #[test]
    fn run_with_extra_arguments_is_rejected() {
        // Catches an unquoted name with a space, which would otherwise run the
        // wrong job or none at all.
        let err = parse_args(&["run", "my", "job"]).unwrap_err();
        assert!(err.to_string().contains("exactly one job name"));
    }

    #[test]
    fn help_and_list_are_recognised() {
        assert_eq!(parse_args(&["--help"]).unwrap(), Command::Help);
        assert_eq!(parse_args(&["-h"]).unwrap(), Command::Help);
        assert_eq!(parse_args(&["list"]).unwrap(), Command::List);
    }

    #[test]
    fn an_unknown_command_points_at_help() {
        let err = parse_args(&["serve"]).unwrap_err();
        assert!(err.to_string().contains("--help"));
    }
}
