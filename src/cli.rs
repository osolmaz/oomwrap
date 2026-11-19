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
use crate::host::Host;
use crate::limits::{Size, parse_duration};
use crate::policy::Floors;
use crate::shims::{self, ScriptFloors, default_bin_dir};
use crate::supervise::{self, EarlyoomRequirement, RunOptions};

const DEFAULTS: Defaults = Defaults {
    min_mem: "24G",
    min_swap: "4G",
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

    #[arg(long, default_value = DEFAULTS.min_mem)]
    min_mem: Size,

    #[arg(long, default_value = DEFAULTS.min_swap)]
    min_swap: Size,

    #[arg(long, default_value = DEFAULTS.poll, value_parser = parse_duration)]
    poll: Duration,

    #[arg(long, default_value = DEFAULTS.term_grace, value_parser = parse_duration)]
    term_grace: Duration,

    #[arg(long, conflicts_with = "allow_no_earlyoom")]
    require_earlyoom: bool,

    #[arg(long, conflicts_with = "require_earlyoom")]
    allow_no_earlyoom: bool,

    #[arg(long)]
    event_log: Option<PathBuf>,

    #[arg(long)]
    use_systemd_scope: bool,

    #[arg(long)]
    memory_high: Option<Size>,

    #[arg(long)]
    memory_max: Option<Size>,

    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<OsString>,
}

#[derive(Args, Debug)]
struct InstallShimsArgs {
    #[arg(long)]
    bin_dir: Option<PathBuf>,

    #[arg(long = "tool")]
    tools: Vec<String>,

    #[arg(long, default_value = DEFAULTS.min_mem)]
    min_mem: String,

    #[arg(long, default_value = DEFAULTS.min_swap)]
    min_swap: String,

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

    #[arg(long, default_value = DEFAULTS.min_mem)]
    min_mem: String,

    #[arg(long, default_value = DEFAULTS.min_swap)]
    min_swap: String,

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
                min_mem: &args.min_mem,
                min_swap: &args.min_swap,
            };
            shims::install_shims(&bin_dir, &args.tools, &floors, args.force).map(|()| 0)
        }
        Commands::UninstallShims(args) => {
            let bin_dir = bin_dir_or_default(args.bin_dir, host)?;
            shims::uninstall_shims(&bin_dir, &args.tools).map(|()| 0)
        }
        Commands::Wrap(args) => {
            let floors = ScriptFloors {
                min_mem: &args.min_mem,
                min_swap: &args.min_swap,
            };
            shims::wrap(&args.path, &floors, args.force).map(|()| 0)
        }
        Commands::Unwrap(args) => shims::unwrap(&args.path).map(|()| 0),
    }
}

fn run_guard(args: RunArgs, host: &Host) -> Result<i32> {
    if args.use_systemd_scope || args.memory_high.is_some() || args.memory_max.is_some() {
        bail!("systemd scope support is planned but not implemented in this release");
    }
    let earlyoom = if args.require_earlyoom {
        EarlyoomRequirement::Required
    } else if args.allow_no_earlyoom {
        EarlyoomRequirement::NotRequired
    } else {
        EarlyoomRequirement::ProfileDefault
    };
    let options = RunOptions {
        profile: args.profile,
        floors: Floors {
            mem_available: args.min_mem,
            swap_free: args.min_swap,
        },
        poll: args.poll,
        term_grace: args.term_grace,
        earlyoom,
        event_log: args.event_log,
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
    fn help_is_not_a_failure() {
        let error = Cli::try_parse_from(["oomwrap", "--help"]).unwrap_err();
        assert_eq!(error.exit_code(), 0);
    }
}
