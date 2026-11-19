//! Parsers of kernel text files. They read no files themselves.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

/// The two `/proc/meminfo` values that the guard reads, in bytes.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) struct MemInfo {
    pub(crate) mem_available_bytes: u64,
    pub(crate) swap_free_bytes: u64,
}

pub(crate) fn parse_meminfo(content: &str) -> Result<MemInfo> {
    let mut mem_available_bytes = None;
    let mut swap_free_bytes = None;

    for line in content.lines() {
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else {
            continue;
        };
        let Some(value) = parts.next() else {
            continue;
        };
        let kb: u64 = value
            .parse()
            .with_context(|| format!("invalid meminfo value in {line:?}"))?;
        match key.trim_end_matches(':') {
            "MemAvailable" => mem_available_bytes = Some(kb * 1024),
            "SwapFree" => swap_free_bytes = Some(kb * 1024),
            _ => {}
        }
    }

    Ok(MemInfo {
        mem_available_bytes: mem_available_bytes
            .ok_or_else(|| anyhow!("MemAvailable missing from meminfo"))?,
        swap_free_bytes: swap_free_bytes.ok_or_else(|| anyhow!("SwapFree missing from meminfo"))?,
    })
}

/// Reads the process group ID, the fifth field of `/proc/<pid>/stat`.
pub(crate) fn parse_process_group_id(stat: &str) -> Result<i32> {
    let comm_end = stat
        .rfind(')')
        .ok_or_else(|| anyhow!("missing process command in stat"))?;
    let mut fields = stat[comm_end + 1..].split_whitespace();
    let _state = fields
        .next()
        .ok_or_else(|| anyhow!("missing process state in stat"))?;
    let _ppid = fields
        .next()
        .ok_or_else(|| anyhow!("missing parent pid in stat"))?;
    fields
        .next()
        .ok_or_else(|| anyhow!("missing process group id in stat"))?
        .parse()
        .context("invalid process group id in stat")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_meminfo() {
        let mem = parse_meminfo(
            r#"
MemTotal:       1000000 kB
MemAvailable:   123456 kB
SwapFree:          789 kB
"#,
        )
        .unwrap();
        assert_eq!(mem.mem_available_bytes, 123456 * 1024);
        assert_eq!(mem.swap_free_bytes, 789 * 1024);
    }
}
