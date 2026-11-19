//! The JSON-lines event log.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::readings::MemInfo;

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

/// What an event is about: the run, or the check that refused it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EventLabel {
    Run,
    Earlyoom,
    Memory,
    Swap,
}

#[derive(Debug, Serialize)]
pub(crate) struct Event {
    unix_ms: u128,
    event: EventKind,
    label: EventLabel,
    pid: Option<u32>,
    command: Vec<String>,
    detail: Option<String>,
    mem_available_bytes: Option<u64>,
    swap_free_bytes: Option<u64>,
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
            mem_available_bytes: None,
            swap_free_bytes: None,
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

    pub(crate) fn with_mem(mut self, mem: MemInfo) -> Self {
        self.mem_available_bytes = Some(mem.mem_available_bytes);
        self.swap_free_bytes = Some(mem.swap_free_bytes);
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
                    .with_mem(MemInfo {
                        mem_available_bytes: 1,
                        swap_free_bytes: 2,
                    })
                    .with_exit(3),
            )
            .unwrap();
        logger
            .log(Event::new(EventKind::Refused, EventLabel::Swap))
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
        assert_eq!(lines[0]["swap_free_bytes"], 2);
        assert_eq!(lines[0]["exit_code"], 3);
        assert_eq!(lines[1]["event"], "refused");
        assert_eq!(lines[1]["label"], "swap");
    }

    #[test]
    fn logger_without_path_writes_nothing() {
        let mut logger = EventLogger::new(None).unwrap();
        logger
            .log(Event::new(EventKind::Preflight, EventLabel::Run))
            .unwrap();
    }
}
