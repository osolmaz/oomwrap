//! How the guarded command runs: in its own process group, and with the
//! `cgroup` backend also in its own transient scope.

mod checks;
mod scope;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use nix::errno::Errno;
use nix::sys::signal::Signal;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use serde::Serialize;

use crate::host::read_rss_bytes;
use crate::readings::{JobReadings, MemoryEvents, parse_process_group_id};
use crate::signal::{ProcessId, group_exists, signal_group, signal_process};
pub(crate) use checks::{
    Manager, ScopeCgroup, Unavailable, check, check_scope, systemd_run_args, unit_name,
};
pub(crate) use scope::{Scope, ScopeLock};

/// How long a new process group may take to appear, and to empty after a signal.
pub(crate) const PROCESS_GROUP_SETTLE: Duration = Duration::from_millis(100);
const PROCESS_GROUP_KILL_WAIT: Duration = Duration::from_secs(1);
/// The base of the exit code of a process that ended on a signal.
pub(crate) const SIGNAL_EXIT_BASE: i32 = 128;

/// The mechanism that holds and stops a job, as events name it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BackendKind {
    /// A transient scope with kernel limits; the stop goes through `cgroup.kill`.
    Cgroup,
    /// The process-group guard: oomwrap polls the domain and signals the group.
    Poll,
}

/// A running job and the mechanism that holds it.
pub(crate) enum Backend {
    Cgroup(Scope),
    Poll(ProcessGroup),
}

impl Backend {
    /// The job's process group, whose leader is the guard's child.
    pub(crate) fn group(&mut self) -> &mut ProcessGroup {
        match self {
            Self::Cgroup(scope) => &mut scope.group,
            Self::Poll(group) => group,
        }
    }

    /// The readings of the job's own cgroup; `poll` has none.
    pub(crate) fn read_job(&self) -> Result<Option<JobReadings>> {
        match self {
            Self::Cgroup(scope) => scope.read().map(Some),
            Self::Poll(_) => Ok(None),
        }
    }

    /// The job's out-of-memory counters after its main process ended.
    pub(crate) fn final_events(&self) -> Option<MemoryEvents> {
        match self {
            Self::Cgroup(scope) => Some(scope.final_events()),
            Self::Poll(_) => None,
        }
    }

    pub(crate) fn has_members(&self) -> bool {
        match self {
            Self::Cgroup(scope) => scope.has_members(),
            Self::Poll(group) => group.has_members(),
        }
    }

    /// `SIGTERM`, `term_grace`, then `SIGKILL` to the group, or
    /// `cgroup.kill` to the scope.
    pub(crate) fn terminate(&mut self, term_grace: Duration) -> Result<()> {
        match self {
            Self::Cgroup(scope) => {
                scope.terminate(term_grace);
                Ok(())
            }
            Self::Poll(group) => group.terminate(term_grace),
        }
    }

    /// Ends the job without a grace: after a kill at its limit, the job has
    /// already lost a process.
    pub(crate) fn kill(&mut self) {
        match self {
            Self::Cgroup(scope) => scope.kill(),
            Self::Poll(group) => group.kill(),
        }
    }
}

/// How the leader of a group ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LeaderExit {
    Code(i32),
    /// The leader ended on this signal number.
    Signal(i32),
}

impl LeaderExit {
    /// The exit code that a shell reports: the code, or 128 + the signal number.
    pub(crate) fn code(self) -> i32 {
        match self {
            Self::Code(code) => code,
            Self::Signal(signal) => SIGNAL_EXIT_BASE + signal,
        }
    }
}

/// What a look at the leader found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LeaderState {
    Running,
    /// The leader has just stopped on a signal, such as `SIGTSTP` from Ctrl-Z.
    Stopped,
    Exited(LeaderExit),
}

/// A command started as the leader of a new process group. oomwrap waits for
/// the leader with `waitpid`, which also reports a stop of the job.
pub(crate) struct ProcessGroup {
    pgid: ProcessId,
    exit: Option<LeaderExit>,
    /// The leader was seen stopped and not continued since.
    stopped: bool,
}

impl ProcessGroup {
    pub(crate) fn spawn(program: &OsStr, args: &[OsString]) -> io::Result<Self> {
        let child = Command::new(program).args(args).process_group(0).spawn()?;
        let Some(pgid) = ProcessId::from_child_id(child.id()) else {
            return Err(io::Error::other(format!(
                "child process ID {} cannot be signaled; the child runs unguarded",
                child.id()
            )));
        };
        // The guard waits for the child with waitpid; dropping the handle
        // neither waits nor signals.
        drop(child);
        Ok(Self {
            pgid,
            exit: None,
            stopped: false,
        })
    }

    pub(crate) fn pgid(&self) -> ProcessId {
        self.pgid
    }

    /// The process ID of the leader, which is also the group's ID.
    pub(crate) fn leader_pid(&self) -> u32 {
        self.pgid.as_raw().unsigned_abs()
    }

    /// Looks at the leader without blocking.
    pub(crate) fn poll(&mut self) -> io::Result<LeaderState> {
        if let Some(exit) = self.exit {
            return Ok(LeaderState::Exited(exit));
        }
        let flags = WaitPidFlag::WNOHANG | WaitPidFlag::WUNTRACED | WaitPidFlag::WCONTINUED;
        match waitpid(self.pgid.as_pid(), Some(flags)).map_err(io::Error::from)? {
            WaitStatus::Stopped(..) => {
                self.stopped = true;
                Ok(LeaderState::Stopped)
            }
            WaitStatus::Continued(_) => {
                self.stopped = false;
                Ok(LeaderState::Running)
            }
            status => Ok(self.record(status)),
        }
    }

    /// Waits until the leader exits.
    pub(crate) fn wait(&mut self) -> io::Result<LeaderExit> {
        loop {
            if let Some(exit) = self.exit {
                return Ok(exit);
            }
            let status = waitpid(self.pgid.as_pid(), None).map_err(io::Error::from)?;
            self.record(status);
        }
    }

    fn record(&mut self, status: WaitStatus) -> LeaderState {
        let exit = match status {
            WaitStatus::Exited(_, code) => LeaderExit::Code(code),
            WaitStatus::Signaled(_, signal, _) => LeaderExit::Signal(signal as i32),
            _ => return LeaderState::Running,
        };
        self.exit = Some(exit);
        LeaderState::Exited(exit)
    }

    /// Reaps the leader after a stop; its status no longer matters.
    // `terminate` already reaps a leader that exited, so skipping this wait
    // changes nothing that a test can see: it only blocks for a leader that
    // still runs in uninterruptible sleep, and oomwrap exits right after.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) fn reap(&mut self) {
        let _ = self.wait();
    }

    /// Tells whether any member of the group is still running.
    pub(crate) fn has_members(&self) -> bool {
        process_group_exists(self.pgid)
    }

    /// The resident memory of the group's members, as `poll` measures a job.
    pub(crate) fn rss_bytes(&self) -> Option<u64> {
        let pids = process_group_member_pids(self.pgid).ok()?;
        Some(
            pids.iter()
                .filter_map(|pid| read_rss_bytes(&Path::new("/proc").join(pid.to_string())).ok())
                .flatten()
                .sum(),
        )
    }

    /// Sends `SIGTERM` to the group, waits up to `term_grace`, then sends `SIGKILL`.
    pub(crate) fn terminate(&mut self, term_grace: Duration) -> Result<()> {
        terminate_process_group(self, term_grace)
    }

    /// Ends a launch that never started the command: `SIGKILL` to the group,
    /// then waits for the leader.
    pub(crate) fn kill(&mut self) {
        let _ = signal_process_group(self.pgid, Signal::SIGKILL);
        let _ = self.wait();
    }

    /// Continues a stopped job.
    pub(crate) fn resume(&mut self) {
        let _ = signal_process_group(self.pgid, Signal::SIGCONT);
        self.stopped = false;
    }
}

// The integration suite covers TERM/KILL behavior with real child processes.
// Sub-millisecond deadline boundary mutations here are noisy for cargo-mutants.
#[cfg_attr(test, mutants::skip)]
fn terminate_process_group(group: &mut ProcessGroup, term_grace: Duration) -> Result<()> {
    let pgid = group.pgid;
    let _ = signal_process_group(pgid, Signal::SIGTERM);
    if group.stopped {
        // A stopped process handles SIGTERM only once it runs again.
        group.resume();
    }
    let started = Instant::now();
    let mut seen_group = false;
    loop {
        let _ = group.poll();
        let group_exists = process_group_exists(pgid);
        if group_exists {
            seen_group = true;
        } else if seen_group {
            return Ok(());
        }

        let elapsed = started.elapsed();
        if seen_group && elapsed >= term_grace {
            break;
        }
        if !seen_group {
            if elapsed >= PROCESS_GROUP_SETTLE {
                return Ok(());
            }
            thread::sleep(
                PROCESS_GROUP_SETTLE
                    .saturating_sub(elapsed)
                    .min(Duration::from_millis(20)),
            );
            continue;
        }

        let remaining = term_grace.saturating_sub(elapsed);
        if remaining.is_zero() {
            continue;
        }
        thread::sleep(remaining.min(Duration::from_millis(20)));
    }

    let _ = signal_process_group(pgid, Signal::SIGKILL);
    let kill_started = Instant::now();
    while kill_started.elapsed() < PROCESS_GROUP_KILL_WAIT {
        let _ = group.poll();
        if !process_group_exists(pgid) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = group.poll();
    Ok(())
}

// Thin /proc adapter; exercised through process-group integration tests.
#[cfg_attr(test, mutants::skip)]
fn process_group_exists(pgid: ProcessId) -> bool {
    match group_exists(pgid) {
        Ok(()) => return true,
        Err(Errno::ESRCH) => {}
        Err(_) => return true,
    }
    process_group_member_pids(pgid)
        .map(|pids| !pids.is_empty())
        .unwrap_or(false)
}

// Chooses signal targets; `.cargo/mutants.toml` keeps cargo-mutants out of it.
#[cfg_attr(test, mutants::skip)]
fn signal_process_group(pgid: ProcessId, signal: Signal) -> nix::Result<()> {
    let group_result = signal_group(pgid, signal);
    if group_result.is_ok() {
        return Ok(());
    }

    let mut signaled_member = false;
    if let Ok(pids) = process_group_member_pids(pgid) {
        for pid in pids {
            if signal_process(pid, signal).is_ok() {
                signaled_member = true;
            }
        }
    }
    if signaled_member {
        Ok(())
    } else {
        group_result
    }
}

// Chooses signal targets; `.cargo/mutants.toml` keeps cargo-mutants out of it.
#[cfg_attr(test, mutants::skip)]
fn process_group_member_pids(pgid: ProcessId) -> Result<Vec<ProcessId>> {
    let mut pids = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        // An entry name that is not a process ID above 1 never becomes a target.
        let Some(pid) = entry.file_name().to_str().and_then(ProcessId::parse) else {
            continue;
        };
        if read_process_group_id(&entry.path()).ok() == Some(pgid.as_raw()) {
            pids.push(pid);
        }
    }
    Ok(pids)
}

pub(crate) fn read_process_group_id(proc_dir: &Path) -> Result<i32> {
    parse_process_group_id(&fs::read_to_string(proc_dir.join("stat"))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sleeper(seconds: &str) -> ProcessGroup {
        ProcessGroup::spawn(OsStr::new("sleep"), &[seconds.into()]).unwrap()
    }

    /// Polls the leader until it exits, for at most 3 s.
    fn wait_for_exit(group: &mut ProcessGroup) -> Option<LeaderExit> {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(3) {
            if let LeaderState::Exited(exit) = group.poll().unwrap() {
                return Some(exit);
            }
            thread::sleep(Duration::from_millis(20));
        }
        None
    }

    /// Never leaves the own child running when a check fails.
    fn end(group: &mut ProcessGroup) {
        let _ = signal_group(group.pgid, Signal::SIGKILL);
        let _ = group.wait();
    }

    #[test]
    fn process_group_members_are_only_the_own_child() {
        let mut group = sleeper("30");
        let members = process_group_member_pids(group.pgid).unwrap();
        end(&mut group);
        assert_eq!(members, vec![group.pgid]);
        assert!(!process_group_exists(group.pgid));
    }

    #[test]
    fn terminate_ends_the_own_child_group() {
        let mut group = sleeper("30");
        assert_eq!(
            i64::from(group.leader_pid()),
            i64::from(group.pgid().as_raw())
        );
        let terminated = group.terminate(Duration::from_millis(50));
        let exit = wait_for_exit(&mut group);
        if exit.is_none() {
            end(&mut group);
        }
        terminated.unwrap();
        let exit = exit.expect("the group must end after terminate");
        assert_eq!(exit, LeaderExit::Signal(Signal::SIGTERM as i32));
        assert_eq!(exit.code(), 143);
        assert!(!group.has_members());
    }

    #[test]
    fn poll_reports_a_stop_once_and_resume_continues() {
        let mut group = sleeper("30");
        signal_group(group.pgid, Signal::SIGSTOP).unwrap();
        let started = Instant::now();
        let mut state = group.poll().unwrap();
        while state == LeaderState::Running && started.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(5));
            state = group.poll().unwrap();
        }
        let after = group.poll().unwrap();
        let stopped = group.stopped;
        group.resume();
        let resumed = group.stopped;
        end(&mut group);
        assert_eq!(state, LeaderState::Stopped);
        assert_eq!(after, LeaderState::Running);
        assert!(stopped);
        assert!(!resumed);
    }

    #[test]
    fn exits_are_codes_or_signals() {
        let mut group =
            ProcessGroup::spawn(OsStr::new("sh"), &["-c".into(), "exit 7".into()]).unwrap();
        assert_eq!(group.wait().unwrap(), LeaderExit::Code(7));
        assert_eq!(
            group.poll().unwrap(),
            LeaderState::Exited(LeaderExit::Code(7))
        );
        assert_eq!(LeaderExit::Code(7).code(), 7);
        assert_eq!(LeaderExit::Signal(9).code(), 137);
    }

    #[test]
    fn the_resident_memory_of_the_group_is_known() {
        let mut group = sleeper("30");
        let rss = group.rss_bytes();
        end(&mut group);
        assert!(rss.is_some_and(|bytes| bytes > 0), "{rss:?}");
        assert_eq!(group.rss_bytes(), Some(0));
    }

    #[test]
    fn kill_ends_and_reaps_the_own_launch() {
        let mut group = sleeper("30");
        group.kill();
        assert_eq!(
            group.poll().unwrap(),
            LeaderState::Exited(LeaderExit::Signal(Signal::SIGKILL as i32))
        );
    }
}
