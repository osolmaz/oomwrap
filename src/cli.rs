//! The command line: arguments, parsing at the border, and dispatch.

use std::error::Error as StdError;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{Args, Parser, Subcommand};

use crate::config::{BackendRequest, Policy, Profile, Settings, effective_profile};
use crate::doctor;
use crate::events::EventLog;
use crate::host::Host;
use crate::job_limits::JobLimitSettings;
use crate::launch;
use crate::limits::{
    DEFAULT_POLL, DEFAULT_TERM_GRACE, GuardPolicy, Pressure, PressureLimit, Size, SizeSetting,
    parse_duration,
};
use crate::shims::{self, default_bin_dir};
use crate::supervise::{self, MachineGuardRequirement, RunOptions};

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
    /// Keeps a job's scope until its guard has read why the job ended.
    #[command(hide = true)]
    HoldScope(HoldScopeArgs),
}

#[derive(Args, Debug)]
struct ExecJobArgs {
    /// The control socket of the guard.
    #[arg(long)]
    control: PathBuf,

    /// The lock of the guard; `exec-job` starts a holder of the scope.
    #[arg(long)]
    hold_scope: Option<PathBuf>,

    #[arg(required = true, last = true)]
    command: Vec<OsString>,
}

#[derive(Args, Debug)]
struct HoldScopeArgs {
    #[arg(long)]
    lock: PathBuf,

    /// Wait for the lock; without it, start the holder that waits.
    #[arg(long)]
    stay: bool,
}

#[derive(Args, Debug)]
struct DoctorArgs {
    #[arg(long)]
    json: bool,

    /// The profile whose settings the report resolves
    #[arg(long, value_enum, default_value_t = Profile::Generic)]
    profile: Profile,
}

#[derive(Args, Debug)]
struct InspectArgs {
    #[arg(long)]
    json: bool,
}

/// The values of `--backend`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum)]
enum BackendFlag {
    /// `cgroup` when its checks pass, `poll` otherwise
    Auto,
    /// A transient scope with kernel limits; exit 3 when it is not available
    Cgroup,
    /// The process-group guard, without kernel limits
    Poll,
}

impl From<BackendFlag> for BackendRequest {
    fn from(flag: BackendFlag) -> Self {
        match flag {
            BackendFlag::Auto => Self::Auto,
            BackendFlag::Cgroup => Self::Cgroup,
            BackendFlag::Poll => Self::Poll,
        }
    }
}

#[derive(Args, Debug)]
struct RunArgs {
    #[arg(long, value_enum, default_value_t = Profile::Auto)]
    profile: Profile,

    /// The backend [default: auto]
    #[arg(long, value_enum)]
    backend: Option<BackendFlag>,

    /// Hard limit of the job [default: smallest of 85%, the free room minus --min-mem, and the lowest memory.max above the scope]
    #[arg(long)]
    memory_max: Option<Size>,

    /// Soft limit of the job [default: 90% of --memory-max]
    #[arg(long)]
    memory_high: Option<Size>,

    /// The job's own swap limit [default: not set]
    #[arg(long)]
    memory_swap_max: Option<Size>,

    /// Floor for the memory domain's free room [default: 15%, at least 256M, at most 16G]
    #[arg(long)]
    min_mem: Option<Size>,

    /// PSI `full avg10` limit of the memory domain, while it is short of memory [default: 10%]
    #[arg(long)]
    domain_pressure: Option<Pressure>,

    /// How long the domain's pressure must last before a stop [default: 10s]
    // A negative number reaches the parser, which names the flag and the fault.
    #[arg(long, allow_negative_numbers = true, value_parser = parse_duration)]
    domain_pressure_for: Option<Duration>,

    /// PSI `full avg10` limit of the job's cgroup [default: 60%]
    #[arg(long)]
    job_pressure: Option<Pressure>,

    /// How long the job's pressure must last before a stop [default: 30s]
    #[arg(long, allow_negative_numbers = true, value_parser = parse_duration)]
    job_pressure_for: Option<Duration>,

    /// How often the guard reads [default: 1s]
    #[arg(long, allow_negative_numbers = true, value_parser = parse_duration)]
    poll: Option<Duration>,

    /// How long a stop waits after SIGTERM [default: 10s]
    #[arg(long, allow_negative_numbers = true, value_parser = parse_duration)]
    term_grace: Option<Duration>,

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

    /// The run comes from the shim or wrapper of this tool; with --profile auto, its name can tell the profile
    #[arg(long, value_name = "TOOL")]
    shim: Option<String>,

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
        Commands::Doctor(args) => doctor::doctor(host, args.profile, args.json).map(|()| 0),
        Commands::Inspect(args) => doctor::inspect(host, args.json).map(|()| 0),
        Commands::Run(args) => run_guard(args, host),
        Commands::InstallShims(args) => {
            let bin_dir = bin_dir_or_default(args.bin_dir, host)?;
            shims::install_shims(&bin_dir, &args.tools, args.force).map(|()| 0)
        }
        Commands::UninstallShims(args) => {
            let bin_dir = bin_dir_or_default(args.bin_dir, host)?;
            shims::uninstall_shims(&bin_dir, &args.tools).map(|()| 0)
        }
        Commands::Wrap(args) => shims::wrap(host, &args.path, args.force).map(|()| 0),
        Commands::Unwrap(args) => shims::unwrap(host, &args.path).map(|()| 0),
        Commands::ExecJob(args) => Ok(launch::exec_job(
            &args.control,
            args.hold_scope.as_deref(),
            &args.command,
            &host.proc_root,
        )),
        Commands::HoldScope(args) => Ok(launch::hold_scope(&args.lock, args.stay)),
    }
}

impl RunArgs {
    /// The settings that the flags give; the policy file fills the rest.
    fn settings(&self) -> Settings {
        let require_machine_guard = if self.require_machine_guard {
            Some(true)
        } else if self.allow_no_machine_guard {
            Some(false)
        } else {
            None
        };
        Settings {
            backend: self.backend.map(BackendRequest::from),
            memory_max: self.memory_max,
            memory_high: self.memory_high,
            memory_swap_max: self.memory_swap_max,
            min_mem: self.min_mem,
            domain_pressure: self.domain_pressure,
            domain_pressure_for: self.domain_pressure_for,
            job_pressure: self.job_pressure,
            job_pressure_for: self.job_pressure_for,
            poll: self.poll,
            term_grace: self.term_grace,
            require_machine_guard,
            event_log: self.event_log.clone(),
        }
    }
}

fn run_guard(args: RunArgs, host: &Host) -> Result<i32> {
    let command: Vec<String> = args
        .command
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let profile = effective_profile(args.profile, &command, args.shim.as_deref());
    let policy = Policy::load(&host.policy_files)?;
    let settings = policy.resolve(&args.settings(), profile).settings;
    let job_limits = JobLimitSettings {
        memory_max: settings.memory_max,
        memory_high: settings.memory_high,
        memory_swap_max: settings.memory_swap_max,
    };
    let backend = settings.backend.unwrap_or(BackendRequest::Auto);
    if backend == BackendRequest::Poll && job_limits.is_explicit() {
        bail!(
            "--memory-max, --memory-high and --memory-swap-max need the cgroup backend; drop --backend poll"
        );
    }
    let options = RunOptions {
        profile,
        shim: args.shim,
        machine_guards: policy.machine_guards(),
        policy: guard_policy(&settings),
        job_limits,
        backend,
        poll: settings.poll.unwrap_or(DEFAULT_POLL),
        term_grace: settings.term_grace.unwrap_or(DEFAULT_TERM_GRACE),
        machine_guard: match settings.require_machine_guard {
            Some(true) => MachineGuardRequirement::Required,
            Some(false) => MachineGuardRequirement::NotRequired,
            None => MachineGuardRequirement::ProfileDefault,
        },
        event_log: settings.event_log.unwrap_or(EventLog::Default),
        log_command: args.log_command,
        record_readings: args.record_readings,
        command: args.command,
    };
    supervise::run(options, host).map(|outcome| outcome.exit_code())
}

/// The stop policy: each setting that is not given takes its default.
fn guard_policy(settings: &Settings) -> GuardPolicy {
    let defaults = GuardPolicy::default();
    GuardPolicy {
        min_mem: settings
            .min_mem
            .map_or(defaults.min_mem, SizeSetting::Given),
        job_pressure: PressureLimit {
            at_least: settings
                .job_pressure
                .unwrap_or(defaults.job_pressure.at_least),
            lasting: settings
                .job_pressure_for
                .unwrap_or(defaults.job_pressure.lasting),
        },
        domain_pressure: PressureLimit {
            at_least: settings
                .domain_pressure
                .unwrap_or(defaults.domain_pressure.at_least),
            lasting: settings
                .domain_pressure_for
                .unwrap_or(defaults.domain_pressure.lasting),
        },
    }
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

    /// The help text of the pressure flags names the defaults that apply.
    #[test]
    fn the_help_names_the_default_pressures() {
        use crate::limits::{DEFAULT_DOMAIN_PRESSURE, DEFAULT_JOB_PRESSURE, PressureLimit};
        use clap::CommandFactory;
        let command = Cli::command();
        let run = command.find_subcommand("run").unwrap();
        let help = |id: &str| {
            let arg = run.get_arguments().find(|arg| arg.get_id() == id).unwrap();
            arg.get_help().unwrap().to_string()
        };
        let percent =
            |limit: PressureLimit| format!("[default: {}%]", limit.at_least.hundredths() / 100);
        let seconds = |limit: PressureLimit| format!("[default: {}s]", limit.lasting.as_secs());
        for (flag, limit) in [
            ("domain_pressure", DEFAULT_DOMAIN_PRESSURE),
            ("job_pressure", DEFAULT_JOB_PRESSURE),
        ] {
            assert!(
                help(flag).ends_with(&percent(limit)),
                "{flag}: {}",
                help(flag)
            );
            let lasting = format!("{flag}_for");
            assert!(
                help(&lasting).ends_with(&seconds(limit)),
                "{lasting}: {}",
                help(&lasting)
            );
        }
    }

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
