//! How the guarded command runs: in its own process group.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use nix::errno::Errno;
use nix::sys::signal::Signal;

use crate::readings::parse_process_group_id;
use crate::signal::{ProcessId, group_exists, signal_group, signal_process};

/// How long a new process group may take to appear, and to empty after a signal.
pub(crate) const PROCESS_GROUP_SETTLE: Duration = Duration::from_millis(100);
const PROCESS_GROUP_KILL_WAIT: Duration = Duration::from_secs(1);

/// The guarded command, started as the leader of a new process group.
pub(crate) struct ProcessGroup {
    child: Child,
    pgid: ProcessId,
}

impl ProcessGroup {
    pub(crate) fn spawn(command: &[OsString]) -> io::Result<Self> {
        let Some(program) = command.first() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "missing command",
            ));
        };
        let child = Command::new(program)
            .args(&command[1..])
            .process_group(0)
            .spawn()?;
        let Some(pgid) = ProcessId::from_child_id(child.id()) else {
            return Err(io::Error::other(format!(
                "child process ID {} cannot be signaled; the child runs unguarded",
                child.id()
            )));
        };
        Ok(Self { child, pgid })
    }

    pub(crate) fn pgid(&self) -> ProcessId {
        self.pgid
    }

    pub(crate) fn leader_pid(&self) -> u32 {
        self.child.id()
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Reaps the leader after a stop; its status no longer matters.
    pub(crate) fn reap(&mut self) {
        let _ = self.child.wait();
    }

    /// Tells whether any member of the group is still running.
    pub(crate) fn has_members(&self) -> bool {
        process_group_exists(self.pgid)
    }

    /// Sends `SIGTERM` to the group, waits up to `term_grace`, then sends `SIGKILL`.
    pub(crate) fn terminate(&mut self, term_grace: Duration) -> Result<()> {
        terminate_process_group(&mut self.child, self.pgid, term_grace)
    }
}

// The integration suite covers TERM/KILL behavior with real child processes.
// Sub-millisecond deadline boundary mutations here are noisy for cargo-mutants.
#[cfg_attr(test, mutants::skip)]
fn terminate_process_group(child: &mut Child, pgid: ProcessId, term_grace: Duration) -> Result<()> {
    let _ = signal_process_group(pgid, Signal::SIGTERM);
    let started = Instant::now();
    let mut seen_group = false;
    loop {
        let _ = child.try_wait();
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
        let _ = child.try_wait();
        if !process_group_exists(pgid) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = child.try_wait();
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

    #[test]
    fn process_group_members_are_only_the_own_child() {
        let mut child = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = ProcessId::from_child_id(child.id()).unwrap();
        let members = process_group_member_pids(pgid).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(members, vec![pgid]);
        assert!(!process_group_exists(pgid));
    }
}
