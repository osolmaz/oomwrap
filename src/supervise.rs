//! `oomwrap run`: the choice of the backend, preflight, the launch
//! handshake, the watch loop, job control, and one stop path.

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

use crate::backend::{
    Backend, BackendKind, LeaderExit, LeaderState, Manager, ProcessGroup, SIGNAL_EXIT_BASE, Scope,
    ScopeCgroup, ScopeLock, Unavailable, check, check_scope, systemd_run_args, unit_name,
};
use crate::config::{Profile, effective_profile};
use crate::domain::{Domain, MemoryDomain};
use crate::events::{
    EventKind, EventLog, EventLogger, EventReason, RunContext, TraceWriter, program_name,
};
use crate::host::Host;
use crate::job_limits::{JobLimitSettings, JobLimits, LaunchRoom, Resolution};
use crate::launch::{
    Answer, ControlSocket, Exec, JOB_OOM_SCORE_ADJ, REPORT_TIMEOUT, Report, Waited, Watch,
    check_report, exec_job_args, guard_exe,
};
use crate::limits::{GuardPolicy, StopLimits, format_bytes};
use crate::policy::{Decision, PressureWindow, StopReason, kill_reason, preflight, stagger_delay};
use crate::readings::{DomainReadings, Readings};
use crate::signal::stop_self;
use terminal::{TerminalForeground, restore_terminal};

/// The exit code of a job that the guard stopped.
const STOPPED_EXIT_CODE: i32 = 137;
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

/// The backend that `--backend` asks for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BackendRequest {
    /// `cgroup` when its checks pass, `poll` otherwise.
    Auto,
    Cgroup,
    Poll,
}

pub(crate) struct RunOptions {
    pub(crate) profile: Profile,
    pub(crate) policy: GuardPolicy,
    pub(crate) job_limits: JobLimitSettings,
    pub(crate) backend: BackendRequest,
    pub(crate) poll: Duration,
    pub(crate) term_grace: Duration,
    pub(crate) machine_guard: MachineGuardRequirement,
    pub(crate) event_log: EventLog,
    pub(crate) log_command: bool,
    pub(crate) record_readings: Option<PathBuf>,
    pub(crate) command: Vec<OsString>,
}

impl RunOptions {
    /// `auto` may start the job with `poll` when no job limit was asked for.
    fn may_fall_back(&self) -> bool {
        self.backend == BackendRequest::Auto && !self.job_limits.is_explicit()
    }
}

/// Why `oomwrap run` did not start the command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Refusal {
    /// A required machine-wide guard is not active.
    NoMachineGuard,
    /// The launch failed before the command started.
    LaunchFailed,
    /// The `cgroup` backend is required and not available.
    CgroupUnavailable,
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
            Self::Refused(
                Refusal::NoMachineGuard | Refusal::LaunchFailed | Refusal::CgroupUnavailable,
            ) => CANNOT_GUARD_EXIT_CODE,
            Self::Refused(Refusal::Preflight(_)) => PREFLIGHT_EXIT_CODE,
            Self::Stopped(_) => STOPPED_EXIT_CODE,
            Self::Interrupted(signal) => SIGNAL_EXIT_BASE + signal,
        }
    }
}

/// The backend that a run starts with.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Plan {
    Cgroup(Manager),
    /// `why` says why `auto` did not take `cgroup`.
    Poll {
        why: Option<String>,
    },
}

/// `--backend auto` takes `cgroup` when its checks pass. When they fail,
/// it takes `poll`, unless a job limit was asked for, which only a scope
/// can hold.
fn choose_plan(request: BackendRequest, explicit: bool, host: &Host) -> Result<Plan, Unavailable> {
    if request == BackendRequest::Poll {
        return Ok(Plan::Poll { why: None });
    }
    let manager = Manager::of(host);
    match check(host, manager) {
        Ok(()) => Ok(Plan::Cgroup(manager)),
        Err(unavailable) if request == BackendRequest::Auto && !explicit => Ok(Plan::Poll {
            why: Some(format!("auto took poll: {unavailable}")),
        }),
        Err(unavailable) => Err(unavailable),
    }
}

fn unavailable_message(unavailable: &Unavailable, request: BackendRequest) -> String {
    let need = if request == BackendRequest::Cgroup {
        "--backend cgroup needs it"
    } else {
        "--memory-max, --memory-high and --memory-swap-max need it"
    };
    format!("the cgroup backend is not available: {unavailable}; {need}")
}

/// One step of a run: go on with the value, or end with the outcome.
enum Step<T> {
    Next(T),
    Done(Outcome),
}

/// The domain after preflight, with the resolved stop conditions.
struct Prepared {
    memory: MemoryDomain,
    readings: DomainReadings,
    limits: StopLimits,
}

/// A job that runs, with what the watch needs.
struct Running {
    job: Backend,
    memory: MemoryDomain,
    limits: StopLimits,
}

/// How a launch ended.
enum Launched {
    Running(Box<Running>),
    /// The command did not start; the text says why.
    Failed(String),
    Ended(Outcome),
}

/// The result of the handshake with `exec-job`.
enum Handshake<T> {
    Started(ProcessGroup, T),
    Failed(String),
    Ended(Outcome),
}

/// What the check of a scope gives: the scope, and the domain above it.
struct ScopeStart {
    cgroup: ScopeCgroup,
    memory: MemoryDomain,
    limits: StopLimits,
    /// The process that keeps the scope until the guard is done.
    holder: Option<u32>,
}

pub(crate) fn run(options: RunOptions, host: &Host) -> Result<Outcome> {
    let command = command_strings(&options.command);
    let profile = effective_profile(options.profile, &command);
    let plan = choose_plan(options.backend, options.job_limits.is_explicit(), host);
    let job_cgroup = match &plan {
        Ok(Plan::Cgroup(manager)) => Some(manager.scope_parent()),
        _ => host.own_cgroup()?,
    };
    let memory = MemoryDomain::find_from(host, job_cgroup.as_deref())?;
    let context = RunContext {
        backend: match plan {
            Ok(Plan::Cgroup(_)) => BackendKind::Cgroup,
            _ => BackendKind::Poll,
        },
        domain: memory.domain.clone(),
        program: command.first().map(|program| program_name(program)),
        command: options.log_command.then(|| command.clone()),
        pid: None,
        unit: None,
        cgroup: None,
    };
    let mut run = Supervisor {
        terminal: None,
        logger: EventLogger::open(&options.event_log, host.state_dir())?,
        context,
        trace: TraceWriter::create(options.record_readings.as_deref())?,
        term_grace: options.term_grace,
    };
    run.logger.log(&run.context.event(EventKind::Preflight))?;
    let plan = match plan {
        Ok(plan) => plan,
        Err(unavailable) => {
            let message = unavailable_message(&unavailable, options.backend);
            return run.refuse(Refusal::CgroupUnavailable, &message);
        }
    };
    if needs_machine_guard(options.machine_guard, profile)
        && host.machine_guards(job_cgroup.as_deref())?.is_empty()
    {
        let message = missing_guard_message(profile, host.in_container);
        return run.refuse(Refusal::NoMachineGuard, &message);
    }
    let signals = ShutdownSignals::register()?;
    let started = match plan {
        Plan::Cgroup(manager) => {
            run.start_in_scope(&options, host, manager, profile, memory, &signals)?
        }
        Plan::Poll { why } => run.start_in_place(&options, host, why, memory, &signals)?,
    };
    match started {
        Step::Next(mut running) => run.watch(&mut running, host, options.poll, &signals),
        Step::Done(outcome) => Ok(outcome),
    }
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
    readings: &Readings,
    limits: &StopLimits,
) -> String {
    let unknown = || "unknown".to_string();
    match reason {
        StopReason::DomainFloor => format!(
            "{reason}: the memory domain ({domain}) has {} free, below --min-mem {}",
            format_bytes(readings.domain.free_bytes),
            format_bytes(limits.min_free_bytes)
        ),
        StopReason::DomainPressure => format!(
            "{reason}: memory pressure of the domain ({domain}) is {}, at or above --domain-pressure {}",
            readings
                .domain
                .full_avg10
                .map_or_else(unknown, |pressure| pressure.to_string()),
            limits.domain_pressure.at_least
        ),
        StopReason::JobPressure => format!(
            "{reason}: memory pressure of the job is {}, at or above --job-pressure {}",
            readings
                .job
                .and_then(|job| job.full_avg10)
                .map_or_else(unknown, |pressure| pressure.to_string()),
            limits.job_pressure.at_least
        ),
        StopReason::JobLimit | StopReason::OutsideOomKill => describe_kill(reason),
    }
}

/// Explains a kill by the kernel, which needs no reading.
fn describe_kill(reason: StopReason) -> String {
    if reason == StopReason::JobLimit {
        format!("{reason}: the job reached its memory.max, and the kernel killed in it")
    } else {
        format!("{reason}: an out-of-memory killer outside the job's limit killed a job process")
    }
}

/// Why the supervisor ends the job early.
enum StopCause {
    Pressure {
        reason: StopReason,
        readings: Option<Readings>,
        message: String,
    },
    Signal(i32),
    GuardError(anyhow::Error),
}

struct Supervisor {
    terminal: Option<TerminalForeground>,
    logger: EventLogger,
    context: RunContext,
    trace: TraceWriter,
    term_grace: Duration,
}

impl Supervisor {
    /// A refusal before the launch; a failed write of its event fails the run.
    fn refuse(&mut self, refusal: Refusal, message: &str) -> Result<Outcome> {
        let reason = match refusal {
            Refusal::NoMachineGuard => EventReason::NoMachineGuard,
            Refusal::LaunchFailed => EventReason::LaunchFailed,
            Refusal::CgroupUnavailable => EventReason::CgroupUnavailable,
            Refusal::Preflight(reason) => EventReason::Stop(reason),
        };
        let event = self
            .context
            .event(EventKind::Refused)
            .with_reason(reason)
            .with_detail(message);
        self.logger.log(&event)?;
        eprintln!("refusing to launch: {message}");
        Ok(Outcome::Refused(refusal))
    }

    fn refuse_preflight(
        &mut self,
        reason: StopReason,
        readings: DomainReadings,
        message: &str,
    ) -> Result<Outcome> {
        let event = self
            .context
            .event(EventKind::Refused)
            .with_reason(EventReason::Stop(reason))
            .with_readings(Readings {
                elapsed_ms: 0,
                domain: readings,
                job: None,
            })
            .with_detail(message);
        self.logger.log(&event)?;
        eprintln!("refusing to launch: {message}");
        Ok(Outcome::Refused(Refusal::Preflight(reason)))
    }

    /// Reads the domain, resolves the stop conditions, and refuses the
    /// launch when a domain condition already holds.
    fn prepare(
        &mut self,
        host: &Host,
        policy: &GuardPolicy,
        memory: MemoryDomain,
    ) -> Result<Step<Prepared>> {
        let readings = memory.read(host)?;
        let limits = policy.resolve(readings.size_bytes)?;
        if let Some(reason) = preflight(&limits, &readings) {
            let at_launch = Readings {
                elapsed_ms: 0,
                domain: readings,
                job: None,
            };
            let message = describe(reason, &memory.domain, &at_launch, &limits);
            return self
                .refuse_preflight(reason, readings, &message)
                .map(Step::Done);
        }
        Ok(Step::Next(Prepared {
            memory,
            readings,
            limits,
        }))
    }

    /// Starts the job in a new scope. When the launch fails before the
    /// command, `auto` starts it once in place with `poll`.
    fn start_in_scope(
        &mut self,
        options: &RunOptions,
        host: &Host,
        manager: Manager,
        profile: Profile,
        memory: MemoryDomain,
        signals: &ShutdownSignals,
    ) -> Result<Step<Running>> {
        let prepared = match self.prepare(host, &options.policy, memory)? {
            Step::Next(prepared) => prepared,
            Step::Done(outcome) => return Ok(Step::Done(outcome)),
        };
        let room = LaunchRoom {
            domain_bytes: prepared.readings.size_bytes,
            free_bytes: prepared.readings.free_bytes,
            min_free_bytes: prepared.limits.min_free_bytes,
            above_scope_max: prepared.memory.lowest_max(),
            page_size: host.page_size,
        };
        let job_limits = match options.job_limits.resolve(&room)? {
            Resolution::Limits(limits) => limits,
            Resolution::NoRoom => {
                let message = format!(
                    "domain_floor: the memory domain ({}) has {} free, which leaves the job no room above --min-mem {}",
                    prepared.memory.domain,
                    format_bytes(prepared.readings.free_bytes),
                    format_bytes(prepared.limits.min_free_bytes)
                );
                return self
                    .refuse_preflight(StopReason::DomainFloor, prepared.readings, &message)
                    .map(Step::Done);
            }
        };
        let unit = unit_name(&profile.name());
        match self.launch_scope(host, options, manager, &unit, &job_limits, signals)? {
            Launched::Running(running) => {
                let detail = format!("cgroup backend: the scope {unit} holds the job limits");
                self.log_launched(&running.limits, Some(job_limits), detail);
                Ok(Step::Next(*running))
            }
            Launched::Failed(cause) if options.may_fall_back() => {
                eprintln!(
                    "warning: the cgroup launch failed: {cause}; starting the command with the poll backend"
                );
                let why = format!("auto took poll: the cgroup launch failed: {cause}");
                let memory = MemoryDomain::find(host)?;
                self.start_in_place(options, host, Some(why), memory, signals)
            }
            Launched::Failed(cause) => self.refuse_launch(&cause).map(Step::Done),
            Launched::Ended(outcome) => Ok(Step::Done(outcome)),
        }
    }

    fn launch_scope(
        &mut self,
        host: &Host,
        options: &RunOptions,
        manager: Manager,
        unit: &str,
        job_limits: &JobLimits,
        signals: &ShutdownSignals,
    ) -> Result<Launched> {
        self.context.unit = Some(unit.to_string());
        let lock = match manager.lock_dir(host) {
            Some(dir) => ScopeLock::acquire(&dir, unit),
            None => Err(anyhow!("no folder for the lock")),
        };
        let lock = match lock {
            Ok(lock) => lock,
            Err(error) => return Ok(Launched::Failed(format!("{error:#}"))),
        };
        let socket = ControlSocket::bind(host.control_parent())?;
        let args = systemd_run_args(
            manager,
            unit,
            job_limits,
            &guard_exe(),
            &exec_job_args(socket.path(), Some(lock.path()), &options.command),
        );
        let group = match ProcessGroup::spawn(host.systemd_run.as_os_str(), &args) {
            Ok(group) => group,
            Err(error) => {
                let cause = format!("cannot run {}: {error}", host.systemd_run.display());
                return Ok(Launched::Failed(cause));
            }
        };
        self.context.pid = Some(group.leader_pid());
        let check = |report: &Report| {
            let cgroup = check_scope(host, report.cgroup.as_deref(), unit, job_limits)?;
            let fault = |error: anyhow::Error| format!("{error:#}");
            let memory = MemoryDomain::find_from(host, Some(&cgroup.parent())).map_err(fault)?;
            let readings = memory.read(host).map_err(fault)?;
            let limits = options.policy.resolve(readings.size_bytes).map_err(fault)?;
            Ok(ScopeStart {
                cgroup,
                memory,
                limits,
                holder: report.holder_pid,
            })
        };
        match self.handshake(socket, group, signals, check)? {
            Handshake::Started(group, start) => {
                self.context.cgroup = Some(start.cgroup.path.clone());
                self.context.domain = start.memory.domain.clone();
                Ok(Launched::Running(Box::new(Running {
                    job: Backend::Cgroup(Scope::new(group, start.cgroup, start.holder, lock)),
                    memory: start.memory,
                    limits: start.limits,
                })))
            }
            Handshake::Failed(cause) => Ok(Launched::Failed(cause)),
            Handshake::Ended(outcome) => Ok(Launched::Ended(outcome)),
        }
    }

    /// Starts the job where oomwrap runs, with the `poll` backend.
    fn start_in_place(
        &mut self,
        options: &RunOptions,
        host: &Host,
        why: Option<String>,
        memory: MemoryDomain,
        signals: &ShutdownSignals,
    ) -> Result<Step<Running>> {
        self.context.backend = BackendKind::Poll;
        self.context.domain = memory.domain.clone();
        self.context.unit = None;
        self.context.pid = None;
        let prepared = match self.prepare(host, &options.policy, memory)? {
            Step::Next(prepared) => prepared,
            Step::Done(outcome) => return Ok(Step::Done(outcome)),
        };
        let socket = ControlSocket::bind(host.control_parent())?;
        let exe = guard_exe();
        let group = ProcessGroup::spawn(
            exe.as_os_str(),
            &exec_job_args(socket.path(), None, &options.command),
        )
        .context("failed to start the launch")?;
        self.context.pid = Some(group.leader_pid());
        match self.handshake(socket, group, signals, |_| Ok(()))? {
            Handshake::Started(group, ()) => {
                let no_limit =
                    "poll backend: no job limit holds; the domain's stop conditions apply";
                let detail =
                    why.map_or_else(|| no_limit.to_string(), |why| format!("{why}; {no_limit}"));
                self.log_launched(&prepared.limits, None, detail);
                Ok(Step::Next(Running {
                    job: Backend::Poll(group),
                    memory: prepared.memory,
                    limits: prepared.limits,
                }))
            }
            Handshake::Failed(cause) => self.refuse_launch(&cause).map(Step::Done),
            Handshake::Ended(outcome) => Ok(Step::Done(outcome)),
        }
    }

    fn log_launched(&mut self, limits: &StopLimits, job_limits: Option<JobLimits>, detail: String) {
        let mut event = self
            .context
            .event(EventKind::Launched)
            .with_limits(*limits)
            .with_oom_score_adj(JOB_OOM_SCORE_ADJ)
            .with_detail(detail);
        if let Some(job_limits) = job_limits {
            event = event.with_job_limits(job_limits);
        }
        self.logger.log_or_warn(&event);
    }

    /// The handshake with `exec-job`: wait for its report, check it, hand
    /// over the terminal, answer `go`, and wait for the exec.
    fn handshake<T>(
        &mut self,
        socket: ControlSocket,
        mut group: ProcessGroup,
        signals: &ShutdownSignals,
        check: impl FnOnce(&Report) -> Result<T, String>,
    ) -> Result<Handshake<T>> {
        let waited = socket.wait_for_report(REPORT_TIMEOUT, || watch_launch(signals, &mut group));
        let (mut control, report) = match waited {
            Waited::Reported(control, report) => (control, report),
            Waited::Failed(failure) => {
                group.kill();
                return Ok(Handshake::Failed(failure.to_string()));
            }
            Waited::Interrupted(signal) => {
                group.kill();
                return Ok(Handshake::Ended(self.interrupted_before_start(signal)));
            }
        };
        let checked = check_report(&report, group.leader_pid())
            .map_err(|fault| fault.to_string())
            .and_then(|()| check(&report));
        let value = match checked {
            Ok(value) => value,
            Err(cause) => {
                let _ = control.answer(Answer::Abort);
                group.kill();
                return Ok(Handshake::Failed(cause));
            }
        };
        warn_about_score(&report);
        match TerminalForeground::handoff_to_job(group.pgid()) {
            Ok(terminal) => self.terminal = terminal,
            Err(error) => {
                let _ = control.answer(Answer::Abort);
                group.kill();
                return Err(error);
            }
        }
        if let Err(error) = control.answer(Answer::Go) {
            restore_terminal(&mut self.terminal);
            group.kill();
            return Err(anyhow!(error).context("failed to start the command"));
        }
        match control.wait_for_exec(REPORT_TIMEOUT) {
            Exec::Started => Ok(Handshake::Started(group, value)),
            Exec::Failed(failure) => {
                restore_terminal(&mut self.terminal);
                group.reap();
                let event = self
                    .context
                    .event(EventKind::ExecFailed)
                    .with_exit(failure.exit_code())
                    .with_detail(failure.message.clone());
                self.logger.log_or_warn(&event);
                eprintln!("oomwrap: {}", failure.message);
                Ok(Handshake::Ended(Outcome::NotRun(failure.exit_code())))
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
        running: &mut Running,
        host: &Host,
        poll: Duration,
        signals: &ShutdownSignals,
    ) -> Result<Outcome> {
        let started = Instant::now();
        let mut window = PressureWindow::default();
        let mut staggered = false;
        let job = &mut running.job;
        let mut state = job.group().poll();
        loop {
            if let Some(signal) = signals.pending() {
                return self.stop(job, StopCause::Signal(signal));
            }
            match state {
                Ok(LeaderState::Exited(exit)) => return self.finish(job, exit),
                Ok(LeaderState::Stopped) => self.follow_job_stop(job),
                Ok(LeaderState::Running) => {}
                Err(error) => return self.stop(job, StopCause::GuardError(anyhow!(error))),
            }
            let readings = match read(job, &running.memory, host, started) {
                Ok(readings) => readings,
                // The job may have ended between the look and the reading,
                // and systemd may have removed its scope.
                Err(error) => match job.group().poll() {
                    Ok(LeaderState::Exited(exit)) => return self.finish(job, exit),
                    _ => return self.stop(job, StopCause::GuardError(error)),
                },
            };
            let decision = Decision::evaluate(&running.limits, &readings, &mut window);
            self.trace.record(&readings, decision);
            let mut wait = poll;
            match decision {
                // Every guard of the domain sees the same reading: wait,
                // read again, and stop only if the condition still holds.
                Decision::Stop(reason) if reason.is_domain() && !staggered => {
                    staggered = true;
                    let job_bytes = match readings.job {
                        Some(job) => Some(job.current_bytes),
                        None => job.group().rss_bytes(),
                    };
                    wait = stagger_delay(job_bytes, &readings.domain);
                }
                Decision::Stop(reason) => {
                    let message =
                        describe(reason, &running.memory.domain, &readings, &running.limits);
                    return self.stop(
                        job,
                        StopCause::Pressure {
                            reason,
                            readings: Some(readings),
                            message,
                        },
                    );
                }
                Decision::Continue => staggered = false,
            }
            state = sleep_until_poll_signal_or_change(wait, signals, job.group());
        }
    }

    /// The job stopped, as on Ctrl-Z. With the terminal handed over, oomwrap
    /// takes it back and stops itself, so the shell sees a stopped job; when
    /// the shell continues oomwrap, the job gets the terminal again and
    /// continues too. Without a terminal, the job stays stopped until
    /// whoever stopped it continues it.
    fn follow_job_stop(&mut self, job: &mut Backend) {
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
        job.group().resume();
    }

    /// The command's main process exited: end what remains of the job. A
    /// job that the kernel killed at its limit ends as `job_limit`, not with
    /// the command's own exit.
    fn finish(&mut self, job: &mut Backend, exit: LeaderExit) -> Result<Outcome> {
        if let Some(reason) = job.final_events().and_then(|events| kill_reason(&events)) {
            let message = describe_kill(reason);
            return self.stop(
                job,
                StopCause::Pressure {
                    reason,
                    readings: None,
                    message,
                },
            );
        }
        let code = exit.code();
        let mut event = self.context.event(EventKind::Exited).with_exit(code);
        if let LeaderExit::Signal(signal) = exit {
            event = event.with_signal(signal);
        }
        self.logger.log_or_warn(&event);
        if job.has_members() {
            let cleanup = self
                .context
                .event(EventKind::Cleanup)
                .with_detail("the command exited while processes of the job still ran");
            self.logger.log_or_warn(&cleanup);
        }
        restore_terminal(&mut self.terminal);
        job.terminate(self.term_grace)?;
        Ok(Outcome::Exited(code))
    }

    /// The one path that ends a running job before its command ends.
    fn stop(&mut self, job: &mut Backend, cause: StopCause) -> Result<Outcome> {
        restore_terminal(&mut self.terminal);
        let target = describe_job(job);
        let event = match &cause {
            StopCause::Pressure {
                reason,
                readings,
                message,
            } => {
                eprintln!("stopping the job: {message}; terminating {target}");
                let mut event = self
                    .context
                    .event(EventKind::Stopped)
                    .with_reason(EventReason::Stop(*reason))
                    .with_exit(STOPPED_EXIT_CODE)
                    .with_detail(message.clone());
                if let Some(readings) = readings {
                    event = event.with_readings(*readings);
                }
                event
            }
            StopCause::Signal(signal) => {
                eprintln!("received signal {signal}; terminating {target}");
                self.context
                    .event(EventKind::Interrupted)
                    .with_signal(*signal)
                    .with_exit(SIGNAL_EXIT_BASE + signal)
            }
            StopCause::GuardError(error) => {
                eprintln!("guard error after launch: {error:#}; terminating {target}");
                self.context
                    .event(EventKind::GuardError)
                    .with_detail(format!("{error:#}"))
            }
        };
        self.logger.log_or_warn(&event);
        let terminated = match &cause {
            // The job has already lost a process: no grace.
            StopCause::Pressure { reason, .. } if kills_at_once(*reason) => {
                job.kill();
                Ok(())
            }
            _ => job.terminate(self.term_grace),
        };
        job.group().reap();
        match cause {
            StopCause::Pressure { reason, .. } => terminated.map(|()| Outcome::Stopped(reason)),
            StopCause::Signal(signal) => terminated.map(|()| Outcome::Interrupted(signal)),
            StopCause::GuardError(error) => Err(error),
        }
    }
}

/// After a kill at the job's limit or from outside, the job ends at once.
fn kills_at_once(reason: StopReason) -> bool {
    matches!(reason, StopReason::JobLimit | StopReason::OutsideOomKill)
}

fn describe_job(job: &Backend) -> String {
    match job {
        Backend::Cgroup(scope) => format!("scope {}", scope.cgroup().path),
        Backend::Poll(group) => format!("process group {}", group.pgid()),
    }
}

fn read(job: &Backend, memory: &MemoryDomain, host: &Host, started: Instant) -> Result<Readings> {
    Ok(Readings {
        elapsed_ms: elapsed_ms(started),
        domain: memory.read(host)?,
        job: job.read_job()?,
    })
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
    if let Some(error) = &report.holder_error {
        eprintln!("warning: {error}; a kill just before the job ends can go unseen");
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
    use crate::host::tests::FakeHost;
    use crate::limits::{Pressure, PressureLimit};
    use crate::readings::{JobReadings, MemoryEvents};

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
        assert_eq!(Outcome::Refused(Refusal::CgroupUnavailable).exit_code(), 3);
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
    fn auto_takes_poll_unless_a_job_limit_needs_a_scope() {
        let fake = FakeHost::new();
        let host = fake.host();
        assert_eq!(
            choose_plan(BackendRequest::Poll, true, &host),
            Ok(Plan::Poll { why: None })
        );
        let Ok(Plan::Poll { why: Some(why) }) = choose_plan(BackendRequest::Auto, false, &host)
        else {
            panic!("auto must fall back to poll");
        };
        assert!(why.starts_with("auto took poll: the host does not use cgroup v2 alone"));
        assert_eq!(
            choose_plan(BackendRequest::Auto, true, &host),
            Err(Unavailable::NotCgroupV2Only)
        );
        assert_eq!(
            choose_plan(BackendRequest::Cgroup, false, &host),
            Err(Unavailable::NotCgroupV2Only)
        );
        fake.proc_file(
            "self/mountinfo",
            "1 1 0:1 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n",
        );
        fake.own_cgroup("/user.slice/user-1000.slice/user@1000.service/app.slice/a.scope");
        fake.cgroup_file(
            "/user.slice/user-1000.slice/user@1000.service/app.slice/a.scope",
            "cgroup.kill",
            "",
        );
        let mut host = fake.host();
        host.runtime_dir = Some(PathBuf::from("/run/user/1000"));
        assert_eq!(
            choose_plan(BackendRequest::Auto, true, &host),
            Ok(Plan::Cgroup(Manager::User { uid: 1000 }))
        );
    }

    #[test]
    fn the_unavailable_message_names_what_needs_the_scope() {
        let unavailable = Unavailable::NoCgroupKill;
        assert_eq!(
            unavailable_message(&unavailable, BackendRequest::Cgroup),
            "the cgroup backend is not available: the kernel has no cgroup.kill (Linux 5.14 or later); --backend cgroup needs it"
        );
        assert!(
            unavailable_message(&unavailable, BackendRequest::Auto)
                .ends_with("--memory-max, --memory-high and --memory-swap-max need it")
        );
    }

    #[test]
    fn only_kernel_kills_end_the_job_at_once() {
        assert!(kills_at_once(StopReason::JobLimit));
        assert!(kills_at_once(StopReason::OutsideOomKill));
        assert!(!kills_at_once(StopReason::JobPressure));
        assert!(!kills_at_once(StopReason::DomainFloor));
        assert!(!kills_at_once(StopReason::DomainPressure));
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
        let readings = Readings {
            elapsed_ms: 0,
            domain: DomainReadings {
                size_bytes: 16 * GIB,
                free_bytes: GIB,
                full_avg10: Pressure::from_hundredths(2550),
            },
            job: Some(JobReadings {
                current_bytes: GIB,
                swap_current_bytes: 0,
                full_avg10: Pressure::from_hundredths(7000),
                events: MemoryEvents::default(),
            }),
        };
        let say =
            |reason, readings: &Readings| describe(reason, &Domain::Machine, readings, &limits);
        assert_eq!(
            say(StopReason::DomainFloor, &readings),
            "domain_floor: the memory domain (machine) has 1.0GiB free, below --min-mem 2.0GiB"
        );
        assert_eq!(
            say(StopReason::DomainPressure, &readings),
            "domain_pressure: memory pressure of the domain (machine) is 25.50%, at or above --domain-pressure 20.00%"
        );
        assert_eq!(
            say(StopReason::JobPressure, &readings),
            "job_pressure: memory pressure of the job is 70.00%, at or above --job-pressure 60.00%"
        );
        let mut no_psi = readings;
        no_psi.domain.full_avg10 = None;
        no_psi.job = None;
        assert!(say(StopReason::DomainPressure, &no_psi).contains("is unknown"));
        assert!(say(StopReason::JobPressure, &no_psi).contains("is unknown"));
        assert!(
            say(StopReason::JobLimit, &readings)
                .starts_with("job_limit: the job reached its memory.max")
        );
        assert!(
            say(StopReason::OutsideOomKill, &readings)
                .starts_with("outside_oom_kill: an out-of-memory killer")
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
