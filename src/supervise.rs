//! `oomwrap run`: preflight, launch, the watch loop, and one stop path.

mod terminal;

use std::ffi::OsString;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};

use crate::backend::ProcessGroup;
use crate::config::{Profile, effective_profile};
use crate::events::{Event, EventKind, EventLabel, EventLogger};
use crate::host::Host;
use crate::limits::format_bytes;
use crate::policy::{Floors, StopReason, evaluate};
use crate::readings::MemInfo;
use terminal::{TerminalForeground, restore_terminal};

/// The exit code of a job that the guard stopped.
const STOPPED_EXIT_CODE: i32 = 137;
/// The base of the exit code for a job that ended on a signal.
const SIGNAL_EXIT_BASE: i32 = 128;

/// Whether a run needs an active `earlyoom`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EarlyoomRequirement {
    /// The profile decides.
    ProfileDefault,
    Required,
    NotRequired,
}

pub(crate) struct RunOptions {
    pub(crate) profile: Profile,
    pub(crate) floors: Floors,
    pub(crate) poll: Duration,
    pub(crate) term_grace: Duration,
    pub(crate) earlyoom: EarlyoomRequirement,
    pub(crate) event_log: Option<PathBuf>,
    pub(crate) command: Vec<OsString>,
}

/// Why `oomwrap run` refused to launch the command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Refusal {
    EarlyoomMissing,
    Preflight(StopReason),
}

/// How a run ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Outcome {
    /// The command ended on its own with this code.
    Exited(i32),
    Refused(Refusal),
    /// The guard stopped the job.
    Stopped(StopReason),
    /// oomwrap received this signal number and ended the job.
    Interrupted(i32),
}

impl Outcome {
    pub(crate) fn exit_code(self) -> i32 {
        match self {
            Self::Exited(code) => code,
            Self::Refused(Refusal::EarlyoomMissing) => 3,
            Self::Refused(Refusal::Preflight(_)) => 4,
            Self::Stopped(_) => STOPPED_EXIT_CODE,
            Self::Interrupted(signal) => SIGNAL_EXIT_BASE + signal,
        }
    }
}

pub(crate) fn run(options: RunOptions, host: &Host) -> Result<Outcome> {
    let command = command_strings(&options.command);
    let profile = effective_profile(options.profile, &command);
    let require_earlyoom = match options.earlyoom {
        EarlyoomRequirement::ProfileDefault => profile.requires_earlyoom(),
        EarlyoomRequirement::Required => true,
        EarlyoomRequirement::NotRequired => false,
    };

    let mut logger = EventLogger::new(options.event_log.as_deref())?;
    logger.log(Event::new(EventKind::Preflight, EventLabel::Run).with_command(command.clone()))?;

    if require_earlyoom && !host.earlyoom_active()? {
        logger.log(
            Event::new(EventKind::Refused, EventLabel::Earlyoom)
                .with_command(command.clone())
                .with_detail("earlyoom is required but not active"),
        )?;
        eprintln!("refusing to launch: earlyoom is required for profile {profile:?}");
        return Ok(Outcome::Refused(Refusal::EarlyoomMissing));
    }

    let mem = host.read_meminfo()?;
    if let Some(reason) = evaluate(&options.floors, &mem) {
        logger.log(
            Event::new(EventKind::Refused, refusal_label(reason))
                .with_command(command.clone())
                .with_mem(mem)
                .with_detail(refusal_detail(reason)),
        )?;
        eprintln!(
            "refusing to launch: {}",
            below_floor(reason, "is below floor")
        );
        return Ok(Outcome::Refused(Refusal::Preflight(reason)));
    }

    let signals = ShutdownSignals::register()?;
    let group = ProcessGroup::spawn(&options.command)?;
    let mut supervisor = Supervisor {
        group,
        terminal: None,
        logger,
        command,
        term_grace: options.term_grace,
    };
    match TerminalForeground::handoff_to_child(supervisor.group.pgid()) {
        Ok(terminal) => supervisor.terminal = terminal,
        Err(error) => return supervisor.stop(StopCause::GuardError(error)),
    }
    let launched = supervisor.event(EventKind::Launched);
    supervisor.logger.log_or_warn(launched);
    supervisor.watch(host, &options.floors, options.poll, &signals)
}

/// Why the supervisor ends the job early.
enum StopCause {
    Pressure { reason: StopReason, mem: MemInfo },
    Signal(i32),
    GuardError(anyhow::Error),
}

struct Supervisor {
    group: ProcessGroup,
    terminal: Option<TerminalForeground>,
    logger: EventLogger,
    command: Vec<String>,
    term_grace: Duration,
}

impl Supervisor {
    fn watch(
        &mut self,
        host: &Host,
        floors: &Floors,
        poll: Duration,
        signals: &ShutdownSignals,
    ) -> Result<Outcome> {
        loop {
            if let Some(signal) = signals.pending() {
                return self.stop(StopCause::Signal(signal));
            }
            match self.group.try_wait() {
                Ok(Some(status)) => return self.finish(status),
                Ok(None) => {}
                Err(error) => return self.stop(StopCause::GuardError(anyhow!(error))),
            }
            let mem = match host.read_meminfo() {
                Ok(mem) => mem,
                Err(error) => return self.stop(StopCause::GuardError(error)),
            };
            if let Some(reason) = evaluate(floors, &mem) {
                return self.stop(StopCause::Pressure { reason, mem });
            }
            match sleep_until_poll_signal_or_exit(poll, signals, &mut self.group) {
                Ok(Some(status)) => return self.finish(status),
                Ok(None) => {}
                Err(error) => return self.stop(StopCause::GuardError(anyhow!(error))),
            }
        }
    }

    /// The command's main process exited: end what remains of its group.
    fn finish(&mut self, status: ExitStatus) -> Result<Outcome> {
        let code = status_to_code(status);
        let exit = self.event(EventKind::ChildExit).with_exit(code);
        self.logger.log_or_warn(exit);
        if self.group.has_members() {
            let cleanup = self
                .event(EventKind::ProcessGroupCleanup)
                .with_detail("leader exited while process group still had running members");
            self.logger.log_or_warn(cleanup);
        }
        restore_terminal(&mut self.terminal);
        self.group.terminate(self.term_grace)?;
        Ok(Outcome::Exited(code))
    }

    /// The one path that ends a running job before its command ends.
    fn stop(&mut self, cause: StopCause) -> Result<Outcome> {
        restore_terminal(&mut self.terminal);
        let pgid = self.group.pgid();
        match &cause {
            StopCause::Pressure { reason, mem } => {
                let detail = below_floor(*reason, "below floor");
                let event = self
                    .event(EventKind::MemoryPressureKill)
                    .with_mem(*mem)
                    .with_detail(detail.clone());
                self.logger.log_or_warn(event);
                eprintln!("memory pressure: {detail}; terminating process group {pgid}");
            }
            StopCause::Signal(signal) => {
                let event = self
                    .event(EventKind::ParentSignalKill)
                    .with_detail(format!("received signal {signal}"));
                self.logger.log_or_warn(event);
                eprintln!("received signal {signal}; terminating process group {pgid}");
            }
            StopCause::GuardError(error) => {
                eprintln!("guard error after launch: {error:#}; terminating process group {pgid}");
            }
        }
        let terminated = self.group.terminate(self.term_grace);
        match cause {
            StopCause::Pressure { reason, .. } => {
                terminated?;
                self.group.reap();
                Ok(Outcome::Stopped(reason))
            }
            StopCause::Signal(signal) => {
                terminated?;
                self.group.reap();
                Ok(Outcome::Interrupted(signal))
            }
            StopCause::GuardError(error) => {
                self.group.reap();
                Err(error)
            }
        }
    }

    fn event(&self, kind: EventKind) -> Event {
        Event::new(kind, EventLabel::Run)
            .with_command(self.command.clone())
            .with_pid(self.group.leader_pid())
    }
}

fn refusal_label(reason: StopReason) -> EventLabel {
    match reason {
        StopReason::MemAvailable { .. } => EventLabel::Memory,
        StopReason::SwapFree { .. } => EventLabel::Swap,
    }
}

fn refusal_detail(reason: StopReason) -> &'static str {
    match reason {
        StopReason::MemAvailable { .. } => "MemAvailable is below the configured floor",
        StopReason::SwapFree { .. } => "SwapFree is below the configured floor",
    }
}

/// Formats a floor reason as "`MemAvailable 1.0MiB <relation> 2.0MiB`".
fn below_floor(reason: StopReason, relation: &str) -> String {
    let (name, observed, floor) = match reason {
        StopReason::MemAvailable {
            observed_bytes,
            floor_bytes,
        } => ("MemAvailable", observed_bytes, floor_bytes),
        StopReason::SwapFree {
            observed_bytes,
            floor_bytes,
        } => ("SwapFree", observed_bytes, floor_bytes),
    };
    format!(
        "{name} {} {relation} {}",
        format_bytes(observed),
        format_bytes(floor)
    )
}

/// The shutdown signals that oomwrap forwards as a stop of the job.
struct ShutdownSignals {
    received: Arc<AtomicUsize>,
}

impl ShutdownSignals {
    fn register() -> Result<Self> {
        let received = Arc::new(AtomicUsize::new(0));
        for signal_number in [SIGINT, SIGTERM, SIGHUP, SIGQUIT] {
            signal_hook::flag::register_usize(
                signal_number,
                Arc::clone(&received),
                signal_number as usize,
            )
            .with_context(|| format!("failed to register signal handler for {signal_number}"))?;
        }
        Ok(Self { received })
    }

    fn pending(&self) -> Option<i32> {
        match self.received.load(Ordering::SeqCst) {
            0 => None,
            signal_number => Some(signal_number as i32),
        }
    }
}

// A mutant here only changes how often the loop wakes up; no test can see
// that without depending on timing.
#[cfg_attr(test, mutants::skip)]
fn sleep_until_poll_signal_or_exit(
    poll: Duration,
    signals: &ShutdownSignals,
    group: &mut ProcessGroup,
) -> io::Result<Option<ExitStatus>> {
    let started = Instant::now();
    while signals.pending().is_none() {
        if let Some(status) = group.try_wait()? {
            return Ok(Some(status));
        }
        let elapsed = started.elapsed();
        if elapsed >= poll {
            break;
        }
        let remaining = poll.saturating_sub(elapsed);
        if remaining.is_zero() {
            break;
        }
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }
    Ok(None)
}

pub(crate) fn command_strings(command: &[OsString]) -> Vec<String> {
    command
        .iter()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect()
}

fn status_to_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    if let Some(signal) = status.signal() {
        return SIGNAL_EXIT_BASE + signal;
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::read_process_group_id;
    use crate::host::{command_invokes_executable, read_cmdline, read_rss_bytes};
    use crate::readings::parse_process_group_id;

    #[test]
    fn command_and_proc_helpers_parse_expected_shapes() {
        let command = command_strings(&[OsString::from("vllm"), OsString::from("serve")]);
        assert_eq!(command, vec!["vllm".to_string(), "serve".to_string()]);
        assert!(command_invokes_executable(
            "/usr/bin/earlyoom -m 5",
            "earlyoom"
        ));
        assert!(command_invokes_executable("earlyoom -m 5", "earlyoom"));
        assert!(!command_invokes_executable(
            "oomwrap run --require-earlyoom",
            "earlyoom"
        ));
        assert!(!command_invokes_executable(
            "python3 -c 'earlyoom'",
            "earlyoom"
        ));

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmdline"), b"vllm\0serve\0").unwrap();
        std::fs::write(
            dir.path().join("status"),
            "Name:\tvllm\nVmRSS:\t42 kB\nState:\tS\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("stat"), "123 (vllm worker) S 1 456 456 0\n").unwrap();
        assert_eq!(read_cmdline(dir.path()).unwrap(), "vllm serve");
        assert_eq!(read_rss_bytes(dir.path()).unwrap(), Some(42 * 1024));
        assert_eq!(read_process_group_id(dir.path()).unwrap(), 456);
        assert_eq!(
            parse_process_group_id("123 (name with spaces) S 1 789 789 0").unwrap(),
            789
        );
    }

    #[test]
    fn outcomes_map_to_exit_codes() {
        let reason = StopReason::SwapFree {
            observed_bytes: 1,
            floor_bytes: 2,
        };
        assert_eq!(Outcome::Exited(7).exit_code(), 7);
        assert_eq!(Outcome::Refused(Refusal::EarlyoomMissing).exit_code(), 3);
        assert_eq!(Outcome::Refused(Refusal::Preflight(reason)).exit_code(), 4);
        assert_eq!(Outcome::Stopped(reason).exit_code(), 137);
        assert_eq!(Outcome::Interrupted(15).exit_code(), 143);
    }

    #[test]
    fn floor_messages_name_the_value_and_the_floor() {
        let reason = StopReason::MemAvailable {
            observed_bytes: 1024 * 1024,
            floor_bytes: 2 * 1024 * 1024,
        };
        assert_eq!(
            below_floor(reason, "below floor"),
            "MemAvailable 1.0MiB below floor 2.0MiB"
        );
        assert_eq!(refusal_label(reason), EventLabel::Memory);
        assert_eq!(
            refusal_detail(reason),
            "MemAvailable is below the configured floor"
        );
    }
}
