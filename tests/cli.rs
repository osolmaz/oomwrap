use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::prelude::*;
use predicates::prelude::*;
use tempfile::{TempDir, tempdir};

const KIB_PER_GIB: u64 = 1024 * 1024;
/// The fake machine has 16 GiB.
const MACHINE_KIB: u64 = 16 * KIB_PER_GIB;
/// Free room far above every floor that the tests set.
const ROOMY_KIB: u64 = 10 * KIB_PER_GIB;
/// Free room above the default floor of 2.4 GiB, but below a quarter of the
/// machine: the machine is short of memory, so its pressure counts.
const SHORT_KIB: u64 = 3 * KIB_PER_GIB;
/// 1 MiB of free room: below the `2M` floor of the stop tests.
const LOW_KIB: u64 = 1024;
/// A guard waits up to 2 s and reads again before a stop for a domain
/// reason, so that the largest of the domain's jobs stops first.
const STAGGER_SECS: f32 = 2.0;

fn meminfo(available_kib: u64) -> String {
    format!(
        "MemTotal: {MACHINE_KIB} kB\nMemAvailable: {available_kib} kB\nSwapTotal: 8388608 kB\nSwapFree: 8388608 kB\n"
    )
}

/// A fake `systemd-run`: it records its arguments, makes the scope in the fake
/// cgroup tree from the `-p` limits, gives the job a fake `/proc/self/cgroup`
/// that names the scope, and runs only what follows `--`. The file
/// `systemd-run.mode` makes it fail, hang, write a wrong limit, or answer the
/// guard with an old protocol in place of `exec-job`.
const FAKE_SYSTEMD_RUN: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_DIR/systemd-run.log"
unit=; max=max; high=max; swap=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --) shift; break ;;
    --unit) unit=$2; shift 2 ;;
    -p) case "$2" in
          MemoryMax=*) max=${2#MemoryMax=} ;;
          MemoryHigh=*) high=${2#MemoryHigh=} ;;
          MemorySwapMax=*) swap=${2#MemorySwapMax=} ;;
        esac
        shift 2 ;;
    *) shift ;;
  esac
done
mode=$(cat "$FAKE_DIR/systemd-run.mode" 2>/dev/null)
case "$mode" in
  fail) echo "Failed to connect to bus: No medium found" >&2; exit 1 ;;
  hang) sleep 30; exit 1 ;;
  wrong-max) max=4096 ;;
esac
[ "$max" = infinity ] && max=max
[ "$high" = infinity ] && high=max
[ "$swap" = infinity ] && swap=max
path="$FAKE_SCOPE_PARENT/$unit.scope"
dir="$OOMWRAP_CGROUP_ROOT$path"
mkdir -p "$dir"
echo "$max" > "$dir/memory.max"
echo "$high" > "$dir/memory.high"
[ -n "$swap" ] && echo "$swap" > "$dir/memory.swap.max"
echo 4096 > "$dir/memory.current"
printf 'oom 0\noom_kill 0\n' > "$dir/memory.events"
echo $$ > "$dir/cgroup.procs"
: > "$dir/cgroup.kill"
job="$FAKE_DIR/job-proc"
mkdir -p "$job/self"
printf '0::%s\n' "$path" > "$job/self/cgroup"
if [ "$mode" = old-protocol ]; then
  exec python3 -c 'import json, os, socket, sys
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
s.sendall((json.dumps({"protocol": 0, "pid": os.getpid(), "cgroup": None, "oom_score_adj_error": None}) + "\n").encode())
s.recv(100)' "$4"
fi
OOMWRAP_PROC_ROOT="$job" exec "$@"
"#;

/// Shell text that sets `dir` to the job's fake scope folder.
const SCOPE_DIR: &str =
    r#"dir="$OOMWRAP_CGROUP_ROOT$(cut -d: -f3 "$OOMWRAP_PROC_ROOT/self/cgroup")""#;

const V2_MOUNTINFO: &str = "30 1 0:26 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n";

/// The cgroup of the user manager of the user who runs the tests.
fn user_manager() -> String {
    let uid = nix::unistd::getuid();
    format!("/user.slice/user-{uid}.slice/user@{uid}.service")
}

fn runs_as_root() -> bool {
    nix::unistd::geteuid().is_root()
}

/// Where `systemd-run --scope` puts a scope: under the user manager, or
/// under the system manager for root.
fn scope_parent() -> String {
    if runs_as_root() {
        "/system.slice".to_string()
    } else {
        format!("{}/app.slice", user_manager())
    }
}

fn page_size() -> u64 {
    let size = nix::unistd::sysconf(nix::unistd::SysconfVar::PAGE_SIZE)
        .unwrap()
        .unwrap();
    u64::try_from(size).unwrap()
}

fn psi(full_avg10: &str) -> String {
    format!(
        "some avg10={full_avg10} avg60=0.00 avg300=0.00 total=1\nfull avg10={full_avg10} avg60=0.00 avg300=0.00 total=1\n"
    )
}

/// A fake machine: a `/proc` tree that oomwrap reads for memory through
/// `OOMWRAP_PROC_ROOT`, and room for the test's own files. A test changes the
/// readings while a job runs. Processes stay real.
struct FakeMachine {
    dir: TempDir,
}

impl FakeMachine {
    fn new(available_kib: u64) -> Self {
        let machine = Self {
            dir: tempdir().unwrap(),
        };
        fs::create_dir_all(machine.proc_root().join("pressure")).unwrap();
        fs::write(machine.meminfo_path(), meminfo(available_kib)).unwrap();
        for dir in ["cgroup", "runtime", "linger"] {
            fs::create_dir_all(machine.file(dir)).unwrap();
        }
        write_executable(&machine.file("systemd-run"), FAKE_SYSTEMD_RUN);
        machine
    }

    /// The variables that point oomwrap at this machine. The fake
    /// `systemd-run` always stands in for the real one.
    fn env(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("OOMWRAP_PROC_ROOT", self.proc_root()),
            ("OOMWRAP_CGROUP_ROOT", self.file("cgroup")),
            ("OOMWRAP_SYSTEMD_RUN", self.file("systemd-run")),
            ("OOMWRAP_LINGER_DIR", self.file("linger")),
            ("XDG_STATE_HOME", self.state_home()),
            ("XDG_RUNTIME_DIR", self.file("runtime")),
            ("XDG_CONFIG_HOME", self.file("config")),
            ("OOMWRAP_SYSTEM_POLICY", self.file("etc/oomwrap.toml")),
            ("FAKE_DIR", self.dir.path().to_path_buf()),
            ("FAKE_SCOPE_PARENT", PathBuf::from(scope_parent())),
        ]
    }

    /// Makes a cgroup v2 host whose caller runs in `own`, with `cgroup.kill`.
    fn cgroup_v2(&self, own: &str) {
        fs::create_dir_all(self.proc_root().join("self")).unwrap();
        fs::write(self.proc_root().join("self/mountinfo"), V2_MOUNTINFO).unwrap();
        fs::write(self.proc_root().join("self/cgroup"), format!("0::{own}\n")).unwrap();
        let dir = self.cgroup_dir(own);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cgroup.kill"), "").unwrap();
    }

    /// A cgroup v2 host whose caller runs inside the user manager: every
    /// check of the `cgroup` backend passes.
    fn in_user_manager(&self) {
        self.cgroup_v2(&format!("{}/shell.service", scope_parent()));
    }

    fn cgroup_dir(&self, path: &str) -> PathBuf {
        self.file("cgroup").join(path.trim_start_matches('/'))
    }

    /// Writes the user's policy file.
    fn policy(&self, text: &str) -> PathBuf {
        let path = self.file("config/oomwrap/config.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        path
    }

    /// Writes the system policy file.
    fn system_policy(&self, text: &str) -> PathBuf {
        let path = self.file("etc/oomwrap.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        path
    }

    fn systemd_run_mode(&self, mode: &str) {
        fs::write(self.file("systemd-run.mode"), mode).unwrap();
    }

    fn systemd_run_log(&self) -> String {
        fs::read_to_string(self.file("systemd-run.log")).unwrap_or_default()
    }

    fn proc_root(&self) -> PathBuf {
        self.dir.path().join("proc")
    }

    /// A path for the test's own files, outside the fake `/proc`.
    fn file(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn meminfo_path(&self) -> PathBuf {
        self.proc_root().join("meminfo")
    }

    fn pressure_path(&self) -> PathBuf {
        self.proc_root().join("pressure/memory")
    }

    fn set_pressure(&self, full_avg10: &str) {
        fs::write(self.pressure_path(), psi(full_avg10)).unwrap();
    }

    /// A shell command that sets the free room while a job runs.
    fn shell_set_available(&self, available_kib: u64) -> String {
        format!(
            "printf '{}' > {}",
            meminfo(available_kib).replace('\n', "\\n"),
            sh_quote(&self.meminfo_path().display().to_string())
        )
    }

    /// A shell command that sets the pressure while a job runs.
    fn shell_set_pressure(&self, full_avg10: &str) -> String {
        format!(
            "printf '{}' > {}",
            psi(full_avg10).replace('\n', "\\n"),
            sh_quote(&self.pressure_path().display().to_string())
        )
    }

    /// The folder of the default event log of this machine's runs.
    fn state_home(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    fn oomwrap(&self) -> Command {
        let mut cmd = cli();
        cmd.envs(self.env());
        cmd
    }

    /// `oomwrap run` with the generic profile and no machine-wide guard
    /// required; the test adds its flags and its command.
    fn run(&self) -> Command {
        let mut cmd = self.oomwrap();
        cmd.args(["run", "--profile", "generic", "--allow-no-machine-guard"]);
        cmd
    }

    /// The same as [`Self::run`], as shell text.
    fn run_line(&self) -> String {
        let env: Vec<String> = self
            .env()
            .iter()
            .map(|(name, value)| format!("{name}={}", sh_quote(&value.display().to_string())))
            .collect();
        format!(
            "{} {} run --profile generic --allow-no-machine-guard",
            env.join(" "),
            sh_quote(&oomwrap_bin())
        )
    }
}

/// `oomwrap` with its state and policy folders under the test target
/// folder, so no test reads or writes the real home.
fn cli() -> Command {
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let mut cmd = oomwrap();
    cmd.env("XDG_STATE_HOME", tmp.join("state"))
        .env("XDG_CONFIG_HOME", tmp.join("config"))
        .env("OOMWRAP_SYSTEM_POLICY", tmp.join("no-system-policy.toml"));
    cmd
}

/// The binary under test: `OOMWRAP_TEST_BIN` when it is set, such as a
/// release build, and the build of this test run otherwise.
fn oomwrap_path() -> PathBuf {
    std::env::var_os("OOMWRAP_TEST_BIN").map_or_else(
        || PathBuf::from(env!("CARGO_BIN_EXE_oomwrap")),
        PathBuf::from,
    )
}

fn oomwrap() -> Command {
    Command::new(oomwrap_path())
}

fn oomwrap_bin() -> String {
    oomwrap_path().to_string_lossy().into_owned()
}

fn wait_for_file(path: &Path) -> bool {
    for _ in 0..500 {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn process_exists(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

fn wait_for_process_exit(pid: u32) {
    for _ in 0..100 {
        if !process_exists(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("process {pid} is still alive");
}

fn read_pid(path: &Path) -> u32 {
    fs::read_to_string(path).unwrap().trim().parse().unwrap()
}

fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn write_executable(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Runs a shim or wrapper with `dirs` in front of `PATH`, on a fake machine
/// with room and a machine-wide guard.
fn guarded_tool(tool: &Path, dirs: &[&Path], machine: &FakeMachine) -> Command {
    let system_path = std::env::var("PATH").unwrap_or_default();
    let mut parts: Vec<String> = dirs.iter().map(|dir| dir.display().to_string()).collect();
    parts.push(system_path);
    let mut cmd = Command::new(tool);
    cmd.env("PATH", parts.join(":"))
        .envs(machine.env())
        .env("OOMWRAP_MACHINE_GUARD_ACTIVE", "1");
    cmd
}

fn tar_paths(bytes: &[u8]) -> Vec<String> {
    let mut paths = Vec::new();
    let mut offset = 0;

    while offset + 512 <= bytes.len() {
        let header = &bytes[offset..offset + 512];
        if header.iter().all(|byte| *byte == 0) {
            break;
        }

        let name_end = header[..100]
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(100);
        paths.push(String::from_utf8(header[..name_end].to_vec()).unwrap());

        let size_text = String::from_utf8_lossy(&header[124..136]);
        let size_text = size_text.trim_matches(['\0', ' ']);
        let size = if size_text.is_empty() {
            0
        } else {
            usize::from_str_radix(size_text, 8).unwrap()
        };
        offset += 512 + size.div_ceil(512) * 512;
    }

    paths
}

/// Runs `shell_command` with bash on a fresh pseudo-terminal, writes `input`
/// to it, and returns the output, or `None` when `script` is not installed.
fn run_on_pty(shell_command: &str, input: &[u8]) -> Option<Output> {
    let available = Command::new("script")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !available {
        return None;
    }
    let wrapped = format!("bash -c {}", sh_quote(shell_command));
    let mut child = Command::new("script")
        .args(["-qfec", &wrapped, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(input).unwrap();
    drop(child.stdin.take());
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(5) {
            // The test's own `script` child.
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "timed out on the pseudo-terminal\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Some(child.wait_with_output().unwrap())
}

fn read_events(path: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn skillflag_lists_embedded_skill() {
    let mut cmd = cli();
    cmd.args(["--skill", "list", "--json"]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("\"id\":\"memory-safe-launch\""));
}

#[test]
fn skillflag_shows_canonical_skill() {
    let output = oomwrap()
        .args(["--skill", "show", "memory-safe-launch"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let expected = fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join(".agents/skills/memory-safe-launch/SKILL.md"),
    )
    .unwrap();
    assert_eq!(output.stdout, expected);
}

#[test]
fn skillflag_exports_complete_skill() {
    let output = oomwrap()
        .args(["--skill", "export", "memory-safe-launch"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let paths = tar_paths(&output.stdout);
    assert!(paths.contains(&"memory-safe-launch/SKILL.md".to_owned()));
    assert!(paths.contains(&"memory-safe-launch/agents/openai.yaml".to_owned()));
    assert!(paths.contains(
        &"memory-safe-launch/references/unified-memory-inference-recovery.md".to_owned()
    ));
}

#[test]
fn doctor_prints_status() {
    let machine = FakeMachine::new(ROOMY_KIB);
    machine.set_pressure("1.50");
    let mut cmd = machine.oomwrap();
    cmd.arg("doctor");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("oomwrap doctor"))
        .stdout(predicate::str::contains(
            "memory domain: machine size=16.0GiB free=10.0GiB pressure=1.50%",
        ));
}

#[test]
fn doctor_and_inspect_can_emit_json() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let mut doctor = machine.oomwrap();
    doctor.args(["doctor", "--json"]);
    doctor
        .assert()
        .success()
        .stdout(predicate::str::contains("\"machine_guards\""))
        .stdout(predicate::str::contains("\"kind\": \"machine\""));

    let mut inspect = machine.oomwrap();
    inspect.args(["inspect", "--json"]);
    inspect
        .assert()
        .success()
        .stdout(predicate::str::contains("\"default_tools\""))
        .stdout(predicate::str::contains("\"key\": \"min-mem\""))
        .stdout(predicate::str::contains(
            "\"value\": \"15%, at least 256M, at most 16G\"",
        ));
}

#[test]
fn inspect_prints_human_summary() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let mut cmd = machine.oomwrap();
    cmd.arg("inspect");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("oomwrap inspect"))
        .stdout(predicate::str::contains("(not present)"))
        .stdout(predicate::str::contains("backend: poll"));
}

#[test]
fn run_returns_child_exit_code() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let mut cmd = machine.run();
    cmd.args(["--", "bash", "-lc", "exit 7"]);
    cmd.assert().code(7);
}

#[test]
fn run_returns_signal_exit_code() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let mut cmd = machine.run();
    // The command's own shell ends itself.
    cmd.args(["--", "bash", "-lc", "kill -TERM $$"]);
    cmd.assert().code(143);
}

#[test]
fn run_allows_child_to_read_from_controlling_tty() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let shell_command = format!(
        "{} -- bash -lc {}",
        machine.run_line(),
        sh_quote("read x; echo got:$x")
    );
    let Some(output) = run_on_pty(&shell_command, b"from-tty\n") else {
        return;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "stdout:\n{stdout}");
    assert!(stdout.contains("got:from-tty"), "stdout:\n{stdout}");
}

#[test]
fn run_returns_the_terminal_to_the_caller() {
    let machine = FakeMachine::new(ROOMY_KIB);
    // When oomwrap keeps the terminal, the shell's read stops on SIGTTIN.
    let shell_command = format!("{} -- sleep 0.2; read x; echo after:$x", machine.run_line());
    let Some(output) = run_on_pty(&shell_command, b"from-tty\n") else {
        return;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "stdout:\n{stdout}");
    assert!(stdout.contains("after:from-tty"), "stdout:\n{stdout}");
}

#[test]
fn ctrl_z_stops_the_shell_job_and_fg_resumes_it() {
    let machine = FakeMachine::new(ROOMY_KIB);
    // The job stops itself as on Ctrl-Z. The shell must see a stopped job,
    // and after `fg` the job must hold the terminal again to read from it.
    let shell_command = format!(
        "set -m; {} -- bash -c {}; echo stopped:$?; fg; echo done:$?",
        machine.run_line(),
        sh_quote("kill -TSTP $$; read x; echo resumed:$x")
    );
    let Some(output) = run_on_pty(&shell_command, b"from-tty\n") else {
        return;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("stopped:148"), "stdout:\n{stdout}");
    assert!(stdout.contains("resumed:from-tty"), "stdout:\n{stdout}");
    assert!(stdout.contains("done:0"), "stdout:\n{stdout}");
}

#[test]
fn run_makes_the_job_the_first_victim_of_an_out_of_memory_kill() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let mut cmd = machine.run();
    cmd.args(["--", "cat", "/proc/self/oom_score_adj"]);
    cmd.assert().success().stdout("1000\n");
}

#[test]
fn the_control_socket_never_reaches_the_command() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let mut cmd = machine.run();
    cmd.args(["--", "bash", "-c", "ls -l /proc/$$/fd/"]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains(" 0 -> "))
        .stdout(predicate::str::contains("socket:").not());
}

#[test]
fn a_guarded_command_keeps_its_own_skill_flag() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let mut cmd = machine.run();
    cmd.args(["--", "echo", "--skill", "list"]);
    cmd.assert().success().stdout("--skill list\n");
}

#[test]
fn the_command_starts_once() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let starts = machine.file("starts");
    let mut cmd = machine.run();
    cmd.args([
        "--",
        "bash",
        "-c",
        &format!("echo start >> {}", starts.display()),
    ]);
    cmd.assert().success();
    assert_eq!(fs::read_to_string(starts).unwrap(), "start\n");
}

#[test]
fn a_command_that_cannot_run_ends_as_in_a_shell() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let event_log = machine.file("events.jsonl");
    let mut missing = machine.run();
    missing.args([
        "--event-log",
        event_log.to_str().unwrap(),
        "--",
        "oomwrap-test-no-such-program",
    ]);
    missing.assert().code(127).stderr(predicate::str::contains(
        "cannot run oomwrap-test-no-such-program",
    ));
    let events = read_events(&event_log);
    let failed = events
        .iter()
        .find(|event| event["event"] == "exec_failed")
        .unwrap();
    assert_eq!(failed["exit_code"], 127);
    assert!(events.iter().all(|event| event["event"] != "launched"));

    let not_executable = machine.file("data.txt");
    fs::write(&not_executable, "data").unwrap();
    let mut denied = machine.run();
    denied.args(["--", not_executable.to_str().unwrap()]);
    denied.assert().code(126);
}

/// The one file of the default event log of `machine`.
/// The one event log file of the day; the state folder may also hold the
/// record of wrapped paths.
fn default_log(machine: &FakeMachine) -> PathBuf {
    let dir = machine.state_home().join("oomwrap");
    let files: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.file_name().unwrap() != "wrapped-paths")
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    let name = files[0].file_name().unwrap().to_str().unwrap().to_string();
    assert!(
        name.starts_with("events-") && name.ends_with(".jsonl"),
        "{name}"
    );
    files[0].clone()
}

#[test]
fn the_default_event_log_holds_the_program_name_only() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let mut cmd = machine.run();
    cmd.args(["--", "bash", "-c", "exit 0", "--token=secret-value"]);
    cmd.assert().success();
    let log = default_log(&machine);
    let text = fs::read_to_string(&log).unwrap();
    assert!(!text.contains("secret-value"), "{text}");
    let events = read_events(&log);
    let kinds: Vec<_> = events.iter().map(|event| event["event"].clone()).collect();
    assert_eq!(kinds, ["preflight", "launched", "exited"]);
    for event in &events {
        assert_eq!(event["schema_version"], 1);
        assert_eq!(event["backend"], "poll");
        assert_eq!(event["program"], "bash");
        assert!(event.get("command").is_none());
    }
    let launched = &events[1];
    assert_eq!(launched["oom_score_adj"], 1000);
    assert!(launched["limits"]["min_free_bytes"].as_u64().unwrap() > 0);
    assert!(
        launched["detail"]
            .as_str()
            .unwrap()
            .contains("no job limit")
    );

    let mut logged = machine.run();
    logged.args([
        "--log-command",
        "--",
        "bash",
        "-c",
        "exit 0",
        "--token=secret-value",
    ]);
    logged.assert().success();
    assert!(fs::read_to_string(&log).unwrap().contains("secret-value"));
}

#[test]
fn the_event_log_can_be_turned_off_and_a_broken_default_only_warns() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let mut off = machine.run();
    off.args(["--event-log", "none", "--", "bash", "-c", "exit 0"]);
    off.assert().success();
    assert!(!machine.state_home().exists());

    let not_a_folder = machine.file("state-file");
    fs::write(&not_a_folder, "").unwrap();
    let mut broken = machine.run();
    broken
        .env("XDG_STATE_HOME", &not_a_folder)
        .args(["--", "bash", "-c", "exit 0"]);
    broken.assert().success().stderr(predicate::str::contains(
        "running without the default event log",
    ));

    let mut explicit = machine.run();
    explicit.args([
        "--event-log",
        not_a_folder.join("x.jsonl").to_str().unwrap(),
        "--",
        "bash",
        "-c",
        "exit 0",
    ]);
    explicit
        .assert()
        .code(2)
        .stderr(predicate::str::contains("failed to"));
}

#[test]
fn concurrent_runs_never_mix_event_lines() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let event_log = machine.file("events.jsonl");
    let long_argument = "x".repeat(16 * 1024);
    let runs: Vec<_> = (0..8)
        .map(|_| {
            let mut cmd = machine.run();
            cmd.args([
                "--log-command",
                "--event-log",
                event_log.to_str().unwrap(),
                "--",
                "bash",
                "-c",
                "exit 0",
                &long_argument,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
            cmd.spawn().unwrap()
        })
        .collect();
    for mut run in runs {
        assert!(run.wait().unwrap().success());
    }
    // Each line parses on its own: no write landed inside another.
    let events = read_events(&event_log);
    assert_eq!(events.len(), 8 * 3);
    assert!(
        events
            .iter()
            .all(|event| event["command"][3] == long_argument.as_str())
    );
}

#[test]
fn record_readings_writes_each_reading_and_decision() {
    let machine = FakeMachine::new(ROOMY_KIB);
    machine.set_pressure("1.25");
    let trace = machine.file("trace.jsonl");
    let mut cmd = machine.run();
    cmd.args([
        "--poll",
        "50ms",
        "--record-readings",
        trace.to_str().unwrap(),
        "--",
        "bash",
        "-c",
        "sleep 0.3",
    ]);
    cmd.assert().success();
    let lines = read_events(&trace);
    assert!(lines.len() >= 3, "{lines:?}");
    for line in &lines {
        assert_eq!(line["decision"], "continue");
        assert_eq!(line["readings"]["domain"]["size_bytes"], MACHINE_KIB * 1024);
        assert_eq!(line["readings"]["domain"]["full_avg10"], 1.25);
        assert!(line["readings"]["job"].is_null());
    }
    let elapsed: Vec<_> = lines
        .iter()
        .map(|line| line["readings"]["elapsed_ms"].as_u64().unwrap())
        .collect();
    assert!(
        elapsed.windows(2).all(|pair| pair[0] <= pair[1]),
        "{elapsed:?}"
    );
}

#[test]
fn inference_profiles_require_a_machine_guard_unless_overridden() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let guarded_run = |guard_active: &str, args: &[&str]| {
        let mut cmd = machine.oomwrap();
        cmd.env("OOMWRAP_MACHINE_GUARD_ACTIVE", guard_active)
            .env_remove("container")
            .arg("run")
            .args(args);
        cmd
    };
    let shell_exit = ["--", "bash", "-lc", "exit 0"];

    guarded_run("1", &[&["--profile", "vllm"][..], &shell_exit].concat())
        .assert()
        .success();
    guarded_run("0", &[&["--profile", "vllm"][..], &shell_exit].concat())
        .assert()
        .code(3)
        .stderr(predicate::str::contains(
            "profile vllm requires a machine-wide guard",
        ))
        .stderr(predicate::str::contains("container").not());
    guarded_run(
        "0",
        &[
            "--",
            "python3",
            "-m",
            "vllm.entrypoints.openai.api_server",
            "--help",
        ],
    )
    .assert()
    .code(3)
    .stderr(predicate::str::contains("requires a machine-wide guard"));
    guarded_run("0", &["--", "text-generation-launcher", "--help"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("profile tgi requires"));
    guarded_run(
        "0",
        &[
            &["--profile", "vllm", "--allow-no-machine-guard"][..],
            &shell_exit,
        ]
        .concat(),
    )
    .assert()
    .success();
    guarded_run(
        "1",
        &[
            &["--profile", "generic", "--require-machine-guard"][..],
            &shell_exit,
        ]
        .concat(),
    )
    .assert()
    .success();
    guarded_run(
        "0",
        &[
            &["--profile", "generic", "--require-machine-guard"][..],
            &shell_exit,
        ]
        .concat(),
    )
    .assert()
    .code(3);
}

#[test]
fn a_container_explains_why_no_machine_guard_is_visible() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let mut cmd = machine.oomwrap();
    cmd.env("OOMWRAP_MACHINE_GUARD_ACTIVE", "0")
        .env("container", "docker")
        .args(["run", "--profile", "vllm", "--", "bash", "-lc", "exit 0"]);
    cmd.assert()
        .code(3)
        .stderr(predicate::str::contains("inside a container"))
        .stderr(predicate::str::contains("--allow-no-machine-guard"));
}

#[test]
fn run_refuses_the_flags_of_0_1_0() {
    let machine = FakeMachine::new(ROOMY_KIB);
    for flags in [
        &["--min-swap", "0"][..],
        &["--require-earlyoom"][..],
        &["--allow-no-earlyoom"][..],
    ] {
        let mut cmd = machine.oomwrap();
        cmd.args(["run", "--profile", "generic"])
            .args(flags)
            .args(["--", "bash", "-lc", "exit 0"]);
        cmd.assert()
            .code(2)
            .stderr(predicate::str::contains("unexpected argument"));
    }
}

#[test]
fn explicit_limits_need_the_cgroup_backend() {
    // The fake machine has no cgroup v2 hierarchy.
    let machine = FakeMachine::new(ROOMY_KIB);
    let counter = machine.file("ran");
    let count = format!("echo ran >> {}", sh_quote(&counter.display().to_string()));
    for flags in [
        &["--memory-max", "1G"][..],
        &["--memory-high", "1G"][..],
        &["--memory-swap-max", "0"][..],
        &["--backend", "cgroup"][..],
    ] {
        let mut cmd = machine.run();
        cmd.args(flags).args(["--", "sh", "-c", &count]);
        cmd.assert().code(3).stderr(predicate::str::contains(
            "refusing to launch: the cgroup backend is not available: the host does not use cgroup v2 alone",
        ));
    }
    assert!(
        !counter.exists(),
        "the command must not run without its limits"
    );
    let events = read_events(&default_log(&machine));
    let refused = events
        .iter()
        .find(|event| event["event"] == "refused")
        .unwrap();
    assert_eq!(refused["reason"], "cgroup_unavailable");
    let mut poll = machine.run();
    poll.args(["--backend", "poll", "--memory-max", "1G", "--", "true"]);
    poll.assert()
        .code(2)
        .stderr(predicate::str::contains("drop --backend poll"));
    let mut old = machine.run();
    old.args(["--use-systemd-scope", "--", "true"]);
    old.assert()
        .code(2)
        .stderr(predicate::str::contains("--use-systemd-scope"));
}

#[test]
fn run_reads_sizes_in_binary_units_only() {
    let machine = FakeMachine::new(ROOMY_KIB);
    for bad in ["1GB", "1g", "1.5G", "101%"] {
        let mut cmd = machine.run();
        cmd.args(["--min-mem", bad, "--", "bash", "-lc", "exit 0"]);
        cmd.assert()
            .code(2)
            .stderr(predicate::str::contains("invalid --min-mem"));
    }
}

#[test]
fn run_allows_thresholds_equal_to_available_memory() {
    let machine = FakeMachine::new(2048);
    let mut cmd = machine.run();
    cmd.args(["--min-mem", "2M", "--", "bash", "-lc", "exit 0"]);
    cmd.assert().success();
}

#[test]
fn run_refuses_when_preflight_memory_is_below_floor() {
    let machine = FakeMachine::new(LOW_KIB);
    let mut cmd = machine.run();
    cmd.args(["--min-mem", "2M", "--", "bash", "-lc", "exit 0"]);
    cmd.assert()
        .code(4)
        .stderr(predicate::str::contains("refusing to launch: domain_floor"));
}

#[test]
fn run_default_floor_is_a_bounded_share_of_the_domain() {
    // 15% of 16 GiB is 2.4 GiB.
    let tight = FakeMachine::new(2 * KIB_PER_GIB);
    let mut cmd = tight.run();
    cmd.args(["--", "bash", "-lc", "exit 0"]);
    cmd.assert()
        .code(4)
        .stderr(predicate::str::contains("below --min-mem 2.4GiB"));

    let enough = FakeMachine::new(3 * KIB_PER_GIB);
    let mut cmd = enough.run();
    cmd.args(["--", "bash", "-lc", "exit 0"]);
    cmd.assert().success();

    let mut share = enough.run();
    share.args(["--min-mem", "25%", "--", "bash", "-lc", "exit 0"]);
    share
        .assert()
        .code(4)
        .stderr(predicate::str::contains("below --min-mem 4.0GiB"));
}

#[test]
fn run_refuses_a_floor_that_leaves_no_room() {
    let machine = FakeMachine::new(ROOMY_KIB);
    for floor in ["100%", "16G", "infinity"] {
        let mut cmd = machine.run();
        cmd.args(["--min-mem", floor, "--", "bash", "-lc", "exit 0"]);
        cmd.assert()
            .code(2)
            .stderr(predicate::str::contains("--min-mem"));
    }
}

#[test]
fn run_ignores_full_swap_while_room_and_pressure_are_fine() {
    // The false stop of 0.1.0: idle pages fill the swap, much RAM stays free.
    let machine = FakeMachine::new(ROOMY_KIB);
    fs::write(
        machine.meminfo_path(),
        format!(
            "MemTotal: {MACHINE_KIB} kB\nMemAvailable: {ROOMY_KIB} kB\nSwapTotal: 8388608 kB\nSwapFree: 0 kB\n"
        ),
    )
    .unwrap();
    machine.set_pressure("0.00");
    let mut cmd = machine.run();
    cmd.args(["--poll", "50ms", "--", "bash", "-lc", "sleep 0.3; exit 0"]);
    cmd.assert().success();
}

#[test]
fn the_pressure_of_a_machine_with_room_stops_nothing() {
    // One job at its own memory.max stalls a quiet machine: its system-wide
    // pressure is high while most of its memory is free.
    let machine = FakeMachine::new(ROOMY_KIB);
    machine.set_pressure("71.00");
    let mut cmd = machine.run();
    cmd.args([
        "--domain-pressure-for",
        "100ms",
        "--poll",
        "50ms",
        "--",
        "bash",
        "-lc",
        "sleep 0.5; exit 3",
    ]);
    cmd.assert().code(3);
}

#[test]
fn run_refuses_when_domain_pressure_already_holds() {
    let machine = FakeMachine::new(SHORT_KIB);
    machine.set_pressure("30.00");
    let mut cmd = machine.run();
    cmd.args(["--", "bash", "-lc", "exit 0"]);
    cmd.assert().code(4).stderr(predicate::str::contains(
        "refusing to launch: domain_pressure: memory pressure of the domain (machine) is 30.00%",
    ));

    let mut higher_limit = machine.run();
    higher_limit.args(["--domain-pressure", "50%", "--", "bash", "-lc", "exit 0"]);
    higher_limit.assert().success();
}

#[test]
fn run_stops_on_lasting_domain_pressure() {
    let machine = FakeMachine::new(SHORT_KIB);
    machine.set_pressure("0.00");
    let event_log = machine.file("events.jsonl");
    let script = format!(
        "sleep 0.2; {}; sleep 20",
        machine.shell_set_pressure("55.00")
    );
    let mut cmd = machine.run();
    cmd.args([
        "--domain-pressure-for",
        "300ms",
        "--poll",
        "50ms",
        "--term-grace",
        "100ms",
        "--event-log",
        event_log.to_str().unwrap(),
        "--",
        "bash",
        "-lc",
        &script,
    ]);
    let started = Instant::now();
    cmd.assert().code(137).stderr(predicate::str::contains(
        "stopping the job: domain_pressure",
    ));
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(450), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    let events = read_events(&event_log);
    let stop = events
        .iter()
        .find(|event| event["event"] == "stopped")
        .unwrap();
    assert_eq!(stop["reason"], "domain_pressure");
    assert_eq!(stop["readings"]["domain"]["full_avg10"], 55.0);
}

#[test]
fn run_reads_a_limited_cgroup_as_the_domain() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let cgroup_root = machine.file("cgroup");
    let job_slice = cgroup_root.join("job.slice");
    fs::create_dir_all(&job_slice).unwrap();
    fs::create_dir_all(machine.proc_root().join("self")).unwrap();
    fs::write(machine.proc_root().join("self/cgroup"), "0::/job.slice\n").unwrap();
    fs::write(job_slice.join("memory.max"), "1073741824\n").unwrap();
    fs::write(job_slice.join("memory.current"), "943718400\n").unwrap();
    fs::write(job_slice.join("memory.stat"), "anon 1\ninactive_file 0\n").unwrap();
    let event_log = machine.file("events.jsonl");

    let mut cmd = machine.run();
    cmd.env("OOMWRAP_CGROUP_ROOT", &cgroup_root).args([
        "--min-mem",
        "200M",
        "--event-log",
        event_log.to_str().unwrap(),
        "--",
        "bash",
        "-lc",
        "exit 0",
    ]);
    cmd.assert().code(4).stderr(predicate::str::contains(
        "the memory domain (cgroup /job.slice) has 124.0MiB free",
    ));
    let events = read_events(&event_log);
    assert_eq!(events[0]["domain"]["kind"], "cgroup");
    assert_eq!(events[0]["domain"]["path"], "/job.slice");
}

#[test]
fn run_kills_process_group_when_memory_drops() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let event_log = machine.file("events.jsonl");
    let script = format!(
        "sleep 0.2; {}; sleep 20",
        machine.shell_set_available(LOW_KIB)
    );

    let mut cmd = machine.run();
    cmd.args([
        "--min-mem",
        "2M",
        "--poll",
        "50ms",
        "--term-grace",
        "100ms",
        "--event-log",
        event_log.to_str().unwrap(),
        "--",
        "bash",
        "-lc",
        &script,
    ]);
    let started = Instant::now();
    cmd.assert().code(137);
    assert!(
        started.elapsed().as_secs() < 5,
        "guard should kill the sleeping process promptly"
    );
    let events = read_events(&event_log);
    let stop = events
        .iter()
        .find(|event| event["event"] == "stopped")
        .unwrap();
    assert_eq!(stop["reason"], "domain_floor");
    assert_eq!(stop["domain"]["kind"], "machine");
    assert_eq!(stop["readings"]["domain"]["free_bytes"], LOW_KIB * 1024);
}

#[test]
fn run_escalates_to_sigkill_when_child_ignores_sigterm() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let script = format!(
        "trap '' TERM; sleep 0.2; {}; sleep 20",
        machine.shell_set_available(LOW_KIB)
    );

    let mut cmd = machine.run();
    cmd.args([
        "--min-mem",
        "2M",
        "--poll",
        "50ms",
        "--term-grace",
        "800ms",
        "--",
        "bash",
        "-lc",
        &script,
    ]);
    let started = Instant::now();
    cmd.assert().code(137);
    let elapsed = started.elapsed();
    assert!(
        elapsed.as_millis() >= 700,
        "guard should wait for TERM grace before KILL"
    );
    assert!(
        elapsed.as_secs() < 10,
        "guard should escalate instead of waiting for the child sleep"
    );
}

#[test]
fn run_sends_one_sigterm_during_grace_window() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let term_log = machine.file("terms.log");
    let script = format!(
        "count=0; trap 'count=$((count+1)); echo term:$count >> {}; if [ $count -gt 1 ]; then exit 42; fi; sleep 0.4; exit 0' TERM; {}; while true; do sleep 1; done",
        term_log.display(),
        machine.shell_set_available(LOW_KIB)
    );

    let mut cmd = machine.run();
    cmd.args([
        "--min-mem",
        "2M",
        "--poll",
        "50ms",
        "--term-grace",
        "1s",
        "--",
        "bash",
        "-lc",
        &script,
    ]);
    let started = Instant::now();
    cmd.assert().code(137);

    assert!(
        started.elapsed() >= Duration::from_millis(350),
        "guard should let the first SIGTERM cleanup run"
    );
    assert_eq!(fs::read_to_string(term_log).unwrap(), "term:1\n");
}

#[test]
fn run_reaps_cooperative_term_without_waiting_full_grace() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let script = format!(
        "trap 'exit 0' TERM; sleep 0.2; {}; while true; do sleep 1; done",
        machine.shell_set_available(LOW_KIB)
    );

    let mut cmd = machine.run();
    cmd.args([
        "--min-mem",
        "2M",
        "--poll",
        "50ms",
        "--term-grace",
        "5s",
        "--",
        "bash",
        "-lc",
        &script,
    ]);
    let started = Instant::now();
    cmd.assert().code(137);
    assert!(
        started.elapsed().as_secs_f32() < STAGGER_SECS + 1.0,
        "guard should not wait full grace after the child exits on TERM"
    );
}

#[test]
fn run_kills_remaining_group_members_after_leader_exits() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let background_pid_path = machine.file("background.pid");
    let script = format!(
        "trap 'exit 0' TERM; (trap '' TERM; echo $BASHPID > {}; while true; do sleep 1; done) & sleep 0.2; {}; while true; do sleep 1; done",
        background_pid_path.display(),
        machine.shell_set_available(LOW_KIB)
    );

    let mut cmd = machine.run();
    cmd.args([
        "--min-mem",
        "2M",
        "--poll",
        "50ms",
        "--term-grace",
        "150ms",
        "--",
        "bash",
        "-lc",
        &script,
    ]);
    cmd.assert().code(137);
    wait_for_process_exit(read_pid(&background_pid_path));
}

#[test]
fn run_cleans_background_group_after_leader_exits() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let background_pid_path = machine.file("background.pid");
    let ready_path = machine.file("background.ready");
    let script = format!(
        "(exec >/dev/null 2>&1 < /dev/null; trap '' TERM; echo $BASHPID > {}; touch {}; while true; do sleep 1; done) & while [ ! -f {} ]; do sleep 0.01; done; exit 0",
        background_pid_path.display(),
        ready_path.display(),
        ready_path.display()
    );

    let mut cmd = machine.run();
    cmd.args(["--term-grace", "100ms", "--", "bash", "-lc", &script]);
    cmd.assert().success();
    wait_for_process_exit(read_pid(&background_pid_path));
}

#[test]
fn run_logs_group_cleanup_only_when_members_remain() {
    let machine = FakeMachine::new(ROOMY_KIB);
    for (script, members_remain) in [
        ("sleep 30 >/dev/null 2>&1 & exit 0", true),
        ("exit 0", false),
    ] {
        let event_log = machine.file(&format!("{members_remain}.jsonl"));
        let mut cmd = machine.run();
        cmd.args([
            "--term-grace",
            "100ms",
            "--event-log",
            event_log.to_str().unwrap(),
            "--",
            "bash",
            "-c",
            script,
        ]);
        cmd.assert().success();
        let events = fs::read_to_string(&event_log).unwrap();
        assert_eq!(events.contains("\"cleanup\""), members_remain, "{events}");
    }
}

#[test]
fn run_cleans_process_group_when_monitoring_fails_after_spawn() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let child_pid_path = machine.file("child.pid");
    let script = format!(
        "echo $$ > {}; rm {}; sleep 20",
        child_pid_path.display(),
        machine.meminfo_path().display()
    );

    let mut cmd = machine.run();
    cmd.args([
        "--min-mem",
        "2M",
        "--poll",
        "50ms",
        "--term-grace",
        "100ms",
        "--",
        "bash",
        "-lc",
        &script,
    ]);
    cmd.assert()
        .code(2)
        .stderr(predicate::str::contains("guard error after launch"))
        .stderr(predicate::str::contains("failed to read"));
    wait_for_process_exit(read_pid(&child_pid_path));
}

#[test]
fn run_cleans_process_group_when_guard_receives_term() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let child_pid_path = machine.file("child.pid");
    let script = format!(
        "echo $$ > {}; trap '' TERM; sleep 20",
        child_pid_path.display()
    );

    let mut cmd = machine.run();
    cmd.args([
        "--min-mem",
        "1M",
        "--poll",
        "1s",
        "--term-grace",
        "100ms",
        "--",
        "bash",
        "-lc",
        &script,
    ]);
    // The guard is this test's own child; only it receives the signals below.
    let mut guard = cmd.spawn().unwrap();
    if !wait_for_file(&child_pid_path) {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(guard.id().to_string())
            .status();
        let _ = guard.wait();
        panic!("timed out waiting for {}", child_pid_path.display());
    }
    let child_pid = read_pid(&child_pid_path);

    Command::new("kill")
        .arg("-TERM")
        .arg(guard.id().to_string())
        .assert()
        .success();
    let status = guard.wait().unwrap();

    assert_eq!(status.code(), Some(143));
    wait_for_process_exit(child_pid);
}

#[test]
fn run_rejects_invalid_durations_without_panic() {
    let machine = FakeMachine::new(ROOMY_KIB);
    for (flag, value) in [
        ("--poll", "-1"),
        ("--term-grace", "NaN"),
        ("--domain-pressure-for", "x"),
    ] {
        let mut cmd = machine.run();
        cmd.args([flag, value, "--", "bash", "-lc", "exit 0"]);
        cmd.assert()
            .code(2)
            .stderr(predicate::str::contains(format!("invalid {flag}")))
            .stderr(predicate::str::contains("panicked").not());
    }
    let mut pressure = machine.run();
    pressure.args(["--domain-pressure", "120%", "--", "bash", "-lc", "exit 0"]);
    pressure
        .assert()
        .code(2)
        .stderr(predicate::str::contains("invalid --domain-pressure"));
}

#[test]
fn run_handles_huge_durations_without_panic() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let mut poll = machine.run();
    poll.args([
        "--min-mem",
        "1M",
        "--poll",
        "10000000000000000000s",
        "--",
        "bash",
        "-lc",
        "sleep 0.1",
    ]);
    poll.assert()
        .success()
        .stderr(predicate::str::contains("panicked").not());

    let script = format!(
        "trap 'exit 0' TERM; sleep 0.2; {}; while true; do sleep 1; done",
        machine.shell_set_available(LOW_KIB)
    );
    let mut term_grace = machine.run();
    term_grace.args([
        "--min-mem",
        "2M",
        "--poll",
        "50ms",
        "--term-grace",
        "10000000000000000000s",
        "--",
        "bash",
        "-lc",
        &script,
    ]);
    let started = Instant::now();
    term_grace
        .assert()
        .code(137)
        .stderr(predicate::str::contains("panicked").not());
    assert!(
        started.elapsed().as_secs_f32() < STAGGER_SECS + 1.0,
        "cooperative child should not wait huge TERM grace"
    );
}

#[test]
fn path_shim_resolves_real_binary_without_recursing() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let shim_dir = machine.file("shims");
    let real_dir = machine.file("real");
    fs::create_dir_all(&real_dir).unwrap();
    write_executable(
        &real_dir.join("fake-vllm"),
        "#!/usr/bin/env bash\necho real-fake-vllm \"$@\"\n",
    );

    let mut install = cli();
    install.args([
        "install-shims",
        "--bin-dir",
        shim_dir.to_str().unwrap(),
        "--tool",
        "fake-vllm",
    ]);
    install.assert().success();

    let mut run = guarded_tool(
        &shim_dir.join("fake-vllm"),
        &[&shim_dir, &real_dir],
        &machine,
    );
    run.arg("smoke");
    run.assert()
        .success()
        .stdout(predicate::str::contains("real-fake-vllm smoke"));
}

#[test]
fn a_shim_takes_its_floor_from_the_policy_file() {
    // 2 GiB free is below the default floor of 2.4 GiB on the 16 GiB machine.
    let machine = FakeMachine::new(2 * KIB_PER_GIB);
    let shim_dir = machine.file("shims");
    let real_dir = machine.file("real");
    fs::create_dir_all(&real_dir).unwrap();
    write_executable(
        &real_dir.join("fake-tool"),
        "#!/usr/bin/env bash\necho ran \"$@\"\n",
    );

    let mut install = machine.oomwrap();
    install.args([
        "install-shims",
        "--bin-dir",
        shim_dir.to_str().unwrap(),
        "--tool",
        "fake-tool",
    ]);
    install.assert().success();
    let script = fs::read_to_string(shim_dir.join("fake-tool")).unwrap();
    assert!(script.contains("oomwrap script 2"), "{script}");
    assert!(
        script.contains(r#"run --shim "$tool" -- "$real" "$@""#),
        "{script}"
    );
    assert!(!script.contains("min-mem"), "{script}");

    let shim = shim_dir.join("fake-tool");
    let mut refused = guarded_tool(&shim, &[&shim_dir, &real_dir], &machine);
    refused
        .assert()
        .code(4)
        .stderr(predicate::str::contains("below --min-mem 2.4GiB"));

    machine.policy("[profile.generic]\nmin-mem = \"1G\"\n");
    let mut run = guarded_tool(&shim, &[&shim_dir, &real_dir], &machine);
    run.arg("x")
        .assert()
        .success()
        .stdout(predicate::str::contains("ran x"));
    let events = read_events(&default_log(&machine));
    assert!(events.iter().all(|event| event["shim"] == "fake-tool"));
}

#[test]
fn path_shim_can_run_wrapped_binary_later_in_path() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let shim_dir = machine.file("shims");
    let real_dir = machine.file("real");
    fs::create_dir_all(&real_dir).unwrap();
    let real = real_dir.join("vllm");
    write_executable(
        &real,
        "#!/usr/bin/env bash\necho wrapped-path-real \"$@\"\n",
    );

    let mut wrap = cli();
    wrap.args(["wrap", real.to_str().unwrap()]);
    wrap.assert().success();

    let mut install = cli();
    install.args([
        "install-shims",
        "--bin-dir",
        shim_dir.to_str().unwrap(),
        "--tool",
        "vllm",
    ]);
    install.assert().success();

    let mut run = guarded_tool(&shim_dir.join("vllm"), &[&shim_dir, &real_dir], &machine);
    run.arg("smoke");
    run.assert()
        .success()
        .stdout(predicate::str::contains("wrapped-path-real smoke"));
}

#[test]
fn path_shim_treats_its_tool_name_as_data() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let shim_dir = machine.file("shims");
    let payload = "$(touch sentinel)";
    let mut install = machine.oomwrap();
    install
        .arg("install-shims")
        .arg("--bin-dir")
        .arg(&shim_dir)
        .args(["--tool", payload]);
    install.assert().success();

    let mut run = guarded_tool(&shim_dir.join(payload), &[&shim_dir], &machine);
    run.current_dir(machine.file(""));
    run.assert().code(127).stderr(predicate::str::contains(
        "could not resolve real executable",
    ));
    assert!(
        !machine.file("sentinel").exists(),
        "a generated shim ran its tool name as shell code"
    );
}

#[test]
fn install_default_shims_and_refuse_unmanaged_collision() {
    let dir = tempdir().unwrap();
    let shim_dir = dir.path().join("shims");

    let mut install = cli();
    install.args(["install-shims", "--bin-dir", shim_dir.to_str().unwrap()]);
    install.assert().success();
    assert!(shim_dir.join("vllm").exists());
    assert!(shim_dir.join("llama-server").exists());

    fs::write(shim_dir.join("custom-tool"), "not managed\n").unwrap();
    let mut collision = cli();
    collision.args([
        "install-shims",
        "--bin-dir",
        shim_dir.to_str().unwrap(),
        "--tool",
        "custom-tool",
    ]);
    collision
        .assert()
        .code(2)
        .stderr(predicate::str::contains("refusing to replace"));

    let mut uninstall = cli();
    uninstall.args([
        "uninstall-shims",
        "--bin-dir",
        shim_dir.to_str().unwrap(),
        "--tool",
        "vllm",
    ]);
    uninstall.assert().success();
    assert!(!shim_dir.join("vllm").exists());
}

#[test]
fn install_force_replaces_symlink_without_touching_target() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let shim_dir = machine.file("shims");
    let real_dir = machine.file("real");
    fs::create_dir_all(&shim_dir).unwrap();
    fs::create_dir_all(&real_dir).unwrap();

    let real = real_dir.join("fake-vllm");
    write_executable(&real, "#!/usr/bin/env bash\necho real-after-force \"$@\"\n");
    let original_real = fs::read_to_string(&real).unwrap();

    let shim = shim_dir.join("fake-vllm");
    symlink(&real, &shim).unwrap();

    let mut install = cli();
    install.args([
        "install-shims",
        "--force",
        "--bin-dir",
        shim_dir.to_str().unwrap(),
        "--tool",
        "fake-vllm",
    ]);
    install.assert().success();

    assert_eq!(fs::read_to_string(&real).unwrap(), original_real);
    assert!(
        !fs::symlink_metadata(&shim)
            .unwrap()
            .file_type()
            .is_symlink()
    );

    let mut run = guarded_tool(&shim, &[&shim_dir, &real_dir], &machine);
    run.arg("smoke");
    run.assert()
        .success()
        .stdout(predicate::str::contains("real-after-force smoke"));
}

#[test]
fn install_refuses_dangling_symlink_without_force() {
    let dir = tempdir().unwrap();
    let shim_dir = dir.path().join("shims");
    fs::create_dir_all(&shim_dir).unwrap();

    let shim = shim_dir.join("fake-vllm");
    symlink(dir.path().join("missing"), &shim).unwrap();

    let mut install = cli();
    install.args([
        "install-shims",
        "--bin-dir",
        shim_dir.to_str().unwrap(),
        "--tool",
        "fake-vllm",
    ]);
    install
        .assert()
        .code(2)
        .stderr(predicate::str::contains("refusing to replace"));
    assert!(
        fs::symlink_metadata(&shim)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn install_and_uninstall_reject_path_like_tool_names() {
    let dir = tempdir().unwrap();
    let shim_dir = dir.path().join("shims");
    let escaped = dir.path().join("escape");

    for tool in ["../escape", escaped.to_str().unwrap()] {
        let mut install = cli();
        install.args([
            "install-shims",
            "--bin-dir",
            shim_dir.to_str().unwrap(),
            "--tool",
            tool,
        ]);
        install
            .assert()
            .code(2)
            .stderr(predicate::str::contains("bare executable name"));
        assert!(!escaped.exists());
    }

    let mut uninstall = cli();
    uninstall.args([
        "uninstall-shims",
        "--bin-dir",
        shim_dir.to_str().unwrap(),
        "--tool",
        "../escape",
    ]);
    uninstall
        .assert()
        .code(2)
        .stderr(predicate::str::contains("bare executable name"));
}

#[test]
fn uninstall_shims_does_not_delete_absolute_wrapper() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let bin_dir = machine.file("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let target = bin_dir.join("vllm");
    write_executable(&target, "#!/usr/bin/env bash\necho still-wrapped \"$@\"\n");

    let mut wrap = cli();
    wrap.args(["wrap", target.to_str().unwrap()]);
    wrap.assert().success();

    let mut uninstall = cli();
    uninstall.args([
        "uninstall-shims",
        "--bin-dir",
        bin_dir.to_str().unwrap(),
        "--tool",
        "vllm",
    ]);
    uninstall.assert().success();

    assert!(target.exists());
    assert!(target.with_file_name("vllm.real").exists());

    let mut guarded = guarded_tool(&target, &[], &machine);
    guarded.arg("after-uninstall");
    guarded
        .assert()
        .success()
        .stdout(predicate::str::contains("still-wrapped after-uninstall"));
}

#[test]
fn wrap_and_unwrap_round_trip() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let target = machine.file("vllm");
    write_executable(&target, "#!/usr/bin/env bash\necho wrapped-real \"$@\"\n");

    let mut wrap = cli();
    wrap.args(["wrap", target.to_str().unwrap()]);
    wrap.assert().success();
    assert!(target.with_file_name("vllm.real").exists());

    let mut guarded = guarded_tool(&target, &[], &machine);
    guarded.arg("absolute");
    guarded
        .assert()
        .success()
        .stdout(predicate::str::contains("wrapped-real absolute"));

    let mut unwrap = cli();
    unwrap.args(["unwrap", target.to_str().unwrap()]);
    unwrap.assert().success();
    assert!(!target.with_file_name("vllm.real").exists());

    let mut restored = Command::new(&target);
    restored.arg("restored");
    restored
        .assert()
        .success()
        .stdout(predicate::str::contains("wrapped-real restored"));
}

#[test]
fn unwrap_restores_original_symlink_even_if_target_is_missing() {
    let dir = tempdir().unwrap();
    let actual = dir.path().join("actual-vllm");
    let target = dir.path().join("vllm");
    write_executable(&actual, "#!/usr/bin/env bash\necho actual \"$@\"\n");
    symlink(&actual, &target).unwrap();

    let mut wrap = cli();
    wrap.args(["wrap", target.to_str().unwrap()]);
    wrap.assert().success();

    fs::remove_file(&actual).unwrap();

    let mut unwrap = cli();
    unwrap.args(["unwrap", target.to_str().unwrap()]);
    unwrap.assert().success();

    assert_eq!(fs::read_link(&target).unwrap(), actual);
    assert!(!target.with_file_name("vllm.real").exists());
}

#[test]
fn unwrap_symlink_to_wrapper_uses_wrapper_sidecar() {
    let dir = tempdir().unwrap();
    let real_dir = dir.path().join("real");
    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&real_dir).unwrap();
    fs::create_dir_all(&bin_dir).unwrap();

    let target = real_dir.join("vllm");
    write_executable(&target, "#!/usr/bin/env bash\necho actual \"$@\"\n");

    let mut wrap = cli();
    wrap.args(["wrap", target.to_str().unwrap()]);
    wrap.assert().success();

    let symlink_path = bin_dir.join("vllm");
    let unrelated_sidecar = bin_dir.join("vllm.real");
    symlink(&target, &symlink_path).unwrap();
    fs::write(&unrelated_sidecar, "unrelated\n").unwrap();

    let mut unwrap = cli();
    unwrap.args(["unwrap", symlink_path.to_str().unwrap()]);
    unwrap.assert().success();

    assert_eq!(fs::read_link(&symlink_path).unwrap(), target);
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        "#!/usr/bin/env bash\necho actual \"$@\"\n"
    );
    assert!(!target.with_file_name("vllm.real").exists());
    assert_eq!(
        fs::read_to_string(&unrelated_sidecar).unwrap(),
        "unrelated\n"
    );
}

#[test]
fn absolute_wrapper_resolves_symlink_to_real_path() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let real_dir = machine.file("real");
    let bin_dir = machine.file("bin");
    fs::create_dir_all(&real_dir).unwrap();
    fs::create_dir_all(&bin_dir).unwrap();

    let target = real_dir.join("vllm");
    write_executable(&target, "#!/usr/bin/env bash\necho symlink-real \"$@\"\n");

    let mut wrap = cli();
    wrap.args(["wrap", target.to_str().unwrap()]);
    wrap.assert().success();

    let symlink_path = bin_dir.join("vllm");
    symlink(&target, &symlink_path).unwrap();

    let mut guarded = guarded_tool(&symlink_path, &[], &machine);
    guarded.arg("via-symlink");
    guarded
        .assert()
        .success()
        .stdout(predicate::str::contains("symlink-real via-symlink"));
}

#[test]
fn wrapped_inference_tool_still_requires_a_machine_guard() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let target = machine.file("vllm");
    write_executable(&target, "#!/usr/bin/env bash\necho should-not-run\n");

    let mut wrap = cli();
    wrap.args(["wrap", target.to_str().unwrap()]);
    wrap.assert().success();

    let mut guarded = guarded_tool(&target, &[], &machine);
    guarded.env("OOMWRAP_MACHINE_GUARD_ACTIVE", "0");
    guarded
        .assert()
        .code(3)
        .stderr(predicate::str::contains("requires a machine-wide guard"));
}

#[test]
fn absolute_wrapper_treats_its_file_name_as_data() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let target = machine.file("$(touch sentinel)");
    write_executable(&target, "#!/usr/bin/env bash\necho real-ran\n");

    let mut wrap = machine.oomwrap();
    wrap.arg("wrap").arg(&target);
    wrap.assert().success();

    let mut guarded = guarded_tool(&target, &[], &machine);
    guarded.current_dir(machine.file(""));
    guarded
        .assert()
        .success()
        .stdout(predicate::str::contains("real-ran"));
    assert!(
        !machine.file("sentinel").exists(),
        "a generated wrapper ran its file name as shell code"
    );
    let events = read_events(&default_log(&machine));
    assert_eq!(event(&events, "launched")["shim"], "$(touch sentinel)");
}

#[test]
fn wrap_and_unwrap_validate_bad_inputs() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("missing");
    let mut wrap_missing = cli();
    wrap_missing.args(["wrap", missing.to_str().unwrap()]);
    wrap_missing
        .assert()
        .code(2)
        .stderr(predicate::str::contains("cannot wrap missing path"));

    let directory = dir.path().join("directory");
    fs::create_dir(&directory).unwrap();
    let mut wrap_directory = cli();
    wrap_directory.args(["wrap", directory.to_str().unwrap()]);
    wrap_directory
        .assert()
        .code(2)
        .stderr(predicate::str::contains("cannot wrap non-file path"));

    let target = dir.path().join("not-executable");
    fs::write(&target, "plain text\n").unwrap();
    let mut wrap_non_executable = cli();
    wrap_non_executable.args(["wrap", target.to_str().unwrap()]);
    wrap_non_executable
        .assert()
        .code(2)
        .stderr(predicate::str::contains("cannot wrap non-executable path"));

    let mut unwrap_plain = cli();
    unwrap_plain.args(["unwrap", target.to_str().unwrap()]);
    unwrap_plain
        .assert()
        .code(2)
        .stderr(predicate::str::contains("not an oomwrap managed wrapper"));
}

#[test]
fn wrap_refuses_existing_real_path_without_force() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("vllm");
    write_executable(&target, "#!/usr/bin/env bash\necho target\n");
    fs::write(dir.path().join("vllm.real"), "already exists\n").unwrap();

    let mut cmd = cli();
    cmd.args(["wrap", target.to_str().unwrap()]);
    cmd.assert()
        .code(2)
        .stderr(predicate::str::contains("refusing to overwrite"));
}

#[test]
fn wrap_refuses_dangling_real_sidecar_without_force() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("vllm");
    write_executable(&target, "#!/usr/bin/env bash\necho target\n");
    symlink(dir.path().join("missing"), dir.path().join("vllm.real")).unwrap();

    let mut cmd = cli();
    cmd.args(["wrap", target.to_str().unwrap()]);
    cmd.assert()
        .code(2)
        .stderr(predicate::str::contains("refusing to overwrite"));
}

const GIB: u64 = 1024 * 1024 * 1024;

/// A file that counts each start of the command, and the shell text that
/// adds one line to it.
fn counting(machine: &FakeMachine) -> (PathBuf, String) {
    let counter = machine.file("ran");
    let count = format!("echo ran >> {}", sh_quote(&counter.display().to_string()));
    (counter, count)
}

fn runs(counter: &Path) -> usize {
    fs::read_to_string(counter).map_or(0, |text| text.lines().count())
}

fn event<'a>(events: &'a [serde_json::Value], kind: &str) -> &'a serde_json::Value {
    events
        .iter()
        .find(|event| event["event"] == kind)
        .unwrap_or_else(|| panic!("no {kind} event in {events:?}"))
}

fn detail(event: &serde_json::Value) -> &str {
    event["detail"].as_str().unwrap_or_default()
}

#[test]
fn the_cgroup_backend_starts_the_job_in_a_scope_with_its_limits() {
    let machine = FakeMachine::new(ROOMY_KIB);
    machine.in_user_manager();
    let (counter, count) = counting(&machine);
    let locks = machine.file("runtime/oomwrap");
    let script = format!(
        "{count}; ls {}; cat /proc/self/oom_score_adj; exit 7",
        sh_quote(&locks.display().to_string())
    );
    let mut cmd = machine.run();
    cmd.args(["--min-mem", "1G", "--", "sh", "-c", &script]);
    let output = cmd.assert().code(7).get_output().stdout.clone();
    let stdout = String::from_utf8(output).unwrap();
    assert_eq!(runs(&counter), 1);
    assert!(stdout.starts_with("oomwrap-generic-"), "{stdout}");
    assert!(stdout.contains(".lock\n"), "{stdout}");
    assert!(stdout.ends_with("1000\n"), "{stdout}");
    assert_eq!(
        fs::read_dir(&locks).unwrap().count(),
        0,
        "the lock goes with its guard"
    );

    // 10G free minus the 1G floor is below 85% of the 16G machine.
    let max = 9 * GIB;
    let high = max * 90 / 100 / page_size() * page_size();
    let log = machine.systemd_run_log();
    let user = if runs_as_root() { "" } else { "--user " };
    assert!(
        log.starts_with(&format!(
            "{user}--scope --quiet --collect --unit oomwrap-generic-"
        )),
        "{log}"
    );
    assert!(
        log.contains(&format!(
            "-p MemoryMax={max} -p MemoryHigh={high} -- /proc/"
        )),
        "{log}"
    );
    assert!(!log.contains("MemorySwapMax"), "{log}");

    let events = read_events(&default_log(&machine));
    let launched = event(&events, "launched");
    let unit = launched["unit"].as_str().unwrap();
    assert_eq!(launched["backend"], "cgroup");
    assert_eq!(
        launched["cgroup"],
        format!("{}/{unit}.scope", scope_parent())
    );
    assert_eq!(launched["job_limits"]["memory_max"], max);
    assert_eq!(launched["job_limits"]["memory_high"], high);
    assert!(detail(launched).contains("holds the job limits"));
    assert_eq!(event(&events, "exited")["exit_code"], 7);
}

#[test]
fn explicit_limits_go_to_the_scope_in_whole_pages() {
    let machine = FakeMachine::new(ROOMY_KIB);
    machine.in_user_manager();
    let mut cmd = machine.run();
    cmd.args([
        "--memory-max",
        "100000001",
        "--memory-swap-max",
        "0",
        "--backend",
        "cgroup",
        "--",
        "true",
    ]);
    cmd.assert().success();
    let max = 100_000_001 / page_size() * page_size();
    let high = 100_000_001 * 90 / 100 / page_size() * page_size();
    let log = machine.systemd_run_log();
    assert!(
        log.contains(&format!(
            "-p MemoryMax={max} -p MemoryHigh={high} -p MemorySwapMax=0 --"
        )),
        "{log}"
    );
}

/// Runs `sh -c <script>` in a fake scope with `flags`; the script finds its
/// scope folder in `$dir`.
fn run_in_fake_scope(machine: &FakeMachine, flags: &[&str], script: &str) -> Output {
    let mut cmd = machine.run();
    cmd.args(flags)
        .args(["--", "sh", "-c", &format!("{SCOPE_DIR}; {script}")]);
    cmd.output().unwrap()
}

#[test]
fn a_kernel_kill_in_the_scope_ends_the_job_at_once() {
    for (counts, reason) in [
        ("oom 1\\noom_kill 1", "job_limit"),
        ("oom 0\\noom_kill 1", "outside_oom_kill"),
    ] {
        let machine = FakeMachine::new(ROOMY_KIB);
        machine.in_user_manager();
        let scope_file = machine.file("scope");
        let script = format!(
            "echo \"$dir\" > {}; sleep 0.2; printf '{counts}\\n' > \"$dir/memory.events\"; sleep 30",
            sh_quote(&scope_file.display().to_string())
        );
        let started = Instant::now();
        let output = run_in_fake_scope(
            &machine,
            &["--poll", "50ms", "--term-grace", "20s"],
            &script,
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(137), "{stderr}");
        assert!(
            stderr.contains(&format!("stopping the job: {reason}")),
            "{stderr}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a kernel kill gets no grace"
        );
        let dir = PathBuf::from(fs::read_to_string(&scope_file).unwrap().trim());
        assert_eq!(fs::read_to_string(dir.join("cgroup.kill")).unwrap(), "1");
        let events = read_events(&default_log(&machine));
        let stopped = event(&events, "stopped");
        assert_eq!(stopped["reason"], reason);
        assert_eq!(stopped["readings"]["job"]["events"]["oom_kill"], 1);
    }
}

#[test]
fn a_job_killed_at_its_limit_ends_as_job_limit_after_its_scope_is_gone() {
    let machine = FakeMachine::new(ROOMY_KIB);
    machine.in_user_manager();
    // systemd removes the scope once the kernel killed its last process.
    let script = "printf 'oom 1\\noom_kill 1\\n' > \"$dir/memory.events\"; sleep 0.3; mv \"$dir\" \"$dir.gone\"; exit 0";
    let output = run_in_fake_scope(&machine, &["--poll", "10s"], script);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(137), "{stderr}");
    assert!(stderr.contains("stopping the job: job_limit"), "{stderr}");
    let events = read_events(&default_log(&machine));
    assert_eq!(event(&events, "stopped")["reason"], "job_limit");
}

#[test]
fn lasting_job_pressure_stops_the_job() {
    let machine = FakeMachine::new(ROOMY_KIB);
    machine.in_user_manager();
    let script = format!(
        "sleep 0.2; printf '{}' > \"$dir/memory.pressure\"; sleep 30",
        psi("70.00").replace('\n', "\\n")
    );
    let flags = [
        "--poll",
        "50ms",
        "--job-pressure",
        "50%",
        "--job-pressure-for",
        "100ms",
        "--term-grace",
        "100ms",
    ];
    let output = run_in_fake_scope(&machine, &flags, &script);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(137), "{stderr}");
    assert!(
        stderr.contains(
            "job_pressure: memory pressure of the job is 70.00%, at or above --job-pressure 50.00%"
        ),
        "{stderr}"
    );
}

#[test]
fn a_failed_scope_launch_falls_back_to_poll_once_or_exits_with_3() {
    for (mode, cause) in [
        ("wrong-max", "the scope's memory.max is 4096B"),
        ("fail", "before its report"),
        ("old-protocol", "exec-job speaks protocol version 0"),
    ] {
        let machine = FakeMachine::new(ROOMY_KIB);
        machine.in_user_manager();
        machine.systemd_run_mode(mode);
        let (counter, count) = counting(&machine);
        let mut auto = machine.run();
        auto.args(["--", "sh", "-c", &count]);
        auto.assert().success().stderr(predicate::str::contains(
            "starting the command with the poll backend",
        ));
        assert_eq!(runs(&counter), 1, "{mode}: the command starts once");
        let events = read_events(&default_log(&machine));
        let launched = event(&events, "launched");
        assert_eq!(launched["backend"], "poll", "{mode}");
        assert!(launched.get("unit").is_none(), "{mode}");
        assert!(
            detail(launched).starts_with("auto took poll: the cgroup launch failed: "),
            "{mode}: {launched}"
        );
        assert!(detail(launched).contains(cause), "{mode}: {launched}");

        let mut required = machine.run();
        required.args(["--backend", "cgroup", "--", "sh", "-c", &count]);
        required
            .assert()
            .code(3)
            .stderr(predicate::str::contains(cause));
        assert_eq!(
            runs(&counter),
            1,
            "{mode}: --backend cgroup never starts it"
        );
    }
}

#[test]
fn a_signal_during_the_scope_launch_ends_oomwrap_before_the_command() {
    let machine = FakeMachine::new(ROOMY_KIB);
    machine.in_user_manager();
    machine.systemd_run_mode("hang");
    let (counter, count) = counting(&machine);
    let mut cmd = machine.run();
    cmd.args(["--", "sh", "-c", &count])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // The guard is this test's own child; only it receives the signal.
    let mut guard = cmd.spawn().unwrap();
    let started = wait_for_file(&machine.file("systemd-run.log"));
    std::thread::sleep(Duration::from_millis(100));
    Command::new("kill")
        .arg("-TERM")
        .arg(guard.id().to_string())
        .assert()
        .success();
    let status = guard.wait().unwrap();
    assert!(started, "the fake systemd-run never ran");
    assert_eq!(status.code(), Some(143));
    assert_eq!(runs(&counter), 0);
}

#[test]
fn auto_keeps_the_job_in_place_when_a_scope_would_leave_a_limit() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let own = format!("{}/limited.service", scope_parent());
    machine.cgroup_v2(&own);
    fs::write(machine.cgroup_dir(&own).join("memory.high"), "8589934592\n").unwrap();
    machine.run().args(["--", "true"]).assert().success();
    let events = read_events(&default_log(&machine));
    let launched = event(&events, "launched");
    assert_eq!(launched["backend"], "poll");
    assert!(
        detail(launched).contains(&format!("the caller's cgroup {own} sets memory.high")),
        "{launched}"
    );
    assert_eq!(machine.systemd_run_log(), "");
}

#[test]
fn a_session_caller_needs_lingering_for_a_scope() {
    if runs_as_root() {
        // The system manager needs no lingering.
        return;
    }
    let machine = FakeMachine::new(ROOMY_KIB);
    let uid = nix::unistd::getuid();
    machine.cgroup_v2(&format!("/user.slice/user-{uid}.slice/session-2.scope"));
    machine.run().args(["--", "true"]).assert().success();
    assert_eq!(machine.systemd_run_log(), "");
    let user = nix::unistd::User::from_uid(uid).unwrap().unwrap();
    fs::write(machine.file("linger").join(user.name), "").unwrap();
    machine.run().args(["--", "true"]).assert().success();
    assert!(machine.systemd_run_log().contains("--scope"));
    let text = fs::read_to_string(default_log(&machine)).unwrap();
    assert!(text.contains("lingering is off"), "{text}");
    assert!(text.contains("\"backend\":\"cgroup\""), "{text}");
}

#[test]
fn no_room_for_the_job_at_launch_refuses_with_4() {
    let machine = FakeMachine::new(KIB_PER_GIB);
    machine.in_user_manager();
    let mut cmd = machine.run();
    cmd.args(["--min-mem", "1G", "--", "true"]);
    cmd.assert().code(4).stderr(predicate::str::contains(
        "leaves the job no room above --min-mem 1.0GiB",
    ));
    assert_eq!(machine.systemd_run_log(), "");
}

#[test]
fn the_holder_of_a_scope_stays_until_the_guard_releases_its_lock() {
    let dir = tempfile::tempdir().unwrap();
    let lock_path = dir.path().join("oomwrap-generic-1.lock");
    let lock = fs::File::create(&lock_path).unwrap();
    lock.try_lock().unwrap();
    let output = oomwrap()
        .args(["hold-scope", "--lock"])
        .arg(&lock_path)
        .output()
        .unwrap();
    assert!(output.status.success());
    let holder: u32 = String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // The holder is no child of the process that started it, and has its
    // own process group, so the job's group signals never reach it.
    let stat = fs::read_to_string(format!("/proc/{holder}/stat")).unwrap();
    let fields: Vec<&str> = stat
        .rsplit(')')
        .next()
        .unwrap()
        .split_whitespace()
        .collect();
    assert_eq!(fields[2], holder.to_string(), "{stat}");
    std::thread::sleep(Duration::from_millis(200));
    assert!(process_exists(holder), "the holder must wait for the lock");
    drop(lock);
    wait_for_process_exit(holder);

    let mut gone = assert_cmd::Command::new(oomwrap_path());
    gone.args(["hold-scope", "--stay", "--lock"])
        .arg(dir.path().join("missing.lock"))
        .timeout(Duration::from_secs(5))
        .assert()
        .success();
}

#[test]
fn the_policy_file_sets_what_the_flags_leave_open() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let system = machine.system_policy("min-mem = \"12G\"\n");
    let mut refused = machine.run();
    refused.args(["--", "true"]);
    refused
        .assert()
        .code(4)
        .stderr(predicate::str::contains("below --min-mem 12.0GiB"));

    // The user's file comes before the system's, a flag before both.
    machine.policy("min-mem = \"1G\"\n");
    machine.run().args(["--", "true"]).assert().success();
    let mut flag = machine.run();
    flag.args(["--min-mem", "11G", "--", "true"]);
    flag.assert().code(4);

    let mut contradiction = machine.run();
    machine.policy("backend = \"poll\"\n");
    contradiction.args(["--memory-max", "1G", "--", "true"]);
    contradiction
        .assert()
        .code(2)
        .stderr(predicate::str::contains("drop --backend poll"));

    fs::write(&system, "min-swap = \"1G\"\n").unwrap();
    let mut invalid = machine.run();
    invalid.args(["--", "true"]);
    invalid
        .assert()
        .code(2)
        .stderr(predicate::str::contains(format!(
            "invalid policy file {}: unknown field `min-swap`",
            system.display()
        )));
}

#[test]
fn a_profile_section_can_lift_the_guard_requirement() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let mut vllm = machine.oomwrap();
    vllm.env("OOMWRAP_MACHINE_GUARD_ACTIVE", "0")
        .args(["run", "--profile", "vllm", "--", "true"]);
    vllm.assert().code(3);
    machine.policy("[profile.vllm]\nrequire-machine-guard = false\n");
    let mut lifted = machine.oomwrap();
    lifted.env("OOMWRAP_MACHINE_GUARD_ACTIVE", "0").args([
        "run",
        "--profile",
        "vllm",
        "--",
        "true",
    ]);
    lifted.assert().success();
}

#[test]
fn a_guard_that_the_policy_file_names_counts() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let name = format!("oomwrap-test-guard-{}", std::process::id());
    // The test's own child, which only the test ends.
    let mut guard = Command::new("bash")
        .args(["-c", &format!("exec -a {name} sleep 30")])
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    machine.policy(&format!("machine-guards = [\"{name}\"]\n"));
    let mut vllm = machine.oomwrap();
    vllm.args(["run", "--profile", "vllm", "--", "true"]);
    let status = vllm.output().unwrap().status;
    guard.kill().unwrap();
    guard.wait().unwrap();
    assert!(status.success(), "{status:?}");
}

#[test]
fn doctor_names_each_setting_its_source_and_outdated_scripts() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let policy = machine.policy("poll = \"2s\"\n[profile.generic]\nmin-mem = \"1G\"\n");
    let bin = machine.file("bin");
    fs::create_dir_all(&bin).unwrap();
    write_executable(
        &bin.join("vllm"),
        "#!/usr/bin/env bash\n# oomwrap managed path shim\nexec oomwrap run --min-swap 4G -- \"$real\"\n",
    );
    let mut doctor = machine.oomwrap();
    doctor.env("PATH", &bin).arg("doctor");
    let output = doctor.assert().success().get_output().stdout.clone();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("settings for profile generic:"), "{text}");
    assert!(
        text.contains(&format!(
            "min-mem: 1G ({} [profile.generic])",
            policy.display()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!("poll: 2s ({})", policy.display())),
        "{text}"
    );
    assert!(text.contains("term-grace: 10s (default)"), "{text}");
    assert!(text.contains("backend: poll (auto took poll: "), "{text}");
    assert!(
        text.contains(&format!(
            "comes from an earlier oomwrap and passes flags that this version does not have; write it again: oomwrap install-shims --bin-dir '{}' --tool 'vllm'",
            bin.display()
        )),
        "{text}"
    );

    let mut json = machine.oomwrap();
    json.env("PATH", &bin)
        .args(["doctor", "--json", "--profile", "vllm"]);
    let output = json.assert().success().get_output().stdout.clone();
    let report: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["profile"], "vllm");
    assert_eq!(report["scripts"][0]["kind"], "path_shim");
    assert_eq!(report["scripts"][0]["outdated"], true);
    let min_mem = report["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|setting| setting["key"] == "min-mem")
        .unwrap();
    assert_eq!(min_mem["source"]["kind"], "default");
}

#[test]
fn wrap_rewrites_an_old_wrapper_in_place_and_records_it() {
    let machine = FakeMachine::new(ROOMY_KIB);
    let target = machine.file("tool");
    write_executable(
        &target.with_file_name("tool.real"),
        "#!/usr/bin/env bash\necho real\n",
    );
    // A wrapper of an earlier version, with its own flags.
    write_executable(
        &target,
        "#!/usr/bin/env bash\n# oomwrap managed absolute wrapper\nexec oomwrap run --min-swap 4G -- \"$0.real\"\n",
    );
    let mut wrap = machine.oomwrap();
    wrap.arg("wrap").arg(&target);
    wrap.assert()
        .success()
        .stdout(predicate::str::contains("rewrote wrapper"));
    let script = fs::read_to_string(&target).unwrap();
    assert!(script.contains("oomwrap script 2"), "{script}");
    let record = machine.state_home().join("oomwrap/wrapped-paths");
    assert_eq!(
        fs::read_to_string(&record).unwrap(),
        format!("{}\n", target.display())
    );
    guarded_tool(&target, &[], &machine)
        .assert()
        .success()
        .stdout(predicate::str::contains("real"));

    let mut doctor = machine.oomwrap();
    doctor
        .env("PATH", machine.file("none"))
        .args(["doctor", "--json"]);
    let output = doctor.assert().success().get_output().stdout.clone();
    let report: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["scripts"][0]["kind"], "wrapper");
    assert_eq!(report["scripts"][0]["outdated"], false);

    let mut unwrap = machine.oomwrap();
    unwrap.arg("unwrap").arg(&target);
    unwrap.assert().success();
    assert_eq!(fs::read_to_string(&record).unwrap(), "");
}
