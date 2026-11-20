//! The machine that oomwrap runs on. Environment variables are read once, in
//! [`Host::from_env`], and every other module asks this value.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};
use nix::sys::statvfs::statvfs;
use nix::unistd::{SysconfVar, User, geteuid, getuid, sysconf};
use serde::{Deserialize, Serialize};

use crate::readings::{
    CgroupMounts, MemInfo, parse_cgroup_v2_path, parse_meminfo, parse_mountinfo,
};

/// Test seam: a directory read in place of `/proc` for memory and cgroup
/// readings. Process control always uses the real `/proc`.
const PROC_ROOT_VAR: &str = "OOMWRAP_PROC_ROOT";
/// Test seam: a directory read in place of the cgroup v2 mount.
const CGROUP_ROOT_VAR: &str = "OOMWRAP_CGROUP_ROOT";
/// Test seam: claims that a machine-wide guard is (`1`) or is not (`0`) active.
const MACHINE_GUARD_VAR: &str = "OOMWRAP_MACHINE_GUARD_ACTIVE";
/// Test seam: the program that starts a transient scope.
const SYSTEMD_RUN_VAR: &str = "OOMWRAP_SYSTEMD_RUN";
/// Test seam: the folder of the users with lingering on.
const LINGER_DIR_VAR: &str = "OOMWRAP_LINGER_DIR";
/// Test seam: the system policy file, `/etc/oomwrap.toml` outside tests.
const SYSTEM_POLICY_VAR: &str = "OOMWRAP_SYSTEM_POLICY";
/// The page size when `sysconf` cannot tell it.
const FALLBACK_PAGE_SIZE: u64 = 4096;

pub(crate) struct Host {
    /// Where memory and cgroup readings come from; `/proc` outside tests.
    pub(crate) proc_root: PathBuf,
    /// Overrides the cgroup v2 mount point that `mountinfo` names.
    pub(crate) cgroup_root_override: Option<PathBuf>,
    /// `Some` when the environment overrides the machine-wide guard check.
    pub(crate) machine_guard_override: Option<bool>,
    /// oomwrap runs in a container, where a guard of the host is not visible.
    pub(crate) in_container: bool,
    pub(crate) home: Option<PathBuf>,
    /// The folders of `PATH`, where `doctor` looks for shims.
    pub(crate) path_dirs: Vec<PathBuf>,
    /// `XDG_STATE_HOME`, the folder of the default event log.
    pub(crate) state_home: Option<PathBuf>,
    /// `XDG_RUNTIME_DIR`, where the control socket of a launch goes.
    pub(crate) runtime_dir: Option<PathBuf>,
    /// The folder for temporary files, from `TMPDIR`.
    pub(crate) temp_dir: PathBuf,
    /// The program that starts a transient scope; `systemd-run` outside tests.
    pub(crate) systemd_run: PathBuf,
    /// `/var/lib/systemd/linger`: one file per user with lingering on.
    pub(crate) linger_dir: PathBuf,
    pub(crate) uid: u32,
    /// The name of the user, which names the user's lingering file.
    pub(crate) user_name: Option<String>,
    /// oomwrap runs as root and uses the system manager.
    pub(crate) is_root: bool,
    /// The page size, which is not 4 KiB on every machine.
    pub(crate) page_size: u64,
    /// The policy files, system first, then the user's.
    pub(crate) policy_files: Vec<PathBuf>,
}

impl Host {
    // Thin environment adapter; tests build a `Host` directly.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) fn from_env() -> Self {
        Self {
            proc_root: env::var_os(PROC_ROOT_VAR)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/proc")),
            cgroup_root_override: env::var_os(CGROUP_ROOT_VAR).map(PathBuf::from),
            machine_guard_override: env::var_os(MACHINE_GUARD_VAR).map(|value| {
                matches!(
                    value.to_string_lossy().as_ref(),
                    "1" | "true" | "yes" | "on"
                )
            }),
            in_container: env::var_os("container").is_some()
                || Path::new("/.dockerenv").exists()
                || Path::new("/run/.containerenv").exists(),
            home: env::var_os("HOME").map(PathBuf::from),
            path_dirs: env::var_os("PATH")
                .map(|path| env::split_paths(&path).collect())
                .unwrap_or_default(),
            state_home: absolute_path_var("XDG_STATE_HOME"),
            runtime_dir: absolute_path_var("XDG_RUNTIME_DIR"),
            temp_dir: env::temp_dir(),
            systemd_run: env::var_os(SYSTEMD_RUN_VAR)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("systemd-run")),
            linger_dir: env::var_os(LINGER_DIR_VAR)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/var/lib/systemd/linger")),
            uid: getuid().as_raw(),
            user_name: User::from_uid(getuid())
                .ok()
                .flatten()
                .map(|user| user.name),
            is_root: geteuid().is_root(),
            page_size: sysconf(SysconfVar::PAGE_SIZE)
                .ok()
                .flatten()
                .and_then(|size| u64::try_from(size).ok())
                .filter(|size| *size > 0)
                .unwrap_or(FALLBACK_PAGE_SIZE),
            policy_files: policy_files(),
        }
    }

    /// Tells whether lingering is on for the user, so that the user manager
    /// outlives the user's last session.
    pub(crate) fn lingering(&self) -> bool {
        self.user_name
            .as_ref()
            .is_some_and(|name| self.linger_dir.join(name).exists())
    }

    /// `${XDG_STATE_HOME:-~/.local/state}`.
    pub(crate) fn state_dir(&self) -> Result<PathBuf> {
        match &self.state_home {
            Some(dir) => Ok(dir.clone()),
            None => Ok(self.home()?.join(".local/state")),
        }
    }

    /// The folder for the control socket: the user's runtime folder when it
    /// exists, the temporary folder otherwise.
    pub(crate) fn control_parent(&self) -> &Path {
        match &self.runtime_dir {
            Some(dir) if dir.is_dir() => dir,
            _ => &self.temp_dir,
        }
    }

    /// Reads a file under the proc root, such as `meminfo`.
    pub(crate) fn read_proc(&self, relative: &str) -> Result<String> {
        read_file(&self.proc_root.join(relative))
    }

    pub(crate) fn meminfo(&self) -> Result<MemInfo> {
        parse_meminfo(&self.read_proc("meminfo")?)
    }

    pub(crate) fn cgroup_mounts(&self) -> Result<CgroupMounts> {
        match self.read_proc("self/mountinfo") {
            Ok(text) => parse_mountinfo(&text),
            Err(error) if is_not_found(&error) => Ok(CgroupMounts::default()),
            Err(error) => Err(error),
        }
    }

    /// The directory of the cgroup v2 hierarchy, when there is one.
    pub(crate) fn cgroup_root(&self) -> Result<Option<PathBuf>> {
        if let Some(root) = &self.cgroup_root_override {
            return Ok(Some(root.clone()));
        }
        Ok(self.cgroup_mounts()?.v2_mount)
    }

    /// The cgroup v2 path of oomwrap itself, such as `/user.slice/a.scope`.
    pub(crate) fn own_cgroup(&self) -> Result<Option<String>> {
        match self.read_proc("self/cgroup") {
            Ok(text) => Ok(parse_cgroup_v2_path(&text)),
            Err(error) if is_not_found(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn home(&self) -> Result<&Path> {
        self.home
            .as_deref()
            .ok_or_else(|| anyhow!("HOME is not set"))
    }

    /// Finds the machine-wide guards that watch a job in `job_cgroup`, with
    /// the extra guards that the policy file names.
    pub(crate) fn machine_guards(
        &self,
        job_cgroup: Option<&str>,
        declared: &[String],
    ) -> Result<Vec<MachineGuard>> {
        if let Some(active) = self.machine_guard_override {
            return Ok(if active {
                vec![MachineGuard::Declared("test".to_string())]
            } else {
                Vec::new()
            });
        }
        let processes = process_list()?;
        let own_pid = std::process::id();
        let mut guards = Vec::new();
        if runs_program(&processes, own_pid, "earlyoom") {
            guards.push(MachineGuard::Earlyoom);
        }
        if runs_program(&processes, own_pid, "systemd-oomd")
            && job_cgroup.is_some_and(systemd_oomd_watches)
        {
            guards.push(MachineGuard::SystemdOomd);
        }
        for name in declared {
            if runs_program(&processes, own_pid, name) {
                guards.push(MachineGuard::Declared(name.clone()));
            }
        }
        Ok(guards)
    }
}

/// A machine-wide guard that acts when oomwrap cannot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MachineGuard {
    Earlyoom,
    /// `systemd-oomd`, when a cgroup above the job asks it to watch.
    SystemdOomd,
    /// A guard that the configuration names by its process name.
    Declared(String),
}

/// `/etc/oomwrap.toml`, then `${XDG_CONFIG_HOME:-~/.config}/oomwrap/config.toml`.
// Thin environment adapter; tests set the variables.
#[cfg_attr(test, mutants::skip)]
fn policy_files() -> Vec<PathBuf> {
    let system = env::var_os(SYSTEM_POLICY_VAR)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/oomwrap.toml"));
    let config_home = absolute_path_var("XDG_CONFIG_HOME")
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    let user = config_home.map(|dir| dir.join("oomwrap/config.toml"));
    std::iter::once(system).chain(user).collect()
}

/// An environment variable that names an absolute path; the XDG rules
/// ignore a relative one.
fn absolute_path_var(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

pub(crate) fn read_file(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))
}

fn is_not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
}

/// Tells whether `systemd-oomd` watches a cgroup at or above `job_cgroup`:
/// a unit on the path sets `ManagedOOMMemoryPressure=kill` or
/// `ManagedOOMSwap=kill`. Units inside a user manager ask that manager.
// Thin adapter around systemctl; the parser below is tested.
#[cfg_attr(test, mutants::skip)]
fn systemd_oomd_watches(job_cgroup: &str) -> bool {
    let (system_units, user_units) = units_on_path(job_cgroup);
    let query = |user: bool, units: &[String]| -> bool {
        if units.is_empty() {
            return false;
        }
        let mut command = Command::new("systemctl");
        if user {
            command.arg("--user");
        }
        command
            .args([
                "show",
                "--property=ManagedOOMMemoryPressure,ManagedOOMSwap",
                "--",
            ])
            .args(units);
        command
            .output()
            .ok()
            .filter(|output| output.status.success())
            .is_some_and(|output| asks_oomd_to_kill(&String::from_utf8_lossy(&output.stdout)))
    };
    query(false, &system_units) || query(true, &user_units)
}

/// Splits a cgroup path into the units of the system manager and those of a
/// user manager (below `user@<uid>.service`). The root slice comes first.
fn units_on_path(cgroup: &str) -> (Vec<String>, Vec<String>) {
    let mut system_units = vec!["-.slice".to_string()];
    let mut user_units = Vec::new();
    let mut inside_user_manager = false;
    for component in cgroup
        .split('/')
        .filter(|component| is_unit_name(component))
    {
        if inside_user_manager {
            user_units.push(component.to_string());
        } else {
            system_units.push(component.to_string());
        }
        if component.starts_with("user@") && component.ends_with(".service") {
            inside_user_manager = true;
        }
    }
    (system_units, user_units)
}

fn is_unit_name(component: &str) -> bool {
    [".slice", ".service", ".scope"]
        .iter()
        .any(|suffix| component.ends_with(suffix))
}

/// Reads `systemctl show` output for a `kill` setting of systemd-oomd.
fn asks_oomd_to_kill(show_output: &str) -> bool {
    show_output.lines().any(|line| {
        matches!(
            line.trim(),
            "ManagedOOMMemoryPressure=kill" | "ManagedOOMSwap=kill"
        )
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ProcessInfo {
    pub(crate) pid: u32,
    pub(crate) command: String,
    pub(crate) rss_bytes: Option<u64>,
}

/// Tells whether another process than `own_pid` runs `program`.
fn runs_program(processes: &[ProcessInfo], own_pid: u32, program: &str) -> bool {
    processes.iter().any(|process| {
        process.pid != own_pid && command_invokes_executable(&process.command, program)
    })
}

/// Tells whether the program of `command` has the file name `executable`.
pub(crate) fn command_invokes_executable(command: &str, executable: &str) -> bool {
    command
        .split_whitespace()
        .next()
        .and_then(|program| Path::new(program).file_name())
        .and_then(|name| name.to_str())
        == Some(executable)
}

// Thin /proc inventory adapter. Higher-level process detection is covered with
// integration smoke tests, while this wrapper is environment-dependent.
#[cfg_attr(test, mutants::skip)]
pub(crate) fn process_list() -> Result<Vec<ProcessInfo>> {
    let mut processes = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(pid_str) = file_name.to_str() else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        let proc_dir = entry.path();
        let command = read_cmdline(&proc_dir).unwrap_or_default();
        if command.is_empty() {
            continue;
        }
        processes.push(ProcessInfo {
            pid,
            command,
            rss_bytes: read_rss_bytes(&proc_dir).ok().flatten(),
        });
    }
    Ok(processes)
}

pub(crate) fn read_cmdline(proc_dir: &Path) -> Result<String> {
    let raw = fs::read(proc_dir.join("cmdline"))?;
    let parts: Vec<String> = raw
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).to_string())
        .collect();
    Ok(parts.join(" "))
}

pub(crate) fn read_rss_bytes(proc_dir: &Path) -> Result<Option<u64>> {
    let status = fs::read_to_string(proc_dir.join("status"))?;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("VmRSS:") {
            let kb: u64 = value
                .split_whitespace()
                .next()
                .ok_or_else(|| anyhow!("missing VmRSS value"))?
                .parse()?;
            return Ok(Some(kb * 1024));
        }
    }
    Ok(None)
}

// Thin statvfs adapter.
#[cfg_attr(test, mutants::skip)]
pub(crate) fn disk_available(path: &Path) -> Result<u64> {
    let stats = statvfs(path)?;
    Ok(stats.blocks_available() * stats.fragment_size())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fake proc root and cgroup root in temporary directories.
    pub(crate) struct FakeHost {
        dir: tempfile::TempDir,
    }

    impl FakeHost {
        pub(crate) fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir_all(dir.path().join("proc/self")).unwrap();
            fs::create_dir_all(dir.path().join("cgroup")).unwrap();
            Self { dir }
        }

        pub(crate) fn cgroup_root(&self) -> PathBuf {
            self.dir.path().join("cgroup")
        }

        pub(crate) fn proc_file(&self, relative: &str, content: &str) {
            let path = self.dir.path().join("proc").join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }

        pub(crate) fn meminfo(&self, total: u64, available: u64, hugetlb: u64) {
            self.proc_file(
                "meminfo",
                &format!(
                    "MemTotal: {} kB\nMemAvailable: {} kB\nHugetlb: {} kB\n",
                    total / 1024,
                    available / 1024,
                    hugetlb / 1024
                ),
            );
        }

        pub(crate) fn own_cgroup(&self, path: &str) {
            self.proc_file("self/cgroup", &format!("0::{path}\n"));
        }

        pub(crate) fn cgroup_file(&self, path: &str, file: &str, content: &str) {
            let dir = self.cgroup_root().join(path.trim_start_matches('/'));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(file), content).unwrap();
        }

        pub(crate) fn host(&self) -> Host {
            Host {
                proc_root: self.dir.path().join("proc"),
                cgroup_root_override: Some(self.cgroup_root()),
                machine_guard_override: Some(false),
                in_container: false,
                home: None,
                path_dirs: Vec::new(),
                state_home: None,
                runtime_dir: None,
                temp_dir: self.dir.path().to_path_buf(),
                systemd_run: PathBuf::from("/nonexistent/systemd-run"),
                linger_dir: self.dir.path().join("linger"),
                uid: 1000,
                user_name: Some("user".to_string()),
                is_root: false,
                page_size: 4096,
                policy_files: Vec::new(),
            }
        }

        pub(crate) fn linger(&self) {
            let dir = self.dir.path().join("linger");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("user"), "").unwrap();
        }

        pub(crate) fn host_without_cgroups(&self) -> Host {
            Host {
                cgroup_root_override: None,
                ..self.host()
            }
        }
    }

    fn process(pid: u32, command: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            command: command.to_string(),
            rss_bytes: None,
        }
    }

    #[test]
    fn finds_programs_in_other_processes_only() {
        let processes = [process(10, "bash"), process(11, "/usr/bin/earlyoom -m 5")];
        assert!(runs_program(&processes, 10, "earlyoom"));
        assert!(!runs_program(&processes, 11, "earlyoom"));
        assert!(!runs_program(&processes[..1], 99, "earlyoom"));
        assert!(!runs_program(&processes, 10, "systemd-oomd"));
    }

    #[test]
    fn matches_the_program_name_only() {
        assert!(command_invokes_executable(
            "/usr/bin/earlyoom -m 5",
            "earlyoom"
        ));
        assert!(command_invokes_executable("earlyoom -m 5", "earlyoom"));
        assert!(!command_invokes_executable(
            "oomwrap run --require-machine-guard",
            "earlyoom"
        ));
        assert!(!command_invokes_executable(
            "python3 -c 'earlyoom'",
            "earlyoom"
        ));
    }

    #[test]
    fn reads_proc_helpers() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("cmdline"), b"vllm\0serve\0").unwrap();
        fs::write(
            dir.path().join("status"),
            "Name:\tvllm\nVmRSS:\t42 kB\nState:\tS\n",
        )
        .unwrap();
        assert_eq!(read_cmdline(dir.path()).unwrap(), "vllm serve");
        assert_eq!(read_rss_bytes(dir.path()).unwrap(), Some(42 * 1024));
        fs::write(dir.path().join("status"), "Name:\tkthread\n").unwrap();
        assert_eq!(read_rss_bytes(dir.path()).unwrap(), None);
    }

    #[test]
    fn splits_units_between_the_system_and_the_user_manager() {
        let (system, user) = units_on_path(
            "/user.slice/user-1000.slice/user@1000.service/app.slice/oomwrap-x.scope",
        );
        assert_eq!(
            system,
            [
                "-.slice",
                "user.slice",
                "user-1000.slice",
                "user@1000.service"
            ]
        );
        assert_eq!(user, ["app.slice", "oomwrap-x.scope"]);
        let (system, user) = units_on_path("/system.slice/job.service/sub");
        assert_eq!(system, ["-.slice", "system.slice", "job.service"]);
        assert!(user.is_empty());
    }

    #[test]
    fn reads_the_oomd_settings_of_systemctl() {
        assert!(asks_oomd_to_kill(
            "ManagedOOMMemoryPressure=auto\nManagedOOMSwap=kill\n"
        ));
        assert!(asks_oomd_to_kill("ManagedOOMMemoryPressure=kill\n"));
        assert!(!asks_oomd_to_kill(
            "ManagedOOMMemoryPressure=auto\nManagedOOMSwap=auto\n"
        ));
    }

    #[test]
    fn the_guard_override_decides_alone() {
        let fake = FakeHost::new();
        let mut host = fake.host();
        assert!(host.machine_guards(None, &[]).unwrap().is_empty());
        host.machine_guard_override = Some(true);
        assert_eq!(host.machine_guards(None, &[]).unwrap().len(), 1);
    }

    #[test]
    fn state_and_runtime_folders_follow_the_xdg_rules() {
        let fake = FakeHost::new();
        let mut host = fake.host();
        assert!(host.state_dir().is_err());
        host.home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            host.state_dir().unwrap(),
            PathBuf::from("/home/u/.local/state")
        );
        host.state_home = Some(PathBuf::from("/state"));
        assert_eq!(host.state_dir().unwrap(), PathBuf::from("/state"));
        assert_eq!(host.control_parent(), host.temp_dir.as_path());
        host.runtime_dir = Some(PathBuf::from("/missing/runtime"));
        assert_eq!(host.control_parent(), host.temp_dir.as_path());
        let runtime = fake.cgroup_root();
        host.runtime_dir = Some(runtime.clone());
        assert_eq!(host.control_parent(), runtime.as_path());
    }

    #[test]
    fn lingering_is_a_file_named_after_the_user() {
        let fake = FakeHost::new();
        let mut host = fake.host();
        assert!(!host.lingering());
        fake.linger();
        assert!(host.lingering());
        host.user_name = None;
        assert!(!host.lingering());
    }

    #[test]
    fn missing_cgroup_files_mean_no_cgroup() {
        let fake = FakeHost::new();
        let host = fake.host_without_cgroups();
        assert_eq!(host.cgroup_root().unwrap(), None);
        assert_eq!(host.own_cgroup().unwrap(), None);
        assert!(host.meminfo().is_err());
        assert!(host.home().is_err());
        fake.proc_file(
            "self/mountinfo",
            "30 1 0:26 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n",
        );
        assert_eq!(
            host.cgroup_root().unwrap(),
            Some(PathBuf::from("/sys/fs/cgroup"))
        );
    }
}
