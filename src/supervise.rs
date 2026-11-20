//! `oomwrap run`: preflight, the launch handshake, the watch loop, job
//! control, and one stop path.

mod terminal;

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};

use crate::backend::{BackendKind, LeaderExit, LeaderState, ProcessGroup};
use crate::config::{Profile, effective_profile};
use crate::domain::{Domain, MemoryDomain};
use crate::events::{
    EventKind, EventLog, EventLogger, EventReason, RunContext, TraceWriter, program_name,
};
use crate::host::Host;
use crate::launch::{
    Answer, ControlSocket, Exec, JOB_OOM_SCORE_ADJ, REPORT_TIMEOUT, Report, Waited, Watch,
    check_report, exec_job_args, guard_exe,
};
use crate::limits::{GuardPolicy, StopLimits, format_bytes};
use crate::policy::{Decision, PressureWindow, StopReason, preflight};
use crate::readings::{DomainReadings, Readings};
use crate::signal::stop_self;
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
    pub(crate) event_log: EventLog,
    pub(crate) log_command: bool,
    pub(crate) record_readings: Option<PathBuf>,
    pub(crate) command: Vec<OsString>,
}

/// Why `oomwrap run` did not start the command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Refusal {
    /// A required machine-wide guard is not active.
    NoMachineGuard,
    /// The launch failed before the command started.
    LaunchFailed,
    /// A domain condition already holds.
    Preflight(StopReason),
}

/// How a run ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Outcome {
    /// The command ended on its own with this code.
    Exited(i32),
    Refused(Refusal),
    /// The command could not be executed: 127 when it was not found, 126
    /// when it cannot run, as a shell reports it.
    NotRun(i32),
    /// The guard stopped the job.
    Stopped(StopReason),
    /// oomwrap received this signal number and ended the job.
    Interrupted(i32),
}

impl Outcome {
    pub(crate) fn exit_code(self) -> i32 {
        match self {
            Self::Exited(code) | Self::NotRun(code) => code,
            Self::Refused(Refusal::NoMachineGuard | Refusal::LaunchFailed) => {
                CANNOT_GUARD_EXIT_CODE
            }
            Self::Refused(Refusal::Preflight(_)) => PREFLIGHT_EXIT_CODE,
            Self::Stopped(_) => STOPPED_EXIT_CODE,
            Self::Interrupted(signal) => SIGNAL_EXIT_BASE + signal,
        }
    }
}

pub(crate) fn run(options: RunOptions, host: &Host) -> Result<Outcome> {
    let command = command_strings(&options.command);
    let profile = effective_profile(options.profile, &command);
    let memory = MemoryDomain::find(host)?;
    let mut context = RunContext {
        backend: BackendKind::Poll,
        domain: memory.domain.clone(),
        program: command.first().map(|program| program_name(program)),
        command: options.log_command.then(|| command.clone()),
        pid: None,
    };
    let mut logger = EventLogger::open(&options.event_log, host.state_dir())?;
    let trace = TraceWriter::create(options.record_readings.as_deref())?;
    logger.log(&context.event(EventKind::Preflight))?;

    if needs_machine_guard(options.machine_guard, profile)
        && host
            .machine_guards(host.own_cgroup()?.as_deref())?
            .is_empty()
    {
        let message = missing_guard_message(profile, host.in_container);
        logger.log(
            &context
                .event(EventKind::Refused)
                .with_reason(EventReason::NoMachineGuard)
                .with_detail(message.clone()),
        )?;
        eprintln!("refusing to launch: {message}");
        return Ok(Outcome::Refused(Refusal::NoMachineGuard));
    }

    let domain_readings = memory.read(host)?;
    let limits = options.policy.resolve(domain_readings.size_bytes)?;
    if let Some(reason) = preflight(&limits, &domain_readings) {
        let message = describe(reason, &memory.domain, &domain_readings, &limits);
        logger.log(
            &context
                .event(EventKind::Refused)
                .with_reason(EventReason::Stop(reason))
                .with_readings(Readings {
                    elapsed_ms: 0,
                    domain: domain_readings,
                    job: None,
                })
                .with_detail(message.clone()),
        )?;
        eprintln!("refusing to launch: {message}");
        return Ok(Outcome::Refused(Refusal::Preflight(reason)));
    }

    let signals = ShutdownSignals::register()?;
    let socket = ControlSocket::bind(host.control_parent())?;
    let exe = guard_exe();
    let group = ProcessGroup::spawn(
        exe.as_os_str(),
        &exec_job_args(socket.path(), &options.command),
    )
    .context("failed to start the launch")?;
    context.pid = Some(group.leader_pid());
    let mut supervisor = Supervisor {
        group,
        terminal: None,
        logger,
        context,
        memory,
        trace,
        term_grace: options.term_grace,
    };
    if let Some(outcome) = supervisor.launch(socket, &signals)? {
        return Ok(outcome);
    }
    let launched = supervisor
        .context
        .event(EventKind::Launched)
        .with_limits(limits)
        .with_oom_score_adj(JOB_OOM_SCORE_ADJ)
        .with_detail("poll backend: no job limit holds; the domain's stop conditions apply");
    supervisor.logger.log_or_warn(&launched);
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
    context: RunContext,
    memory: MemoryDomain,
    trace: TraceWriter,
    term_grace: Duration,
}

impl Supervisor {
    /// The handshake with `exec-job`. Returns the outcome when the command
    /// did not start, and `None` when it runs.
    fn launch(
        &mut self,
        socket: ControlSocket,
        signals: &ShutdownSignals,
    ) -> Result<Option<Outcome>> {
        let group = &mut self.group;
        let waited = socket.wait_for_report(REPORT_TIMEOUT, || watch_launch(signals, group));
        let (mut control, report) = match waited {
            Waited::Reported(control, report) => (control, report),
            Waited::Failed(failure) => {
                self.group.kill();
                return self.refuse_launch(&failure.to_string()).map(Some);
            }
            Waited::Interrupted(signal) => {
                self.group.kill();
                return Ok(Some(self.interrupted_before_start(signal)));
            }
        };
        if let Err(fault) = check_report(&report, self.group.leader_pid()) {
            let _ = control.answer(Answer::Abort);
            self.group.kill();
            return self.refuse_launch(&fault.to_string()).map(Some);
        }
        warn_about_score(&report);
        match TerminalForeground::handoff_to_job(self.group.pgid()) {
            Ok(terminal) => self.terminal = terminal,
            Err(error) => {
                let _ = control.answer(Answer::Abort);
                self.group.kill();
                return Err(error);
            }
        }
        if let Err(error) = control.answer(Answer::Go) {
            restore_terminal(&mut self.terminal);
            self.group.kill();
            return Err(anyhow!(error).context("failed to start the command"));
        }
        match control.wait_for_exec(REPORT_TIMEOUT) {
            Exec::Started => Ok(None),
            Exec::Failed(failure) => {
                restore_terminal(&mut self.terminal);
                self.group.reap();
                let event = self
                    .context
                    .event(EventKind::ExecFailed)
                    .with_exit(failure.exit_code())
                    .with_detail(failure.message.clone());
                self.logger.log_or_warn(&event);
                eprintln!("oomwrap: {}", failure.message);
                Ok(Some(Outcome::NotRun(failure.exit_code())))
            }
        }
    }

    fn refuse_launch(&mut self, cause: &str) -> Result<Outcome> {
        let event = self
            .context
            .event(EventKind::Refused)
            .with_reason(EventReason::LaunchFailed)
            .with_detail(cause);
        self.logger.log_or_warn(&event);
        eprintln!("refusing to launch: {cause}; the command did not start");
        Ok(Outcome::Refused(Refusal::LaunchFailed))
    }

    fn interrupted_before_start(&mut self, signal: i32) -> Outcome {
        let event = self
            .context
            .event(EventKind::Interrupted)
            .with_signal(signal)
            .with_detail("received the signal during the launch; the command did not start");
        self.logger.log_or_warn(&event);
        eprintln!("received signal {signal} during the launch; the command did not start");
        Outcome::Interrupted(signal)
    }

    fn watch(
        &mut self,
        host: &Host,
        limits: &StopLimits,
        poll: Duration,
        signals: &ShutdownSignals,
    ) -> Result<Outcome> {
        let started = Instant::now();
        let mut window = PressureWindow::default();
        let mut state = self.group.poll();
        loop {
            if let Some(signal) = signals.pending() {
                return self.stop(StopCause::Signal(signal));
            }
            match state {
                Ok(LeaderState::Exited(exit)) => return self.finish(exit),
                Ok(LeaderState::Stopped) => self.follow_job_stop(),
                Ok(LeaderState::Running) => {}
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
            let decision = Decision::evaluate(limits, &readings, &mut window);
            self.trace.record(&readings, decision);
            if let Decision::Stop(reason) = decision {
                let message = describe(reason, &self.memory.domain, &readings.domain, limits);
                return self.stop(StopCause::Pressure {
                    reason,
                    readings,
                    message,
                });
            }
            state = sleep_until_poll_signal_or_change(poll, signals, &mut self.group);
        }
    }

    /// The job stopped, as on Ctrl-Z. With the terminal handed over, oomwrap
    /// takes it back and stops itself, so the shell sees a stopped job; when
    /// the shell continues oomwrap, the job gets the terminal again and
    /// continues too. Without a terminal, the job stays stopped until
    /// whoever stopped it continues it.
    fn follow_job_stop(&mut self) {
        let Some(terminal) = &mut self.terminal else {
            return;
        };
        if let Err(error) = terminal.take_back() {
            eprintln!("warning: {error:#}");
        }
        if let Err(error) = stop_self() {
            eprintln!("warning: failed to stop oomwrap with the job: {error}");
        }
        if let Err(error) = terminal.give_to_job() {
            eprintln!("warning: {error:#}");
        }
        self.group.resume();
    }

    /// The command's main process exited: end what remains of its group.
    fn finish(&mut self, exit: LeaderExit) -> Result<Outcome> {
        let code = exit.code();
        let mut event = self.context.event(EventKind::Exited).with_exit(code);
        if let LeaderExit::Signal(signal) = exit {
            event = event.with_signal(signal);
        }
        self.logger.log_or_warn(&event);
        if self.group.has_members() {
            let cleanup = self.context.event(EventKind::Cleanup).with_detail(
                "the command exited while its process group still had running members",
            );
            self.logger.log_or_warn(&cleanup);
        }
        restore_terminal(&mut self.terminal);
        self.group.terminate(self.term_grace)?;
        Ok(Outcome::Exited(code))
    }

    /// The one path that ends a running job before its command ends.
    fn stop(&mut self, cause: StopCause) -> Result<Outcome> {
        restore_terminal(&mut self.terminal);
        let pgid = self.group.pgid();
        let event = match &cause {
            StopCause::Pressure {
                reason,
                readings,
                message,
            } => {
                eprintln!("memory pressure: {message}; terminating process group {pgid}");
                self.context
                    .event(EventKind::Stopped)
                    .with_reason(EventReason::Stop(*reason))
                    .with_readings(*readings)
                    .with_exit(STOPPED_EXIT_CODE)
                    .with_detail(message.clone())
            }
            StopCause::Signal(signal) => {
                eprintln!("received signal {signal}; terminating process group {pgid}");
                self.context
                    .event(EventKind::Interrupted)
                    .with_signal(*signal)
                    .with_exit(SIGNAL_EXIT_BASE + signal)
            }
            StopCause::GuardError(error) => {
                eprintln!("guard error after launch: {error:#}; terminating process group {pgid}");
                self.context
                    .event(EventKind::GuardError)
                    .with_detail(format!("{error:#}"))
            }
        };
        self.logger.log_or_warn(&event);
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
}

/// What the guard sees of the launch while it waits for the report.
fn watch_launch(signals: &ShutdownSignals, group: &mut ProcessGroup) -> Watch {
    if let Some(signal) = signals.pending() {
        return Watch::Interrupted(signal);
    }
    match group.poll() {
        Ok(LeaderState::Running | LeaderState::Stopped) => Watch::Continue,
        Ok(LeaderState::Exited(_)) | Err(_) => Watch::LaunchEnded,
    }
}

fn warn_about_score(report: &Report) {
    if let Some(error) = &report.oom_score_adj_error {
        eprintln!(
            "warning: the job could not raise its oom_score_adj to {JOB_OOM_SCORE_ADJ}: {error}"
        );
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

/// Sleeps one poll interval, and returns early when a signal arrives or the
/// leader stops or exits.
// A mutant here only changes how often the loop wakes up; no test can see
// that without depending on timing.
#[cfg_attr(test, mutants::skip)]
fn sleep_until_poll_signal_or_change(
    poll: Duration,
    signals: &ShutdownSignals,
    group: &mut ProcessGroup,
) -> io::Result<LeaderState> {
    let started = Instant::now();
    while signals.pending().is_none() {
        let state = group.poll()?;
        if state != LeaderState::Running {
            return Ok(state);
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
    Ok(LeaderState::Running)
}

pub(crate) fn command_strings(command: &[OsString]) -> Vec<String> {
    command
        .iter()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect()
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
        assert_eq!(Outcome::NotRun(127).exit_code(), 127);
        assert_eq!(Outcome::Refused(Refusal::NoMachineGuard).exit_code(), 3);
        assert_eq!(Outcome::Refused(Refusal::LaunchFailed).exit_code(), 3);
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

    #[test]
    fn a_signal_or_the_end_of_the_launch_ends_the_wait() {
        let signals = ShutdownSignals {
            received: Arc::new(AtomicUsize::new(0)),
        };
        let mut group = ProcessGroup::spawn("true".as_ref(), &[]).unwrap();
        group.wait().unwrap();
        assert_eq!(watch_launch(&signals, &mut group), Watch::LaunchEnded);
        signals.received.store(2, Ordering::SeqCst);
        assert_eq!(watch_launch(&signals, &mut group), Watch::Interrupted(2));
        let mut sleeper = ProcessGroup::spawn("sleep".as_ref(), &["5".into()]).unwrap();
        signals.received.store(0, Ordering::SeqCst);
        let running = watch_launch(&signals, &mut sleeper);
        sleeper.kill();
        assert_eq!(running, Watch::Continue);
    }
}
