//! The kernel limits of a job in its own cgroup: `memory.max`, `memory.high`
//! and `memory.swap.max`, their defaults, and the rounding to whole pages
//! that the kernel applies. Pure: no IO.

use std::fmt;

use anyhow::{Result, bail};
use serde::{Serialize, Serializer};

use crate::limits::{Size, format_bytes};

/// The default `--memory-max`: this share of the domain, before the caps.
const DEFAULT_MAX_PERCENT: u64 = 85;
/// The default `--memory-high`: this share of the resolved `--memory-max`.
const DEFAULT_HIGH_PERCENT: u64 = 90;
const PERCENT_SCALE: u64 = 100;

/// A limit as the kernel holds it: whole pages, or no limit (`max`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KernelLimit {
    Bytes(u64),
    Max,
}

impl KernelLimit {
    /// The value of a systemd property such as `MemoryMax=`.
    pub(crate) fn property_value(self) -> String {
        match self {
            Self::Bytes(bytes) => bytes.to_string(),
            Self::Max => "infinity".to_string(),
        }
    }

    /// Reads a cgroup limit file back in whole pages: `max` or a byte count.
    pub(crate) fn from_file(value: Option<u64>, page_size: u64) -> Self {
        match value {
            Some(bytes) => Self::Bytes(round_down_to_pages(bytes, page_size)),
            None => Self::Max,
        }
    }

    fn bytes(self) -> Option<u64> {
        match self {
            Self::Bytes(bytes) => Some(bytes),
            Self::Max => None,
        }
    }
}

impl fmt::Display for KernelLimit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bytes(bytes) => formatter.write_str(&format_bytes(*bytes)),
            Self::Max => formatter.write_str("max"),
        }
    }
}

impl Serialize for KernelLimit {
    /// Serializes as bytes, or as `"max"` for no limit.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Bytes(bytes) => serializer.serialize_u64(*bytes),
            Self::Max => serializer.serialize_str("max"),
        }
    }
}

/// Rounds a size down to a whole number of pages, as the kernel stores it.
pub(crate) fn round_down_to_pages(bytes: u64, page_size: u64) -> u64 {
    if page_size == 0 {
        return bytes;
    }
    bytes - bytes % page_size
}

/// The limits that flags or the policy file gave; each one that is set is
/// a requirement that only the `cgroup` backend can meet.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct JobLimitSettings {
    pub(crate) memory_max: Option<Size>,
    pub(crate) memory_high: Option<Size>,
    pub(crate) memory_swap_max: Option<Size>,
}

/// What the default `--memory-max` is capped by at launch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LaunchRoom {
    pub(crate) domain_bytes: u64,
    pub(crate) free_bytes: u64,
    /// The resolved `--min-mem`.
    pub(crate) min_free_bytes: u64,
    /// The lowest `memory.max` of the cgroups above the scope.
    pub(crate) above_scope_max: Option<u64>,
    pub(crate) page_size: u64,
}

/// The limits that the scope gets, in whole pages.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct JobLimits {
    pub(crate) memory_max: KernelLimit,
    pub(crate) memory_high: KernelLimit,
    /// Not set by default: the kernel's normal swap behavior applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) memory_swap_max: Option<KernelLimit>,
}

/// The outcome of resolving the limits at launch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Resolution {
    Limits(JobLimits),
    /// The default `--memory-max` leaves the job no room: the free room at
    /// launch is at or below the floor.
    NoRoom,
}

impl JobLimitSettings {
    /// Tells whether a flag or the policy file set any job limit.
    pub(crate) fn is_explicit(&self) -> bool {
        self.memory_max.is_some() || self.memory_high.is_some() || self.memory_swap_max.is_some()
    }

    /// Resolves the limits against the domain. When only one of
    /// `--memory-max` and `--memory-high` is given, the other follows by
    /// the default ratio. Only a default `--memory-max` is capped.
    pub(crate) fn resolve(&self, room: &LaunchRoom) -> Result<Resolution> {
        let given = |size: Option<Size>| size.map(|size| size.resolve(room.domain_bytes));
        let (max, high) = match (given(self.memory_max), given(self.memory_high)) {
            (Some(max), Some(high)) => (max, high),
            (Some(max), None) => (max, max.map(|max| share(max, DEFAULT_HIGH_PERCENT))),
            (None, Some(high)) => (high.map(max_for_high), high),
            (None, None) => {
                let max = default_max(room);
                if max == 0 {
                    return Ok(Resolution::NoRoom);
                }
                (Some(max), Some(share(max, DEFAULT_HIGH_PERCENT)))
            }
        };
        let limits = JobLimits {
            memory_max: kernel_limit(max, room.page_size),
            memory_high: kernel_limit(high, room.page_size),
            memory_swap_max: given(self.memory_swap_max)
                .map(|swap| kernel_limit(swap, room.page_size)),
        };
        check_order(&limits)?;
        Ok(Resolution::Limits(limits))
    }
}

/// The smallest of `85%` of the domain, the free room at launch minus the
/// floor, and the lowest `memory.max` above the scope.
fn default_max(room: &LaunchRoom) -> u64 {
    let mut max = share(room.domain_bytes, DEFAULT_MAX_PERCENT)
        .min(room.free_bytes.saturating_sub(room.min_free_bytes));
    if let Some(above) = room.above_scope_max {
        max = max.min(above);
    }
    round_down_to_pages(max, room.page_size)
}

fn share(bytes: u64, percent: u64) -> u64 {
    let value = u128::from(bytes) * u128::from(percent) / u128::from(PERCENT_SCALE);
    // A share of at most 100% never exceeds the u64 it came from.
    u64::try_from(value).unwrap_or(bytes)
}

/// The `--memory-max` that a `--memory-high` alone implies.
fn max_for_high(high: u64) -> u64 {
    let value = u128::from(high) * u128::from(PERCENT_SCALE) / u128::from(DEFAULT_HIGH_PERCENT);
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn kernel_limit(bytes: Option<u64>, page_size: u64) -> KernelLimit {
    match bytes {
        Some(bytes) => KernelLimit::Bytes(round_down_to_pages(bytes, page_size)),
        None => KernelLimit::Max,
    }
}

fn check_order(limits: &JobLimits) -> Result<()> {
    if let (Some(high), Some(max)) = (limits.memory_high.bytes(), limits.memory_max.bytes())
        && high > max
    {
        bail!(
            "--memory-high {} is above --memory-max {}",
            format_bytes(high),
            format_bytes(max)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;
    const PAGE: u64 = 4096;

    fn room(free_bytes: u64, above_scope_max: Option<u64>) -> LaunchRoom {
        LaunchRoom {
            domain_bytes: 16 * GIB,
            free_bytes,
            min_free_bytes: 2 * GIB,
            above_scope_max,
            page_size: PAGE,
        }
    }

    fn settings(max: Option<&str>, high: Option<&str>, swap: Option<&str>) -> JobLimitSettings {
        let parse = |text: Option<&str>| text.map(|text| text.parse::<Size>().unwrap());
        JobLimitSettings {
            memory_max: parse(max),
            memory_high: parse(high),
            memory_swap_max: parse(swap),
        }
    }

    fn limits(settings: JobLimitSettings, room: &LaunchRoom) -> JobLimits {
        match settings.resolve(room).unwrap() {
            Resolution::Limits(limits) => limits,
            Resolution::NoRoom => panic!("expected limits"),
        }
    }

    #[test]
    fn sizes_round_down_to_whole_pages() {
        assert_eq!(round_down_to_pages(100_000_001, PAGE), 99_999_744);
        assert_eq!(round_down_to_pages(8192, PAGE), 8192);
        assert_eq!(round_down_to_pages(4095, PAGE), 0);
        assert_eq!(round_down_to_pages(70_000, 65_536), 65_536);
        assert_eq!(round_down_to_pages(7, 0), 7);
        assert_eq!(
            KernelLimit::from_file(Some(100_000_001), PAGE),
            KernelLimit::Bytes(99_999_744)
        );
        assert_eq!(KernelLimit::from_file(None, PAGE), KernelLimit::Max);
    }

    #[test]
    fn the_default_max_is_the_smallest_cap() {
        let defaults = JobLimitSettings::default();
        assert!(!defaults.is_explicit());
        // 85% of 16G is 13.6G; 12G free minus the 2G floor is 10G.
        let free_caps = limits(defaults, &room(12 * GIB, None));
        assert_eq!(free_caps.memory_max, KernelLimit::Bytes(10 * GIB));
        assert_eq!(free_caps.memory_high, KernelLimit::Bytes(9 * GIB));
        assert_eq!(free_caps.memory_swap_max, None);
        let share_caps = limits(defaults, &room(16 * GIB, None));
        assert_eq!(
            share_caps.memory_max,
            KernelLimit::Bytes(round_down_to_pages(16 * GIB * 85 / 100, PAGE))
        );
        let above_caps = limits(defaults, &room(12 * GIB, Some(3 * GIB + 5)));
        assert_eq!(above_caps.memory_max, KernelLimit::Bytes(3 * GIB));
    }

    #[test]
    fn no_room_at_launch_refuses() {
        let defaults = JobLimitSettings::default();
        assert_eq!(
            defaults.resolve(&room(2 * GIB, None)).unwrap(),
            Resolution::NoRoom
        );
        assert_eq!(
            defaults.resolve(&room(GIB, None)).unwrap(),
            Resolution::NoRoom
        );
        assert_eq!(
            defaults.resolve(&room(2 * GIB + PAGE - 1, None)).unwrap(),
            Resolution::NoRoom
        );
        assert!(matches!(
            defaults.resolve(&room(2 * GIB + PAGE, None)).unwrap(),
            Resolution::Limits(_)
        ));
    }

    #[test]
    fn an_explicit_max_is_used_as_given() {
        let given = limits(settings(Some("20G"), None, None), &room(4 * GIB, None));
        assert!(settings(Some("20G"), None, None).is_explicit());
        assert_eq!(given.memory_max, KernelLimit::Bytes(20 * GIB));
        assert_eq!(given.memory_high, KernelLimit::Bytes(18 * GIB));
        let share = limits(settings(Some("50%"), None, None), &room(4 * GIB, None));
        assert_eq!(share.memory_max, KernelLimit::Bytes(8 * GIB));
        let unlimited = limits(settings(Some("infinity"), None, None), &room(4 * GIB, None));
        assert_eq!(unlimited.memory_max, KernelLimit::Max);
        assert_eq!(unlimited.memory_high, KernelLimit::Max);
    }

    #[test]
    fn a_high_alone_implies_the_max() {
        let high = limits(settings(None, Some("9G"), None), &room(4 * GIB, None));
        assert!(settings(None, Some("9G"), None).is_explicit());
        assert_eq!(high.memory_high, KernelLimit::Bytes(9 * GIB));
        assert_eq!(high.memory_max, KernelLimit::Bytes(10 * GIB));
        let unlimited = limits(settings(None, Some("infinity"), None), &room(4 * GIB, None));
        assert_eq!(unlimited.memory_max, KernelLimit::Max);
        assert_eq!(max_for_high(u64::MAX), u64::MAX);
    }

    #[test]
    fn both_given_must_be_in_order() {
        let both = limits(settings(Some("8G"), Some("8G"), None), &room(4 * GIB, None));
        assert_eq!(both.memory_high, KernelLimit::Bytes(8 * GIB));
        let error = settings(Some("8G"), Some("9G"), None)
            .resolve(&room(4 * GIB, None))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "--memory-high 9.0GiB is above --memory-max 8.0GiB"
        );
        let soft_only = limits(
            settings(Some("infinity"), Some("8G"), None),
            &room(4 * GIB, None),
        );
        assert_eq!(soft_only.memory_high, KernelLimit::Bytes(8 * GIB));
        assert!(
            settings(Some("8G"), Some("infinity"), None)
                .resolve(&room(4 * GIB, None))
                .is_ok()
        );
    }

    #[test]
    fn a_swap_limit_is_set_only_when_given() {
        let swap = limits(settings(None, None, Some("1G")), &room(12 * GIB, None));
        assert!(settings(None, None, Some("1G")).is_explicit());
        assert_eq!(swap.memory_swap_max, Some(KernelLimit::Bytes(GIB)));
        assert_eq!(swap.memory_max, KernelLimit::Bytes(10 * GIB));
        let none = limits(settings(None, None, Some("0")), &room(12 * GIB, None));
        assert_eq!(none.memory_swap_max, Some(KernelLimit::Bytes(0)));
    }

    #[test]
    fn limits_print_and_serialize() {
        assert_eq!(KernelLimit::Bytes(4096).property_value(), "4096");
        assert_eq!(KernelLimit::Max.property_value(), "infinity");
        assert_eq!(KernelLimit::Bytes(GIB).to_string(), "1.0GiB");
        assert_eq!(KernelLimit::Max.to_string(), "max");
        let value = serde_json::to_value(JobLimits {
            memory_max: KernelLimit::Bytes(GIB),
            memory_high: KernelLimit::Max,
            memory_swap_max: None,
        })
        .unwrap();
        assert_eq!(value["memory_max"], GIB);
        assert_eq!(value["memory_high"], "max");
        assert!(value.get("memory_swap_max").is_none());
    }
}
