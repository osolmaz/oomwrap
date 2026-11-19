//! The JSON-lines event log.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::domain::Domain;
use crate::policy::StopReason;
use crate::readings::DomainReadings;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EventKind {
    Preflight,
    Refused,
    Launched,
    ChildExit,
    ProcessGroupCleanup,
    MemoryPressureKill,
    ParentSignalKill,
}

/// What an event is about: the run, the check that refused it, or the
/// reason of a stop.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EventLabel {
    Run,
    MachineGuard,
    #[serde(untagged)]
    Reason(StopReason),
}

#[derive(Debug, Serialize)]
pub(crate) struct Event {
    unix_ms: u128,
    event: EventKind,
    label: EventLabel,
    pid: Option<u32>,
    command: Vec<String>,
    detail: Option<String>,
    domain: Option<Domain>,
    readings: Option<DomainReadings>,
    exit_code: Option<i32>,
}

impl Event {
    pub(crate) fn new(event: EventKind, label: EventLabel) -> Self {
        Self {
            unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            event,
            label,
            pid: None,
            command: Vec::new(),
            detail: None,
            domain: None,
            readings: None,
            exit_code: None,
        }
    }

    pub(crate) fn with_pid(mut self, pid: u32) -> Self {
        self.pid = Some(pid);
        self
    }

    pub(crate) fn with_command(mut self, command: Vec<String>) -> Self {
        self.command = command;
        self
    }

    pub(crate) fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub(crate) fn with_domain(mut self, domain: &Domain) -> Self {
        self.domain = Some(domain.clone());
        self
    }

    pub(crate) fn with_readings(mut self, readings: DomainReadings) -> Self {
        self.readings = Some(readings);
        self
    }

    pub(crate) fn with_exit(mut self, exit_code: i32) -> Self {
        self.exit_code = Some(exit_code);
        self
    }
}

pub(crate) struct EventLogger {
    file: Option<File>,
}

impl EventLogger {
    pub(crate) fn new(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self { file: None });
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open event log {}", path.display()))?;
        Ok(Self { file: Some(file) })
    }

    /// Writes one event as one line, in one `write` to a file opened for
    /// append, so that the events of concurrent jobs never mix inside a line.
    pub(crate) fn log(&mut self, event: Event) -> Result<()> {
        let Some(file) = &mut self.file else {
            return Ok(());
        };
        let mut line = serde_json::to_vec(&event)?;
        line.push(b'\n');
        file.write_all(&line)?;
        Ok(())
    }

    /// Logs an event after the launch, when a failed write must not stop the job.
    pub(crate) fn log_or_warn(&mut self, event: Event) {
        if let Err(error) = self.log(event) {
            eprintln!("warning: failed to write event log after launch: {error:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_one_json_line_per_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/events.jsonl");
        let mut logger = EventLogger::new(Some(&path)).unwrap();
        logger
            .log(
                Event::new(EventKind::ChildExit, EventLabel::Run)
                    .with_pid(7)
                    .with_command(vec!["true".into()])
                    .with_detail("done")
                    .with_domain(&Domain::Machine)
                    .with_readings(DomainReadings {
                        size_bytes: 4,
                        free_bytes: 2,
                        full_avg10: None,
                    })
                    .with_exit(3),
            )
            .unwrap();
        logger
            .log(Event::new(
                EventKind::Refused,
                EventLabel::Reason(StopReason::DomainFloor),
            ))
            .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["event"], "child_exit");
        assert_eq!(lines[0]["label"], "run");
        assert_eq!(lines[0]["pid"], 7);
        assert_eq!(lines[0]["detail"], "done");
        assert_eq!(lines[0]["domain"]["kind"], "machine");
        assert_eq!(lines[0]["readings"]["free_bytes"], 2);
        assert_eq!(lines[0]["exit_code"], 3);
        assert_eq!(lines[1]["event"], "refused");
        assert_eq!(lines[1]["label"], "domain_floor");
        assert_eq!(
            serde_json::to_value(EventLabel::MachineGuard).unwrap(),
            "machine_guard"
        );
    }

    #[test]
    fn logger_without_path_writes_nothing() {
        let mut logger = EventLogger::new(None).unwrap();
        logger
            .log(Event::new(EventKind::Preflight, EventLabel::Run))
            .unwrap();
    }
}
