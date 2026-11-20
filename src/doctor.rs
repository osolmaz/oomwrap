//! `oomwrap doctor` and `oomwrap inspect`: what the host offers, which
//! backend `auto` would take, the memory domain, every resolved setting
//! with its source, and what needs repair. Neither command changes the host.

use std::env;
use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use crate::backend::{Manager, is_own_scope_dir};
use crate::config::{
    BackendRequest, Policy, Profile, Settings, Source, default_shim_tools, engine_process_names,
};
use crate::domain::{Domain, MemoryDomain, cgroup_dir, read_cgroup_limit, self_and_ancestors};
use crate::host::{Host, MachineGuard, ProcessInfo, disk_available, process_list};
use crate::job_limits::JobLimitSettings;
use crate::limits::{
    DEFAULT_DOMAIN_PRESSURE, DEFAULT_JOB_PRESSURE, DEFAULT_MIN_MEM, DEFAULT_POLL,
    DEFAULT_TERM_GRACE, format_bytes, format_duration,
};
use crate::readings::{DomainReadings, parse_arc_size};
use crate::shims::{
    FoundScript, current_oomwrap_bin, default_bin_dir, find_scripts, wrapped_paths,
};
use crate::supervise::{Plan, choose_plan, unavailable_message};

/// What the host offers the `cgroup` backend, and what `auto` takes.
#[derive(Debug, Serialize)]
struct CgroupReport {
    v2_mounted: bool,
    v1_mounted: bool,
    own_cgroup: Option<String>,
    /// The cgroup of the service manager that would start a scope.
    manager: String,
    lingering: bool,
    /// The controllers that the manager's cgroup offers its units.
    controllers: Vec<String>,
    cgroup_kill: bool,
    psi: bool,
    /// `cgroup`, `poll`, or `none` when `run` would exit with 3.
    backend: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    backend_detail: Option<String>,
}

/// A cgroup at or above the caller that sets a memory limit.
#[derive(Debug, Serialize)]
struct CgroupLimit {
    cgroup: String,
    memory_max: Option<u64>,
    memory_high: Option<u64>,
    memory_swap_max: Option<u64>,
}

/// One resolved setting and where it came from.
#[derive(Debug, Serialize)]
struct SettingReport {
    key: &'static str,
    value: String,
    source: Source,
}

#[derive(Debug, Serialize)]
struct PolicyFileReport {
    path: PathBuf,
    exists: bool,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    os: String,
    arch: String,
    cgroup: CgroupReport,
    domain: Domain,
    readings: DomainReadings,
    limits_above: Vec<CgroupLimit>,
    profile: String,
    policy_files: Vec<PolicyFileReport>,
    settings: Vec<SettingReport>,
    machine_guards: Vec<MachineGuard>,
    hugetlb_bytes: u64,
    zfs_arc_bytes: Option<u64>,
    /// `oomwrap-*` scopes whose guard holds no lock.
    scopes_without_guard: Vec<String>,
    scripts: Vec<FoundScript>,
    home_disk_available_bytes: Option<u64>,
    known_high_memory_processes: Vec<ProcessInfo>,
    warnings: Vec<String>,
}

impl DoctorReport {
    fn collect(host: &Host, profile: Profile) -> Result<Self> {
        let policy = Policy::load(&host.policy_files)?;
        let resolved = policy.resolve(&Settings::default(), profile);
        let own = host.own_cgroup()?;
        let memory = MemoryDomain::find(host)?;
        let mut report = Self {
            os: env::consts::OS.to_string(),
            arch: env::consts::ARCH.to_string(),
            cgroup: cgroup_report(host, &resolved.settings)?,
            domain: memory.domain.clone(),
            readings: memory.read(host)?,
            limits_above: limits_above(host, own.as_deref())?,
            profile: profile.name(),
            policy_files: policy_files(host),
            settings: setting_reports(&resolved.settings, resolved.sources, profile),
            machine_guards: host.machine_guards(own.as_deref(), &policy.machine_guards())?,
            hugetlb_bytes: host.meminfo()?.hugetlb_bytes,
            zfs_arc_bytes: host
                .read_proc("spl/kstat/zfs/arcstats")
                .ok()
                .and_then(|text| parse_arc_size(&text)),
            scopes_without_guard: scopes_without_guard(host),
            scripts: scripts(host),
            home_disk_available_bytes: host
                .home
                .as_deref()
                .and_then(|path| disk_available(path).ok()),
            known_high_memory_processes: known_high_memory_processes()?,
            warnings: Vec::new(),
        };
        report.warnings = warnings(&report, host.in_container);
        Ok(report)
    }
}

fn policy_files(host: &Host) -> Vec<PolicyFileReport> {
    host.policy_files
        .iter()
        .map(|path| PolicyFileReport {
            path: path.clone(),
            exists: path.exists(),
        })
        .collect()
}

fn cgroup_report(host: &Host, settings: &Settings) -> Result<CgroupReport> {
    let mounts = host.cgroup_mounts()?;
    let root = host.cgroup_root()?;
    let own = host.own_cgroup()?;
    let manager = Manager::of(host);
    let cgroup_kill = match (&root, &own) {
        (Some(root), Some(own)) => cgroup_dir(root, own).join("cgroup.kill").exists(),
        _ => false,
    };
    let (backend, backend_detail) = backend_choice(host, settings);
    Ok(CgroupReport {
        v2_mounted: mounts.v2_mount.is_some(),
        v1_mounted: mounts.has_v1,
        own_cgroup: own,
        manager: manager.cgroup(),
        lingering: host.lingering(),
        controllers: controllers(root.as_deref(), &manager.cgroup()),
        cgroup_kill,
        psi: host.read_proc("pressure/memory").is_ok(),
        backend,
        backend_detail,
    })
}

fn controllers(root: Option<&Path>, cgroup: &str) -> Vec<String> {
    root.and_then(|root| {
        std::fs::read_to_string(cgroup_dir(root, cgroup).join("cgroup.controllers")).ok()
    })
    .map(|text| text.split_whitespace().map(str::to_string).collect())
    .unwrap_or_default()
}

/// The backend that `run` would take with these settings, and why.
fn backend_choice(host: &Host, settings: &Settings) -> (&'static str, Option<String>) {
    let request = settings.backend.unwrap_or(BackendRequest::Auto);
    let explicit = JobLimitSettings {
        memory_max: settings.memory_max,
        memory_high: settings.memory_high,
        memory_swap_max: settings.memory_swap_max,
    }
    .is_explicit();
    match choose_plan(request, explicit, host) {
        Ok(Plan::Cgroup(_)) => ("cgroup", None),
        Ok(Plan::Poll { why }) => ("poll", why),
        Err(unavailable) => ("none", Some(unavailable_message(&unavailable, request))),
    }
}

/// The cgroups at and above the caller that set a memory limit, closest
/// first.
fn limits_above(host: &Host, own: Option<&str>) -> Result<Vec<CgroupLimit>> {
    let (Some(root), Some(own)) = (host.cgroup_root()?, own) else {
        return Ok(Vec::new());
    };
    let mut limits = Vec::new();
    for cgroup in self_and_ancestors(own) {
        let limit = CgroupLimit {
            memory_max: read_cgroup_limit(&root, &cgroup, "memory.max")?,
            memory_high: read_cgroup_limit(&root, &cgroup, "memory.high")?,
            memory_swap_max: read_cgroup_limit(&root, &cgroup, "memory.swap.max")?,
            cgroup,
        };
        if [limit.memory_max, limit.memory_high, limit.memory_swap_max]
            .iter()
            .any(Option::is_some)
        {
            limits.push(limit);
        }
    }
    Ok(limits)
}

fn setting_reports(
    settings: &Settings,
    sources: Vec<Source>,
    profile: Profile,
) -> Vec<SettingReport> {
    settings
        .entries()
        .into_iter()
        .zip(sources)
        .map(|((key, value), source)| SettingReport {
            key,
            value: value.unwrap_or_else(|| default_text(key, profile)),
            source,
        })
        .collect()
}

/// The default of a setting, as the reader of a report needs it.
fn default_text(key: &str, profile: Profile) -> String {
    match key {
        "backend" => "auto".to_string(),
        "memory-max" => "smallest of 85%, the free room minus --min-mem, and the lowest memory.max above the scope".to_string(),
        "memory-high" => "90% of --memory-max".to_string(),
        "memory-swap-max" => "not set".to_string(),
        "min-mem" => DEFAULT_MIN_MEM.to_string(),
        "domain-pressure" => DEFAULT_DOMAIN_PRESSURE.at_least.to_string(),
        "domain-pressure-for" => format_duration(DEFAULT_DOMAIN_PRESSURE.lasting),
        "job-pressure" => DEFAULT_JOB_PRESSURE.at_least.to_string(),
        "job-pressure-for" => format_duration(DEFAULT_JOB_PRESSURE.lasting),
        "poll" => format_duration(DEFAULT_POLL),
        "term-grace" => format_duration(DEFAULT_TERM_GRACE),
        "require-machine-guard" => profile.requires_machine_guard().to_string(),
        _ => "one file per day in ${XDG_STATE_HOME:-~/.local/state}/oomwrap".to_string(),
    }
}

/// `oomwrap-*` scopes under the manager whose lock nobody holds: their
/// guard is gone, so their kernel limits hold but no pressure stop.
fn scopes_without_guard(host: &Host) -> Vec<String> {
    let manager = Manager::of(host);
    let (Ok(Some(root)), Some(locks)) = (host.cgroup_root(), manager.lock_dir(host)) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(cgroup_dir(&root, &manager.scope_parent())) else {
        return Vec::new();
    };
    let mut lost: Vec<String> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|dir| is_own_scope_dir(dir))
        .filter_map(|dir| {
            let name = dir.file_name()?.to_str()?;
            Some(name.strip_suffix(".scope")?.to_string())
        })
        .filter(|unit| !lock_is_held(&locks.join(format!("{unit}.lock"))))
        .collect();
    lost.sort();
    lost
}

/// Tells whether a guard holds its lock. A missing lock file has no guard.
fn lock_is_held(path: &Path) -> bool {
    File::open(path).is_ok_and(|file| file.try_lock_shared().is_err())
}

fn scripts(host: &Host) -> Vec<FoundScript> {
    let mut dirs = host.path_dirs.clone();
    if let Ok(bin_dir) = default_bin_dir(host) {
        dirs.push(bin_dir);
    }
    find_scripts(&dirs, &wrapped_paths(host))
}

fn warnings(report: &DoctorReport, in_container: bool) -> Vec<String> {
    let mut warnings = guard_warnings(&report.machine_guards, in_container);
    if report.readings.full_avg10.is_none() {
        warnings.push(
            "the kernel reports no memory pressure (PSI) for the domain; the pressure stops are off"
                .to_string(),
        );
    }
    if !report.known_high_memory_processes.is_empty() {
        warnings.push("known high-memory processes are already running".to_string());
    }
    warnings.extend(memory_warnings(report));
    warnings.extend(repair_warnings(report));
    warnings
}

fn guard_warnings(guards: &[MachineGuard], in_container: bool) -> Vec<String> {
    let mut warnings = Vec::new();
    if guards.is_empty() {
        let mut warning = "no machine-wide guard is active; the inference profiles will refuse to launch by default".to_string();
        if in_container {
            warning.push_str(" (inside a container, a guard of the host is not visible)");
        }
        warnings.push(warning);
    }
    if guards.contains(&MachineGuard::Earlyoom) && guards.contains(&MachineGuard::SystemdOomd) {
        warnings.push(
            "earlyoom and systemd-oomd both watch the job's cgroups; two tools may kill for the same pressure"
                .to_string(),
        );
    }
    warnings
}

/// Memory that the free room does not show as it is, and a manager that
/// cannot hold memory limits.
fn memory_warnings(report: &DoctorReport) -> Vec<String> {
    let mut warnings = Vec::new();
    if report.hugetlb_bytes > 0 {
        warnings.push(format!(
            "{} is reserved for huge pages; the machine domain leaves it out of its size",
            format_bytes(report.hugetlb_bytes)
        ));
    }
    if let Some(arc) = report.zfs_arc_bytes.filter(|bytes| *bytes > 0) {
        warnings.push(format!(
            "a ZFS ARC holds {}; MemAvailable does not count it as free, so the free room reads low",
            format_bytes(arc)
        ));
    }
    let cgroup = &report.cgroup;
    let controllers = &cgroup.controllers;
    if !controllers.is_empty() && !controllers.iter().any(|name| name == "memory") {
        warnings.push(format!(
            "the cgroup {} offers no memory controller, so a scope gets no memory limits",
            cgroup.manager
        ));
    }
    warnings
}

/// Leftovers that need a hand: scopes without a guard, outdated scripts.
fn repair_warnings(report: &DoctorReport) -> Vec<String> {
    let scopes = report.scopes_without_guard.iter().map(|unit| {
        format!(
            "the scope {unit} runs without its guard: its kernel limits hold, but no pressure stop"
        )
    });
    let scripts = report
        .scripts
        .iter()
        .filter(|script| script.outdated)
        .map(|script| {
            format!(
                "{} comes from an earlier oomwrap and passes flags that this version does not have; write it again: {}",
                script.path.display(),
                script.fix
            )
        });
    scopes.chain(scripts).collect()
}

pub(crate) fn doctor(host: &Host, profile: Profile, json: bool) -> Result<()> {
    let report = DoctorReport::collect(host, profile)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!("oomwrap doctor");
    println!("  os: {} {}", report.os, report.arch);
    print_cgroup(&report.cgroup);
    print_domain(&report);
    println!("  settings for profile {}:", report.profile);
    for setting in &report.settings {
        println!(
            "    {}: {} ({})",
            setting.key, setting.value, setting.source
        );
    }
    println!(
        "  machine-wide guards: {}",
        guard_names(&report.machine_guards)
    );
    print_processes(&report);
    for warning in &report.warnings {
        println!("  warning: {warning}");
    }
    Ok(())
}

fn print_cgroup(cgroup: &CgroupReport) {
    println!(
        "  cgroup v2: {}, cgroup v1: {}, cgroup.kill: {}, PSI: {}",
        yes_no(cgroup.v2_mounted),
        yes_no(cgroup.v1_mounted),
        yes_no(cgroup.cgroup_kill),
        yes_no(cgroup.psi)
    );
    let controllers = if cgroup.controllers.is_empty() {
        "none".to_string()
    } else {
        cgroup.controllers.join(" ")
    };
    println!(
        "  manager: {} (lingering: {}, controllers: {controllers})",
        cgroup.manager,
        yes_no(cgroup.lingering)
    );
    match &cgroup.backend_detail {
        Some(detail) => println!("  backend: {} ({detail})", cgroup.backend),
        None => println!("  backend: {}", cgroup.backend),
    }
}

fn print_domain(report: &DoctorReport) {
    println!(
        "  memory domain: {} size={} free={} pressure={}",
        report.domain,
        format_bytes(report.readings.size_bytes),
        format_bytes(report.readings.free_bytes),
        report.readings.full_avg10.map_or_else(
            || "not reported".to_string(),
            |pressure| pressure.to_string()
        )
    );
    for limit in &report.limits_above {
        println!(
            "    limit at {}: memory.max={} memory.high={} memory.swap.max={}",
            limit.cgroup,
            limit_text(limit.memory_max),
            limit_text(limit.memory_high),
            limit_text(limit.memory_swap_max)
        );
    }
}

fn print_processes(report: &DoctorReport) {
    if let Some(home_disk_available) = report.home_disk_available_bytes {
        println!(
            "  home disk available: {}",
            format_bytes(home_disk_available)
        );
    }
    println!(
        "  known high-memory processes: {}",
        report.known_high_memory_processes.len()
    );
    for process in report.known_high_memory_processes.iter().take(10) {
        println!(
            "    pid={} rss={} cmd={}",
            process.pid,
            format_bytes(process.rss_bytes.unwrap_or(0)),
            process.command
        );
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn limit_text(limit: Option<u64>) -> String {
    limit.map_or_else(|| "max".to_string(), format_bytes)
}

fn guard_names(guards: &[MachineGuard]) -> String {
    if guards.is_empty() {
        return "none".to_string();
    }
    guards
        .iter()
        .map(|guard| match guard {
            MachineGuard::Earlyoom => "earlyoom".to_string(),
            MachineGuard::SystemdOomd => "systemd-oomd".to_string(),
            MachineGuard::Declared(name) => name.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Serialize)]
struct InspectReport {
    oomwrap_bin: String,
    home_bin_dir: String,
    default_tools: Vec<String>,
    doctor: DoctorReport,
}

pub(crate) fn inspect(host: &Host, json: bool) -> Result<()> {
    let info = InspectReport {
        oomwrap_bin: current_oomwrap_bin()?.display().to_string(),
        home_bin_dir: default_bin_dir(host)?.display().to_string(),
        default_tools: default_shim_tools(),
        doctor: DoctorReport::collect(host, Profile::Generic)?,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&info)?);
        return Ok(());
    }
    println!("oomwrap inspect");
    println!("  binary: {}", info.oomwrap_bin);
    println!("  default bin dir: {}", info.home_bin_dir);
    println!("  default tools: {}", info.default_tools.join(", "));
    for file in &info.doctor.policy_files {
        let state = if file.exists { "read" } else { "not present" };
        println!("  policy file: {} ({state})", file.path.display());
    }
    println!("  backend: {}", info.doctor.cgroup.backend);
    Ok(())
}

fn known_high_memory_processes() -> Result<Vec<ProcessInfo>> {
    let current_pid = std::process::id();
    Ok(process_list()?
        .into_iter()
        .filter(|process| process.pid != current_pid)
        .filter(|process| is_known_high_memory_command(&process.command))
        .collect())
}

fn is_known_high_memory_command(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    engine_process_names().any(|name| lower.contains(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::tests::FakeHost;

    const V2_ONLY: &str = "30 1 0:26 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n";
    const MANAGER: &str = "/user.slice/user-1000.slice/user@1000.service";

    /// The warnings that the fake host decides; the real process list can
    /// hold engines that the tests did not start.
    fn host_warnings(report: &DoctorReport) -> Vec<&String> {
        report
            .warnings
            .iter()
            .filter(|warning| !warning.starts_with("known high-memory processes"))
            .collect()
    }

    #[test]
    fn matches_engine_names_anywhere_in_the_command() {
        assert!(is_known_high_memory_command("/opt/bin/llama-server -m x"));
        assert!(is_known_high_memory_command("python3 -m VLLM.entrypoints"));
        assert!(is_known_high_memory_command("torchrun --nproc 8 train.py"));
        assert!(!is_known_high_memory_command("bash -lc make"));
    }

    /// A cgroup v2 host whose caller runs inside the user manager.
    fn cgroup_host(fake: &FakeHost) -> Host {
        fake.proc_file("self/mountinfo", V2_ONLY);
        fake.proc_file(
            "pressure/memory",
            "full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
        );
        fake.meminfo(16 << 30, 10 << 30, 0);
        let own = format!("{MANAGER}/app.slice/shell.service");
        fake.own_cgroup(&own);
        fake.cgroup_file(&own, "cgroup.kill", "");
        fake.cgroup_file(MANAGER, "cgroup.controllers", "cpu io memory pids\n");
        let mut host = fake.host();
        host.runtime_dir = Some(fake.cgroup_root().join("run"));
        host.machine_guard_override = Some(true);
        host.home = None;
        host
    }

    #[test]
    fn the_report_names_the_backend_the_domain_and_the_settings() {
        let fake = FakeHost::new();
        let mut host = cgroup_host(&fake);
        let policy = fake.cgroup_root().join("config.toml");
        std::fs::write(&policy, "[profile.vllm]\nmin-mem = \"2G\"\n").unwrap();
        host.policy_files = vec![policy.clone(), fake.cgroup_root().join("none.toml")];
        fake.cgroup_file(MANAGER, "memory.high", "8589934592\n");
        let report = DoctorReport::collect(&host, Profile::Vllm).unwrap();
        assert_eq!(report.cgroup.backend, "cgroup");
        assert!(report.cgroup.cgroup_kill && report.cgroup.v2_mounted && report.cgroup.psi);
        assert!(!report.cgroup.v1_mounted);
        assert_eq!(report.cgroup.controllers, ["cpu", "io", "memory", "pids"]);
        assert_eq!(report.limits_above.len(), 1);
        assert_eq!(report.limits_above[0].cgroup, MANAGER);
        assert_eq!(report.limits_above[0].memory_high, Some(8 << 30));
        let setting = |key: &str| report.settings.iter().find(|s| s.key == key).unwrap();
        assert_eq!(setting("min-mem").value, "2G");
        assert_eq!(
            setting("min-mem").source.to_string(),
            format!("{} [profile.vllm]", policy.display())
        );
        let guard = setting("require-machine-guard");
        assert_eq!(
            (guard.value.as_str(), &guard.source),
            ("true", &Source::Default)
        );
        assert!(report.policy_files[0].exists && !report.policy_files[1].exists);
        assert!(host_warnings(&report).is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn every_default_has_a_text() {
        let count = Settings::default().entries().len();
        let reports = setting_reports(
            &Settings::default(),
            vec![Source::Default; count],
            Profile::Generic,
        );
        let texts: Vec<&str> = reports.iter().map(|report| report.value.as_str()).collect();
        assert_eq!(
            texts,
            [
                "auto",
                "smallest of 85%, the free room minus --min-mem, and the lowest memory.max above the scope",
                "90% of --memory-max",
                "not set",
                "15%, at least 256M, at most 16G",
                "20.00%",
                "10s",
                "60.00%",
                "30s",
                "1s",
                "10s",
                "false",
                "one file per day in ${XDG_STATE_HOME:-~/.local/state}/oomwrap",
            ]
        );
    }

    #[test]
    fn a_host_without_cgroup_v2_names_why_and_what_run_does() {
        let fake = FakeHost::new();
        fake.meminfo(16 << 30, 10 << 30, 1 << 30);
        fake.proc_file("spl/kstat/zfs/arcstats", "size 4 2147483648\n");
        let mut host = fake.host_without_cgroups();
        host.machine_guard_override = Some(false);
        let report = DoctorReport::collect(&host, Profile::Generic).unwrap();
        assert_eq!(report.cgroup.backend, "poll");
        let detail = report.cgroup.backend_detail.clone().unwrap();
        assert!(detail.starts_with("auto took poll: "), "{detail}");
        assert!(report.limits_above.is_empty());
        assert_eq!(report.zfs_arc_bytes, Some(2 << 30));
        let text = report.warnings.join("\n");
        assert!(text.contains("no machine-wide guard"), "{text}");
        assert!(text.contains("1.0GiB is reserved for huge pages"), "{text}");
        assert!(text.contains("a ZFS ARC holds 2.0GiB"), "{text}");
        let cgroup = Settings {
            backend: Some(BackendRequest::Cgroup),
            ..Settings::default()
        };
        let (backend, detail) = backend_choice(&host, &cgroup);
        assert_eq!(backend, "none");
        assert!(detail.unwrap().contains("--backend cgroup needs it"));
        let poll = Settings {
            backend: Some(BackendRequest::Poll),
            ..Settings::default()
        };
        assert_eq!(backend_choice(&host, &poll), ("poll", None));
    }

    #[test]
    fn a_scope_whose_lock_nobody_holds_has_lost_its_guard() {
        let fake = FakeHost::new();
        let host = cgroup_host(&fake);
        let parent = format!("{MANAGER}/app.slice");
        for unit in ["oomwrap-g-1", "oomwrap-g-2", "oomwrap-g-3"] {
            fake.cgroup_file(&format!("{parent}/{unit}.scope"), "cgroup.procs", "");
        }
        fake.cgroup_file(&format!("{parent}/other.scope"), "cgroup.procs", "");
        let locks = host.runtime_dir.clone().unwrap().join("oomwrap");
        std::fs::create_dir_all(&locks).unwrap();
        let held = File::create(locks.join("oomwrap-g-1.lock")).unwrap();
        held.try_lock().unwrap();
        File::create(locks.join("oomwrap-g-2.lock")).unwrap();
        assert_eq!(scopes_without_guard(&host), ["oomwrap-g-2", "oomwrap-g-3"]);
        let report = DoctorReport::collect(&host, Profile::Generic).unwrap();
        assert!(
            host_warnings(&report)[0].contains("the scope oomwrap-g-2 runs without its guard"),
            "{:?}",
            report.warnings
        );
        drop(held);
    }

    #[test]
    fn warns_about_guards_memory_and_controllers() {
        let both = [MachineGuard::Earlyoom, MachineGuard::SystemdOomd];
        assert!(guard_warnings(&both, false)[0].contains("two tools may kill"));
        assert!(guard_warnings(&[], true)[0].contains("inside a container"));
        assert!(guard_warnings(&[MachineGuard::Earlyoom], false).is_empty());
        let fake = FakeHost::new();
        let host = cgroup_host(&fake);
        fake.cgroup_file(MANAGER, "cgroup.controllers", "cpu pids\n");
        let report = DoctorReport::collect(&host, Profile::Generic).unwrap();
        assert!(memory_warnings(&report)[0].contains("offers no memory controller"));
    }

    #[test]
    fn names_the_guards_and_limits() {
        assert_eq!(guard_names(&[]), "none");
        assert_eq!(
            guard_names(&[
                MachineGuard::Earlyoom,
                MachineGuard::SystemdOomd,
                MachineGuard::Declared("custom".to_string())
            ]),
            "earlyoom, systemd-oomd, custom"
        );
        assert_eq!(limit_text(None), "max");
        assert_eq!(limit_text(Some(1 << 30)), "1.0GiB");
        assert_eq!((yes_no(true), yes_no(false)), ("yes", "no"));
    }
}
