//! The machine that oomwrap runs on. Environment variables are read once, in
//! [`Host::from_env`], and every other module asks this value.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use nix::sys::statvfs::statvfs;
use serde::{Deserialize, Serialize};

use crate::readings::{MemInfo, parse_meminfo};

/// Test seam: a file read in place of `/proc/meminfo`.
const MEMINFO_PATH_VAR: &str = "OOMWRAP_MEMINFO_PATH";
/// Test seam: claims that `earlyoom` is or is not active.
const EARLYOOM_ACTIVE_VAR: &str = "OOMWRAP_EARLYOOM_ACTIVE";

pub(crate) struct Host {
    pub(crate) meminfo_path: PathBuf,
    /// `Some` when the environment overrides the `earlyoom` process check.
    pub(crate) earlyoom_override: Option<bool>,
    pub(crate) home: Option<PathBuf>,
}

impl Host {
    // Thin environment adapter; tests build a `Host` directly.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) fn from_env() -> Self {
        Self {
            meminfo_path: env::var_os(MEMINFO_PATH_VAR)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/proc/meminfo")),
            earlyoom_override: env::var_os(EARLYOOM_ACTIVE_VAR).map(|value| {
                matches!(
                    value.to_string_lossy().as_ref(),
                    "1" | "true" | "yes" | "on"
                )
            }),
            home: env::var_os("HOME").map(PathBuf::from),
        }
    }

    pub(crate) fn read_meminfo(&self) -> Result<MemInfo> {
        let path = &self.meminfo_path;
        parse_meminfo(
            &fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))?,
        )
    }

    pub(crate) fn earlyoom_active(&self) -> Result<bool> {
        if let Some(active) = self.earlyoom_override {
            return Ok(active);
        }
        Ok(runs_earlyoom(&process_list()?, std::process::id()))
    }

    pub(crate) fn home(&self) -> Result<&Path> {
        self.home
            .as_deref()
            .ok_or_else(|| anyhow!("HOME is not set"))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ProcessInfo {
    pub(crate) pid: u32,
    pub(crate) command: String,
    pub(crate) rss_bytes: Option<u64>,
}

/// Tells whether another process than `own_pid` runs the `earlyoom` program.
fn runs_earlyoom(processes: &[ProcessInfo], own_pid: u32) -> bool {
    processes.iter().any(|process| {
        process.pid != own_pid && command_invokes_executable(&process.command, "earlyoom")
    })
}

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
mod tests {
    use super::*;

    fn process(pid: u32, command: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            command: command.to_string(),
            rss_bytes: None,
        }
    }

    #[test]
    fn finds_earlyoom_in_other_processes_only() {
        let processes = [process(10, "bash"), process(11, "/usr/bin/earlyoom -m 5")];
        assert!(runs_earlyoom(&processes, 10));
        assert!(!runs_earlyoom(&processes, 11));
        assert!(!runs_earlyoom(&processes[..1], 99));
    }

    #[test]
    fn reads_meminfo_from_the_host_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meminfo");
        std::fs::write(&path, "MemAvailable: 2 kB\nSwapFree: 1 kB\n").unwrap();
        let host = Host {
            meminfo_path: path,
            earlyoom_override: Some(false),
            home: None,
        };
        assert_eq!(host.read_meminfo().unwrap().mem_available_bytes, 2048);
        assert!(!host.earlyoom_active().unwrap());
        assert!(host.home().is_err());
    }
}
