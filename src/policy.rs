//! The decision to stop a job, or to refuse its launch. Pure: no IO.

use crate::limits::Size;
use crate::readings::MemInfo;

/// The memory floors of one run.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Floors {
    pub(crate) mem_available: Size,
    pub(crate) swap_free: Size,
}

/// Why the guard stops a job or refuses its launch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StopReason {
    /// `MemAvailable` fell below the floor.
    MemAvailable {
        observed_bytes: u64,
        floor_bytes: u64,
    },
    /// `SwapFree` fell below the floor.
    SwapFree {
        observed_bytes: u64,
        floor_bytes: u64,
    },
}

/// Returns the first floor that `mem` is below, or `None` when all hold.
/// A value equal to its floor holds.
pub(crate) fn evaluate(floors: &Floors, mem: &MemInfo) -> Option<StopReason> {
    if mem.mem_available_bytes < floors.mem_available.bytes() {
        return Some(StopReason::MemAvailable {
            observed_bytes: mem.mem_available_bytes,
            floor_bytes: floors.mem_available.bytes(),
        });
    }
    if mem.swap_free_bytes < floors.swap_free.bytes() {
        return Some(StopReason::SwapFree {
            observed_bytes: mem.swap_free_bytes,
            floor_bytes: floors.swap_free.bytes(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn floors(mem: &str, swap: &str) -> Floors {
        Floors {
            mem_available: mem.parse().unwrap(),
            swap_free: swap.parse().unwrap(),
        }
    }

    fn mem(available: u64, swap: u64) -> MemInfo {
        MemInfo {
            mem_available_bytes: available,
            swap_free_bytes: swap,
        }
    }

    #[test]
    fn values_equal_to_their_floors_hold() {
        assert_eq!(evaluate(&floors("2K", "1K"), &mem(2048, 1024)), None);
    }

    #[test]
    fn memory_floor_comes_before_swap_floor() {
        assert_eq!(
            evaluate(&floors("2K", "1K"), &mem(2047, 0)),
            Some(StopReason::MemAvailable {
                observed_bytes: 2047,
                floor_bytes: 2048
            })
        );
        assert_eq!(
            evaluate(&floors("2K", "1K"), &mem(4096, 1023)),
            Some(StopReason::SwapFree {
                observed_bytes: 1023,
                floor_bytes: 1024
            })
        );
    }
}
