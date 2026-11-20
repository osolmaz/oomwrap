//! Parsers of kernel text files and the reading types built from them.
//! They read no files themselves.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::limits::Pressure;

const KIB: u64 = 1024;

/// The `/proc/meminfo` values that the machine domain uses, in bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MemInfo {
    pub(crate) total_bytes: u64,
    pub(crate) available_bytes: u64,
    /// Memory reserved for huge pages, which a job cannot use.
    pub(crate) hugetlb_bytes: u64,
}

pub(crate) fn parse_meminfo(content: &str) -> Result<MemInfo> {
    let mut total_bytes = None;
    let mut available_bytes = None;
    let mut hugetlb_bytes = 0;

    for line in content.lines() {
        let mut parts = line.split_whitespace();
        let (Some(key), Some(value)) = (parts.next(), parts.next()) else {
            continue;
        };
        let read_kib = || -> Result<u64> {
            let kib: u64 = value
                .parse()
                .with_context(|| format!("invalid meminfo value in {line:?}"))?;
            kib.checked_mul(KIB)
                .ok_or_else(|| anyhow!("meminfo value too large in {line:?}"))
        };
        match key {
            "MemTotal:" => total_bytes = Some(read_kib()?),
            "MemAvailable:" => available_bytes = Some(read_kib()?),
            "Hugetlb:" => hugetlb_bytes = read_kib()?,
            _ => {}
        }
    }

    Ok(MemInfo {
        total_bytes: total_bytes.ok_or_else(|| anyhow!("MemTotal missing from meminfo"))?,
        available_bytes: available_bytes
            .ok_or_else(|| anyhow!("MemAvailable missing from meminfo"))?,
        hugetlb_bytes,
    })
}

/// Reads the `full avg10` value of a PSI file such as `/proc/pressure/memory`.
pub(crate) fn parse_psi_full_avg10(content: &str) -> Result<Pressure> {
    let line = content
        .lines()
        .find(|line| line.starts_with("full "))
        .ok_or_else(|| anyhow!("no full line in pressure file"))?;
    let value = line
        .split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))
        .ok_or_else(|| anyhow!("no avg10 in {line:?}"))?;
    Pressure::parse_decimal(value)
}

/// The counters of a cgroup's `memory.events`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct MemoryEvents {
    /// Times the cgroup reached `memory.max` and an allocation was about to fail.
    pub(crate) oom: u64,
    /// Processes of the cgroup that an out-of-memory killer killed.
    pub(crate) oom_kill: u64,
}

/// The size of a ZFS ARC from `/proc/spl/kstat/zfs/arcstats`: the third
/// column of its `size` line.
pub(crate) fn parse_arc_size(content: &str) -> Option<u64> {
    content.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        (parts.next() == Some("size"))
            .then(|| parts.nth(1)?.parse().ok())
            .flatten()
    })
}

/// Reads a cgroup's `memory.events`; a counter that is missing counts as 0.
pub(crate) fn parse_memory_events(content: &str) -> Result<MemoryEvents> {
    Ok(MemoryEvents {
        oom: keyed_value(content, "oom")?.unwrap_or(0),
        oom_kill: keyed_value(content, "oom_kill")?.unwrap_or(0),
    })
}

/// Reads `key value` lines, as `memory.events` and `memory.stat` hold them.
pub(crate) fn keyed_value(content: &str, key: &str) -> Result<Option<u64>> {
    for line in content.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() == Some(key) {
            let value = parts
                .next()
                .ok_or_else(|| anyhow!("missing value for {key}"))?;
            return value
                .parse()
                .map(Some)
                .with_context(|| format!("invalid value for {key}: {value:?}"));
        }
    }
    Ok(None)
}

/// A cgroup limit file such as `memory.max`: `max` or a byte count.
pub(crate) fn parse_limit(content: &str) -> Result<Option<u64>> {
    let text = content.trim();
    if text == "max" {
        return Ok(None);
    }
    text.parse()
        .map(Some)
        .with_context(|| format!("invalid cgroup limit: {text:?}"))
}

/// A single byte count, as `memory.current` holds it.
pub(crate) fn parse_bytes(content: &str) -> Result<u64> {
    let text = content.trim();
    text.parse()
        .with_context(|| format!("invalid byte count: {text:?}"))
}

/// The cgroup v2 path of a process from `/proc/<pid>/cgroup`, such as
/// `/user.slice/session-2.scope`; `None` when the process has no v2 entry.
pub(crate) fn parse_cgroup_v2_path(content: &str) -> Option<String> {
    content
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::to_owned)
}

/// What `/proc/self/mountinfo` says about cgroup file systems.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct CgroupMounts {
    /// Where the cgroup v2 hierarchy is mounted.
    pub(crate) v2_mount: Option<PathBuf>,
    /// Whether any cgroup v1 hierarchy is mounted: a hybrid host.
    pub(crate) has_v1: bool,
}

pub(crate) fn parse_mountinfo(content: &str) -> Result<CgroupMounts> {
    let mut mounts = CgroupMounts::default();
    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        let (before, after) = line
            .split_once(" - ")
            .ok_or_else(|| anyhow!("invalid mountinfo line: {line:?}"))?;
        let mount_point = before
            .split_whitespace()
            .nth(4)
            .ok_or_else(|| anyhow!("missing mount point in {line:?}"))?;
        match after.split_whitespace().next() {
            Some("cgroup2") if mounts.v2_mount.is_none() => {
                mounts.v2_mount = Some(PathBuf::from(unescape_mount_path(mount_point)));
            }
            Some("cgroup") => mounts.has_v1 = true,
            _ => {}
        }
    }
    Ok(mounts)
}

/// mountinfo writes a space, tab, newline or backslash in a path as `\` and
/// three octal digits.
fn unescape_mount_path(text: &str) -> String {
    // Each piece after a backslash is read once, so the loop always ends.
    let mut pieces = text.split('\\');
    let mut out = pieces.next().unwrap_or_default().to_string();
    for piece in pieces {
        match octal_byte(piece) {
            Some(byte) => {
                out.push(char::from(byte));
                out.push_str(&piece[3..]);
            }
            None => {
                out.push('\\');
                out.push_str(piece);
            }
        }
    }
    out
}

/// The byte of three octal digits at the start of `text`.
fn octal_byte(text: &str) -> Option<u8> {
    let digits = text.get(..3)?;
    if !digits.bytes().all(|byte| (b'0'..=b'7').contains(&byte)) {
        return None;
    }
    u8::from_str_radix(digits, 8).ok()
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

/// The memory domain at one moment.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct DomainReadings {
    pub(crate) size_bytes: u64,
    pub(crate) free_bytes: u64,
    /// `None` when the kernel has no PSI.
    pub(crate) full_avg10: Option<Pressure>,
}

/// The job's own cgroup at one moment; only the `cgroup` backend has it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct JobReadings {
    pub(crate) current_bytes: u64,
    pub(crate) swap_current_bytes: u64,
    pub(crate) full_avg10: Option<Pressure>,
    pub(crate) events: MemoryEvents,
}

/// One reading of everything the policy decides on.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Readings {
    /// Milliseconds since the launch.
    pub(crate) elapsed_ms: u64,
    pub(crate) domain: DomainReadings,
    pub(crate) job: Option<JobReadings>,
}

/// Checks that a reading is internally consistent.
pub(crate) fn check_domain(readings: &DomainReadings) -> Result<()> {
    if readings.size_bytes == 0 {
        bail!("the memory domain has a size of zero");
    }
    Ok(())
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
Hugetlb:          2048 kB
"#,
        )
        .unwrap();
        assert_eq!(mem.total_bytes, 1_000_000 * 1024);
        assert_eq!(mem.available_bytes, 123_456 * 1024);
        assert_eq!(mem.hugetlb_bytes, 2048 * 1024);
        let no_hugetlb = parse_meminfo("MemTotal: 2 kB\nMemAvailable: 1 kB\n").unwrap();
        assert_eq!(no_hugetlb.hugetlb_bytes, 0);
        assert!(parse_meminfo("MemTotal: 2 kB\n").is_err());
        assert!(parse_meminfo("MemAvailable: 2 kB\n").is_err());
        assert!(parse_meminfo("MemTotal: x kB\nMemAvailable: 1 kB\n").is_err());
        assert!(parse_meminfo("MemTotal: 99999999999999999 kB\nMemAvailable: 1 kB\n").is_err());
    }

    #[test]
    fn reads_the_size_of_a_zfs_arc() {
        let stats = "13 1 0x01 123 33456 1 2\nname type data\nhits 4 100\nsize 4 8589934592\n";
        assert_eq!(parse_arc_size(stats), Some(8_589_934_592));
        assert_eq!(parse_arc_size("size 4 x\n"), None);
        assert_eq!(parse_arc_size("size\n"), None);
        assert_eq!(parse_arc_size("c_max 4 1\n"), None);
    }

    #[test]
    fn parses_memory_events() {
        let events =
            parse_memory_events("low 0\nhigh 12\nmax 3\noom 2\noom_kill 1\noom_group_kill 0\n")
                .unwrap();
        assert_eq!(
            events,
            MemoryEvents {
                oom: 2,
                oom_kill: 1
            }
        );
        assert_eq!(parse_memory_events("").unwrap(), MemoryEvents::default());
        assert!(parse_memory_events("oom x\n").is_err());
        assert!(parse_memory_events("oom_kill\n").is_err());
    }

    #[test]
    fn parses_psi() {
        let text = "some avg10=1.50 avg60=0.00 avg300=0.00 total=1\nfull avg10=12.34 avg60=1.00 avg300=0.00 total=2\n";
        assert_eq!(parse_psi_full_avg10(text).unwrap().hundredths(), 1234);
        assert!(parse_psi_full_avg10("some avg10=1.00 total=1\n").is_err());
        assert!(parse_psi_full_avg10("full avg60=1.00 total=1\n").is_err());
        assert!(parse_psi_full_avg10("full avg10=x total=1\n").is_err());
    }

    #[test]
    fn parses_cgroup_counters_and_limits() {
        let events = "low 0\nhigh 7\nmax 3\noom 2\noom_kill 1\noom_group_kill 0\n";
        assert_eq!(keyed_value(events, "oom").unwrap(), Some(2));
        assert_eq!(keyed_value(events, "oom_kill").unwrap(), Some(1));
        assert_eq!(keyed_value("", "oom").unwrap(), None);
        assert!(keyed_value("oom x\n", "oom").is_err());
        assert!(keyed_value("oom\n", "oom").is_err());
        assert_eq!(
            keyed_value("anon 1\ninactive_file 42\n", "inactive_file").unwrap(),
            Some(42)
        );
        assert_eq!(parse_limit("max\n").unwrap(), None);
        assert_eq!(parse_limit("4096\n").unwrap(), Some(4096));
        assert!(parse_limit("lots").is_err());
        assert_eq!(parse_bytes("123\n").unwrap(), 123);
        assert!(parse_bytes("").is_err());
    }

    #[test]
    fn parses_cgroup_paths() {
        assert_eq!(
            parse_cgroup_v2_path("0::/user.slice/a.scope\n").as_deref(),
            Some("/user.slice/a.scope")
        );
        assert_eq!(
            parse_cgroup_v2_path("12:memory:/x\n0::/\n").as_deref(),
            Some("/")
        );
        assert_eq!(parse_cgroup_v2_path("12:memory:/x\n"), None);
    }

    #[test]
    fn parses_mountinfo() {
        let pure = "30 1 0:26 / /sys/fs/cgroup rw,nosuid - cgroup2 cgroup2 rw,nsdelegate\n";
        assert_eq!(
            parse_mountinfo(pure).unwrap(),
            CgroupMounts {
                v2_mount: Some(PathBuf::from("/sys/fs/cgroup")),
                has_v1: false,
            }
        );
        let hybrid = "30 1 0:26 / /sys/fs/cgroup/unified rw - cgroup2 cgroup2 rw\n\
                      31 1 0:27 / /sys/fs/cgroup/memory rw - cgroup cgroup rw,memory\n";
        let mounts = parse_mountinfo(hybrid).unwrap();
        assert!(mounts.has_v1);
        assert_eq!(
            mounts.v2_mount,
            Some(PathBuf::from("/sys/fs/cgroup/unified"))
        );
        let escaped = "30 1 0:26 / /mnt/a\\040b rw - cgroup2 cgroup2 rw\n";
        assert_eq!(
            parse_mountinfo(escaped).unwrap().v2_mount,
            Some(PathBuf::from("/mnt/a b"))
        );
        assert!(parse_mountinfo("broken line\n").is_err());
        assert!(parse_mountinfo("1 2 3 - cgroup2\n").is_err());
        assert_eq!(unescape_mount_path("a\\b"), "a\\b");
        assert_eq!(unescape_mount_path("a\\+12b"), "a\\+12b");
        assert_eq!(unescape_mount_path("a\\777"), "a\\777");
        assert_eq!(unescape_mount_path("\\134\\011"), "\\\t");
        assert_eq!(unescape_mount_path("a\\0"), "a\\0");
    }

    #[test]
    fn parses_process_group_ids() {
        assert_eq!(
            parse_process_group_id("123 (name with spaces) S 1 789 789 0").unwrap(),
            789
        );
        assert!(parse_process_group_id("123 name S 1 789").is_err());
        assert!(parse_process_group_id("123 (x)").is_err());
        assert!(parse_process_group_id("123 (x) S").is_err());
        assert!(parse_process_group_id("123 (x) S 1").is_err());
        assert!(parse_process_group_id("123 (x) S 1 y").is_err());
    }

    #[test]
    fn domain_readings_need_a_size() {
        let mut readings = DomainReadings {
            size_bytes: 0,
            free_bytes: 0,
            full_avg10: None,
        };
        assert!(check_domain(&readings).is_err());
        readings.size_bytes = 1;
        assert!(check_domain(&readings).is_ok());
    }
}
