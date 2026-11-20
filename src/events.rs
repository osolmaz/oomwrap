//! The JSON-lines event log, and the trace that `--record-readings` writes.
//!
//! Each line is built in memory and written with one `write` to a file opened
//! for append, so the lines of concurrent jobs never mix.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::backend::BackendKind;
use crate::domain::Domain;
use crate::job_limits::JobLimits;
use crate::limits::StopLimits;
use crate::policy::{Decision, StopReason};
use crate::readings::Readings;

/// The version of the event and trace formats. 0.1.0 wrote no version.
pub(crate) const SCHEMA_VERSION: u32 = 1;
/// The default log keeps the files of this many days, today included.
const RETENTION_DAYS: i64 = 14;
const LOG_PREFIX: &str = "events-";
const LOG_SUFFIX: &str = ".jsonl";
const SECONDS_PER_DAY: u64 = 86_400;

/// Where the events of a run go.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum EventLog {
    /// One file per day in the state folder; a failure to open it only warns.
    Default,
    /// `--event-log none`.
    Off,
    /// An explicit file; a failure to open it stops the launch.
    File(PathBuf),
}

impl FromStr for EventLog {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        match text {
            "" => bail!("write a path, or none to turn the log off"),
            "none" => Ok(Self::Off),
            path => Ok(Self::File(PathBuf::from(path))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EventKind {
    Preflight,
    Refused,
    Launched,
    /// The command could not be executed.
    ExecFailed,
    /// The command ended on its own.
    Exited,
    /// Members of the job's group remained after the command ended.
    Cleanup,
    /// The guard stopped the job.
    Stopped,
    /// oomwrap received a signal and ended the job.
    Interrupted,
    /// oomwrap could no longer guard the job and ended it.
    GuardError,
}

/// Why an event happened: a stop reason, or the check that refused a run.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EventReason {
    NoMachineGuard,
    LaunchFailed,
    /// The `cgroup` backend was required and is not available.
    CgroupUnavailable,
    #[serde(untagged)]
    Stop(StopReason),
}

/// The fields that every event of one run carries.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct RunContext {
    pub(crate) backend: BackendKind,
    pub(crate) domain: Domain,
    /// The file name of the program, never its folder or its arguments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) program: Option<String>,
    /// The full command line, only with `--log-command`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) command: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pid: Option<u32>,
    /// The scope's unit name, in the `cgroup` backend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) unit: Option<String>,
    /// The scope's cgroup path, in the `cgroup` backend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cgroup: Option<String>,
}

impl RunContext {
    pub(crate) fn event(&self, event: EventKind) -> Event {
        Event {
            schema_version: SCHEMA_VERSION,
            unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            event,
            context: self.clone(),
            reason: None,
            readings: None,
            limits: None,
            job_limits: None,
            oom_score_adj: None,
            exit_code: None,
            signal: None,
            detail: None,
        }
    }
}

/// The file name of a program path, as events record it.
pub(crate) fn program_name(program: &str) -> String {
    Path::new(program).file_name().map_or_else(
        || program.to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

#[derive(Debug, Serialize)]
pub(crate) struct Event {
    schema_version: u32,
    unix_ms: u128,
    event: EventKind,
    #[serde(flatten)]
    context: RunContext,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<EventReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    readings: Option<Readings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limits: Option<StopLimits>,
    /// The kernel limits of the job's scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    job_limits: Option<JobLimits>,
    #[serde(skip_serializing_if = "Option::is_none")]
    oom_score_adj: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signal: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl Event {
    pub(crate) fn with_reason(mut self, reason: EventReason) -> Self {
        self.reason = Some(reason);
        self
    }

    pub(crate) fn with_readings(mut self, readings: Readings) -> Self {
        self.readings = Some(readings);
        self
    }

    pub(crate) fn with_limits(mut self, limits: StopLimits) -> Self {
        self.limits = Some(limits);
        self
    }

    pub(crate) fn with_job_limits(mut self, limits: JobLimits) -> Self {
        self.job_limits = Some(limits);
        self
    }

    pub(crate) fn with_oom_score_adj(mut self, score: i32) -> Self {
        self.oom_score_adj = Some(score);
        self
    }

    pub(crate) fn with_exit(mut self, exit_code: i32) -> Self {
        self.exit_code = Some(exit_code);
        self
    }

    pub(crate) fn with_signal(mut self, signal: i32) -> Self {
        self.signal = Some(signal);
        self
    }

    pub(crate) fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

pub(crate) struct EventLogger {
    file: Option<File>,
}

impl EventLogger {
    /// Opens the log of `setting`. `state_dir` is the folder of the default
    /// log, `${XDG_STATE_HOME:-~/.local/state}`.
    pub(crate) fn open(setting: &EventLog, state_dir: Result<PathBuf>) -> Result<Self> {
        let file = match setting {
            EventLog::Off => None,
            EventLog::File(path) => Some(open_append(path)?),
            EventLog::Default => match open_default(state_dir, SystemTime::now()) {
                Ok(file) => Some(file),
                Err(error) => {
                    eprintln!("warning: running without the default event log: {error:#}");
                    None
                }
            },
        };
        Ok(Self { file })
    }

    /// Writes one event as one line.
    pub(crate) fn log(&mut self, event: &Event) -> Result<()> {
        match &mut self.file {
            Some(file) => write_line(file, event),
            None => Ok(()),
        }
    }

    /// Logs an event after the launch, when a failed write must not stop the job.
    pub(crate) fn log_or_warn(&mut self, event: &Event) {
        if let Err(error) = self.log(event) {
            eprintln!("warning: failed to write event log after launch: {error:#}");
        }
    }
}

fn write_line(file: &mut File, value: &impl Serialize) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    file.write_all(&line)?;
    Ok(())
}

fn open_append(path: &Path) -> Result<File> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open event log {}", path.display()))
}

/// Opens today's file of the default log and removes the files of days
/// that are past the retention.
fn open_default(state_dir: Result<PathBuf>, now: SystemTime) -> Result<File> {
    let dir = state_dir?.join("oomwrap");
    let today = days_since_epoch(now);
    let file = open_append(&dir.join(log_file_name(today)))?;
    prune_logs(&dir, today);
    Ok(file)
}

fn days_since_epoch(time: SystemTime) -> i64 {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    i64::try_from(seconds / SECONDS_PER_DAY).unwrap_or(i64::MAX)
}

/// `events-2026-11-20.jsonl` for the day in UTC.
fn log_file_name(days: i64) -> String {
    let (year, month, day) = civil_from_days(days);
    format!("{LOG_PREFIX}{year:04}-{month:02}-{day:02}{LOG_SUFFIX}")
}

/// The day of a log file name, or `None` for any other file.
fn log_file_day(name: &str) -> Option<i64> {
    let date = name.strip_prefix(LOG_PREFIX)?.strip_suffix(LOG_SUFFIX)?;
    let mut parts = date.split('-');
    let (year, month, day) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || year.len() != 4 || month.len() != 2 || day.len() != 2 {
        return None;
    }
    let days = days_from_civil(year.parse().ok()?, month.parse().ok()?, day.parse().ok()?);
    (log_file_name(days) == name).then_some(days)
}

/// Removes the log files of days at least `RETENTION_DAYS` before `today`,
/// and nothing else.
fn prune_logs(dir: &Path, today: i64) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(day) = name.to_str().and_then(log_file_day) else {
            continue;
        };
        if today - day >= RETENTION_DAYS {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Days since 1970-01-01 of a date in the proleptic Gregorian calendar.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = i64::from((month + 9) % 12);
    let day_of_year = (153 * month_index + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The date of a count of days since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    // The month is 1 to 12 and the day 1 to 31 by construction.
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

/// One line of a trace: what the guard read, and what the policy decided.
#[derive(Debug, Serialize)]
struct TraceLine<'a> {
    readings: &'a Readings,
    decision: Decision,
}

/// The trace of `--record-readings`, the format of the replay tests.
pub(crate) struct TraceWriter {
    file: Option<File>,
}

impl TraceWriter {
    pub(crate) fn create(path: Option<&Path>) -> Result<Self> {
        let file = match path {
            Some(path) => Some(
                File::create(path)
                    .with_context(|| format!("failed to create trace {}", path.display()))?,
            ),
            None => None,
        };
        Ok(Self { file })
    }

    /// Writes one reading; after a failed write, the trace stops with a warning.
    pub(crate) fn record(&mut self, readings: &Readings, decision: Decision) {
        let Some(file) = &mut self.file else {
            return;
        };
        if let Err(error) = write_line(file, &TraceLine { readings, decision }) {
            eprintln!("warning: stopped the trace of readings: {error:#}");
            self.file = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::readings::DomainReadings;

    fn context() -> RunContext {
        RunContext {
            backend: BackendKind::Poll,
            domain: Domain::Machine,
            program: Some("true".into()),
            command: None,
            pid: Some(7),
            unit: None,
            cgroup: None,
        }
    }

    fn readings() -> Readings {
        Readings {
            elapsed_ms: 5,
            domain: DomainReadings {
                size_bytes: 4,
                free_bytes: 2,
                full_avg10: None,
            },
            job: None,
        }
    }

    fn lines(path: &Path) -> Vec<serde_json::Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn writes_one_versioned_json_line_per_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/events.jsonl");
        let mut logger =
            EventLogger::open(&EventLog::File(path.clone()), Ok(PathBuf::new())).unwrap();
        let event = context()
            .event(EventKind::Stopped)
            .with_reason(EventReason::Stop(StopReason::DomainFloor))
            .with_readings(readings())
            .with_exit(137)
            .with_signal(15)
            .with_oom_score_adj(1000)
            .with_detail("done");
        logger.log(&event).unwrap();
        logger.log_or_warn(
            &context()
                .event(EventKind::Refused)
                .with_reason(EventReason::NoMachineGuard),
        );
        let lines = lines(&path);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["schema_version"], 1);
        assert_eq!(lines[0]["event"], "stopped");
        assert_eq!(lines[0]["backend"], "poll");
        assert_eq!(lines[0]["domain"]["kind"], "machine");
        assert_eq!(lines[0]["program"], "true");
        assert_eq!(lines[0]["pid"], 7);
        assert_eq!(lines[0]["reason"], "domain_floor");
        assert_eq!(lines[0]["readings"]["domain"]["free_bytes"], 2);
        assert_eq!(lines[0]["exit_code"], 137);
        assert_eq!(lines[0]["signal"], 15);
        assert_eq!(lines[0]["oom_score_adj"], 1000);
        assert_eq!(lines[0]["detail"], "done");
        assert!(lines[0].get("command").is_none());
        assert!(lines[0].get("unit").is_none());
        assert!(lines[0].get("job_limits").is_none());
        assert_eq!(lines[1]["reason"], "no_machine_guard");
        assert!(lines[1].get("readings").is_none());
        assert_eq!(
            serde_json::to_value(EventReason::LaunchFailed).unwrap(),
            "launch_failed"
        );
    }

    #[test]
    fn the_command_line_is_logged_only_when_asked() {
        let mut with_command = context();
        with_command.command = Some(vec!["tool".into(), "--token".into(), "secret".into()]);
        let line = serde_json::to_value(with_command.event(EventKind::Launched)).unwrap();
        assert_eq!(line["command"][2], "secret");
        assert_eq!(program_name("/opt/bin/tool"), "tool");
        assert_eq!(program_name("tool"), "tool");
        assert_eq!(program_name("/"), "/");
    }

    #[test]
    fn a_scope_names_its_unit_cgroup_and_limits() {
        use crate::job_limits::KernelLimit;
        let mut scope = context();
        scope.backend = BackendKind::Cgroup;
        scope.unit = Some("oomwrap-g-1".into());
        scope.cgroup = Some("/a/oomwrap-g-1.scope".into());
        let event = scope.event(EventKind::Launched).with_job_limits(JobLimits {
            memory_max: KernelLimit::Bytes(4096),
            memory_high: KernelLimit::Max,
            memory_swap_max: None,
        });
        let line = serde_json::to_value(event).unwrap();
        assert_eq!(line["backend"], "cgroup");
        assert_eq!(line["unit"], "oomwrap-g-1");
        assert_eq!(line["cgroup"], "/a/oomwrap-g-1.scope");
        assert_eq!(line["job_limits"]["memory_max"], 4096);
        assert_eq!(
            serde_json::to_value(EventReason::CgroupUnavailable).unwrap(),
            "cgroup_unavailable"
        );
    }

    #[test]
    fn log_settings_parse() {
        assert_eq!("none".parse::<EventLog>().unwrap(), EventLog::Off);
        assert_eq!(
            "a/b.jsonl".parse::<EventLog>().unwrap(),
            EventLog::File(PathBuf::from("a/b.jsonl"))
        );
        assert!("".parse::<EventLog>().is_err());
        let mut off = EventLogger::open(&EventLog::Off, Ok(PathBuf::new())).unwrap();
        off.log(&context().event(EventKind::Preflight)).unwrap();
        assert!(
            EventLogger::open(
                &EventLog::File(PathBuf::from("/proc/none/x")),
                Ok(PathBuf::new())
            )
            .is_err()
        );
    }

    #[test]
    fn the_default_log_is_one_file_per_day_kept_fourteen_days() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("oomwrap");
        fs::create_dir_all(&logs).unwrap();
        let today = days_from_civil(2026, 11, 20);
        let now = UNIX_EPOCH
            + Duration::from_secs(u64::try_from(today).unwrap() * SECONDS_PER_DAY + 3600);
        for (name, kept) in [
            ("events-2026-11-07.jsonl", true),
            ("events-2026-11-06.jsonl", false),
            ("events-2025-01-01.jsonl", false),
            ("events-2026-1-01.jsonl", true),
            ("events-2026-02-30.jsonl", true),
            ("notes.txt", true),
        ] {
            fs::write(logs.join(name), "").unwrap();
            assert_eq!(
                log_file_day(name).is_none_or(|day| today - day < RETENTION_DAYS),
                kept,
                "{name}"
            );
        }
        let mut file = open_default(Ok(dir.path().to_path_buf()), now).unwrap();
        write_line(&mut file, &1).unwrap();
        assert_eq!(
            fs::read_to_string(logs.join("events-2026-11-20.jsonl")).unwrap(),
            "1\n"
        );
        assert!(logs.join("events-2026-11-07.jsonl").exists());
        assert!(!logs.join("events-2026-11-06.jsonl").exists());
        assert!(!logs.join("events-2025-01-01.jsonl").exists());
        assert!(logs.join("events-2026-1-01.jsonl").exists());
        assert!(logs.join("events-2026-02-30.jsonl").exists());
        assert!(logs.join("notes.txt").exists());
    }

    #[test]
    fn a_default_log_that_cannot_open_only_warns() {
        let logger =
            EventLogger::open(&EventLog::Default, Err(anyhow::anyhow!("HOME is not set"))).unwrap();
        assert!(logger.file.is_none());
        let file = tempfile::NamedTempFile::new().unwrap();
        // A regular file where the folder should be.
        let logger = EventLogger::open(&EventLog::Default, Ok(file.path().to_path_buf())).unwrap();
        assert!(logger.file.is_none());
    }

    #[test]
    fn civil_dates_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        for days in [-800_000, -1, 0, 59, 60, 365, 10_957, 20_777, 2_000_000] {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(days_from_civil(year, month, day), days);
        }
        assert_eq!(
            log_file_name(days_from_civil(2026, 1, 5)),
            "events-2026-01-05.jsonl"
        );
        assert_eq!(
            days_since_epoch(UNIX_EPOCH + Duration::from_secs(86_399)),
            0
        );
        assert_eq!(
            days_since_epoch(UNIX_EPOCH + Duration::from_secs(86_400)),
            1
        );
    }

    #[test]
    fn traces_hold_readings_and_decisions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        let mut trace = TraceWriter::create(Some(&path)).unwrap();
        trace.record(&readings(), Decision::Continue);
        trace.record(&readings(), Decision::Stop(StopReason::DomainPressure));
        let lines = lines(&path);
        assert_eq!(lines[0]["decision"], "continue");
        assert_eq!(lines[0]["readings"]["elapsed_ms"], 5);
        assert_eq!(lines[1]["decision"]["stop"], "domain_pressure");
        TraceWriter::create(None)
            .unwrap()
            .record(&readings(), Decision::Continue);
        assert!(TraceWriter::create(Some(&dir.path().join("missing/x"))).is_err());
    }
}
