//! `oomwrap doctor` and `oomwrap inspect`.

use std::env;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::{default_shim_tools, engine_process_names};
use crate::host::{Host, ProcessInfo, disk_available, process_list};
use crate::limits::format_bytes;
use crate::readings::MemInfo;
use crate::shims::{current_oomwrap_bin, default_bin_dir};

/// The defaults that `inspect` reports, as the flags spell them.
pub(crate) struct Defaults {
    pub(crate) min_mem: &'static str,
    pub(crate) min_swap: &'static str,
    pub(crate) poll: &'static str,
    pub(crate) term_grace: &'static str,
}

#[derive(Debug, Serialize, Deserialize)]
struct DoctorReport {
    os: String,
    arch: String,
    mem: MemInfo,
    earlyoom_active: bool,
    home_disk_available_bytes: Option<u64>,
    known_high_memory_processes: Vec<ProcessInfo>,
    warnings: Vec<String>,
}

impl DoctorReport {
    // Report construction depends on live host process and earlyoom state.
    #[cfg_attr(test, mutants::skip)]
    fn collect(host: &Host) -> Result<Self> {
        let mem = host.read_meminfo()?;
        let earlyoom = host.earlyoom_active()?;
        let known_high_memory_processes = known_high_memory_processes()?;
        let mut warnings = Vec::new();
        if !earlyoom {
            warnings.push(
                "earlyoom is not active; high-risk profiles will refuse to launch by default"
                    .to_string(),
            );
        }
        if !known_high_memory_processes.is_empty() {
            warnings.push("known high-memory processes are already running".to_string());
        }

        Ok(Self {
            os: env::consts::OS.to_string(),
            arch: env::consts::ARCH.to_string(),
            mem,
            earlyoom_active: earlyoom,
            home_disk_available_bytes: host
                .home
                .as_deref()
                .and_then(|path| disk_available(path).ok()),
            known_high_memory_processes,
            warnings,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct InspectReport {
    default_min_mem: String,
    default_min_swap: String,
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
        "  memory: available={} swap_free={}",
        format_bytes(report.mem.mem_available_bytes),
        format_bytes(report.mem.swap_free_bytes)
    );
    println!(
        "  earlyoom: {}",
        if report.earlyoom_active {
            "active"
        } else {
            "not active"
        }
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

pub(crate) fn inspect(host: &Host, defaults: &Defaults, json: bool) -> Result<()> {
    let info = InspectReport {
        default_min_mem: defaults.min_mem.to_string(),
        default_min_swap: defaults.min_swap.to_string(),
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
            "  defaults: min_mem={} min_swap={} poll={} term_grace={}",
            info.default_min_mem, info.default_min_swap, info.default_poll, info.default_term_grace
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

    #[test]
    fn matches_engine_names_anywhere_in_the_command() {
        assert!(is_known_high_memory_command("/opt/bin/llama-server -m x"));
        assert!(is_known_high_memory_command("python3 -m VLLM.entrypoints"));
        assert!(is_known_high_memory_command("torchrun --nproc 8 train.py"));
        assert!(!is_known_high_memory_command("bash -lc make"));
    }
}
