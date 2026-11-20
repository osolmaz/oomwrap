//! The command line: arguments, parsing at the border, and dispatch.

use std::error::Error as StdError;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{Args, Parser, Subcommand};

use crate::config::Profile;
use crate::doctor::{self, Defaults};
use crate::events::EventLog;
use crate::host::Host;
use crate::launch;
use crate::limits::{GuardPolicy, Pressure, PressureLimit, Size, SizeSetting, parse_duration};
use crate::shims::{self, ScriptFloors, default_bin_dir};
use crate::supervise::{self, MachineGuardRequirement, RunOptions};

const DEFAULTS: Defaults = Defaults {
    poll: "1s",
    term_grace: "10s",
};

#[derive(Parser, Debug)]
#[command(author, version, about)]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Doctor(DoctorArgs),
    Run(RunArgs),
    InstallShims(InstallShimsArgs),
    UninstallShims(UninstallShimsArgs),
    Wrap(WrapArgs),
    Unwrap(UnwrapArgs),
    Inspect(InspectArgs),
    /// The first process of a guarded job; `oomwrap run` starts it.
    #[command(hide = true)]
    ExecJob(ExecJobArgs),
}

#[derive(Args, Debug)]
struct ExecJobArgs {
    /// The control socket of the guard.
    #[arg(long)]
    control: PathBuf,

    #[arg(required = true, last = true)]
    command: Vec<OsString>,
}

#[derive(Args, Debug)]
struct DoctorArgs {
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct InspectArgs {
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct RunArgs {
    #[arg(long, value_enum, default_value_t = Profile::Auto)]
    profile: Profile,

    /// Floor for the memory domain's free room [default: 15%, at least 256M, at most 16G]
    #[arg(long)]
    min_mem: Option<Size>,

    /// PSI `full avg10` limit of the memory domain [default: 20%]
    #[arg(long)]
    domain_pressure: Option<Pressure>,

    /// How long the domain's pressure must last before a stop [default: 10s]
    // A negative number reaches the parser, which names the flag and the fault.
    #[arg(long, allow_negative_numbers = true, value_parser = parse_duration)]
    domain_pressure_for: Option<Duration>,

    #[arg(long, allow_negative_numbers = true, default_value = DEFAULTS.poll, value_parser = parse_duration)]
    poll: Duration,

    #[arg(long, allow_negative_numbers = true, default_value = DEFAULTS.term_grace, value_parser = parse_duration)]
    term_grace: Duration,

    #[arg(long, conflicts_with = "allow_no_machine_guard")]
    require_machine_guard: bool,

    #[arg(long, conflicts_with = "require_machine_guard")]
    allow_no_machine_guard: bool,

    /// The event log file, or none [default: one file per day in ${XDG_STATE_HOME:-~/.local/state}/oomwrap]
    #[arg(long, value_name = "PATH|none")]
    event_log: Option<EventLog>,

    /// Log the full command line; by default, events hold the program name only
    #[arg(long)]
    log_command: bool,

    /// Write every reading and the policy's decision to this file, as JSON lines
    #[arg(long, value_name = "PATH")]
    record_readings: Option<PathBuf>,

    #[arg(long)]
    use_systemd_scope: bool,

    #[arg(long)]
    memory_high: Option<Size>,

    #[arg(long)]
    memory_max: Option<Size>,

    /// The command starts at its first word; an unknown flag before it is an
    /// error, never the command.
    #[arg(required = true, trailing_var_arg = true)]
    command: Vec<OsString>,
}

#[derive(Args, Debug)]
struct InstallShimsArgs {
    #[arg(long)]
    bin_dir: Option<PathBuf>,

    #[arg(long = "tool")]
    tools: Vec<String>,

    /// The `--min-mem` that the shims pass; without it, `oomwrap run` decides
    #[arg(long)]
    min_mem: Option<String>,

    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug)]
struct UninstallShimsArgs {
    #[arg(long)]
    bin_dir: Option<PathBuf>,

    #[arg(long = "tool")]
    tools: Vec<String>,
}

#[derive(Args, Debug)]
struct WrapArgs {
    path: PathBuf,

    /// The `--min-mem` that the wrapper passes; without it, `oomwrap run` decides
    #[arg(long)]
    min_mem: Option<String>,

    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug)]
struct UnwrapArgs {
    path: PathBuf,
}

/// Parses the arguments and runs the command; returns the exit code.
pub(crate) fn run(host: &Host) -> i32 {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => return report_parse_error(&error),
    };
    match dispatch(cli.command, host) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            2
        }
    }
}

fn dispatch(command: Commands, host: &Host) -> Result<i32> {
    match command {
        Commands::Doctor(args) => doctor::doctor(host, args.json).map(|()| 0),
        Commands::Inspect(args) => doctor::inspect(host, &DEFAULTS, args.json).map(|()| 0),
        Commands::Run(args) => run_guard(args, host),
        Commands::InstallShims(args) => {
            let bin_dir = bin_dir_or_default(args.bin_dir, host)?;
            let floors = ScriptFloors {
                min_mem: args.min_mem.as_deref(),
            };
            shims::install_shims(&bin_dir, &args.tools, &floors, args.force).map(|()| 0)
        }
        Commands::UninstallShims(args) => {
            let bin_dir = bin_dir_or_default(args.bin_dir, host)?;
            shims::uninstall_shims(&bin_dir, &args.tools).map(|()| 0)
        }
        Commands::Wrap(args) => {
            let floors = ScriptFloors {
                min_mem: args.min_mem.as_deref(),
            };
            shims::wrap(&args.path, &floors, args.force).map(|()| 0)
        }
        Commands::Unwrap(args) => shims::unwrap(&args.path).map(|()| 0),
        Commands::ExecJob(args) => Ok(launch::exec_job(&args.control, &args.command)),
    }
}

fn run_guard(args: RunArgs, host: &Host) -> Result<i32> {
    if args.use_systemd_scope || args.memory_high.is_some() || args.memory_max.is_some() {
        bail!("systemd scope support is planned but not implemented in this release");
    }
    let machine_guard = if args.require_machine_guard {
        MachineGuardRequirement::Required
    } else if args.allow_no_machine_guard {
        MachineGuardRequirement::NotRequired
    } else {
        MachineGuardRequirement::ProfileDefault
    };
    let defaults = GuardPolicy::default();
    let policy = GuardPolicy {
        min_mem: args.min_mem.map_or(defaults.min_mem, SizeSetting::Given),
        job_pressure: defaults.job_pressure,
        domain_pressure: PressureLimit {
            at_least: args
                .domain_pressure
                .unwrap_or(defaults.domain_pressure.at_least),
            lasting: args
                .domain_pressure_for
                .unwrap_or(defaults.domain_pressure.lasting),
        },
    };
    let options = RunOptions {
        profile: args.profile,
        policy,
        poll: args.poll,
        term_grace: args.term_grace,
        machine_guard,
        event_log: args.event_log.unwrap_or(EventLog::Default),
        log_command: args.log_command,
        record_readings: args.record_readings,
        command: args.command,
    };
    supervise::run(options, host).map(|outcome| outcome.exit_code())
}

fn bin_dir_or_default(bin_dir: Option<PathBuf>, host: &Host) -> Result<PathBuf> {
    match bin_dir {
        Some(bin_dir) => Ok(bin_dir),
        None => default_bin_dir(host),
    }
}

/// Prints a parse error. An invalid flag value reads as
/// `error: invalid --flag: <cause>`; other errors keep clap's own text.
fn report_parse_error(error: &clap::Error) -> i32 {
    if error.kind() == ErrorKind::ValueValidation
        && let Some(ContextValue::String(arg)) = error.get(ContextKind::InvalidArg)
        && let Some(cause) = error.source()
    {
        let flag = arg.split_whitespace().next().unwrap_or(arg);
        eprintln!("error: invalid {flag}: {}", error_chain(cause));
        return 2;
    }
    let _ = error.print();
    error.exit_code()
}

fn error_chain(error: &(dyn StdError + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_values_name_the_flag_and_the_cause() {
        let error =
            Cli::try_parse_from(["oomwrap", "run", "--poll", "abc", "--", "true"]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ValueValidation);
        let cause = error.source().unwrap();
        assert!(error_chain(cause).starts_with("invalid duration"));
    }

    #[test]
    fn exec_job_takes_the_command_after_the_separator() {
        let cli = Cli::try_parse_from([
            "oomwrap",
            "exec-job",
            "--control",
            "/run/c",
            "--",
            "tool",
            "--skill",
            "-x",
        ])
        .unwrap();
        let Commands::ExecJob(args) = cli.command else {
            panic!("not exec-job");
        };
        assert_eq!(args.control, PathBuf::from("/run/c"));
        assert_eq!(args.command, ["tool", "--skill", "-x"]);
        assert!(Cli::try_parse_from(["oomwrap", "exec-job", "--control", "/c"]).is_err());
    }

    #[test]
    fn the_event_log_takes_a_path_or_none() {
        let parse = |args: &[&str]| {
            let cli =
                Cli::try_parse_from([&["oomwrap", "run"][..], args, &["--", "true"]].concat())
                    .unwrap();
            let Commands::Run(args) = cli.command else {
                panic!("not a run command");
            };
            args
        };
        assert_eq!(parse(&[]).event_log, None);
        assert_eq!(
            parse(&["--event-log", "none"]).event_log,
            Some(EventLog::Off)
        );
        let args = parse(&[
            "--event-log",
            "e.jsonl",
            "--log-command",
            "--record-readings",
            "t.jsonl",
        ]);
        assert_eq!(
            args.event_log,
            Some(EventLog::File(PathBuf::from("e.jsonl")))
        );
        assert!(args.log_command);
        assert_eq!(args.record_readings, Some(PathBuf::from("t.jsonl")));
    }

    #[test]
    fn help_is_not_a_failure() {
        let error = Cli::try_parse_from(["oomwrap", "--help"]).unwrap_err();
        assert_eq!(error.exit_code(), 0);
    }

    #[test]
    fn the_command_takes_its_own_flags_but_an_unknown_flag_is_an_error() {
        let cli = Cli::try_parse_from(["oomwrap", "run", "bash", "-c", "exit 5"]).unwrap();
        let Commands::Run(args) = cli.command else {
            panic!("not a run command");
        };
        assert_eq!(args.command, ["bash", "-c", "exit 5"]);
        let error =
            Cli::try_parse_from(["oomwrap", "run", "--min-swap", "0", "--", "true"]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnknownArgument);
    }
}
