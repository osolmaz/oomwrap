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
use crate::domain::{Domain, MemoryDomain};
use crate::events::{Event, EventKind, EventLabel, EventLogger};
use crate::host::Host;
use crate::limits::{GuardPolicy, StopLimits, format_bytes};
use crate::policy::{Decision, PressureWindow, StopReason, preflight};
use crate::readings::{DomainReadings, Readings};
use terminal::{TerminalForeground, restore_terminal};

/// The exit code of a job that the guard stopped.
const STOPPED_EXIT_CODE: i32 = 137;
/// The base of the exit code for a job that ended on a signal.
const SIGNAL_EXIT_BASE: i32 = 128;
/// oomwrap cannot guard as asked.
const CANNOT_GUARD_EXIT_CODE: i32 = 3;
/// Preflight refused the launch: no room in the domain now.
const PREFLIGHT_EXIT_CODE: i32 = 4;

/// Whether a run needs a machine-wide guard.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MachineGuardRequirement {
    /// The profile decides.
    ProfileDefault,
    Required,
    NotRequired,
}

pub(crate) struct RunOptions {
    pub(crate) profile: Profile,
    pub(crate) policy: GuardPolicy,
    pub(crate) poll: Duration,
    pub(crate) term_grace: Duration,
    pub(crate) machine_guard: MachineGuardRequirement,
    pub(crate) event_log: Option<PathBuf>,
    pub(crate) command: Vec<OsString>,
}

/// Why `oomwrap run` refused to launch the command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Refusal {
    /// A required machine-wide guard is not active.
    NoMachineGuard,
    /// A domain condition already holds.
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
            Self::Refused(Refusal::NoMachineGuard) => CANNOT_GUARD_EXIT_CODE,
            Self::Refused(Refusal::Preflight(_)) => PREFLIGHT_EXIT_CODE,
            Self::Stopped(_) => STOPPED_EXIT_CODE,
            Self::Interrupted(signal) => SIGNAL_EXIT_BASE + signal,
        }
    }
}

pub(crate) fn run(options: RunOptions, host: &Host) -> Result<Outcome> {
    let command = command_strings(&options.command);
    let profile = effective_profile(options.profile, &command);
    let mut logger = EventLogger::new(options.event_log.as_deref())?;
    let memory = MemoryDomain::find(host)?;
    let domain = &memory.domain;
    logger.log(
        Event::new(EventKind::Preflight, EventLabel::Run)
            .with_command(command.clone())
            .with_domain(domain),
    )?;

    if needs_machine_guard(options.machine_guard, profile)
        && host
            .machine_guards(host.own_cgroup()?.as_deref())?
            .is_empty()
    {
        let message = missing_guard_message(profile, host.in_container);
        logger.log(
            Event::new(EventKind::Refused, EventLabel::MachineGuard)
                .with_command(command)
                .with_domain(domain)
                .with_detail(message.clone()),
        )?;
        eprintln!("refusing to launch: {message}");
        return Ok(Outcome::Refused(Refusal::NoMachineGuard));
    }

    let readings = memory.read(host)?;
    let limits = options.policy.resolve(readings.size_bytes)?;
    if let Some(reason) = preflight(&limits, &readings) {
        let message = describe(reason, domain, &readings, &limits);
        logger.log(
            Event::new(EventKind::Refused, EventLabel::Reason(reason))
                .with_command(command)
                .with_domain(domain)
                .with_readings(readings)
                .with_detail(message.clone()),
        )?;
        eprintln!("refusing to launch: {message}");
        return Ok(Outcome::Refused(Refusal::Preflight(reason)));
    }

    let signals = ShutdownSignals::register()?;
    let group = ProcessGroup::spawn(&options.command)?;
    let mut supervisor = Supervisor {
        group,
        terminal: None,
        logger,
        command,
        memory,
        term_grace: options.term_grace,
    };
    match TerminalForeground::handoff_to_child(supervisor.group.pgid()) {
        Ok(terminal) => supervisor.terminal = terminal,
        Err(error) => return supervisor.stop(StopCause::GuardError(error)),
    }
    let launched = supervisor.event(EventKind::Launched);
    supervisor.logger.log_or_warn(launched);
    supervisor.watch(host, &limits, options.poll, &signals)
}

fn needs_machine_guard(requirement: MachineGuardRequirement, profile: Profile) -> bool {
    match requirement {
        MachineGuardRequirement::ProfileDefault => profile.requires_machine_guard(),
        MachineGuardRequirement::Required => true,
        MachineGuardRequirement::NotRequired => false,
    }
}

fn missing_guard_message(profile: Profile, in_container: bool) -> String {
    let mut message = format!(
        "profile {} requires a machine-wide guard (earlyoom, or systemd-oomd watching this job), and none is active",
        profile.name()
    );
    if in_container {
        message.push_str(
            "; inside a container, a guard of the host is not visible: pass --allow-no-machine-guard when the host runs one",
        );
    }
    message
}

/// Explains a stop or a refusal with the reading and the limit behind it.
fn describe(
    reason: StopReason,
    domain: &Domain,
    readings: &DomainReadings,
    limits: &StopLimits,
) -> String {
    match reason {
        StopReason::DomainFloor => format!(
            "{reason}: the memory domain ({domain}) has {} free, below --min-mem {}",
            format_bytes(readings.free_bytes),
            format_bytes(limits.min_free_bytes)
        ),
        StopReason::DomainPressure => format!(
            "{reason}: memory pressure of the domain ({domain}) is {}, at or above --domain-pressure {}",
            readings
                .full_avg10
                .map_or_else(|| "unknown".to_string(), |pressure| pressure.to_string()),
            limits.domain_pressure.at_least
        ),
        StopReason::JobLimit | StopReason::OutsideOomKill | StopReason::JobPressure => {
            reason.to_string()
        }
    }
}

/// Why the supervisor ends the job early.
enum StopCause {
    Pressure {
        reason: StopReason,
        readings: Readings,
        message: String,
    },
    Signal(i32),
    GuardError(anyhow::Error),
}

struct Supervisor {
    group: ProcessGroup,
    terminal: Option<TerminalForeground>,
    logger: EventLogger,
    command: Vec<String>,
    memory: MemoryDomain,
    term_grace: Duration,
}

impl Supervisor {
    fn watch(
        &mut self,
        host: &Host,
        limits: &StopLimits,
        poll: Duration,
        signals: &ShutdownSignals,
    ) -> Result<Outcome> {
        let started = Instant::now();
        let mut window = PressureWindow::default();
        loop {
            if let Some(signal) = signals.pending() {
                return self.stop(StopCause::Signal(signal));
            }
            match self.group.try_wait() {
                Ok(Some(status)) => return self.finish(status),
                Ok(None) => {}
                Err(error) => return self.stop(StopCause::GuardError(anyhow!(error))),
            }
            let readings = match self.memory.read(host) {
                Ok(domain) => Readings {
                    elapsed_ms: elapsed_ms(started),
                    domain,
                    job: None,
                },
                Err(error) => return self.stop(StopCause::GuardError(error)),
            };
            if let Decision::Stop(reason) = Decision::evaluate(limits, &readings, &mut window) {
                let message = describe(reason, &self.memory.domain, &readings.domain, limits);
                return self.stop(StopCause::Pressure {
                    reason,
                    readings,
                    message,
                });
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
            StopCause::Pressure {
                reason,
                readings,
                message,
            } => {
                let event = Event::new(EventKind::MemoryPressureKill, EventLabel::Reason(*reason))
                    .with_command(self.command.clone())
                    .with_pid(self.group.leader_pid())
                    .with_domain(&self.memory.domain)
                    .with_readings(readings.domain)
                    .with_detail(message.clone());
                self.logger.log_or_warn(event);
                eprintln!("memory pressure: {message}; terminating process group {pgid}");
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
            .with_domain(&self.memory.domain)
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
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
    use crate::limits::{Pressure, PressureLimit};

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn splits_command_strings() {
        let command = command_strings(&[OsString::from("vllm"), OsString::from("serve")]);
        assert_eq!(command, vec!["vllm".to_string(), "serve".to_string()]);
    }

    #[test]
    fn outcomes_map_to_exit_codes() {
        let reason = StopReason::DomainFloor;
        assert_eq!(Outcome::Exited(7).exit_code(), 7);
        assert_eq!(Outcome::Refused(Refusal::NoMachineGuard).exit_code(), 3);
        assert_eq!(Outcome::Refused(Refusal::Preflight(reason)).exit_code(), 4);
        assert_eq!(Outcome::Stopped(reason).exit_code(), 137);
        assert_eq!(Outcome::Interrupted(15).exit_code(), 143);
    }

    #[test]
    fn the_requirement_or_the_profile_decides_the_machine_guard() {
        use MachineGuardRequirement::{NotRequired, ProfileDefault, Required};
        assert!(needs_machine_guard(ProfileDefault, Profile::Vllm));
        assert!(!needs_machine_guard(ProfileDefault, Profile::Generic));
        assert!(needs_machine_guard(Required, Profile::Generic));
        assert!(!needs_machine_guard(NotRequired, Profile::Vllm));
    }

    #[test]
    fn the_missing_guard_message_explains_a_container() {
        let host_message = missing_guard_message(Profile::LlamaCpp, false);
        assert!(host_message.starts_with("profile llama-cpp requires a machine-wide guard"));
        assert!(!host_message.contains("container"));
        assert!(missing_guard_message(Profile::Vllm, true).contains("inside a container"));
    }

    #[test]
    fn descriptions_name_the_reading_and_the_limit() {
        let limits = StopLimits {
            min_free_bytes: 2 * GIB,
            job_pressure: PressureLimit {
                at_least: Pressure::from_hundredths(6000).unwrap(),
                lasting: Duration::from_secs(30),
            },
            domain_pressure: PressureLimit {
                at_least: Pressure::from_hundredths(2000).unwrap(),
                lasting: Duration::from_secs(10),
            },
        };
        let readings = DomainReadings {
            size_bytes: 16 * GIB,
            free_bytes: GIB,
            full_avg10: Pressure::from_hundredths(2550),
        };
        assert_eq!(
            describe(
                StopReason::DomainFloor,
                &Domain::Machine,
                &readings,
                &limits
            ),
            "domain_floor: the memory domain (machine) has 1.0GiB free, below --min-mem 2.0GiB"
        );
        assert_eq!(
            describe(
                StopReason::DomainPressure,
                &Domain::Machine,
                &readings,
                &limits
            ),
            "domain_pressure: memory pressure of the domain (machine) is 25.50%, at or above --domain-pressure 20.00%"
        );
        let no_psi = DomainReadings {
            full_avg10: None,
            ..readings
        };
        assert!(
            describe(
                StopReason::DomainPressure,
                &Domain::Machine,
                &no_psi,
                &limits
            )
            .contains("is unknown")
        );
        assert_eq!(
            describe(StopReason::JobLimit, &Domain::Machine, &readings, &limits),
            "job_limit"
        );
    }
}
