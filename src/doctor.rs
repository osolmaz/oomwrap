//! `oomwrap doctor` and `oomwrap inspect`.

use std::env;

use anyhow::Result;
use serde::Serialize;

use crate::config::{default_shim_tools, engine_process_names};
use crate::domain::{Domain, MemoryDomain};
use crate::host::{Host, MachineGuard, ProcessInfo, disk_available, process_list};
use crate::limits::{DEFAULT_DOMAIN_PRESSURE, DEFAULT_MIN_MEM, format_bytes};
use crate::readings::DomainReadings;
use crate::shims::{current_oomwrap_bin, default_bin_dir};

/// The defaults of `oomwrap run` that `inspect` reports, as the flags spell them.
pub(crate) struct Defaults {
    pub(crate) poll: &'static str,
    pub(crate) term_grace: &'static str,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    os: String,
    arch: String,
    domain: Domain,
    readings: DomainReadings,
    machine_guards: Vec<MachineGuard>,
    home_disk_available_bytes: Option<u64>,
    known_high_memory_processes: Vec<ProcessInfo>,
    warnings: Vec<String>,
}

impl DoctorReport {
    // Report construction depends on the live host's processes and guards.
    #[cfg_attr(test, mutants::skip)]
    fn collect(host: &Host) -> Result<Self> {
        let memory = MemoryDomain::find(host)?;
        let readings = memory.read(host)?;
        let machine_guards = host.machine_guards(host.own_cgroup()?.as_deref())?;
        let known_high_memory_processes = known_high_memory_processes()?;
        let warnings = warnings(
            &readings,
            &machine_guards,
            &known_high_memory_processes,
            host.in_container,
        );
        Ok(Self {
            os: env::consts::OS.to_string(),
            arch: env::consts::ARCH.to_string(),
            domain: memory.domain,
            readings,
            machine_guards,
            home_disk_available_bytes: host
                .home
                .as_deref()
                .and_then(|path| disk_available(path).ok()),
            known_high_memory_processes,
            warnings,
        })
    }
}

fn warnings(
    readings: &DomainReadings,
    machine_guards: &[MachineGuard],
    known_high_memory_processes: &[ProcessInfo],
    in_container: bool,
) -> Vec<String> {
    let mut warnings = Vec::new();
    if machine_guards.is_empty() {
        let mut warning = "no machine-wide guard is active; the inference profiles will refuse to launch by default".to_string();
        if in_container {
            warning.push_str(" (inside a container, a guard of the host is not visible)");
        }
        warnings.push(warning);
    }
    if readings.full_avg10.is_none() {
        warnings.push(
            "the kernel reports no memory pressure (PSI) for the domain; the pressure stops are off"
                .to_string(),
        );
    }
    if !known_high_memory_processes.is_empty() {
        warnings.push("known high-memory processes are already running".to_string());
    }
    warnings
}

#[derive(Debug, Serialize)]
struct InspectReport {
    default_min_mem: String,
    default_domain_pressure: String,
    default_domain_pressure_for: String,
    default_poll: String,
    default_term_grace: String,
    default_tools: Vec<String>,
    oomwrap_bin: String,
    home_bin_dir: String,
    doctor: DoctorReport,
}

pub(crate) fn doctor(host: &Host, json: bool) -> Result<()> {
    let report = DoctorReport::collect(host)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!("oomwrap doctor");
    println!("  os: {}", report.os);
    println!("  arch: {}", report.arch);
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
    println!(
        "  machine-wide guards: {}",
        guard_names(&report.machine_guards)
    );
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
    for warning in &report.warnings {
        println!("  warning: {warning}");
    }
    Ok(())
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

pub(crate) fn inspect(host: &Host, defaults: &Defaults, json: bool) -> Result<()> {
    let info = InspectReport {
        default_min_mem: DEFAULT_MIN_MEM.to_string(),
        default_domain_pressure: DEFAULT_DOMAIN_PRESSURE.at_least.to_string(),
        default_domain_pressure_for: format!("{}s", DEFAULT_DOMAIN_PRESSURE.lasting.as_secs()),
        default_poll: defaults.poll.to_string(),
        default_term_grace: defaults.term_grace.to_string(),
        default_tools: default_shim_tools(),
        oomwrap_bin: current_oomwrap_bin()?.display().to_string(),
        home_bin_dir: default_bin_dir(host)?.display().to_string(),
        doctor: DoctorReport::collect(host)?,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        println!("oomwrap inspect");
        println!("  binary: {}", info.oomwrap_bin);
        println!("  default bin dir: {}", info.home_bin_dir);
        println!("  default tools: {}", info.default_tools.join(", "));
        println!(
            "  defaults: min_mem={} domain_pressure={} for {} poll={} term_grace={}",
            info.default_min_mem,
            info.default_domain_pressure,
            info.default_domain_pressure_for,
            info.default_poll,
            info.default_term_grace
        );
    }
    Ok(())
}

// Live process discovery is inherently host-dependent; command matching helpers
// are covered separately.
#[cfg_attr(test, mutants::skip)]
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
    use crate::limits::Pressure;

    #[test]
    fn matches_engine_names_anywhere_in_the_command() {
        assert!(is_known_high_memory_command("/opt/bin/llama-server -m x"));
        assert!(is_known_high_memory_command("python3 -m VLLM.entrypoints"));
        assert!(is_known_high_memory_command("torchrun --nproc 8 train.py"));
        assert!(!is_known_high_memory_command("bash -lc make"));
    }

    #[test]
    fn warns_about_missing_guards_psi_and_heavy_processes() {
        let calm = DomainReadings {
            size_bytes: 1,
            free_bytes: 1,
            full_avg10: Pressure::from_hundredths(0),
        };
        let guard = [MachineGuard::Earlyoom];
        assert!(warnings(&calm, &guard, &[], false).is_empty());
        let no_guard = warnings(&calm, &[], &[], false);
        assert_eq!(no_guard.len(), 1);
        assert!(!no_guard[0].contains("container"));
        assert!(warnings(&calm, &[], &[], true)[0].contains("inside a container"));
        let no_psi = DomainReadings {
            full_avg10: None,
            ..calm
        };
        assert!(warnings(&no_psi, &guard, &[], false)[0].contains("PSI"));
        let heavy = [ProcessInfo {
            pid: 2,
            command: "vllm serve".to_string(),
            rss_bytes: None,
        }];
        assert!(warnings(&calm, &guard, &heavy, false)[0].contains("high-memory"));
    }

    #[test]
    fn names_the_guards() {
        assert_eq!(guard_names(&[]), "none");
        assert_eq!(
            guard_names(&[
                MachineGuard::Earlyoom,
                MachineGuard::SystemdOomd,
                MachineGuard::Declared("custom".to_string())
            ]),
            "earlyoom, systemd-oomd, custom"
        );
    }
}
