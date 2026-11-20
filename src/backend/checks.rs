//! What the `cgroup` backend needs before a launch, the `systemd-run`
//! command line, and the check of the scope that the launch reports.

use std::collections::hash_map::RandomState;
use std::ffi::OsString;
use std::fmt;
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};

use crate::domain::{cgroup_dir, read_cgroup_limit, self_and_ancestors};
use crate::host::Host;
use crate::job_limits::{JobLimits, KernelLimit};

/// The prefix of every scope and lock that oomwrap makes.
pub(crate) const UNIT_PREFIX: &str = "oomwrap-";
const SCOPE_SUFFIX: &str = ".scope";
/// The limit files that a scope gets, and that a caller's cgroup must not set.
const LIMIT_FILES: [&str; 3] = ["memory.max", "memory.high", "memory.swap.max"];

/// The service manager that starts the scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Manager {
    /// The user's own manager, `user@<uid>.service`.
    User { uid: u32 },
    /// The system manager, for a caller that runs as root.
    System,
}

impl Manager {
    pub(crate) fn of(host: &Host) -> Self {
        if host.is_root {
            Self::System
        } else {
            Self::User { uid: host.uid }
        }
    }

    /// The cgroup that holds the manager and its units.
    pub(crate) fn cgroup(self) -> String {
        match self {
            Self::User { uid } => format!("/user.slice/user-{uid}.slice/user@{uid}.service"),
            Self::System => "/".to_string(),
        }
    }

    /// The cgroup where `systemd-run --scope` puts a new scope.
    pub(crate) fn scope_parent(self) -> String {
        match self {
            Self::User { .. } => format!("{}/app.slice", self.cgroup()),
            Self::System => "/system.slice".to_string(),
        }
    }

    /// The folder of the guards' lock files.
    pub(crate) fn lock_dir(self, host: &Host) -> Option<PathBuf> {
        match self {
            Self::User { .. } => host.runtime_dir.as_ref().map(|dir| dir.join("oomwrap")),
            Self::System => Some(PathBuf::from("/run/oomwrap")),
        }
    }
}

/// Why the `cgroup` backend cannot hold the job.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Unavailable {
    /// A cgroup v1 hierarchy is mounted, or no cgroup v2 hierarchy.
    NotCgroupV2Only,
    /// The kernel has no `cgroup.kill` (Linux 5.14 or later).
    NoCgroupKill,
    /// A cgroup that the job would leave sets a limit.
    CallerLimit { cgroup: String, file: &'static str },
    /// The user manager would end with the user's last session.
    NoLingering,
    /// `XDG_RUNTIME_DIR` is not set: no place for the lock, and no user bus.
    NoRuntimeDir,
    /// A file that a check reads could not be read.
    Unreadable(String),
}

impl fmt::Display for Unavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotCgroupV2Only => {
                formatter.write_str("the host does not use cgroup v2 alone (cgroup v1 or a hybrid host)")
            }
            Self::NoCgroupKill => {
                formatter.write_str("the kernel has no cgroup.kill (Linux 5.14 or later)")
            }
            Self::CallerLimit { cgroup, file } => write!(
                formatter,
                "the caller's cgroup {cgroup} sets {file}, which a scope would leave; the job stays in place"
            ),
            Self::NoLingering => formatter.write_str(
                "lingering is off, so the user manager and a scope under it end with the last session (loginctl enable-linger)",
            ),
            Self::NoRuntimeDir => {
                formatter.write_str("XDG_RUNTIME_DIR is not set, so there is no user bus and no lock folder")
            }
            Self::Unreadable(error) => write!(formatter, "a cgroup check failed: {error}"),
        }
    }
}

/// The checks that the launch cannot test: cgroup v2 alone, `cgroup.kill`,
/// no limit on a cgroup that the job would leave, and a manager that
/// outlives the caller's session. The handshake tests the rest.
pub(crate) fn check(host: &Host, manager: Manager) -> Result<(), Unavailable> {
    let unreadable = |error: anyhow::Error| Unavailable::Unreadable(format!("{error:#}"));
    let mounts = host.cgroup_mounts().map_err(unreadable)?;
    let root = host.cgroup_root().map_err(unreadable)?;
    let own = host.own_cgroup().map_err(unreadable)?;
    let (Some(root), Some(own), false) = (root, own, mounts.has_v1) else {
        return Err(Unavailable::NotCgroupV2Only);
    };
    if mounts.v2_mount.is_none() {
        return Err(Unavailable::NotCgroupV2Only);
    }
    if !cgroup_dir(&root, &own).join("cgroup.kill").exists() {
        return Err(Unavailable::NoCgroupKill);
    }
    check_left_cgroups(&root, &own, &manager.cgroup())?;
    if let Manager::User { .. } = manager {
        if !host.lingering() && !is_within(&own, &manager.cgroup()) {
            return Err(Unavailable::NoLingering);
        }
        if manager.lock_dir(host).is_none() {
            return Err(Unavailable::NoRuntimeDir);
        }
    }
    Ok(())
}

/// The cgroups from the caller's up to, and not including, the closest one
/// that also holds the manager must set no limit: a scope leaves them.
fn check_left_cgroups(root: &Path, own: &str, manager: &str) -> Result<(), Unavailable> {
    let shared = common_ancestor(own, manager);
    for cgroup in self_and_ancestors(own)
        .into_iter()
        .take_while(|cgroup| *cgroup != shared)
    {
        for file in LIMIT_FILES {
            let limit = read_cgroup_limit(root, &cgroup, file)
                .map_err(|error| Unavailable::Unreadable(format!("{error:#}")))?;
            if limit.is_some() {
                return Err(Unavailable::CallerLimit { cgroup, file });
            }
        }
    }
    Ok(())
}

/// The closest cgroup that holds both paths.
fn common_ancestor(a: &str, b: &str) -> String {
    let shared: Vec<&str> = a
        .split('/')
        .filter(|part| !part.is_empty())
        .zip(b.split('/').filter(|part| !part.is_empty()))
        .take_while(|(left, right)| left == right)
        .map(|(part, _)| part)
        .collect();
    format!("/{}", shared.join("/"))
}

/// Tells whether `path` is `ancestor` or lies below it.
fn is_within(path: &str, ancestor: &str) -> bool {
    path == ancestor
        || path
            .strip_prefix(ancestor)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// A new unit name, such as `oomwrap-generic-3f2a9c1d0b4e5f60`.
pub(crate) fn unit_name(profile: &str) -> String {
    let random = RandomState::new().hash_one(std::process::id());
    format!("{UNIT_PREFIX}{profile}-{random:016x}")
}

/// The arguments of `systemd-run` that start `program` in a new scope with
/// the job's limits.
pub(crate) fn systemd_run_args(
    manager: Manager,
    unit: &str,
    limits: &JobLimits,
    program: &Path,
    program_args: &[OsString],
) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    if let Manager::User { .. } = manager {
        args.push("--user".into());
    }
    for arg in ["--scope", "--quiet", "--collect", "--unit", unit] {
        args.push(arg.into());
    }
    let mut property = |name: &str, limit: KernelLimit| {
        args.push("-p".into());
        args.push(format!("{name}={}", limit.property_value()).into());
    };
    property("MemoryMax", limits.memory_max);
    property("MemoryHigh", limits.memory_high);
    if let Some(swap) = limits.memory_swap_max {
        property("MemorySwapMax", swap);
    }
    args.push("--".into());
    args.push(program.as_os_str().to_owned());
    args.extend(program_args.iter().cloned());
    args
}

/// The scope that the launch reported, after its check.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScopeCgroup {
    /// The cgroup path, such as `/user.slice/.../oomwrap-x.scope`.
    pub(crate) path: String,
    /// Its folder in the cgroup hierarchy.
    pub(crate) dir: PathBuf,
}

impl ScopeCgroup {
    /// The cgroup above the scope, where the memory domain starts.
    pub(crate) fn parent(&self) -> String {
        match self.path.rfind('/') {
            Some(0) | None => "/".to_string(),
            Some(index) => self.path[..index].to_string(),
        }
    }
}

/// Checks that the launch runs in the requested scope with the requested
/// limits, compared in whole pages.
pub(crate) fn check_scope(
    host: &Host,
    reported: Option<&str>,
    unit: &str,
    limits: &JobLimits,
) -> Result<ScopeCgroup, String> {
    let Some(path) = reported else {
        return Err("the launch reported no cgroup v2 path".to_string());
    };
    let scope_name = format!("{unit}{SCOPE_SUFFIX}");
    if path.rsplit('/').next() != Some(scope_name.as_str()) {
        return Err(format!("the launch runs in {path}, not in {scope_name}"));
    }
    let root = host
        .cgroup_root()
        .map_err(|error| format!("{error:#}"))?
        .ok_or_else(|| "the cgroup v2 hierarchy is gone".to_string())?;
    let expected = [
        ("memory.max", Some(limits.memory_max)),
        ("memory.high", Some(limits.memory_high)),
        ("memory.swap.max", limits.memory_swap_max),
    ];
    for (file, want) in expected {
        let Some(want) = want else {
            continue;
        };
        let found = read_cgroup_limit(&root, path, file).map_err(|error| format!("{error:#}"))?;
        let found = KernelLimit::from_file(found, host.page_size);
        if found != want {
            return Err(format!(
                "the scope's {file} is {found}, not the requested {want}"
            ));
        }
    }
    Ok(ScopeCgroup {
        path: path.to_string(),
        dir: cgroup_dir(&root, path),
    })
}

/// Tells whether a cgroup folder is a scope that oomwrap made; the stop
/// sequence writes `cgroup.kill` only in such a folder.
pub(crate) fn is_own_scope_dir(dir: &Path) -> bool {
    dir.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.starts_with(UNIT_PREFIX)
                && name.ends_with(SCOPE_SUFFIX)
                && name.len() > UNIT_PREFIX.len() + SCOPE_SUFFIX.len()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::tests::FakeHost;

    const USER_MANAGER: &str = "/user.slice/user-1000.slice/user@1000.service";
    const SESSION: &str = "/user.slice/user-1000.slice/session-2.scope";
    const V2_ONLY: &str = "30 1 0:26 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n";

    /// A cgroup v2 host whose caller runs in `own` with `cgroup.kill`.
    fn v2_host(own: &str) -> FakeHost {
        let fake = FakeHost::new();
        fake.proc_file("self/mountinfo", V2_ONLY);
        fake.own_cgroup(own);
        fake.cgroup_file(own, "cgroup.kill", "");
        fake
    }

    fn user() -> Manager {
        Manager::User { uid: 1000 }
    }

    fn limits() -> JobLimits {
        JobLimits {
            memory_max: KernelLimit::Bytes(8192),
            memory_high: KernelLimit::Bytes(4096),
            memory_swap_max: None,
        }
    }

    #[test]
    fn managers_name_their_cgroups_and_lock_folders() {
        let fake = FakeHost::new();
        let mut host = fake.host();
        assert_eq!(Manager::of(&host), user());
        assert_eq!(user().cgroup(), USER_MANAGER);
        assert_eq!(user().scope_parent(), format!("{USER_MANAGER}/app.slice"));
        assert_eq!(user().lock_dir(&host), None);
        host.runtime_dir = Some(PathBuf::from("/run/user/1000"));
        assert_eq!(
            user().lock_dir(&host),
            Some(PathBuf::from("/run/user/1000/oomwrap"))
        );
        host.is_root = true;
        assert_eq!(Manager::of(&host), Manager::System);
        assert_eq!(Manager::System.cgroup(), "/");
        assert_eq!(Manager::System.scope_parent(), "/system.slice");
        assert_eq!(
            Manager::System.lock_dir(&host),
            Some(PathBuf::from("/run/oomwrap"))
        );
    }

    #[test]
    fn a_caller_inside_the_user_manager_passes_every_check() {
        let own = format!("{USER_MANAGER}/app.slice/shell.service");
        let fake = v2_host(&own);
        let mut host = fake.host();
        host.runtime_dir = Some(PathBuf::from("/run/user/1000"));
        assert_eq!(check(&host, user()), Ok(()));
        host.runtime_dir = None;
        assert_eq!(check(&host, user()), Err(Unavailable::NoRuntimeDir));
    }

    #[test]
    fn a_session_caller_needs_lingering() {
        let fake = v2_host(SESSION);
        let mut host = fake.host();
        host.runtime_dir = Some(PathBuf::from("/run/user/1000"));
        assert_eq!(check(&host, user()), Err(Unavailable::NoLingering));
        fake.linger();
        assert_eq!(check(&host, user()), Ok(()));
        host.is_root = true;
        assert_eq!(check(&host, Manager::System), Ok(()));
    }

    #[test]
    fn cgroup_v1_or_no_cgroup_v2_uses_poll() {
        let fake = v2_host(SESSION);
        fake.linger();
        let host = fake.host();
        fake.proc_file(
            "self/mountinfo",
            &format!("{V2_ONLY}31 1 0:27 / /sys/fs/cgroup/memory rw - cgroup cgroup rw,memory\n"),
        );
        assert_eq!(check(&host, user()), Err(Unavailable::NotCgroupV2Only));
        fake.proc_file("self/mountinfo", "");
        assert_eq!(check(&host, user()), Err(Unavailable::NotCgroupV2Only));
        let no_own = FakeHost::new();
        no_own.proc_file("self/mountinfo", V2_ONLY);
        assert_eq!(
            check(&no_own.host(), user()),
            Err(Unavailable::NotCgroupV2Only)
        );
    }

    #[test]
    fn an_old_kernel_without_cgroup_kill_uses_poll() {
        let fake = FakeHost::new();
        fake.proc_file("self/mountinfo", V2_ONLY);
        fake.own_cgroup(SESSION);
        fake.linger();
        assert_eq!(check(&fake.host(), user()), Err(Unavailable::NoCgroupKill));
    }

    #[test]
    fn a_limit_on_a_cgroup_that_the_job_would_leave_keeps_it_in_place() {
        let own = format!("{SESSION}/inner");
        let fake = v2_host(&own);
        fake.linger();
        let mut host = fake.host();
        host.runtime_dir = Some(PathBuf::from("/run/user/1000"));
        fake.cgroup_file(SESSION, "memory.high", "1073741824\n");
        assert_eq!(
            check(&host, user()),
            Err(Unavailable::CallerLimit {
                cgroup: SESSION.to_string(),
                file: "memory.high"
            })
        );
        fake.cgroup_file(SESSION, "memory.high", "max\n");
        // A limit on a cgroup that also holds the manager stays in force.
        fake.cgroup_file("/user.slice/user-1000.slice", "memory.max", "1073741824\n");
        assert_eq!(check(&host, user()), Ok(()));
        fake.cgroup_file(&own, "memory.swap.max", "x\n");
        assert!(matches!(
            check(&host, user()),
            Err(Unavailable::Unreadable(_))
        ));
    }

    #[test]
    fn paths_share_their_closest_ancestor() {
        assert_eq!(
            common_ancestor(SESSION, USER_MANAGER),
            "/user.slice/user-1000.slice"
        );
        assert_eq!(common_ancestor("/system.slice/a.service", "/"), "/");
        assert_eq!(common_ancestor("/a/b", "/a/b"), "/a/b");
        assert!(is_within(
            &format!("{USER_MANAGER}/app.slice"),
            USER_MANAGER
        ));
        assert!(is_within(USER_MANAGER, USER_MANAGER));
        assert!(!is_within(&format!("{USER_MANAGER}x"), USER_MANAGER));
    }

    #[test]
    fn unit_names_are_new_each_time() {
        let first = unit_name("generic");
        assert!(first.starts_with("oomwrap-generic-"));
        assert_eq!(first.len(), "oomwrap-generic-".len() + 16);
        assert_ne!(first, unit_name("generic"));
    }

    #[test]
    fn systemd_run_gets_the_scope_and_the_limits() {
        let args = systemd_run_args(
            user(),
            "oomwrap-generic-1",
            &JobLimits {
                memory_swap_max: Some(KernelLimit::Max),
                ..limits()
            },
            Path::new("/proc/7/exe"),
            &["exec-job".into()],
        );
        let args: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--unit",
                "oomwrap-generic-1",
                "-p",
                "MemoryMax=8192",
                "-p",
                "MemoryHigh=4096",
                "-p",
                "MemorySwapMax=infinity",
                "--",
                "/proc/7/exe",
                "exec-job"
            ]
        );
        let system = systemd_run_args(Manager::System, "u", &limits(), Path::new("p"), &[]);
        assert_eq!(system[0], "--scope");
        assert!(!system.iter().any(|arg| arg == "MemorySwapMax=infinity"));
    }

    #[test]
    fn the_scope_must_be_the_unit_with_the_requested_limits() {
        let fake = FakeHost::new();
        let host = fake.host();
        let path = format!("{USER_MANAGER}/app.slice/oomwrap-g-1.scope");
        fake.cgroup_file(&path, "memory.max", "8192\n");
        fake.cgroup_file(&path, "memory.high", "4097\n");
        let scope = check_scope(&host, Some(&path), "oomwrap-g-1", &limits()).unwrap();
        assert_eq!(scope.path, path);
        assert_eq!(
            scope.dir,
            fake.cgroup_root().join(path.trim_start_matches('/'))
        );
        assert_eq!(scope.parent(), format!("{USER_MANAGER}/app.slice"));
        assert!(check_scope(&host, None, "oomwrap-g-1", &limits()).is_err());
        let elsewhere = check_scope(&host, Some(SESSION), "oomwrap-g-1", &limits()).unwrap_err();
        assert!(
            elsewhere.contains("not in oomwrap-g-1.scope"),
            "{elsewhere}"
        );
        fake.cgroup_file(&path, "memory.high", "max\n");
        let wrong = check_scope(&host, Some(&path), "oomwrap-g-1", &limits()).unwrap_err();
        assert_eq!(
            wrong,
            "the scope's memory.high is max, not the requested 4096B"
        );
        let swap = JobLimits {
            memory_swap_max: Some(KernelLimit::Bytes(0)),
            memory_high: KernelLimit::Max,
            ..limits()
        };
        fake.cgroup_file(&path, "memory.swap.max", "0\n");
        assert!(check_scope(&host, Some(&path), "oomwrap-g-1", &swap).is_ok());
        let gone = fake.host_without_cgroups();
        assert!(check_scope(&gone, Some(&path), "oomwrap-g-1", &limits()).is_err());
    }

    #[test]
    fn the_root_scope_has_the_root_as_parent() {
        let scope = ScopeCgroup {
            path: "/oomwrap-a.scope".to_string(),
            dir: PathBuf::new(),
        };
        assert_eq!(scope.parent(), "/");
    }

    #[test]
    fn only_own_scope_folders_can_be_killed() {
        assert!(is_own_scope_dir(Path::new(
            "/sys/fs/cgroup/a/oomwrap-g-1.scope"
        )));
        assert!(!is_own_scope_dir(Path::new(
            "/sys/fs/cgroup/a/session-2.scope"
        )));
        assert!(!is_own_scope_dir(Path::new(
            "/sys/fs/cgroup/a/oomwrap-g-1.service"
        )));
        assert!(!is_own_scope_dir(Path::new(
            "/sys/fs/cgroup/a/oomwrap-.scope"
        )));
        assert!(!is_own_scope_dir(Path::new("/")));
    }
}
