//! The memory domain: the memory that the job competes for. Of the machine
//! and every cgroup with a `memory.max` on the caller's path, it is the one
//! with the smallest size. The free room is the smallest free room among all
//! of them, because the job can grow only until the first limit binds.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::Serialize;

use crate::host::{Host, read_file};
use crate::readings::{
    DomainReadings, check_domain, keyed_value, parse_bytes, parse_limit, parse_psi_full_avg10,
};

/// The memory that binds first.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Domain {
    Machine,
    /// A cgroup with a `memory.max`, by its path in the cgroup hierarchy.
    Cgroup {
        path: String,
    },
}

impl fmt::Display for Domain {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Machine => formatter.write_str("machine"),
            Self::Cgroup { path } => write!(formatter, "cgroup {path}"),
        }
    }
}

/// The domain and the other limits that can bind before it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MemoryDomain {
    pub(crate) domain: Domain,
    /// Every cgroup with a `memory.max` on the caller's path, closest first.
    limited: Vec<String>,
    cgroup_root: Option<PathBuf>,
}

impl MemoryDomain {
    pub(crate) fn find(host: &Host) -> Result<Self> {
        let machine_bytes = machine_size(host)?;
        let cgroup_root = host.cgroup_root()?;
        let mut limited = Vec::new();
        let mut smallest: Option<(u64, String)> = None;
        if let (Some(root), Some(own)) = (&cgroup_root, host.own_cgroup()?) {
            for path in self_and_ancestors(&own) {
                let Some(max) = read_cgroup_limit(root, &path, "memory.max")? else {
                    continue;
                };
                if smallest.as_ref().is_none_or(|(bytes, _)| max < *bytes) {
                    smallest = Some((max, path.clone()));
                }
                limited.push(path);
            }
        }
        let domain = match smallest {
            Some((bytes, path)) if bytes < machine_bytes => Domain::Cgroup { path },
            _ => Domain::Machine,
        };
        Ok(Self {
            domain,
            limited,
            cgroup_root,
        })
    }

    /// Reads the size and pressure of the domain, and the smallest free room
    /// of the machine and every limited cgroup on the path.
    pub(crate) fn read(&self, host: &Host) -> Result<DomainReadings> {
        let machine = read_machine(host)?;
        let mut readings = machine;
        for path in &self.limited {
            let Some(root) = &self.cgroup_root else {
                bail!("the cgroup hierarchy of {path} is gone");
            };
            let cgroup = read_cgroup(&cgroup_dir(root, path))?;
            readings.free_bytes = readings.free_bytes.min(cgroup.free_bytes);
            if self.domain == (Domain::Cgroup { path: path.clone() }) {
                readings.size_bytes = cgroup.size_bytes;
                readings.full_avg10 = cgroup.full_avg10;
            }
        }
        check_domain(&readings)?;
        Ok(readings)
    }
}

fn machine_size(host: &Host) -> Result<u64> {
    let mem = host.meminfo()?;
    Ok(mem.total_bytes.saturating_sub(mem.hugetlb_bytes))
}

/// The machine: `MemTotal` − `Hugetlb`, `MemAvailable`, and
/// `/proc/pressure/memory`.
fn read_machine(host: &Host) -> Result<DomainReadings> {
    let mem = host.meminfo()?;
    Ok(DomainReadings {
        size_bytes: mem.total_bytes.saturating_sub(mem.hugetlb_bytes),
        free_bytes: mem.available_bytes,
        full_avg10: host
            .read_proc("pressure/memory")
            .ok()
            .and_then(|text| parse_psi_full_avg10(&text).ok()),
    })
}

/// A limited cgroup: free room is `memory.max` − `memory.current` +
/// `inactive_file`, as an estimate of what reclaim can free.
fn read_cgroup(dir: &Path) -> Result<DomainReadings> {
    let Some(max) = parse_limit(&read_file(&dir.join("memory.max"))?)? else {
        bail!("{} lost its memory.max", dir.display());
    };
    let current = parse_bytes(&read_file(&dir.join("memory.current"))?)?;
    let stat = read_file(&dir.join("memory.stat"))?;
    let inactive_file = keyed_value(&stat, "inactive_file")?.unwrap_or(0);
    Ok(DomainReadings {
        size_bytes: max,
        free_bytes: max.saturating_sub(current).saturating_add(inactive_file),
        full_avg10: read_file(&dir.join("memory.pressure"))
            .ok()
            .and_then(|text| parse_psi_full_avg10(&text).ok()),
    })
}

/// The directory of a cgroup path such as `/user.slice/a.scope`.
pub(crate) fn cgroup_dir(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches('/'))
}

/// Reads a limit file of one cgroup; `None` when it says `max` or does not
/// exist, as at the root of the hierarchy.
pub(crate) fn read_cgroup_limit(root: &Path, path: &str, file: &str) -> Result<Option<u64>> {
    let file_path = cgroup_dir(root, path).join(file);
    match std::fs::read_to_string(&file_path) {
        Ok(text) => parse_limit(&text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(anyhow::Error::new(error)
                .context(format!("failed to read {}", file_path.display())))
        }
    }
}

/// `/a/b` gives `/a/b`, `/a` and `/`.
pub(crate) fn self_and_ancestors(path: &str) -> Vec<String> {
    // A bounded loop over the path's parts: no change to it can loop forever.
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    (0..=parts.len())
        .rev()
        .map(|depth| format!("/{}", parts[..depth].join("/")))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::host::tests::FakeHost;

    const GIB: u64 = 1024 * 1024 * 1024;
    const PSI_25: &str = "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=25.00 avg60=0.00 avg300=0.00 total=0\n";

    fn cgroup(path: &str) -> Domain {
        Domain::Cgroup {
            path: path.to_string(),
        }
    }

    /// A limited cgroup with its usage files.
    fn limit(fake: &FakeHost, path: &str, max: u64, current: u64, inactive_file: u64) {
        fake.cgroup_file(path, "memory.max", &max.to_string());
        fake.cgroup_file(path, "memory.current", &current.to_string());
        fake.cgroup_file(
            path,
            "memory.stat",
            &format!("anon 1\ninactive_file {inactive_file}\n"),
        );
    }

    #[test]
    fn lists_a_path_and_its_ancestors() {
        assert_eq!(self_and_ancestors("/a/b"), ["/a/b", "/a", "/"]);
        assert_eq!(self_and_ancestors("/"), ["/"]);
        assert_eq!(self_and_ancestors("/a/"), ["/a", "/"]);
        assert_eq!(self_and_ancestors("a//b"), ["/a/b", "/a", "/"]);
        assert_eq!(self_and_ancestors(""), ["/"]);
    }

    #[test]
    fn no_cgroup_information_means_the_machine() {
        let fake = FakeHost::new();
        fake.meminfo(16 * GIB, 6 * GIB, GIB);
        let host = fake.host_without_cgroups();
        let domain = MemoryDomain::find(&host).unwrap();
        assert_eq!(domain.domain, Domain::Machine);
        assert_eq!(domain.domain.to_string(), "machine");
        let readings = domain.read(&host).unwrap();
        assert_eq!(readings.size_bytes, 15 * GIB);
        assert_eq!(readings.free_bytes, 6 * GIB);
        assert_eq!(readings.full_avg10, None);
        fake.proc_file("pressure/memory", PSI_25);
        let readings = domain.read(&host).unwrap();
        assert_eq!(readings.full_avg10.map(|p| p.hundredths()), Some(2500));
    }

    #[test]
    fn an_unlimited_chain_means_the_machine() {
        let fake = FakeHost::new();
        fake.meminfo(16 * GIB, 6 * GIB, 0);
        fake.own_cgroup("/user.slice/user-1.slice/session-1.scope");
        fake.cgroup_file("/user.slice", "memory.max", "max\n");
        fake.cgroup_file(
            "/user.slice/user-1.slice/session-1.scope",
            "memory.max",
            "max\n",
        );
        let domain = MemoryDomain::find(&fake.host()).unwrap();
        assert_eq!(domain.domain, Domain::Machine);
    }

    #[test]
    fn a_limited_service_is_the_domain() {
        let fake = FakeHost::new();
        fake.meminfo(16 * GIB, 10 * GIB, 0);
        fake.own_cgroup("/system.slice/job.service/sub");
        limit(&fake, "/system.slice", 8 * GIB, 3 * GIB, 0);
        limit(&fake, "/system.slice/job.service", 2 * GIB, GIB, GIB / 4);
        fake.cgroup_file("/system.slice/job.service", "memory.pressure", PSI_25);
        fake.cgroup_file("/system.slice/job.service/sub", "memory.max", "max\n");
        let host = fake.host();
        let domain = MemoryDomain::find(&host).unwrap();
        assert_eq!(domain.domain, cgroup("/system.slice/job.service"));
        assert_eq!(
            domain.domain.to_string(),
            "cgroup /system.slice/job.service"
        );
        let readings = domain.read(&host).unwrap();
        assert_eq!(readings.size_bytes, 2 * GIB);
        assert_eq!(readings.free_bytes, GIB + GIB / 4);
        assert_eq!(readings.full_avg10.map(|p| p.hundredths()), Some(2500));
    }

    #[test]
    fn a_smaller_outer_limit_binds_before_a_closer_one() {
        let fake = FakeHost::new();
        fake.meminfo(16 * GIB, 10 * GIB, 0);
        fake.own_cgroup("/outer/inner");
        limit(&fake, "/outer/inner", 8 * GIB, GIB, 0);
        limit(&fake, "/outer", 2 * GIB, GIB + GIB / 2, 0);
        let host = fake.host();
        let domain = MemoryDomain::find(&host).unwrap();
        assert_eq!(domain.domain, cgroup("/outer"));
        let readings = domain.read(&host).unwrap();
        assert_eq!(readings.size_bytes, 2 * GIB);
        assert_eq!(readings.free_bytes, GIB / 2);
    }

    #[test]
    fn a_larger_outer_limit_with_less_room_bounds_the_free_room() {
        // The siblings of the job's cgroup use most of the outer limit.
        let fake = FakeHost::new();
        fake.meminfo(64 * GIB, 40 * GIB, 0);
        fake.own_cgroup("/outer/inner");
        limit(&fake, "/outer/inner", 4 * GIB, GIB, 0);
        limit(&fake, "/outer", 8 * GIB, 7 * GIB + GIB / 2, 0);
        let host = fake.host();
        let domain = MemoryDomain::find(&host).unwrap();
        assert_eq!(domain.domain, cgroup("/outer/inner"));
        let readings = domain.read(&host).unwrap();
        assert_eq!(readings.size_bytes, 4 * GIB);
        assert_eq!(readings.free_bytes, GIB / 2);
    }

    #[test]
    fn a_limit_above_the_machine_leaves_the_machine_as_the_domain() {
        let fake = FakeHost::new();
        fake.meminfo(16 * GIB, 2 * GIB, 0);
        fake.own_cgroup("/big.slice");
        limit(&fake, "/big.slice", 64 * GIB, GIB, 0);
        let host = fake.host();
        let domain = MemoryDomain::find(&host).unwrap();
        assert_eq!(domain.domain, Domain::Machine);
        let readings = domain.read(&host).unwrap();
        assert_eq!(readings.size_bytes, 16 * GIB);
        assert_eq!(readings.free_bytes, 2 * GIB);
    }

    #[test]
    fn of_two_equal_limits_the_closest_is_the_domain() {
        let fake = FakeHost::new();
        fake.meminfo(16 * GIB, 10 * GIB, 0);
        fake.own_cgroup("/outer/inner");
        limit(&fake, "/outer/inner", 4 * GIB, GIB, 0);
        limit(&fake, "/outer", 4 * GIB, GIB, 0);
        let domain = MemoryDomain::find(&fake.host()).unwrap();
        assert_eq!(domain.domain, cgroup("/outer/inner"));
    }

    #[test]
    fn a_limit_equal_to_the_machine_leaves_the_machine_as_the_domain() {
        let fake = FakeHost::new();
        fake.meminfo(16 * GIB, 10 * GIB, 0);
        fake.own_cgroup("/same.slice");
        limit(&fake, "/same.slice", 16 * GIB, GIB, 0);
        let domain = MemoryDomain::find(&fake.host()).unwrap();
        assert_eq!(domain.domain, Domain::Machine);
    }

    #[test]
    fn a_container_root_with_a_limit_is_the_domain() {
        let fake = FakeHost::new();
        fake.meminfo(16 * GIB, 8 * GIB, 0);
        fake.own_cgroup("/");
        limit(&fake, "/", GIB, 2 * GIB, 0);
        let host = fake.host();
        let domain = MemoryDomain::find(&host).unwrap();
        assert_eq!(domain.domain, cgroup("/"));
        let readings = domain.read(&host).unwrap();
        assert_eq!(readings.free_bytes, 0);
        assert_eq!(readings.full_avg10, None);
    }

    #[test]
    fn an_outer_oomwrap_scope_is_the_domain_of_a_nested_job() {
        let fake = FakeHost::new();
        fake.meminfo(16 * GIB, 8 * GIB, 0);
        let scope = "/user.slice/user-1.slice/user@1.service/app.slice/oomwrap-build-1.scope";
        fake.own_cgroup(scope);
        limit(&fake, scope, 4 * GIB, GIB, 0);
        let domain = MemoryDomain::find(&fake.host()).unwrap();
        assert_eq!(domain.domain, cgroup(scope));
    }

    #[test]
    fn a_cgroup_that_loses_its_limit_is_an_error() {
        let fake = FakeHost::new();
        fake.meminfo(16 * GIB, 8 * GIB, 0);
        fake.own_cgroup("/a");
        limit(&fake, "/a", GIB, 0, 0);
        let host = fake.host();
        let domain = MemoryDomain::find(&host).unwrap();
        fake.cgroup_file("/a", "memory.max", "max\n");
        assert!(domain.read(&host).is_err());
        let gone = MemoryDomain {
            cgroup_root: None,
            ..domain
        };
        assert!(gone.read(&host).is_err());
    }

    #[test]
    fn unreadable_limits_are_errors_and_missing_ones_are_none() {
        let fake = FakeHost::new();
        let root = fake.cgroup_root();
        assert_eq!(
            read_cgroup_limit(&root, "/none", "memory.max").unwrap(),
            None
        );
        fs::create_dir_all(root.join("dir/memory.max")).unwrap();
        assert!(read_cgroup_limit(&root, "/dir", "memory.max").is_err());
    }
}
