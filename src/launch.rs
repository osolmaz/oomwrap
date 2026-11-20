//! The launch handshake. The guard starts `oomwrap exec-job` as the first
//! process of the job. `exec-job` raises its own out-of-memory score, reports
//! where it runs, and executes the command only when the guard answers `go`,
//! so the command starts once, after its limits hold.
//!
//! The two sides talk over a Unix socket in a private folder, one JSON line
//! per message. `exec-job` connects to it by path: safe Rust has no way to
//! take an inherited descriptor by its number, and the crate forbids unsafe
//! code. The socket of `exec-job` closes when the command executes, so the
//! end of the stream tells the guard that the command runs.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::readings::parse_cgroup_v2_path;

/// The version of the messages between the guard and `exec-job`.
pub(crate) const PROTOCOL_VERSION: u32 = 1;
/// The out-of-memory score of every job process: the first victim of a
/// machine-wide kill. A process may raise its own score without privileges.
pub(crate) const JOB_OOM_SCORE_ADJ: i32 = 1000;
/// How long the launch may take to report, and the command to execute.
pub(crate) const REPORT_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the guard looks at signals and the launch while it waits.
const WAIT_SLICE: Duration = Duration::from_millis(5);
const SOCKET_NAME: &str = "control";
/// The exit code of `exec-job` when the command did not start.
const NOT_STARTED_EXIT_CODE: i32 = 3;
/// The exit codes of a shell for a command that was not found, or cannot run.
pub(crate) const NOT_FOUND_EXIT_CODE: i32 = 127;
pub(crate) const CANNOT_RUN_EXIT_CODE: i32 = 126;

/// What `exec-job` reports before the command starts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Report {
    pub(crate) protocol: u32,
    /// The process ID of `exec-job`, which the command keeps.
    pub(crate) pid: u32,
    /// The cgroup v2 path of the job, from `/proc/self/cgroup`.
    pub(crate) cgroup: Option<String>,
    /// Why `exec-job` could not raise its out-of-memory score.
    pub(crate) oom_score_adj_error: Option<String>,
    /// The process that keeps a scope alive after the job ends.
    #[serde(default)]
    pub(crate) holder_pid: Option<u32>,
    /// Why `exec-job` could not start that process.
    #[serde(default)]
    pub(crate) holder_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Answer {
    Go,
    Abort,
}

/// Why `exec-job` could not execute the command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ExecFailure {
    pub(crate) not_found: bool,
    pub(crate) message: String,
}

impl ExecFailure {
    fn new(program: &OsStr, error: &io::Error) -> Self {
        Self {
            not_found: error.kind() == io::ErrorKind::NotFound,
            message: format!("cannot run {}: {error}", program.to_string_lossy()),
        }
    }

    /// 127 for a command that was not found, 126 for one that cannot run.
    pub(crate) fn exit_code(&self) -> i32 {
        if self.not_found {
            NOT_FOUND_EXIT_CODE
        } else {
            CANNOT_RUN_EXIT_CODE
        }
    }
}

/// The program that the launch runs: the guard's own binary, by its process,
/// so that an upgrade of the oomwrap file during a launch cannot mix versions.
pub(crate) fn guard_exe() -> PathBuf {
    PathBuf::from(format!("/proc/{}/exe", std::process::id()))
}

/// The arguments of `exec-job` for a command. `hold` is the guard's lock in
/// the `cgroup` backend: `exec-job` then starts a holder of the scope.
pub(crate) fn exec_job_args(
    control: &Path,
    hold: Option<&Path>,
    command: &[OsString],
) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "exec-job".into(),
        "--control".into(),
        control.as_os_str().to_owned(),
    ];
    if let Some(lock) = hold {
        args.push("--hold-scope".into());
        args.push(lock.as_os_str().to_owned());
    }
    args.push("--".into());
    args.extend(command.iter().cloned());
    args
}

/// `oomwrap hold-scope`: keeps a scope alive after the job's last process
/// ends, until its guard releases the lock. When the kernel kills the last
/// job process, systemd removes an empty scope at once, often before the
/// guard can read `memory.events`; a scope that still holds this process
/// stays. The first call starts the holder in its own process group, prints
/// its process ID and exits, so the holder is no child of the command, and a
/// shell's `wait` never waits for it.
pub(crate) fn hold_scope(lock: &Path, stay: bool) -> i32 {
    if stay {
        // The guard holds the lock for its whole run; a shared lock waits
        // until the guard releases it, or ends.
        if let Ok(file) = fs::File::open(lock) {
            let _ = file.lock_shared();
        }
        return 0;
    }
    let holder = Command::new("/proc/self/exe")
        .arg("hold-scope")
        .arg("--lock")
        .arg(lock)
        .arg("--stay")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match holder {
        Ok(holder) => {
            println!("{}", holder.id());
            0
        }
        Err(error) => {
            eprintln!("oomwrap hold-scope: {error}");
            NOT_STARTED_EXIT_CODE
        }
    }
}

/// Starts the holder of the scope and returns its process ID.
fn start_holder(lock: &Path) -> Result<u32, String> {
    let output = Command::new("/proc/self/exe")
        .arg("hold-scope")
        .arg("--lock")
        .arg(lock)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|error| format!("cannot start the holder of the scope: {error}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    match (output.status.success(), text.trim().parse()) {
        (true, Ok(pid)) => Ok(pid),
        _ => Err(format!(
            "the holder of the scope did not start: {}",
            text.trim()
        )),
    }
}

/// `oomwrap exec-job`: runs as the job's first process. Returns only when the
/// command did not start, with the exit code of `exec-job`. The cgroup comes
/// from `<proc_root>/self/cgroup`, so a test's fake scope can name it.
pub(crate) fn exec_job(
    control: &Path,
    hold: Option<&Path>,
    command: &[OsString],
    proc_root: &Path,
) -> i32 {
    let Some((program, args)) = command.split_first() else {
        eprintln!("oomwrap exec-job: missing command");
        return NOT_STARTED_EXIT_CODE;
    };
    let stream = match UnixStream::connect(control) {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("oomwrap exec-job: cannot reach the guard: {error}");
            return NOT_STARTED_EXIT_CODE;
        }
    };
    // Before the score rises, so the holder keeps the lower score.
    let holder = hold.map(start_holder);
    let report = Report {
        protocol: PROTOCOL_VERSION,
        pid: std::process::id(),
        cgroup: fs::read_to_string(proc_root.join("self/cgroup"))
            .ok()
            .and_then(|text| parse_cgroup_v2_path(&text)),
        oom_score_adj_error: fs::write("/proc/self/oom_score_adj", JOB_OOM_SCORE_ADJ.to_string())
            .err()
            .map(|error| error.to_string()),
        holder_pid: holder
            .as_ref()
            .and_then(|holder| holder.as_ref().ok().copied()),
        holder_error: holder.and_then(Result::err),
    };
    let mut reader = BufReader::new(stream);
    if send(reader.get_mut(), &report).is_err() {
        return NOT_STARTED_EXIT_CODE;
    }
    let mut line = Vec::new();
    let answer = match reader.read_until(b'\n', &mut line) {
        Ok(_) => serde_json::from_slice::<Answer>(&line).ok(),
        Err(_) => None,
    };
    if answer != Some(Answer::Go) {
        // `abort`, or the guard is gone: the command never starts.
        return NOT_STARTED_EXIT_CODE;
    }
    // The socket is close-on-exec, so the command never inherits it.
    let error = Command::new(program).args(args).exec();
    let failure = ExecFailure::new(program, &error);
    let _ = send(reader.get_mut(), &failure);
    failure.exit_code()
}

fn send(stream: &mut UnixStream, message: &impl Serialize) -> io::Result<()> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    stream.write_all(&line)
}

/// What the guard learns while it waits for the launch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Watch {
    Continue,
    /// oomwrap received this signal number.
    Interrupted(i32),
    /// The launch ended.
    LaunchEnded,
}

/// Why the launch failed before its report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LaunchFailure {
    Ended,
    Closed,
    TimedOut,
    Unreadable(String),
}

impl fmt::Display for LaunchFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ended => formatter.write_str("the launch ended before its report"),
            Self::Closed => {
                formatter.write_str("the launch closed the control socket before its report")
            }
            Self::TimedOut => write!(
                formatter,
                "the launch sent no report within {} s",
                REPORT_TIMEOUT.as_secs()
            ),
            Self::Unreadable(cause) => {
                write!(formatter, "the launch sent an unreadable report: {cause}")
            }
        }
    }
}

/// How the wait for the report ended.
#[derive(Debug)]
pub(crate) enum Waited {
    Reported(Control, Report),
    Failed(LaunchFailure),
    Interrupted(i32),
}

/// A report that the guard does not accept.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReportFault {
    Protocol { reported: u32 },
    Process { reported: u32, launched: u32 },
}

impl fmt::Display for ReportFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol { reported } => write!(
                formatter,
                "exec-job speaks protocol version {reported}, this guard speaks {PROTOCOL_VERSION}"
            ),
            Self::Process { reported, launched } => write!(
                formatter,
                "the report came from process {reported}, but the launch is process {launched}"
            ),
        }
    }
}

/// Checks that the report comes from this launch and speaks this protocol.
pub(crate) fn check_report(report: &Report, launched_pid: u32) -> Result<(), ReportFault> {
    if report.protocol != PROTOCOL_VERSION {
        return Err(ReportFault::Protocol {
            reported: report.protocol,
        });
    }
    if report.pid != launched_pid {
        return Err(ReportFault::Process {
            reported: report.pid,
            launched: launched_pid,
        });
    }
    Ok(())
}

/// The guard's end before the launch connects: a listening socket in a
/// folder that only this user can enter.
pub(crate) struct ControlSocket {
    listener: UnixListener,
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl ControlSocket {
    pub(crate) fn bind(parent: &Path) -> Result<Self> {
        // Only this user may enter the folder, so only this user's processes
        // can connect.
        let dir = tempfile::Builder::new()
            .prefix("oomwrap-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(parent)
            .with_context(|| {
                format!("failed to create a control folder in {}", parent.display())
            })?;
        let path = dir.path().join(SOCKET_NAME);
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("failed to bind the control socket {}", path.display()))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            path,
            _dir: dir,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Waits for the launch to connect and report, at most `timeout`.
    /// `check` reports signals and the end of the launch in between.
    pub(crate) fn wait_for_report(
        self,
        timeout: Duration,
        mut check: impl FnMut() -> Watch,
    ) -> Waited {
        let deadline = deadline_after(timeout);
        let stream = loop {
            match check() {
                Watch::Continue => {}
                Watch::Interrupted(signal) => return Waited::Interrupted(signal),
                Watch::LaunchEnded => return Waited::Failed(LaunchFailure::Ended),
            }
            match self.listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Waited::Failed(LaunchFailure::Unreadable(error.to_string())),
            }
            if Instant::now() >= deadline {
                return Waited::Failed(LaunchFailure::TimedOut);
            }
            thread::sleep(WAIT_SLICE);
        };
        // The listener and its folder go here: nobody else can connect.
        drop(self);
        if let Err(error) = stream.set_nonblocking(false) {
            return Waited::Failed(LaunchFailure::Unreadable(error.to_string()));
        }
        let mut control = Control {
            reader: BufReader::new(stream),
        };
        match control.read_message::<Report>(deadline, &mut check) {
            Message::Received(report) => Waited::Reported(control, report),
            Message::Closed => Waited::Failed(LaunchFailure::Closed),
            Message::TimedOut => Waited::Failed(LaunchFailure::TimedOut),
            Message::Unreadable(cause) => Waited::Failed(LaunchFailure::Unreadable(cause)),
            Message::Stopped(Watch::Interrupted(signal)) => Waited::Interrupted(signal),
            Message::Stopped(_) => Waited::Failed(LaunchFailure::Ended),
        }
    }
}

/// How `exec-job` ended the handshake after `go`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Exec {
    /// The command runs: its process holds no socket any more.
    Started,
    Failed(ExecFailure),
}

/// The guard's end of one launch's connection.
#[derive(Debug)]
pub(crate) struct Control {
    reader: BufReader<UnixStream>,
}

enum Message<T> {
    Received(T),
    Closed,
    TimedOut,
    Unreadable(String),
    Stopped(Watch),
}

impl Control {
    pub(crate) fn answer(&mut self, answer: Answer) -> io::Result<()> {
        send(self.reader.get_mut(), &answer)
    }

    /// After `go`: waits until the command executes, which closes the
    /// socket, or `exec-job` reports that it could not run it. A launch that
    /// says nothing within the timeout counts as started; the guard then
    /// watches it as any job.
    pub(crate) fn wait_for_exec(mut self, timeout: Duration) -> Exec {
        match self.read_message::<ExecFailure>(deadline_after(timeout), &mut || Watch::Continue) {
            Message::Received(failure) => Exec::Failed(failure),
            _ => Exec::Started,
        }
    }

    fn read_message<T: DeserializeOwned>(
        &mut self,
        deadline: Instant,
        check: &mut impl FnMut() -> Watch,
    ) -> Message<T> {
        let mut line = Vec::new();
        loop {
            let watch = check();
            if watch != Watch::Continue {
                return Message::Stopped(watch);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Message::TimedOut;
            }
            if let Err(error) = self
                .reader
                .get_ref()
                .set_read_timeout(Some(remaining.min(WAIT_SLICE)))
            {
                return Message::Unreadable(error.to_string());
            }
            match self.reader.read_until(b'\n', &mut line) {
                Ok(0) => return Message::Closed,
                Ok(_) if line.ends_with(b"\n") => {
                    return serde_json::from_slice(&line).map_or_else(
                        |error| Message::Unreadable(error.to_string()),
                        Message::Received,
                    );
                }
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Message::Unreadable(error.to_string()),
            }
        }
    }
}

fn deadline_after(timeout: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(timeout).unwrap_or(now + REPORT_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(protocol: u32, pid: u32) -> Report {
        Report {
            protocol,
            pid,
            cgroup: Some("/a.scope".into()),
            oom_score_adj_error: None,
            holder_pid: None,
            holder_error: None,
        }
    }

    /// A fake `exec-job` on a thread: sends `lines`, then reads one answer.
    fn fake_job(path: &Path, lines: &'static [&'static str]) -> thread::JoinHandle<Option<String>> {
        let path = path.to_path_buf();
        thread::spawn(move || {
            let mut stream = UnixStream::connect(path).ok()?;
            for line in lines {
                stream.write_all(line.as_bytes()).ok()?;
            }
            let mut answer = String::new();
            BufReader::new(stream).read_line(&mut answer).ok()?;
            Some(answer)
        })
    }

    fn new_socket() -> (tempfile::TempDir, ControlSocket) {
        let parent = tempfile::tempdir().unwrap();
        let socket = ControlSocket::bind(parent.path()).unwrap();
        (parent, socket)
    }

    #[test]
    fn a_report_reaches_the_guard_and_the_answer_reaches_the_job() {
        let (_parent, socket) = new_socket();
        let job = fake_job(
            socket.path(),
            &[
                r#"{"protocol":1,"pid":42,"cgroup":"/a.scope","#,
                "\"oom_score_adj_error\":null}\n",
            ],
        );
        let Waited::Reported(mut control, got) =
            socket.wait_for_report(Duration::from_secs(5), || Watch::Continue)
        else {
            panic!("no report");
        };
        assert_eq!(got, report(1, 42));
        control.answer(Answer::Go).unwrap();
        assert_eq!(job.join().unwrap().as_deref(), Some("\"go\"\n"));
        assert_eq!(control.wait_for_exec(Duration::from_secs(5)), Exec::Started);
    }

    #[test]
    fn the_socket_folder_is_private_and_goes_with_the_socket() {
        let (_parent, socket) = new_socket();
        let folder = socket.path().parent().unwrap().to_path_buf();
        let mode = fs::metadata(&folder).unwrap().permissions();
        assert_eq!(mode.mode() & 0o777, 0o700);
        let waited = socket.wait_for_report(Duration::ZERO, || Watch::Continue);
        assert!(matches!(waited, Waited::Failed(LaunchFailure::TimedOut)));
        assert!(!folder.exists());
    }

    #[test]
    fn signals_and_the_end_of_the_launch_stop_the_wait() {
        let (_parent, socket) = new_socket();
        let waited = socket.wait_for_report(Duration::from_secs(5), || Watch::Interrupted(15));
        assert!(matches!(waited, Waited::Interrupted(15)));
        let (_parent, socket) = new_socket();
        let waited = socket.wait_for_report(Duration::from_secs(5), || Watch::LaunchEnded);
        assert!(matches!(waited, Waited::Failed(LaunchFailure::Ended)));
    }

    #[test]
    fn a_signal_while_the_report_is_read_stops_the_wait() {
        let (_parent, socket) = new_socket();
        let path = socket.path().to_path_buf();
        let job = thread::spawn(move || {
            let stream = UnixStream::connect(path).unwrap();
            thread::sleep(Duration::from_millis(300));
            drop(stream);
        });
        let started = Instant::now();
        let waited = socket.wait_for_report(Duration::from_secs(5), || {
            if started.elapsed() > Duration::from_millis(100) {
                Watch::Interrupted(2)
            } else {
                Watch::Continue
            }
        });
        job.join().unwrap();
        assert!(matches!(waited, Waited::Interrupted(2)), "{waited:?}");
    }

    #[test]
    fn a_launch_that_closes_or_sends_garbage_fails() {
        let (_parent, socket) = new_socket();
        let job = fake_job(socket.path(), &[r#"{"protocol":1"#]);
        let path = socket.path().to_path_buf();
        // The fake job waits for an answer; the guard sees no full line.
        let waited = socket.wait_for_report(Duration::from_millis(200), || Watch::Continue);
        assert!(
            matches!(waited, Waited::Failed(LaunchFailure::TimedOut)),
            "{waited:?}"
        );
        // The job reads the end of the stream instead of an answer.
        assert_eq!(job.join().unwrap().as_deref(), Some(""));
        assert!(!path.exists());

        let (_parent, socket) = new_socket();
        let path = socket.path().to_path_buf();
        let job = thread::spawn(move || drop(UnixStream::connect(path).unwrap()));
        let waited = socket.wait_for_report(Duration::from_secs(5), || Watch::Continue);
        job.join().unwrap();
        assert!(
            matches!(waited, Waited::Failed(LaunchFailure::Closed)),
            "{waited:?}"
        );

        let (_parent, socket) = new_socket();
        let job = fake_job(socket.path(), &["not json\n"]);
        let waited = socket.wait_for_report(Duration::from_secs(5), || Watch::Continue);
        assert!(
            matches!(waited, Waited::Failed(LaunchFailure::Unreadable(_))),
            "{waited:?}"
        );
        drop(waited);
        assert_eq!(job.join().unwrap().as_deref(), Some(""));
    }

    #[test]
    fn an_exec_failure_reaches_the_guard() {
        let (_parent, socket) = new_socket();
        let job = fake_job(
            socket.path(),
            &[
                "{\"protocol\":1,\"pid\":7,\"cgroup\":null,\"oom_score_adj_error\":null}\n",
                "{\"not_found\":true,\"message\":\"cannot run x\"}\n",
            ],
        );
        let Waited::Reported(mut control, _) =
            socket.wait_for_report(Duration::from_secs(5), || Watch::Continue)
        else {
            panic!("no report");
        };
        control.answer(Answer::Abort).unwrap();
        assert_eq!(job.join().unwrap().as_deref(), Some("\"abort\"\n"));
        let Exec::Failed(failure) = control.wait_for_exec(Duration::from_secs(5)) else {
            panic!("no failure");
        };
        assert_eq!(failure.exit_code(), 127);
        assert_eq!(failure.message, "cannot run x");
    }

    #[test]
    fn reports_must_match_the_protocol_and_the_launch() {
        assert_eq!(check_report(&report(1, 9), 9), Ok(()));
        let fault = check_report(&report(2, 9), 9).unwrap_err();
        assert_eq!(fault, ReportFault::Protocol { reported: 2 });
        assert!(fault.to_string().contains("protocol version 2"));
        let fault = check_report(&report(1, 8), 9).unwrap_err();
        assert_eq!(
            fault,
            ReportFault::Process {
                reported: 8,
                launched: 9
            }
        );
        assert!(fault.to_string().contains("process 8"));
    }

    #[test]
    fn exec_failures_map_to_shell_codes() {
        let missing = io::Error::from(io::ErrorKind::NotFound);
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        assert_eq!(ExecFailure::new(OsStr::new("x"), &missing).exit_code(), 127);
        let failure = ExecFailure::new(OsStr::new("x"), &denied);
        assert_eq!(failure.exit_code(), 126);
        assert!(failure.message.starts_with("cannot run x: "));
    }

    #[test]
    fn exec_job_arguments_end_with_the_command() {
        let args = exec_job_args(Path::new("/run/c"), None, &["sh".into(), "-c".into()]);
        assert_eq!(args, ["exec-job", "--control", "/run/c", "--", "sh", "-c"]);
        let held = exec_job_args(
            Path::new("/run/c"),
            Some(Path::new("/run/l")),
            &["sh".into()],
        );
        assert_eq!(
            held,
            [
                "exec-job",
                "--control",
                "/run/c",
                "--hold-scope",
                "/run/l",
                "--",
                "sh"
            ]
        );
        assert_eq!(
            guard_exe(),
            PathBuf::from(format!("/proc/{}/exe", std::process::id()))
        );
    }

    #[test]
    fn failures_explain_themselves() {
        assert!(LaunchFailure::Ended.to_string().contains("ended"));
        assert!(LaunchFailure::Closed.to_string().contains("closed"));
        assert!(LaunchFailure::TimedOut.to_string().contains("5 s"));
        assert!(
            LaunchFailure::Unreadable("x".into())
                .to_string()
                .ends_with(": x")
        );
    }

    #[test]
    fn exec_job_without_a_guard_does_not_start_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let command = [OsString::from("touch"), marker.as_os_str().to_owned()];
        assert_eq!(
            exec_job(
                &dir.path().join("missing"),
                None,
                &command,
                Path::new("/proc")
            ),
            3
        );
        assert_eq!(
            exec_job(&dir.path().join("missing"), None, &[], Path::new("/proc")),
            3
        );
        assert!(!marker.exists());
    }
}
