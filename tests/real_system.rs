//! The real-system suite: oomwrap on a real kernel and a real service
//! manager. It runs only when `OOMWRAP_REAL_SYSTEM` names a suite, on
//! throwaway CI runners (`.github/workflows/real-system.yml`), because its
//! jobs run out of memory on purpose, each in a small scope of its own:
//!
//! - `host`: cgroup v2, a user manager and lingering for the runner user;
//! - `container`: a container with a memory limit and no service manager.
//!
//! Without `OOMWRAP_REAL_SYSTEM`, `cargo test` builds this suite and skips
//! it. The same binary is the helper that the jobs run
//! (`real_system helper <op>`), and the machine pressure workflow uses its
//! `ramp`. `OOMWRAP_TEST_BIN` names another oomwrap binary to test, such as
//! a release build.
//!
//! The suite signals only processes that it started, and the only scopes
//! that it makes have the `oomwrap-` prefix and end with their job.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::process::{self, Command, ExitStatus, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const MIB: usize = 1 << 20;
const PAGE: usize = 4096;

type Outcome = Result<(), String>;
type Case = (&'static str, fn(&Suite) -> Outcome);

const HOST_CASES: &[Case] = &[
    ("job_limit", job_limit),
    ("setsid_daemon", setsid_daemon),
    ("job_pressure", job_pressure),
    ("pass_through", pass_through),
    ("forwarded_signal", forwarded_signal),
    ("replaced_binary", replaced_binary),
    ("outer_scope_limit", outer_scope_limit),
];

const CONTAINER_CASES: &[Case] = &[
    ("container_auto", container_auto),
    ("container_floor", container_floor),
];

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("helper") {
        process::exit(helper(&args[1..]));
    }
    let cases = match env::var("OOMWRAP_REAL_SYSTEM").as_deref() {
        Ok("host") => HOST_CASES,
        Ok("container") => CONTAINER_CASES,
        _ => {
            println!("real-system suite skipped: set OOMWRAP_REAL_SYSTEM to host or container");
            return;
        }
    };
    let suite = Suite::new();
    let failed = cases
        .iter()
        .filter(|(name, _)| args.iter().all(|filter| name.contains(filter.as_str())))
        .filter(|(name, case)| !suite.passes(name, *case))
        .count();
    if failed > 0 {
        println!("real-system suite: {failed} failed");
        process::exit(1);
    }
    println!("real-system suite: ok");
}

// ---- The helper that the jobs run ----

fn helper(args: &[String]) -> i32 {
    let arg = |index: usize| args.get(index).map(String::as_str).unwrap_or_default();
    let number = |index: usize| arg(index).parse::<u64>().unwrap_or(0);
    match arg(0) {
        "alloc" => alloc(number(1), number(2)),
        "detach" => detach(Path::new(arg(1)), number(2)),
        "setsid-sleep" => setsid_sleep(Path::new(arg(1))),
        "report" => report(Path::new(arg(1)), number(2), &args[3.min(args.len())..]),
        "ramp" => ramp(arg(1), number(2)),
        other => {
            eprintln!("unknown helper {other:?}");
            2
        }
    }
}

fn mebibytes(count: u64) -> Vec<Vec<u8>> {
    (0..count).map(|_| vec![1_u8; MIB]).collect()
}

/// Allocates and touches `mib` MiB, then holds them.
fn alloc(mib: u64, hold_secs: u64) -> i32 {
    let held = mebibytes(mib);
    sleep(Duration::from_secs(hold_secs));
    black_box(&held);
    0
}

/// Starts a child that leaves the process group and the session, then
/// allocates like `alloc`.
fn detach(pid_file: &Path, mib: u64) -> i32 {
    let started = Command::new(env::current_exe().unwrap())
        .args(["helper", "setsid-sleep"])
        .arg(pid_file)
        .stdin(Stdio::null())
        .spawn();
    if started.is_err() || !wait_until(Duration::from_secs(10), || pid_file.exists()) {
        eprintln!("the detached child did not start");
        return 3;
    }
    alloc(mib, 60)
}

fn setsid_sleep(pid_file: &Path) -> i32 {
    if nix::unistd::setsid().is_err() {
        return 3;
    }
    write_atomic(pid_file, &process::id().to_string());
    sleep(Duration::from_secs(120));
    0
}

/// Writes what the command sees: its environment, folder, arguments, open
/// files and cgroup. Exits with `code`.
fn report(path: &Path, code: u64, args: &[String]) -> i32 {
    let fds: Vec<String> = fs::read_dir("/proc/self/fd")
        .map(|entries| {
            entries
                .filter_map(|entry| fs::read_link(entry.ok()?.path()).ok())
                .map(|target| target.display().to_string())
                .filter(|target| !target.starts_with("/proc/"))
                .collect()
        })
        .unwrap_or_default();
    let env: BTreeMap<String, String> = env::vars().collect();
    let report = json!({
        "env": env,
        "cwd": env::current_dir().ok(),
        "args": args,
        "fds": fds,
        "cgroup": fs::read_to_string("/proc/self/cgroup").unwrap_or_default().trim(),
    });
    write_atomic(path, &report.to_string());
    i32::try_from(code).unwrap_or(1)
}

/// Brings memory under pressure: allocates in steps of 64 MiB up to `size`
/// MiB (`machine`: the free room plus 1 GiB, so the machine must swap),
/// then keeps writing to every page for `hold_secs`, so the pages that were
/// swapped out come back in and out again.
fn ramp(size: &str, hold_secs: u64) -> i32 {
    let target = match size {
        "machine" => mem_available_mib() + 1024,
        mib => mib.parse().unwrap_or(0),
    };
    let mut held = Vec::new();
    while (held.len() as u64) < target {
        held.extend(mebibytes(64.min(target - held.len() as u64)));
        sleep(Duration::from_millis(250));
    }
    touch_pages(&mut held, Instant::now() + Duration::from_secs(hold_secs));
    0
}

fn touch_pages(held: &mut [Vec<u8>], until: Instant) {
    let mut pass = 1_u8;
    while Instant::now() < until {
        pass = pass.wrapping_add(1);
        for page in held.iter_mut().flat_map(|chunk| chunk.chunks_mut(PAGE)) {
            page[0] = pass;
        }
    }
    black_box(held);
}

fn mem_available_mib() -> u64 {
    let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))
        .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
        .map_or(0, |kib| kib / 1024)
}

fn write_atomic(path: &Path, text: &str) {
    let partial = path.with_extension("partial");
    if fs::write(&partial, text).is_ok() {
        let _ = fs::rename(&partial, path);
    }
}

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        sleep(Duration::from_millis(50));
    }
    done()
}

// ---- The runner ----

struct Suite {
    oomwrap: PathBuf,
    helper: PathBuf,
    root: tempfile::TempDir,
}

/// The folder and event log of one case.
struct Run {
    dir: PathBuf,
}

impl Suite {
    fn new() -> Self {
        let oomwrap = env::var_os("OOMWRAP_TEST_BIN").map_or_else(
            || PathBuf::from(env!("CARGO_BIN_EXE_oomwrap")),
            PathBuf::from,
        );
        Self {
            oomwrap,
            helper: env::current_exe().unwrap(),
            root: tempfile::Builder::new()
                .prefix("oomwrap-real-system-")
                .tempdir()
                .unwrap(),
        }
    }

    fn passes(&self, name: &str, case: fn(&Self) -> Outcome) -> bool {
        let started = Instant::now();
        let outcome = case(self);
        let secs = started.elapsed().as_secs_f32();
        match &outcome {
            Ok(()) => println!("case {name}: ok ({secs:.1}s)"),
            Err(error) => {
                println!("case {name}: FAILED ({secs:.1}s): {error}");
                let log = fs::read_to_string(self.run(name).events()).unwrap_or_default();
                println!("events of {name}:\n{log}");
            }
        }
        outcome.is_ok()
    }

    fn run(&self, name: &str) -> Run {
        let dir = self.root.path().join(name);
        fs::create_dir_all(dir.join("work")).unwrap();
        Run { dir }
    }

    /// `oomwrap run` with the generic profile, its own state, policy and
    /// event log, and `flags`; the caller adds `--` and the command.
    fn oomwrap(&self, run: &Run, flags: &[&str]) -> Command {
        self.oomwrap_from(&self.oomwrap, run, flags)
    }

    /// The same as [`Self::oomwrap`], with the oomwrap binary at `program`.
    fn oomwrap_from(&self, program: &Path, run: &Run, flags: &[&str]) -> Command {
        let mut cmd = Command::new(program);
        cmd.env("XDG_STATE_HOME", run.dir.join("state"))
            .env("XDG_CONFIG_HOME", run.dir.join("config"))
            .env(
                "OOMWRAP_SYSTEM_POLICY",
                run.dir.join("no-system-policy.toml"),
            )
            .args(["run", "--profile", "generic", "--allow-no-machine-guard"])
            .arg("--event-log")
            .arg(run.events())
            .args(flags);
        cmd
    }

    /// `oomwrap run <flags> -- <helper> helper <args>`.
    fn helper_job(&self, run: &Run, flags: &[&str], args: &[&str]) -> Command {
        let mut cmd = self.oomwrap(run, flags);
        self.add_helper(&mut cmd, args);
        cmd
    }

    fn add_helper(&self, cmd: &mut Command, args: &[&str]) {
        cmd.arg("--").arg(&self.helper).arg("helper").args(args);
    }
}

impl Run {
    fn events(&self) -> PathBuf {
        self.dir.join("events.jsonl")
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn read_events(&self) -> Result<Vec<Value>, String> {
        let text =
            fs::read_to_string(self.events()).map_err(|error| format!("no event log: {error}"))?;
        text.lines()
            .map(|line| {
                serde_json::from_str(line).map_err(|error| format!("bad event {line}: {error}"))
            })
            .collect()
    }

    fn event(&self, kind: &str) -> Result<Value, String> {
        self.read_events()?
            .into_iter()
            .find(|event| event["event"] == kind)
            .ok_or_else(|| format!("no {kind} event"))
    }

    fn report(&self) -> Result<Value, String> {
        let text = fs::read_to_string(self.file("report.json"))
            .map_err(|error| format!("the command wrote no report: {error}"))?;
        serde_json::from_str(&text).map_err(|error| error.to_string())
    }

    /// Checks one field of the first event of `kind`.
    fn expect(&self, kind: &str, pointer: &str, want: &str) -> Outcome {
        let event = self.event(kind)?;
        let got = event
            .pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or_default();
        check(got == want, || {
            format!("{kind} {pointer} is {got:?}, not {want:?}: {event}")
        })
    }

    /// The job's scope is gone after the run: no process of it is left.
    fn scope_gone(&self) -> Outcome {
        let launched = self.event("launched")?;
        let cgroup = launched["cgroup"]
            .as_str()
            .ok_or("launched has no cgroup")?;
        let dir = PathBuf::from(format!("/sys/fs/cgroup{cgroup}"));
        let gone = wait_until(Duration::from_secs(5), || !dir.exists());
        check(gone, || format!("the scope {cgroup} is still there"))
    }
}

fn check(holds: bool, message: impl FnOnce() -> String) -> Outcome {
    if holds { Ok(()) } else { Err(message()) }
}

/// Runs `cmd` for at most `limit`; a run that takes longer gets `SIGTERM`,
/// then `SIGKILL`. The command is this suite's own child.
fn run_bounded(cmd: &mut Command, limit: Duration) -> Result<ExitStatus, String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot start {cmd:?}: {error}"))?;
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            return Ok(status);
        }
        sleep(Duration::from_millis(50));
    }
    signal_own_child(child.id(), "-TERM");
    if !wait_until(Duration::from_secs(15), || {
        child.try_wait().ok().flatten().is_some()
    }) {
        signal_own_child(child.id(), "-KILL");
    }
    let _ = child.wait();
    Err(format!("the run took longer than {limit:?}"))
}

/// Signals one child that this suite started. `child.id()` is always a
/// real process ID above 1.
fn signal_own_child(pid: u32, signal: &str) {
    let _ = Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .status();
}

fn expect_code(status: ExitStatus, code: i32) -> Outcome {
    check(status.code() == Some(code), || {
        format!("exit status {status}, not {code}")
    })
}

/// Runs a job and checks its exit code.
fn expect_run(mut cmd: Command, code: i32, limit: Duration) -> Outcome {
    expect_code(run_bounded(&mut cmd, limit)?, code)
}

fn io_error(error: std::io::Error) -> String {
    error.to_string()
}

/// The `cgroup` backend with a small scope: no case may take more.
const CGROUP: &[&str] = &["--backend", "cgroup", "--memory-max", "256M"];
/// A scope that an allocation of 200 MiB fills.
const TINY_SCOPE: &[&str] = &[
    "--backend",
    "cgroup",
    "--memory-max",
    "64M",
    "--memory-swap-max",
    "0",
];

// ---- The host cases ----

/// An allocation past `--memory-max 64M`: the kernel's kill, `job_limit`,
/// exit 137, and no process left.
fn job_limit(suite: &Suite) -> Outcome {
    let run = suite.run("job_limit");
    let cmd = suite.helper_job(&run, TINY_SCOPE, &["alloc", "200", "60"]);
    expect_run(cmd, 137, Duration::from_secs(60))?;
    run.expect("stopped", "/reason", "job_limit")
        .and_then(|()| run.scope_gone())
}

/// A child that called `setsid()` ends with the stop of its job.
fn setsid_daemon(suite: &Suite) -> Outcome {
    let run = suite.run("setsid_daemon");
    let pid_file = run.file("daemon.pid");
    let cmd = suite.helper_job(
        &run,
        TINY_SCOPE,
        &["detach", pid_file.to_str().unwrap(), "200"],
    );
    expect_run(cmd, 137, Duration::from_secs(60))?;
    let pid = fs::read_to_string(&pid_file).map_err(|error| format!("no daemon PID: {error}"))?;
    let proc_dir = PathBuf::from(format!("/proc/{}", pid.trim()));
    let ended = wait_until(Duration::from_secs(5), || !is_daemon(&proc_dir));
    check(ended, || format!("the daemon {} still runs", pid.trim()))
}

/// Tells whether the process of `proc_dir` is still the detached helper,
/// and not a new process with a reused ID.
fn is_daemon(proc_dir: &Path) -> bool {
    fs::read(proc_dir.join("cmdline"))
        .is_ok_and(|cmdline| String::from_utf8_lossy(&cmdline).contains("setsid-sleep"))
}

/// A working set far above `--memory-high`: the job stalls, and the guard
/// stops it for `job_pressure`.
fn job_pressure(suite: &Suite) -> Outcome {
    let run = suite.run("job_pressure");
    let flags = [
        CGROUP,
        &[
            "--memory-high",
            "16M",
            "--memory-swap-max",
            "0",
            "--job-pressure",
            "10%",
            "--job-pressure-for",
            "2s",
        ],
    ]
    .concat();
    let cmd = suite.helper_job(&run, &flags, &["alloc", "128", "120"]);
    expect_run(cmd, 137, Duration::from_secs(120))?;
    run.expect("stopped", "/reason", "job_pressure")
        .and_then(|()| run.scope_gone())
}

/// The environment, the working folder, the arguments and the exit code
/// pass through `systemd-run` unchanged; the control socket reaches
/// `exec-job` but not the command.
fn pass_through(suite: &Suite) -> Outcome {
    let run = suite.run("pass_through");
    let report_path = run.file("report.json");
    let value = "two words, $HOME and a 'quote'";
    let mut cmd = suite.helper_job(
        &run,
        CGROUP,
        &["report", report_path.to_str().unwrap(), "7", "a b", ""],
    );
    cmd.env("OOMWRAP_TEST_VALUE", value)
        .current_dir(run.file("work"));
    let mut expected: BTreeMap<String, String> = env::vars().collect();
    for (name, value) in cmd.get_envs() {
        let name = name.to_string_lossy().into_owned();
        match value {
            Some(value) => expected.insert(name, value.to_string_lossy().into_owned()),
            None => expected.remove(&name),
        };
    }
    expect_run(cmd, 7, Duration::from_secs(30))?;
    let report = run.report()?;
    check_report(&report, &expected, &run.file("work"))
}

/// The variable that systemd sets for each unit, so also for the scope.
const UNIT_VARIABLE: &str = "INVOCATION_ID";

fn check_report(report: &Value, expected: &BTreeMap<String, String>, work: &Path) -> Outcome {
    let mut env: BTreeMap<String, String> =
        serde_json::from_value(report["env"].clone()).unwrap_or_default();
    let mut expected = expected.clone();
    env.remove(UNIT_VARIABLE);
    expected.remove(UNIT_VARIABLE);
    check(env == expected, || env_difference(&env, &expected))?;
    let cwd = report["cwd"].as_str().unwrap_or_default();
    check(Path::new(cwd) == work, || {
        format!("cwd {cwd:?}, not {}", work.display())
    })?;
    check(report["args"] == json!(["a b", ""]), || {
        format!("args {}", report["args"])
    })?;
    let fds = report["fds"].as_array().cloned().unwrap_or_default();
    let socket = fds
        .iter()
        .filter_map(Value::as_str)
        .find(|fd| fd.starts_with("socket:"));
    check(socket.is_none(), || {
        format!("the command holds a socket: {fds:?}")
    })?;
    let control = env.values().find(|value| value.ends_with("/control"));
    check(control.is_none(), || {
        format!("the command sees the control socket {control:?}")
    })?;
    let cgroup = report["cgroup"].as_str().unwrap_or_default();
    check(
        cgroup.contains("/oomwrap-generic-") && cgroup.ends_with(".scope"),
        || format!("the command runs in {cgroup}, not in a scope of its own"),
    )
}

fn env_difference(got: &BTreeMap<String, String>, want: &BTreeMap<String, String>) -> String {
    let extra: Vec<&String> = got
        .keys()
        .filter(|name| want.get(*name) != got.get(*name))
        .collect();
    let missing: Vec<&String> = want
        .keys()
        .filter(|name| !got.contains_key(*name))
        .collect();
    format!("the environment changed: different or new {extra:?}, missing {missing:?}")
}

/// `SIGTERM` to oomwrap reaches the command, and oomwrap exits with 143.
fn forwarded_signal(suite: &Suite) -> Outcome {
    let run = suite.run("forwarded_signal");
    let (got, ready) = (run.file("got"), run.file("ready"));
    let script = r#"trap 'echo TERM > "$1"; exit 42' TERM; : > "$2"; while :; do sleep 0.1; done"#;
    let mut cmd = suite.oomwrap(&run, CGROUP);
    cmd.args(["--", "bash", "-c", script, "job"])
        .arg(&got)
        .arg(&ready);
    let mut guard = cmd.stdin(Stdio::null()).spawn().map_err(io_error)?;
    let started = wait_until(Duration::from_secs(20), || ready.exists());
    // The guard is this suite's own child.
    signal_own_child(guard.id(), "-TERM");
    let finished = wait_until(Duration::from_secs(30), || {
        guard.try_wait().ok().flatten().is_some()
    });
    if !finished {
        signal_own_child(guard.id(), "-KILL");
    }
    let status = guard.wait().map_err(io_error)?;
    (check(started, || "the command never started".to_string()))?;
    (expect_code(status, 143))?;
    let text = fs::read_to_string(&got).unwrap_or_default();
    check(text.trim() == "TERM", || {
        format!("the command got {text:?}")
    })
}

/// `exec-job` runs through `/proc/<guard pid>/exe`, so the launch works
/// after the oomwrap file was replaced: the fake `systemd-run` below
/// replaces it and then runs the real one.
fn replaced_binary(suite: &Suite) -> Outcome {
    let run = suite.run("replaced_binary");
    let copy = run.file("oomwrap");
    fs::copy(&suite.oomwrap, &copy).map_err(io_error)?;
    let systemd_run = run.file("systemd-run");
    let script = "#!/bin/sh\nrm -f \"$OOMWRAP_COPY\"\nprintf '#!/bin/sh\\nexit 99\\n' > \"$OOMWRAP_COPY\"\nchmod +x \"$OOMWRAP_COPY\"\nexec systemd-run \"$@\"\n";
    fs::write(&systemd_run, script).map_err(io_error)?;
    make_executable(&systemd_run);
    let report_path = run.file("report.json");
    let mut cmd = suite.oomwrap_from(&copy, &run, CGROUP);
    suite.add_helper(&mut cmd, &["report", report_path.to_str().unwrap(), "0"]);
    cmd.env("OOMWRAP_COPY", &copy)
        .env("OOMWRAP_SYSTEMD_RUN", &systemd_run);
    expect_run(cmd, 0, Duration::from_secs(30))?;
    let replaced = fs::read_to_string(&copy).unwrap_or_default();
    check(replaced.contains("exit 99"), || {
        "the fake systemd-run did not replace the file".to_string()
    })?;
    let cgroup = run.report()?["cgroup"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    check(cgroup.ends_with(".scope"), || {
        format!("the command ran in {cgroup}")
    })
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
}

/// Inside a scope with its own `MemoryMax`, `auto` takes `poll` and the
/// scope is the memory domain.
fn outer_scope_limit(suite: &Suite) -> Outcome {
    let run = suite.run("outer_scope_limit");
    let unit = format!("oomwrap-test-outer-{}-{}", process::id(), unix_secs());
    let inner = suite.helper_job(
        &run,
        &[],
        &["report", run.file("report.json").to_str().unwrap(), "0"],
    );
    let mut cmd = Command::new("systemd-run");
    cmd.args([
        "--user",
        "--scope",
        "--quiet",
        "--collect",
        "--unit",
        &unit,
        "-p",
        "MemoryMax=384M",
        "--",
    ])
    .arg(inner.get_program())
    .args(inner.get_args())
    .envs(
        inner
            .get_envs()
            .filter_map(|(name, value)| Some((name, value?))),
    );
    expect_run(cmd, 0, Duration::from_secs(30))?;
    (run.expect("launched", "/backend", "poll"))?;
    (run.expect("launched", "/domain/kind", "cgroup"))?;
    let launched = run.event("launched")?;
    let path = launched["domain"]["path"].as_str().unwrap_or_default();
    check(path.ends_with(&format!("/{unit}.scope")), || {
        format!("the domain is {path}")
    })
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

// ---- The container cases ----

/// Without a service manager, `auto` takes `poll`, and the container's
/// limit makes its cgroup the memory domain.
fn container_auto(suite: &Suite) -> Outcome {
    let run = suite.run("container_auto");
    let cmd = suite.helper_job(
        &run,
        &[],
        &["report", run.file("report.json").to_str().unwrap(), "0"],
    );
    expect_run(cmd, 0, Duration::from_secs(30))?;
    (run.expect("launched", "/backend", "poll"))?;
    run.expect("launched", "/domain/kind", "cgroup")
}

/// The `poll` backend stops a job when the container's free room falls
/// below `--min-mem`.
fn container_floor(suite: &Suite) -> Outcome {
    let run = suite.run("container_floor");
    let cmd = suite.helper_job(
        &run,
        &["--min-mem", "400M", "--poll", "200ms"],
        &["alloc", "600", "60"],
    );
    expect_run(cmd, 137, Duration::from_secs(60))?;
    run.expect("stopped", "/reason", "domain_floor")
}
