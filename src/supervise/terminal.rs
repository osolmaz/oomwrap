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
    guard_pgrp: Pid,
    job_pgrp: Pid,
    /// The job holds the terminal now.
    job_holds: bool,
}

impl TerminalForeground {
    /// Hands the controlling terminal to the job's group when oomwrap holds
    /// it in the foreground; returns `None` when there is no terminal to hand
    /// over.
    pub(crate) fn handoff_to_job(job: ProcessId) -> Result<Option<Self>> {
        let tty = match OpenOptions::new().read(true).write(true).open("/dev/tty") {
            Ok(tty) => tty,
            Err(_) => return Ok(None),
        };
        let guard_pgrp = match tcgetpgrp(&tty) {
            Ok(pgrp) => pgrp,
            Err(Errno::ENOTTY) | Err(Errno::ENXIO) => return Ok(None),
            Err(error) => bail!("failed to read terminal foreground process group: {error}"),
        };
        if guard_pgrp != getpgrp() {
            return Ok(None);
        }

        let job_pgrp = job.as_pid();
        if guard_pgrp == job_pgrp {
            return Ok(None);
        }
        if !set_foreground_pgrp(&tty, job_pgrp)? {
            return Ok(None);
        }

        Ok(Some(Self {
            tty,
            guard_pgrp,
            job_pgrp,
            job_holds: true,
        }))
    }

    /// Takes the terminal back from the job; does nothing when the job does
    /// not hold it.
    pub(crate) fn take_back(&mut self) -> Result<()> {
        if !self.job_holds {
            return Ok(());
        }
        with_sigttou_blocked(|| tcsetpgrp(&self.tty, self.guard_pgrp))
            .context("failed to restore terminal foreground process group")?;
        self.job_holds = false;
        Ok(())
    }

    /// Gives the terminal to the job again after the shell continued
    /// oomwrap in the foreground. A job continued in the background (`bg`)
    /// stays there, and the terminal stays with the shell.
    pub(crate) fn give_to_job(&mut self) -> Result<()> {
        if self.job_holds {
            return Ok(());
        }
        let foreground =
            tcgetpgrp(&self.tty).context("failed to read terminal foreground process group")?;
        if foreground != self.guard_pgrp {
            return Ok(());
        }
        with_sigttou_blocked(|| tcsetpgrp(&self.tty, self.job_pgrp))
            .context("failed to hand the terminal to the job again")?;
        self.job_holds = true;
        Ok(())
    }
}

impl Drop for TerminalForeground {
    // A backstop for an early return or a panic: every normal path takes
    // the terminal back explicitly first, so a mutant here changes nothing
    // visible.
    #[cfg_attr(test, mutants::skip)]
    fn drop(&mut self) {
        if let Err(error) = self.take_back() {
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
        && let Err(error) = terminal.take_back()
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
