use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::prelude::*;
use predicates::prelude::*;
use tempfile::tempdir;

fn meminfo(mem_available_kb: u64, swap_free_kb: u64) -> String {
    format!(
        "MemTotal: 999999999 kB\nMemAvailable: {mem_available_kb} kB\nSwapFree: {swap_free_kb} kB\n"
    )
}

fn wait_for_file(path: &Path) -> bool {
    for _ in 0..500 {
        if path.exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
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
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("process {pid} is still alive");
}

fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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

#[test]
fn skillflag_lists_embedded_skill() {
    let mut cmd = Command::cargo_bin("oomwrap").unwrap();
    cmd.args(["--skill", "list", "--json"]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("\"id\":\"memory-safe-launch\""));
}

#[test]
fn skillflag_shows_canonical_skill() {
    let output = Command::cargo_bin("oomwrap")
        .unwrap()
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
    let output = Command::cargo_bin("oomwrap")
        .unwrap()
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
    let mut cmd = Command::cargo_bin("oomwrap").unwrap();
    cmd.arg("doctor");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("oomwrap doctor"));
}

#[test]
fn doctor_and_inspect_can_emit_json() {
    let mut doctor = Command::cargo_bin("oomwrap").unwrap();
    doctor.args(["doctor", "--json"]);
    doctor
        .assert()
        .success()
        .stdout(predicate::str::contains("\"earlyoom_active\""));

    let mut inspect = Command::cargo_bin("oomwrap").unwrap();
    inspect.args(["inspect", "--json"]);
    inspect
        .assert()
        .success()
        .stdout(predicate::str::contains("\"default_tools\""));
}

#[test]
fn inspect_prints_human_summary() {
    let mut cmd = Command::cargo_bin("oomwrap").unwrap();
    cmd.arg("inspect");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("oomwrap inspect"));
}

#[test]
fn run_returns_child_exit_code() {
    let mut cmd = Command::cargo_bin("oomwrap").unwrap();
    cmd.args([
        "run",
        "--profile",
        "generic",
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
        "--allow-no-earlyoom",
        "--",
        "bash",
        "-lc",
        "exit 7",
    ]);
    cmd.assert().code(7);
}

#[test]
fn run_returns_signal_exit_code() {
    let mut cmd = Command::cargo_bin("oomwrap").unwrap();
    cmd.args([
        "run",
        "--profile",
        "generic",
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
        "--allow-no-earlyoom",
        "--",
        "bash",
        "-lc",
        "kill -TERM $$",
    ]);
    cmd.assert().code(143);
}

#[test]
fn run_allows_child_to_read_from_controlling_tty() {
    if Command::new("script")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| !status.success())
        .unwrap_or(true)
    {
        return;
    }

    let bin = Command::cargo_bin("oomwrap")
        .unwrap()
        .get_program()
        .to_string_lossy()
        .into_owned();
    let guarded = format!(
        "{} run --profile generic --min-mem 1M --min-swap 0 --allow-no-earlyoom -- bash -lc {}",
        sh_quote(&bin),
        sh_quote("read x; echo got:$x")
    );
    let mut child = Command::new("script")
        .args(["-qfec", &guarded, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"from-tty\n")
        .unwrap();
    drop(child.stdin.take());

    let started = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            let output = child.wait_with_output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "script failed with status {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
                output.status
            );
            assert!(
                stdout.contains("got:from-tty"),
                "child did not read from the pty\nstdout:\n{stdout}\nstderr:\n{stderr}"
            );
            return;
        }
        if started.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "timed out waiting for pty read smoke test\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn high_risk_profiles_require_earlyoom_unless_overridden() {
    let mut present = Command::cargo_bin("oomwrap").unwrap();
    present.env("OOMWRAP_EARLYOOM_ACTIVE", "1");
    present.args([
        "run",
        "--profile",
        "vllm",
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
        "--",
        "bash",
        "-lc",
        "exit 0",
    ]);
    present.assert().success();

    let mut blocked = Command::cargo_bin("oomwrap").unwrap();
    blocked.env("OOMWRAP_EARLYOOM_ACTIVE", "0");
    blocked.args([
        "run",
        "--profile",
        "vllm",
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
        "--",
        "bash",
        "-lc",
        "exit 0",
    ]);
    blocked
        .assert()
        .code(3)
        .stderr(predicate::str::contains("earlyoom is required"));

    let mut python_module = Command::cargo_bin("oomwrap").unwrap();
    python_module.env("OOMWRAP_EARLYOOM_ACTIVE", "0");
    python_module.args([
        "run",
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
        "--",
        "python3",
        "-m",
        "vllm.entrypoints.openai.api_server",
        "--help",
    ]);
    python_module
        .assert()
        .code(3)
        .stderr(predicate::str::contains("earlyoom is required"));

    let mut tgi = Command::cargo_bin("oomwrap").unwrap();
    tgi.env("OOMWRAP_EARLYOOM_ACTIVE", "0");
    tgi.args([
        "run",
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
        "--",
        "text-generation-launcher",
        "--help",
    ]);
    tgi.assert()
        .code(3)
        .stderr(predicate::str::contains("earlyoom is required"));

    let mut allowed = Command::cargo_bin("oomwrap").unwrap();
    allowed.env("OOMWRAP_EARLYOOM_ACTIVE", "0");
    allowed.args([
        "run",
        "--profile",
        "vllm",
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
        "--allow-no-earlyoom",
        "--",
        "bash",
        "-lc",
        "exit 0",
    ]);
    allowed.assert().success();
}

#[test]
fn run_rejects_unsupported_systemd_scope_flag() {
    let mut cmd = Command::cargo_bin("oomwrap").unwrap();
    cmd.args([
        "run",
        "--profile",
        "generic",
        "--allow-no-earlyoom",
        "--use-systemd-scope",
        "--",
        "bash",
        "-lc",
        "exit 0",
    ]);
    cmd.assert()
        .code(2)
        .stderr(predicate::str::contains("systemd scope support"));

    let mut memory_high = Command::cargo_bin("oomwrap").unwrap();
    memory_high.args([
        "run",
        "--profile",
        "generic",
        "--allow-no-earlyoom",
        "--memory-high",
        "1G",
        "--",
        "bash",
        "-lc",
        "exit 0",
    ]);
    memory_high
        .assert()
        .code(2)
        .stderr(predicate::str::contains("systemd scope support"));

    let mut memory_max = Command::cargo_bin("oomwrap").unwrap();
    memory_max.args([
        "run",
        "--profile",
        "generic",
        "--allow-no-earlyoom",
        "--memory-max",
        "1G",
        "--",
        "bash",
        "-lc",
        "exit 0",
    ]);
    memory_max
        .assert()
        .code(2)
        .stderr(predicate::str::contains("systemd scope support"));
}

#[test]
fn run_allows_thresholds_equal_to_available_memory() {
    let dir = tempdir().unwrap();
    let meminfo_path = dir.path().join("meminfo");
    fs::write(&meminfo_path, meminfo(2048, 1024)).unwrap();

    let mut cmd = Command::cargo_bin("oomwrap").unwrap();
    cmd.env("OOMWRAP_MEMINFO_PATH", &meminfo_path);
    cmd.args([
        "run",
        "--profile",
        "generic",
        "--min-mem",
        "2M",
        "--min-swap",
        "1M",
        "--allow-no-earlyoom",
        "--",
        "bash",
        "-lc",
        "exit 0",
    ]);
    cmd.assert().success();
}

#[test]
fn run_refuses_when_preflight_memory_is_below_floor() {
    let dir = tempdir().unwrap();
    let meminfo_path = dir.path().join("meminfo");
    fs::write(&meminfo_path, meminfo(1024, 0)).unwrap();

    let mut cmd = Command::cargo_bin("oomwrap").unwrap();
    cmd.env("OOMWRAP_MEMINFO_PATH", &meminfo_path);
    cmd.args([
        "run",
        "--profile",
        "generic",
        "--min-mem",
        "2M",
        "--min-swap",
        "0",
        "--allow-no-earlyoom",
        "--",
        "bash",
        "-lc",
        "exit 0",
    ]);
    cmd.assert()
        .code(4)
        .stderr(predicate::str::contains("refusing to launch"));
}

#[test]
fn run_refuses_when_preflight_swap_is_below_floor() {
    let dir = tempdir().unwrap();
    let meminfo_path = dir.path().join("meminfo");
    fs::write(&meminfo_path, meminfo(10 * 1024, 0)).unwrap();

    let mut cmd = Command::cargo_bin("oomwrap").unwrap();
    cmd.env("OOMWRAP_MEMINFO_PATH", &meminfo_path);
    cmd.args([
        "run",
        "--profile",
        "generic",
        "--min-mem",
        "1M",
        "--min-swap",
        "1M",
        "--allow-no-earlyoom",
        "--",
        "bash",
        "-lc",
        "exit 0",
    ]);
    cmd.assert()
        .code(4)
        .stderr(predicate::str::contains("SwapFree"));
}

#[test]
fn run_kills_process_group_when_memory_drops() {
    let dir = tempdir().unwrap();
    let meminfo_path = dir.path().join("meminfo");
    let event_log = dir.path().join("events.jsonl");
    fs::write(&meminfo_path, meminfo(10 * 1024 * 1024, 1024)).unwrap();

    let script = format!(
        "sleep 0.2; printf '{}' > {}; sleep 20",
        meminfo(1024, 1024).replace('\n', "\\n"),
        meminfo_path.display()
    );

    let mut cmd = Command::cargo_bin("oomwrap").unwrap();
    cmd.env("OOMWRAP_MEMINFO_PATH", &meminfo_path);
    cmd.args([
        "run",
        "--profile",
        "generic",
        "--min-mem",
        "2M",
        "--min-swap",
        "0",
        "--poll",
        "50ms",
        "--term-grace",
        "100ms",
        "--allow-no-earlyoom",
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
    let events = fs::read_to_string(event_log).unwrap();
    assert!(events.contains("memory_pressure_kill"));
}

#[test]
fn run_escalates_to_sigkill_when_child_ignores_sigterm() {
    let dir = tempdir().unwrap();
    let meminfo_path = dir.path().join("meminfo");
    fs::write(&meminfo_path, meminfo(10 * 1024 * 1024, 1024)).unwrap();
    let script = format!(
        "trap '' TERM; sleep 0.2; printf '{}' > {}; sleep 20",
        meminfo(1024, 1024).replace('\n', "\\n"),
        meminfo_path.display()
    );

    let mut cmd = Command::cargo_bin("oomwrap").unwrap();
    cmd.env("OOMWRAP_MEMINFO_PATH", &meminfo_path);
    cmd.args([
        "run",
        "--profile",
        "generic",
        "--min-mem",
        "2M",
        "--min-swap",
        "0",
        "--poll",
        "50ms",
        "--term-grace",
        "800ms",
        "--allow-no-earlyoom",
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
    let dir = tempdir().unwrap();
    let meminfo_path = dir.path().join("meminfo");
    let term_log = dir.path().join("terms.log");
    fs::write(&meminfo_path, meminfo(10 * 1024 * 1024, 1024)).unwrap();
    let script = format!(
        "count=0; trap 'count=$((count+1)); echo term:$count >> {}; if [ $count -gt 1 ]; then exit 42; fi; sleep 0.4; exit 0' TERM; printf '{}' > {}; while true; do sleep 1; done",
        term_log.display(),
        meminfo(1024, 1024).replace('\n', "\\n"),
        meminfo_path.display()
    );

    let mut cmd = Command::cargo_bin("oomwrap").unwrap();
    cmd.env("OOMWRAP_MEMINFO_PATH", &meminfo_path);
    cmd.args([
        "run",
        "--profile",
        "generic",
        "--min-mem",
        "2M",
        "--min-swap",
        "0",
        "--poll",
        "50ms",
        "--term-grace",
        "1s",
        "--allow-no-earlyoom",
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
    let dir = tempdir().unwrap();
    let meminfo_path = dir.path().join("meminfo");
    fs::write(&meminfo_path, meminfo(10 * 1024 * 1024, 1024)).unwrap();
    let script = format!(
        "trap 'exit 0' TERM; sleep 0.2; printf '{}' > {}; while true; do sleep 1; done",
        meminfo(1024, 1024).replace('\n', "\\n"),
        meminfo_path.display()
    );

    let mut cmd = Command::cargo_bin("oomwrap").unwrap();
    cmd.env("OOMWRAP_MEMINFO_PATH", &meminfo_path);
    cmd.args([
        "run",
        "--profile",
        "generic",
        "--min-mem",
        "2M",
        "--min-swap",
        "0",
        "--poll",
        "50ms",
        "--term-grace",
        "1500ms",
        "--allow-no-earlyoom",
        "--",
        "bash",
        "-lc",
        &script,
    ]);
    let started = Instant::now();
    cmd.assert().code(137);
    assert!(
        started.elapsed().as_secs_f32() < 1.0,
        "guard should not wait full grace after the child exits on TERM"
    );
}

#[test]
fn run_kills_remaining_group_members_after_leader_exits() {
    let dir = tempdir().unwrap();
    let meminfo_path = dir.path().join("meminfo");
    let background_pid_path = dir.path().join("background.pid");
    fs::write(&meminfo_path, meminfo(10 * 1024 * 1024, 1024)).unwrap();
    let script = format!(
        "trap 'exit 0' TERM; (trap '' TERM; echo $BASHPID > {}; while true; do sleep 1; done) & sleep 0.2; printf '{}' > {}; while true; do sleep 1; done",
        background_pid_path.display(),
        meminfo(1024, 1024).replace('\n', "\\n"),
        meminfo_path.display()
    );

    let mut cmd = Command::cargo_bin("oomwrap").unwrap();
    cmd.env("OOMWRAP_MEMINFO_PATH", &meminfo_path);
    cmd.args([
        "run",
        "--profile",
        "generic",
        "--min-mem",
        "2M",
        "--min-swap",
        "0",
        "--poll",
        "50ms",
        "--term-grace",
        "150ms",
        "--allow-no-earlyoom",
        "--",
        "bash",
        "-lc",
        &script,
    ]);
    cmd.assert().code(137);

    let background_pid: u32 = fs::read_to_string(&background_pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    wait_for_process_exit(background_pid);
}

#[test]
fn run_cleans_background_group_after_leader_exits() {
    let dir = tempdir().unwrap();
    let background_pid_path = dir.path().join("background.pid");
    let ready_path = dir.path().join("background.ready");
    let script = format!(
        "(exec >/dev/null 2>&1 < /dev/null; trap '' TERM; echo $BASHPID > {}; touch {}; while true; do sleep 1; done) & while [ ! -f {} ]; do sleep 0.01; done; exit 0",
        background_pid_path.display(),
        ready_path.display(),
        ready_path.display()
    );

    let mut cmd = Command::cargo_bin("oomwrap").unwrap();
    cmd.args([
        "run",
        "--profile",
        "generic",
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
        "--term-grace",
        "100ms",
        "--allow-no-earlyoom",
        "--",
        "bash",
        "-lc",
        &script,
    ]);
    cmd.assert().success();

    let background_pid: u32 = fs::read_to_string(&background_pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    wait_for_process_exit(background_pid);
}

#[test]
fn run_cleans_process_group_when_monitoring_fails_after_spawn() {
    let dir = tempdir().unwrap();
    let meminfo_path = dir.path().join("meminfo");
    let child_pid_path = dir.path().join("child.pid");
    fs::write(&meminfo_path, meminfo(10 * 1024 * 1024, 1024)).unwrap();
    let script = format!(
        "echo $$ > {}; rm {}; sleep 20",
        child_pid_path.display(),
        meminfo_path.display()
    );

    let mut cmd = Command::cargo_bin("oomwrap").unwrap();
    cmd.env("OOMWRAP_MEMINFO_PATH", &meminfo_path);
    cmd.args([
        "run",
        "--profile",
        "generic",
        "--min-mem",
        "2M",
        "--min-swap",
        "0",
        "--poll",
        "50ms",
        "--term-grace",
        "100ms",
        "--allow-no-earlyoom",
        "--",
        "bash",
        "-lc",
        &script,
    ]);
    cmd.assert()
        .code(2)
        .stderr(predicate::str::contains("guard error after launch"))
        .stderr(predicate::str::contains("failed to read"));

    let child_pid: u32 = fs::read_to_string(&child_pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    wait_for_process_exit(child_pid);
}

#[test]
fn run_cleans_process_group_when_guard_receives_term() {
    let dir = tempdir().unwrap();
    let meminfo_path = dir.path().join("meminfo");
    let child_pid_path = dir.path().join("child.pid");
    fs::write(&meminfo_path, meminfo(10 * 1024 * 1024, 1024)).unwrap();
    let script = format!(
        "echo $$ > {}; trap '' TERM; sleep 20",
        child_pid_path.display()
    );

    let mut cmd = Command::cargo_bin("oomwrap").unwrap();
    cmd.env("OOMWRAP_MEMINFO_PATH", &meminfo_path);
    cmd.args([
        "run",
        "--profile",
        "generic",
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
        "--poll",
        "1s",
        "--term-grace",
        "100ms",
        "--allow-no-earlyoom",
        "--",
        "bash",
        "-lc",
        &script,
    ]);
    let mut guard = cmd.spawn().unwrap();
    if !wait_for_file(&child_pid_path) {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(guard.id().to_string())
            .status();
        let _ = guard.wait();
        panic!("timed out waiting for {}", child_pid_path.display());
    }
    let child_pid: u32 = fs::read_to_string(&child_pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

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
    let mut poll = Command::cargo_bin("oomwrap").unwrap();
    poll.args([
        "run",
        "--profile",
        "generic",
        "--allow-no-earlyoom",
        "--poll",
        "-1",
        "--",
        "bash",
        "-lc",
        "exit 0",
    ]);
    poll.assert()
        .code(2)
        .stderr(predicate::str::contains("invalid --poll"))
        .stderr(predicate::str::contains("panicked").not());

    let mut term_grace = Command::cargo_bin("oomwrap").unwrap();
    term_grace.args([
        "run",
        "--profile",
        "generic",
        "--allow-no-earlyoom",
        "--term-grace",
        "NaN",
        "--",
        "bash",
        "-lc",
        "exit 0",
    ]);
    term_grace
        .assert()
        .code(2)
        .stderr(predicate::str::contains("invalid --term-grace"))
        .stderr(predicate::str::contains("panicked").not());
}

#[test]
fn run_handles_huge_durations_without_panic() {
    let mut poll = Command::cargo_bin("oomwrap").unwrap();
    poll.args([
        "run",
        "--profile",
        "generic",
        "--allow-no-earlyoom",
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
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

    let dir = tempdir().unwrap();
    let meminfo_path = dir.path().join("meminfo");
    fs::write(&meminfo_path, meminfo(10 * 1024 * 1024, 1024)).unwrap();
    let script = format!(
        "trap 'exit 0' TERM; sleep 0.2; printf '{}' > {}; while true; do sleep 1; done",
        meminfo(1024, 1024).replace('\n', "\\n"),
        meminfo_path.display()
    );

    let mut term_grace = Command::cargo_bin("oomwrap").unwrap();
    term_grace.env("OOMWRAP_MEMINFO_PATH", &meminfo_path);
    term_grace.args([
        "run",
        "--profile",
        "generic",
        "--allow-no-earlyoom",
        "--min-mem",
        "2M",
        "--min-swap",
        "0",
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
        started.elapsed().as_secs_f32() < 1.0,
        "cooperative child should not wait huge TERM grace"
    );
}

#[test]
fn path_shim_resolves_real_binary_without_recursing() {
    let dir = tempdir().unwrap();
    let shim_dir = dir.path().join("shims");
    let real_dir = dir.path().join("real");
    fs::create_dir_all(&real_dir).unwrap();
    let real = real_dir.join("fake-vllm");
    fs::write(&real, "#!/usr/bin/env bash\necho real-fake-vllm \"$@\"\n").unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();

    let mut install = Command::cargo_bin("oomwrap").unwrap();
    install.args([
        "install-shims",
        "--bin-dir",
        shim_dir.to_str().unwrap(),
        "--tool",
        "fake-vllm",
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
    ]);
    install.assert().success();

    let shim = shim_dir.join("fake-vllm");
    let mut run = Command::new(&shim);
    let system_path = std::env::var("PATH").unwrap_or_default();
    run.env(
        "PATH",
        format!(
            "{}:{}:{system_path}",
            shim_dir.display(),
            real_dir.display()
        ),
    );
    run.env("OOMWRAP_ALLOW_NO_EARLYOOM", "1");
    run.arg("smoke");
    run.assert()
        .success()
        .stdout(predicate::str::contains("real-fake-vllm smoke"));
}

#[test]
fn path_shim_can_run_wrapped_binary_later_in_path() {
    let dir = tempdir().unwrap();
    let shim_dir = dir.path().join("shims");
    let real_dir = dir.path().join("real");
    fs::create_dir_all(&real_dir).unwrap();
    let real = real_dir.join("vllm");
    fs::write(
        &real,
        "#!/usr/bin/env bash\necho wrapped-path-real \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();

    let mut wrap = Command::cargo_bin("oomwrap").unwrap();
    wrap.args([
        "wrap",
        real.to_str().unwrap(),
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
    ]);
    wrap.assert().success();

    let mut install = Command::cargo_bin("oomwrap").unwrap();
    install.args([
        "install-shims",
        "--bin-dir",
        shim_dir.to_str().unwrap(),
        "--tool",
        "vllm",
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
    ]);
    install.assert().success();

    let shim = shim_dir.join("vllm");
    let mut run = Command::new(&shim);
    let system_path = std::env::var("PATH").unwrap_or_default();
    run.env(
        "PATH",
        format!(
            "{}:{}:{system_path}",
            shim_dir.display(),
            real_dir.display()
        ),
    );
    run.env("OOMWRAP_ALLOW_NO_EARLYOOM", "1");
    run.arg("smoke");
    run.assert()
        .success()
        .stdout(predicate::str::contains("wrapped-path-real smoke"));
}

#[test]
fn path_shim_treats_generated_defaults_as_data() {
    let dir = tempdir().unwrap();
    let shim_dir = dir.path().join("shims");
    let real_dir = dir.path().join("real");
    let sentinel = dir.path().join("sentinel");
    let payload = format!("$(touch {})", sentinel.display());
    fs::create_dir_all(&real_dir).unwrap();
    let real = real_dir.join("fake-vllm");
    fs::write(&real, "#!/usr/bin/env bash\necho should-not-run\n").unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();

    let mut install = Command::cargo_bin("oomwrap").unwrap();
    install
        .arg("install-shims")
        .arg("--bin-dir")
        .arg(&shim_dir)
        .args(["--tool", "fake-vllm", "--min-mem"])
        .arg(&payload)
        .args(["--min-swap", "0"]);
    install.assert().success();

    let shim = shim_dir.join("fake-vllm");
    let mut run = Command::new(&shim);
    let system_path = std::env::var("PATH").unwrap_or_default();
    run.env(
        "PATH",
        format!(
            "{}:{}:{system_path}",
            shim_dir.display(),
            real_dir.display()
        ),
    );
    run.env("OOMWRAP_ALLOW_NO_EARLYOOM", "1");
    run.assert()
        .code(2)
        .stderr(predicate::str::contains("invalid --min-mem"));
    assert!(
        !sentinel.exists(),
        "generated shim default executed as shell code"
    );
}

#[test]
fn install_default_shims_and_refuse_unmanaged_collision() {
    let dir = tempdir().unwrap();
    let shim_dir = dir.path().join("shims");

    let mut install = Command::cargo_bin("oomwrap").unwrap();
    install.args([
        "install-shims",
        "--bin-dir",
        shim_dir.to_str().unwrap(),
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
    ]);
    install.assert().success();
    assert!(shim_dir.join("vllm").exists());
    assert!(shim_dir.join("llama-server").exists());

    fs::write(shim_dir.join("custom-tool"), "not managed\n").unwrap();
    let mut collision = Command::cargo_bin("oomwrap").unwrap();
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

    let mut uninstall = Command::cargo_bin("oomwrap").unwrap();
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
    let dir = tempdir().unwrap();
    let shim_dir = dir.path().join("shims");
    let real_dir = dir.path().join("real");
    fs::create_dir_all(&shim_dir).unwrap();
    fs::create_dir_all(&real_dir).unwrap();

    let real = real_dir.join("fake-vllm");
    fs::write(&real, "#!/usr/bin/env bash\necho real-after-force \"$@\"\n").unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
    let original_real = fs::read_to_string(&real).unwrap();

    let shim = shim_dir.join("fake-vllm");
    symlink(&real, &shim).unwrap();

    let mut install = Command::cargo_bin("oomwrap").unwrap();
    install.args([
        "install-shims",
        "--force",
        "--bin-dir",
        shim_dir.to_str().unwrap(),
        "--tool",
        "fake-vllm",
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
    ]);
    install.assert().success();

    assert_eq!(fs::read_to_string(&real).unwrap(), original_real);
    assert!(
        !fs::symlink_metadata(&shim)
            .unwrap()
            .file_type()
            .is_symlink()
    );

    let mut run = Command::new(&shim);
    let system_path = std::env::var("PATH").unwrap_or_default();
    run.env(
        "PATH",
        format!(
            "{}:{}:{system_path}",
            shim_dir.display(),
            real_dir.display()
        ),
    );
    run.env("OOMWRAP_ALLOW_NO_EARLYOOM", "1");
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

    let mut install = Command::cargo_bin("oomwrap").unwrap();
    install.args([
        "install-shims",
        "--bin-dir",
        shim_dir.to_str().unwrap(),
        "--tool",
        "fake-vllm",
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
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

    let mut relative = Command::cargo_bin("oomwrap").unwrap();
    relative.args([
        "install-shims",
        "--bin-dir",
        shim_dir.to_str().unwrap(),
        "--tool",
        "../escape",
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
    ]);
    relative
        .assert()
        .code(2)
        .stderr(predicate::str::contains("bare executable name"));
    assert!(!escaped.exists());

    let mut absolute = Command::cargo_bin("oomwrap").unwrap();
    absolute.args([
        "install-shims",
        "--bin-dir",
        shim_dir.to_str().unwrap(),
        "--tool",
        escaped.to_str().unwrap(),
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
    ]);
    absolute
        .assert()
        .code(2)
        .stderr(predicate::str::contains("bare executable name"));
    assert!(!escaped.exists());

    let mut uninstall = Command::cargo_bin("oomwrap").unwrap();
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
    let dir = tempdir().unwrap();
    let target = dir.path().join("vllm");
    fs::write(&target, "#!/usr/bin/env bash\necho still-wrapped \"$@\"\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

    let mut wrap = Command::cargo_bin("oomwrap").unwrap();
    wrap.args([
        "wrap",
        target.to_str().unwrap(),
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
    ]);
    wrap.assert().success();

    let mut uninstall = Command::cargo_bin("oomwrap").unwrap();
    uninstall.args([
        "uninstall-shims",
        "--bin-dir",
        dir.path().to_str().unwrap(),
        "--tool",
        "vllm",
    ]);
    uninstall.assert().success();

    assert!(target.exists());
    assert!(target.with_file_name("vllm.real").exists());

    let mut guarded = Command::new(&target);
    guarded.env("OOMWRAP_ALLOW_NO_EARLYOOM", "1");
    guarded.arg("after-uninstall");
    guarded
        .assert()
        .success()
        .stdout(predicate::str::contains("still-wrapped after-uninstall"));
}

#[test]
fn wrap_and_unwrap_round_trip() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("vllm");
    fs::write(&target, "#!/usr/bin/env bash\necho wrapped-real \"$@\"\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

    let mut wrap = Command::cargo_bin("oomwrap").unwrap();
    wrap.args([
        "wrap",
        target.to_str().unwrap(),
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
    ]);
    wrap.assert().success();
    assert!(target.with_file_name("vllm.real").exists());

    let mut guarded = Command::new(&target);
    guarded.env("OOMWRAP_ALLOW_NO_EARLYOOM", "1");
    guarded.arg("absolute");
    guarded
        .assert()
        .success()
        .stdout(predicate::str::contains("wrapped-real absolute"));

    let mut unwrap = Command::cargo_bin("oomwrap").unwrap();
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
    fs::write(&actual, "#!/usr/bin/env bash\necho actual \"$@\"\n").unwrap();
    fs::set_permissions(&actual, fs::Permissions::from_mode(0o755)).unwrap();
    symlink(&actual, &target).unwrap();

    let mut wrap = Command::cargo_bin("oomwrap").unwrap();
    wrap.args([
        "wrap",
        target.to_str().unwrap(),
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
    ]);
    wrap.assert().success();

    fs::remove_file(&actual).unwrap();

    let mut unwrap = Command::cargo_bin("oomwrap").unwrap();
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
    fs::write(&target, "#!/usr/bin/env bash\necho actual \"$@\"\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

    let mut wrap = Command::cargo_bin("oomwrap").unwrap();
    wrap.args([
        "wrap",
        target.to_str().unwrap(),
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
    ]);
    wrap.assert().success();

    let symlink_path = bin_dir.join("vllm");
    let unrelated_sidecar = bin_dir.join("vllm.real");
    symlink(&target, &symlink_path).unwrap();
    fs::write(&unrelated_sidecar, "unrelated\n").unwrap();

    let mut unwrap = Command::cargo_bin("oomwrap").unwrap();
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
    let dir = tempdir().unwrap();
    let real_dir = dir.path().join("real");
    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&real_dir).unwrap();
    fs::create_dir_all(&bin_dir).unwrap();

    let target = real_dir.join("vllm");
    fs::write(&target, "#!/usr/bin/env bash\necho symlink-real \"$@\"\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

    let mut wrap = Command::cargo_bin("oomwrap").unwrap();
    wrap.args([
        "wrap",
        target.to_str().unwrap(),
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
    ]);
    wrap.assert().success();

    let symlink_path = bin_dir.join("vllm");
    symlink(&target, &symlink_path).unwrap();

    let mut guarded = Command::new(&symlink_path);
    guarded.env("OOMWRAP_ALLOW_NO_EARLYOOM", "1");
    guarded.arg("via-symlink");
    guarded
        .assert()
        .success()
        .stdout(predicate::str::contains("symlink-real via-symlink"));
}

#[test]
fn wrapped_high_risk_tool_still_requires_earlyoom() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("vllm");
    fs::write(&target, "#!/usr/bin/env bash\necho should-not-run\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

    let mut wrap = Command::cargo_bin("oomwrap").unwrap();
    wrap.args([
        "wrap",
        target.to_str().unwrap(),
        "--min-mem",
        "1M",
        "--min-swap",
        "0",
    ]);
    wrap.assert().success();

    let mut guarded = Command::new(&target);
    guarded.env("OOMWRAP_EARLYOOM_ACTIVE", "0");
    guarded
        .assert()
        .code(3)
        .stderr(predicate::str::contains("earlyoom is required"));
}

#[test]
fn absolute_wrapper_treats_generated_defaults_as_data() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("vllm");
    let sentinel = dir.path().join("sentinel");
    let payload = format!("$(touch {})", sentinel.display());
    fs::write(&target, "#!/usr/bin/env bash\necho should-not-run\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

    let mut wrap = Command::cargo_bin("oomwrap").unwrap();
    wrap.arg("wrap")
        .arg(&target)
        .args(["--min-mem"])
        .arg(&payload)
        .args(["--min-swap", "0"]);
    wrap.assert().success();

    let mut guarded = Command::new(&target);
    guarded.env("OOMWRAP_ALLOW_NO_EARLYOOM", "1");
    guarded
        .assert()
        .code(2)
        .stderr(predicate::str::contains("invalid --min-mem"));
    assert!(
        !sentinel.exists(),
        "generated wrapper default executed as shell code"
    );
}

#[test]
fn wrap_and_unwrap_validate_bad_inputs() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("missing");
    let mut wrap_missing = Command::cargo_bin("oomwrap").unwrap();
    wrap_missing.args(["wrap", missing.to_str().unwrap()]);
    wrap_missing
        .assert()
        .code(2)
        .stderr(predicate::str::contains("cannot wrap missing path"));

    let directory = dir.path().join("directory");
    fs::create_dir(&directory).unwrap();
    let mut wrap_directory = Command::cargo_bin("oomwrap").unwrap();
    wrap_directory.args(["wrap", directory.to_str().unwrap()]);
    wrap_directory
        .assert()
        .code(2)
        .stderr(predicate::str::contains("cannot wrap non-file path"));

    let target = dir.path().join("not-executable");
    fs::write(&target, "plain text\n").unwrap();
    let mut wrap_non_executable = Command::cargo_bin("oomwrap").unwrap();
    wrap_non_executable.args(["wrap", target.to_str().unwrap()]);
    wrap_non_executable
        .assert()
        .code(2)
        .stderr(predicate::str::contains("cannot wrap non-executable path"));

    let mut unwrap_plain = Command::cargo_bin("oomwrap").unwrap();
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
    fs::write(&target, "#!/usr/bin/env bash\necho target\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(dir.path().join("vllm.real"), "already exists\n").unwrap();

    let mut cmd = Command::cargo_bin("oomwrap").unwrap();
    cmd.args(["wrap", target.to_str().unwrap()]);
    cmd.assert()
        .code(2)
        .stderr(predicate::str::contains("refusing to overwrite"));
}

#[test]
fn wrap_refuses_dangling_real_sidecar_without_force() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("vllm");
    fs::write(&target, "#!/usr/bin/env bash\necho target\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    symlink(dir.path().join("missing"), dir.path().join("vllm.real")).unwrap();

    let mut cmd = Command::cargo_bin("oomwrap").unwrap();
    cmd.args(["wrap", target.to_str().unwrap()]);
    cmd.assert()
        .code(2)
        .stderr(predicate::str::contains("refusing to overwrite"));
}
