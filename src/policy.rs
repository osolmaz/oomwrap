//! The decision to stop a job, or to refuse its launch. Pure: no IO.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::limits::{Pressure, PressureLimit, StopLimits};
use crate::readings::{DomainReadings, MemoryEvents, Readings};

/// Why the guard stops a job, or refuses its launch.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StopReason {
    /// The job reached its `memory.max`.
    JobLimit,
    /// An out-of-memory killer outside the job's limit killed a job process.
    OutsideOomKill,
    JobPressure,
    DomainPressure,
    /// The domain's free room fell below `--min-mem`.
    DomainFloor,
}

impl StopReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::JobLimit => "job_limit",
            Self::OutsideOomKill => "outside_oom_kill",
            Self::JobPressure => "job_pressure",
            Self::DomainPressure => "domain_pressure",
            Self::DomainFloor => "domain_floor",
        }
    }
}

impl StopReason {
    /// A reason that every guard of the domain sees at once.
    pub(crate) fn is_domain(self) -> bool {
        matches!(self, Self::DomainPressure | Self::DomainFloor)
    }
}

impl fmt::Display for StopReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What the policy decided on one reading; a trace records it as
/// `"continue"` or `{"stop": "<reason>"}`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Decision {
    Continue,
    Stop(StopReason),
}

/// Since when each pressure has stayed at or above its limit, in
/// milliseconds since the launch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PressureWindow {
    job_since_ms: Option<u64>,
    domain_since_ms: Option<u64>,
}

impl Decision {
    /// Decides on one reading. The first reason of the table in the plan
    /// that holds wins: `job_limit`, `outside_oom_kill`, `job_pressure`,
    /// `domain_pressure`, `domain_floor`.
    pub(crate) fn evaluate(
        limits: &StopLimits,
        readings: &Readings,
        window: &mut PressureWindow,
    ) -> Self {
        let now_ms = readings.elapsed_ms;
        let job_pressure = readings.job.and_then(|job| job.full_avg10);
        let job_pressure_held = holds_for(
            &mut window.job_since_ms,
            job_pressure,
            limits.job_pressure,
            now_ms,
        );
        let domain_pressure_held = holds_for(
            &mut window.domain_since_ms,
            readings.domain.full_avg10,
            limits.domain_pressure,
            now_ms,
        );

        if let Some(reason) = readings.job.and_then(|job| kill_reason(&job.events)) {
            return Self::Stop(reason);
        }
        if job_pressure_held {
            return Self::Stop(StopReason::JobPressure);
        }
        if domain_pressure_held {
            return Self::Stop(StopReason::DomainPressure);
        }
        if below_floor(&readings.domain, limits) {
            return Self::Stop(StopReason::DomainFloor);
        }
        Self::Continue
    }
}

/// The reason when the kernel acted on the job: `oom` rose when the job
/// reached its `memory.max`; `oom_kill` without `oom` means an
/// out-of-memory killer outside the job's limit killed a job process. The
/// job's scope is new, so any count above zero has risen.
pub(crate) fn kill_reason(events: &MemoryEvents) -> Option<StopReason> {
    if events.oom > 0 {
        Some(StopReason::JobLimit)
    } else if events.oom_kill > 0 {
        Some(StopReason::OutsideOomKill)
    } else {
        None
    }
}

/// Updates one window and tells whether its pressure has held long enough.
fn holds_for(
    since_ms: &mut Option<u64>,
    pressure: Option<Pressure>,
    limit: PressureLimit,
    now_ms: u64,
) -> bool {
    match pressure {
        Some(pressure) if pressure >= limit.at_least => {
            let start = *since_ms.get_or_insert(now_ms);
            Duration::from_millis(now_ms.saturating_sub(start)) >= limit.lasting
        }
        _ => {
            *since_ms = None;
            false
        }
    }
}

fn below_floor(domain: &DomainReadings, limits: &StopLimits) -> bool {
    domain.free_bytes < limits.min_free_bytes
}

/// The longest wait before a stop for a domain reason.
pub(crate) const MAX_STAGGER: Duration = Duration::from_secs(2);

/// How long a guard waits and reads again before it stops its job for a
/// domain reason. Every guard of a domain sees the same numbers, so the job
/// with the larger share of the domain's use waits less and stops first;
/// the others usually see the condition end. A job whose use is not known
/// waits the longest.
pub(crate) fn stagger_delay(job_bytes: Option<u64>, domain: &DomainReadings) -> Duration {
    let used = domain.size_bytes.saturating_sub(domain.free_bytes);
    let Some(job) = job_bytes.filter(|_| used > 0) else {
        return MAX_STAGGER;
    };
    let share_left = u128::from(used.saturating_sub(job.min(used)));
    let max_ms = MAX_STAGGER.as_millis();
    let wait_ms = max_ms * share_left / u128::from(used);
    Duration::from_millis(u64::try_from(wait_ms).unwrap_or(u64::MAX))
}

/// The domain reason that refuses a launch now, if any: the floor, or a
/// pressure that already stands at its limit.
pub(crate) fn preflight(limits: &StopLimits, domain: &DomainReadings) -> Option<StopReason> {
    if below_floor(domain, limits) {
        return Some(StopReason::DomainFloor);
    }
    if domain
        .full_avg10
        .is_some_and(|pressure| pressure >= limits.domain_pressure.at_least)
    {
        return Some(StopReason::DomainPressure);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::readings::{JobReadings, MemoryEvents};

    const GIB: u64 = 1024 * 1024 * 1024;

    fn pressure(percent: u32) -> Option<Pressure> {
        Pressure::from_hundredths(percent * 100)
    }

    fn limits() -> StopLimits {
        StopLimits {
            min_free_bytes: GIB,
            job_pressure: PressureLimit {
                at_least: Pressure::from_hundredths(6000).unwrap(),
                lasting: Duration::from_secs(30),
            },
            domain_pressure: PressureLimit {
                at_least: Pressure::from_hundredths(2000).unwrap(),
                lasting: Duration::from_secs(10),
            },
        }
    }

    fn domain(free_bytes: u64, full: u32) -> DomainReadings {
        DomainReadings {
            size_bytes: 16 * GIB,
            free_bytes,
            full_avg10: pressure(full),
        }
    }

    fn job(events: MemoryEvents, full: u32) -> JobReadings {
        JobReadings {
            current_bytes: GIB,
            swap_current_bytes: 0,
            full_avg10: pressure(full),
            events,
        }
    }

    fn at(elapsed_ms: u64, domain: DomainReadings, job: Option<JobReadings>) -> Readings {
        Readings {
            elapsed_ms,
            domain,
            job,
        }
    }

    fn decide(readings: &[Readings]) -> Vec<Decision> {
        let mut window = PressureWindow::default();
        readings
            .iter()
            .map(|reading| Decision::evaluate(&limits(), reading, &mut window))
            .collect()
    }

    #[test]
    fn only_domain_reasons_are_domain_reasons() {
        assert!(StopReason::DomainFloor.is_domain());
        assert!(StopReason::DomainPressure.is_domain());
        assert!(!StopReason::JobLimit.is_domain());
        assert!(!StopReason::OutsideOomKill.is_domain());
        assert!(!StopReason::JobPressure.is_domain());
    }

    #[test]
    fn the_larger_job_waits_less_before_a_domain_stop() {
        // 16G domain with 4G free: 12G in use.
        let busy = domain(4 * GIB, 0);
        assert_eq!(stagger_delay(None, &busy), MAX_STAGGER);
        assert_eq!(stagger_delay(Some(0), &busy), MAX_STAGGER);
        assert_eq!(
            stagger_delay(Some(3 * GIB), &busy),
            Duration::from_millis(1500)
        );
        assert_eq!(
            stagger_delay(Some(9 * GIB), &busy),
            Duration::from_millis(500)
        );
        assert_eq!(stagger_delay(Some(12 * GIB), &busy), Duration::ZERO);
        assert_eq!(stagger_delay(Some(20 * GIB), &busy), Duration::ZERO);
        let idle = domain(16 * GIB, 0);
        assert_eq!(stagger_delay(Some(GIB), &idle), MAX_STAGGER);
    }

    #[test]
    fn full_swap_with_free_room_and_no_pressure_never_stops() {
        // The false stop of 0.1.0: swap is full, but RAM is free and calm.
        let calm = domain(8 * GIB, 0);
        assert_eq!(
            decide(&[at(0, calm, None), at(60_000, calm, None)]),
            [Decision::Continue, Decision::Continue]
        );
    }

    #[test]
    fn domain_floor_stops_at_once_and_its_edge_holds() {
        assert_eq!(
            decide(&[at(0, domain(GIB - 1, 0), None)]),
            [Decision::Stop(StopReason::DomainFloor)]
        );
        assert_eq!(decide(&[at(0, domain(GIB, 0), None)]), [Decision::Continue]);
    }

    #[test]
    fn one_high_pressure_reading_does_not_stop() {
        let high = domain(8 * GIB, 50);
        let calm = domain(8 * GIB, 0);
        assert_eq!(
            decide(&[
                at(0, high, None),
                at(9_999, high, None),
                at(10_000, calm, None),
                at(15_000, high, None),
                at(24_999, high, None),
                at(25_000, high, None),
            ]),
            [
                Decision::Continue,
                Decision::Continue,
                Decision::Continue,
                Decision::Continue,
                Decision::Continue,
                Decision::Stop(StopReason::DomainPressure),
            ]
        );
    }

    #[test]
    fn pressure_at_its_limit_counts() {
        let edge = domain(8 * GIB, 20);
        let below = DomainReadings {
            full_avg10: Pressure::from_hundredths(1999),
            ..edge
        };
        assert_eq!(
            decide(&[at(0, edge, None), at(10_000, edge, None)]),
            [
                Decision::Continue,
                Decision::Stop(StopReason::DomainPressure)
            ]
        );
        assert_eq!(
            decide(&[at(0, below, None), at(10_000, below, None)]),
            [Decision::Continue, Decision::Continue]
        );
    }

    #[test]
    fn job_pressure_needs_its_own_duration() {
        let calm = domain(8 * GIB, 0);
        let stalled = Some(job(MemoryEvents::default(), 70));
        assert_eq!(
            decide(&[
                at(0, calm, stalled),
                at(29_999, calm, stalled),
                at(30_000, calm, stalled)
            ]),
            [
                Decision::Continue,
                Decision::Continue,
                Decision::Stop(StopReason::JobPressure)
            ]
        );
    }

    #[test]
    fn kernel_events_tell_the_job_limit_from_an_outside_kill() {
        let calm = domain(8 * GIB, 0);
        let limit = job(
            MemoryEvents {
                oom: 1,
                oom_kill: 1,
            },
            0,
        );
        let outside = job(
            MemoryEvents {
                oom: 0,
                oom_kill: 1,
            },
            0,
        );
        assert_eq!(
            decide(&[at(0, calm, Some(limit))]),
            [Decision::Stop(StopReason::JobLimit)]
        );
        assert_eq!(
            decide(&[at(0, calm, Some(outside))]),
            [Decision::Stop(StopReason::OutsideOomKill)]
        );
    }

    #[test]
    fn reasons_follow_the_order_of_the_table() {
        let everything = domain(0, 90);
        let stalled_and_killed = job(
            MemoryEvents {
                oom: 0,
                oom_kill: 1,
            },
            90,
        );
        assert_eq!(
            decide(&[at(0, everything, Some(stalled_and_killed))]),
            [Decision::Stop(StopReason::OutsideOomKill)]
        );
        let stalled = job(MemoryEvents::default(), 90);
        let mut window = PressureWindow::default();
        let zero_wait = StopLimits {
            job_pressure: PressureLimit {
                lasting: Duration::ZERO,
                ..limits().job_pressure
            },
            domain_pressure: PressureLimit {
                lasting: Duration::ZERO,
                ..limits().domain_pressure
            },
            ..limits()
        };
        assert_eq!(
            Decision::evaluate(&zero_wait, &at(0, everything, Some(stalled)), &mut window),
            Decision::Stop(StopReason::JobPressure)
        );
        assert_eq!(
            Decision::evaluate(&zero_wait, &at(0, everything, None), &mut window),
            Decision::Stop(StopReason::DomainPressure)
        );
    }

    #[test]
    fn no_psi_turns_pressure_stops_off() {
        let no_psi = DomainReadings {
            full_avg10: None,
            ..domain(8 * GIB, 0)
        };
        assert_eq!(
            decide(&[at(0, no_psi, None), at(60_000, no_psi, None)]),
            [Decision::Continue, Decision::Continue]
        );
    }

    #[test]
    fn preflight_refuses_on_the_floor_or_standing_pressure() {
        assert_eq!(preflight(&limits(), &domain(8 * GIB, 0)), None);
        assert_eq!(
            preflight(&limits(), &domain(GIB - 1, 0)),
            Some(StopReason::DomainFloor)
        );
        assert_eq!(
            preflight(&limits(), &domain(8 * GIB, 20)),
            Some(StopReason::DomainPressure)
        );
        assert_eq!(preflight(&limits(), &domain(8 * GIB, 19)), None);
    }

    #[test]
    fn reasons_name_themselves() {
        for (reason, name) in [
            (StopReason::JobLimit, "job_limit"),
            (StopReason::OutsideOomKill, "outside_oom_kill"),
            (StopReason::JobPressure, "job_pressure"),
            (StopReason::DomainPressure, "domain_pressure"),
            (StopReason::DomainFloor, "domain_floor"),
        ] {
            assert_eq!(reason.to_string(), name);
            assert_eq!(serde_json::to_value(reason).unwrap(), name);
        }
    }
}
