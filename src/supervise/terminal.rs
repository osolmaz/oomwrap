//! Terminal foreground control: the job gets the terminal while it runs, and
//! the caller gets it back when the job ends.

use std::fs::{File, OpenOptions};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nix::errno::Errno;
use nix::sys::signal::{SigSet, SigmaskHow, Signal, pthread_sigmask};
use nix::unistd::{Pid, getpgrp, tcgetpgrp, tcsetpgrp};

use crate::backend::PROCESS_GROUP_SETTLE;
use crate::signal::ProcessId;

pub(crate) struct TerminalForeground {
    tty: File,
    original_pgrp: Pid,
    restored: bool,
}

impl TerminalForeground {
    /// Hands the controlling terminal to `pgid` when oomwrap holds it in the
    /// foreground; returns `None` when there is no terminal to hand over.
    pub(crate) fn handoff_to_child(pgid: ProcessId) -> Result<Option<Self>> {
        let tty = match OpenOptions::new().read(true).write(true).open("/dev/tty") {
            Ok(tty) => tty,
            Err(_) => return Ok(None),
        };
        let original_pgrp = match tcgetpgrp(&tty) {
            Ok(pgrp) => pgrp,
            Err(Errno::ENOTTY) | Err(Errno::ENXIO) => return Ok(None),
            Err(error) => bail!("failed to read terminal foreground process group: {error}"),
        };
        if original_pgrp != getpgrp() {
            return Ok(None);
        }

        let child_pgrp = pgid.as_pid();
        if original_pgrp == child_pgrp {
            return Ok(None);
        }
        if !set_foreground_pgrp(&tty, child_pgrp)? {
            return Ok(None);
        }

        Ok(Some(Self {
            tty,
            original_pgrp,
            restored: false,
        }))
    }

    fn restore(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        with_sigttou_blocked(|| tcsetpgrp(&self.tty, self.original_pgrp))
            .context("failed to restore terminal foreground process group")?;
        self.restored = true;
        Ok(())
    }
}

impl Drop for TerminalForeground {
    // A backstop for an early return or a panic: every normal path restores
    // the terminal explicitly first, so a mutant here changes nothing visible.
    #[cfg_attr(test, mutants::skip)]
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("warning: {error:#}");
        }
    }
}

// Terminal handoff needs a real controlling terminal; the pseudo-terminal
// integration test covers it, and the deadline mutations are timing noise.
#[cfg_attr(test, mutants::skip)]
fn set_foreground_pgrp(tty: &File, pgrp: Pid) -> Result<bool> {
    let started = Instant::now();
    loop {
        match with_sigttou_blocked(|| tcsetpgrp(tty, pgrp)) {
            Ok(()) => return Ok(true),
            Err(Errno::ESRCH | Errno::EPERM) if started.elapsed() < PROCESS_GROUP_SETTLE => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(Errno::ESRCH) => return Ok(false),
            Err(error) => bail!("failed to hand terminal to child process group: {error}"),
        }
    }
}

/// Gives the terminal back to the caller, and warns when that fails.
// The drop of `TerminalForeground` restores the terminal too, a moment later,
// so a mutant here only changes the order against the stop sequence.
#[cfg_attr(test, mutants::skip)]
pub(crate) fn restore_terminal(terminal: &mut Option<TerminalForeground>) {
    if let Some(terminal) = terminal
        && let Err(error) = terminal.restore()
    {
        eprintln!("warning: {error:#}");
    }
}

fn with_sigttou_blocked<T>(operation: impl FnOnce() -> nix::Result<T>) -> nix::Result<T> {
    let mut blocked = SigSet::empty();
    blocked.add(Signal::SIGTTOU);
    let mut previous = SigSet::empty();
    pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&blocked), Some(&mut previous))?;
    let operation_result = operation();
    let restore_result = pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&previous), None);
    match (operation_result, restore_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) | (_, Err(error)) => Err(error),
    }
}
