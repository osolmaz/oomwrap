//! The only module that sends signals.
//!
//! kill(2) gives special meanings to some IDs: 0 is the caller's own process
//! group, -1 is every process that the caller may signal, and 1 is init.
//! killpg(1) is the same call as kill(-1). One wrong ID here can end every
//! process of the user, so every signal goes through [`ProcessId`], which can
//! hold only IDs greater than 1, and this module never computes an ID itself.
//!
//! `clippy.toml` forbids `kill` and `killpg` everywhere else, and
//! `.cargo/mutants.toml` keeps cargo-mutants out of this file.

use std::fmt;

use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;

/// A process ID or process group ID that oomwrap may signal.
///
/// The value is always greater than 1. It is parsed once, where the ID enters
/// oomwrap, and code that holds one can never ask kill(2) for a special ID.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ProcessId(i32);

impl ProcessId {
    /// The smallest ID that oomwrap signals; 0, 1 and negative IDs have special meanings.
    const SMALLEST: i32 = 2;

    /// Returns `None` for 1, 0 and every negative value.
    pub(crate) fn new(raw: i32) -> Option<Self> {
        (raw >= Self::SMALLEST).then_some(Self(raw))
    }

    /// Converts the ID of a spawned child; `None` when it does not fit kill(2).
    pub(crate) fn from_child_id(id: u32) -> Option<Self> {
        i32::try_from(id).ok().and_then(Self::new)
    }

    /// Parses a decimal ID, such as a `/proc` entry name or a `cgroup.procs` line.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        text.trim().parse().ok().and_then(Self::new)
    }

    pub(crate) fn as_raw(self) -> i32 {
        self.0
    }

    pub(crate) fn as_pid(self) -> Pid {
        Pid::from_raw(self.0)
    }
}

impl fmt::Display for ProcessId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Sends `signal` to one process.
#[expect(
    clippy::disallowed_methods,
    reason = "this module is the one place that sends signals"
)]
pub(crate) fn signal_process(pid: ProcessId, signal: Signal) -> nix::Result<()> {
    kill(pid.as_pid(), signal)
}

/// Sends `signal` to every process of one process group.
#[expect(
    clippy::disallowed_methods,
    reason = "this module is the one place that sends signals"
)]
pub(crate) fn signal_group(pgid: ProcessId, signal: Signal) -> nix::Result<()> {
    killpg(pgid.as_pid(), signal)
}

/// Tells whether a process group still has a member that oomwrap may signal.
#[expect(
    clippy::disallowed_methods,
    reason = "signal 0 checks that the group exists and sends nothing"
)]
pub(crate) fn group_exists(pgid: ProcessId) -> nix::Result<()> {
    killpg(pgid.as_pid(), None)
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::Command;

    use super::*;

    #[test]
    fn refuses_special_ids() {
        for raw in [i32::MIN, -2, -1, 0, 1] {
            assert_eq!(ProcessId::new(raw), None, "{raw} must be refused");
        }
        assert_eq!(ProcessId::new(2).map(ProcessId::as_raw), Some(2));
        assert_eq!(
            ProcessId::new(i32::MAX).map(ProcessId::as_raw),
            Some(i32::MAX)
        );
    }

    #[test]
    fn refuses_child_ids_that_wrap_to_negative_values() {
        assert_eq!(ProcessId::from_child_id(0), None);
        assert_eq!(ProcessId::from_child_id(1), None);
        assert_eq!(ProcessId::from_child_id(u32::MAX), None);
        assert_eq!(ProcessId::from_child_id(1 << 31), None);
        assert_eq!(
            ProcessId::from_child_id(4242).map(ProcessId::as_raw),
            Some(4242)
        );
    }

    #[test]
    fn parses_only_ids_above_one() {
        for text in ["", "abc", "-1", "0", "1", " 1\n", "1.5", "self"] {
            assert_eq!(ProcessId::parse(text), None, "{text:?} must be refused");
        }
        assert_eq!(ProcessId::parse(" 42\n").map(ProcessId::as_raw), Some(42));
        assert_eq!(
            ProcessId::parse("42").map(|pid| pid.to_string()),
            Some("42".into())
        );
        assert_eq!(
            ProcessId::parse("42").map(ProcessId::as_pid),
            Some(Pid::from_raw(42))
        );
    }

    #[test]
    fn signals_reach_only_the_own_child_group() {
        let mut child = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = ProcessId::from_child_id(child.id()).unwrap();
        assert!(group_exists(pgid).is_ok());
        signal_group(pgid, Signal::SIGKILL).unwrap();
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(Signal::SIGKILL as i32));
        assert!(group_exists(pgid).is_err());
    }

    #[test]
    fn signals_reach_one_own_child() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = ProcessId::from_child_id(child.id()).unwrap();
        signal_process(pid, Signal::SIGTERM).unwrap();
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(Signal::SIGTERM as i32));
    }
}
