//! The replay of recorded runs. Each trace under `tests/fixtures/traces`
//! goes through the current policy and its defaults and must get the
//! decision that `traces.toml` names, so a change of the policy or of a
//! default is checked against real runs, not only against made-up numbers.
//! The replay leaves out the short wait before a domain stop.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{Decision, PressureWindow, StopReason};
use crate::limits::{GuardPolicy, SizeSetting, StopLimits};
use crate::readings::Readings;

/// A trace names its job only by one of these labels.
const LABELS: &[&str] = &[
    "build",
    "renderer",
    "model load",
    "memory ramp",
    "machine pressure",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    trace: Vec<Trace>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Trace {
    file: String,
    label: String,
    /// Where and how the run was recorded, in generic words.
    recorded: String,
    check: Vec<Check>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct Check {
    /// A `--min-mem` in place of the default, such as `0` to see the
    /// pressure stops alone.
    min_mem: Option<String>,
    /// `continue`, or the reason of the first stop.
    expect: String,
    /// The first stop comes within these seconds of the launch.
    stop_between_s: Option<[u64; 2]>,
}

/// One line of a trace, as `--record-readings` writes it: the readings,
/// and the decision of the version that recorded it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Line {
    readings: Readings,
    #[serde(rename = "decision")]
    _decision: Decision,
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/traces")
}

fn index() -> Index {
    let text = fs::read_to_string(fixtures().join("traces.toml")).unwrap();
    toml::from_str(&text).unwrap()
}

fn read_trace(file: &str) -> Vec<Readings> {
    let text = fs::read_to_string(fixtures().join(file)).unwrap();
    text.lines()
        .map(|line| {
            serde_json::from_str::<Line>(line)
                .unwrap_or_else(|error| panic!("{file}: {error}: {line}"))
                .readings
        })
        .collect()
}

fn limits(check: &Check, domain_bytes: u64) -> StopLimits {
    let mut policy = GuardPolicy::default();
    if let Some(min_mem) = &check.min_mem {
        policy.min_mem = SizeSetting::Given(min_mem.parse().unwrap());
    }
    policy.resolve(domain_bytes).unwrap()
}

/// The first stop of the policy on a trace, with its time in milliseconds.
fn first_stop(readings: &[Readings], limits: &StopLimits) -> Option<(u64, StopReason)> {
    let mut window = PressureWindow::default();
    readings.iter().find_map(
        |reading| match Decision::evaluate(limits, reading, &mut window) {
            Decision::Stop(reason) => Some((reading.elapsed_ms, reason)),
            Decision::Continue => None,
        },
    )
}

/// What is wrong with the outcome of one check, if anything.
fn failure(trace: &Trace, check: &Check, readings: &[Readings]) -> Option<String> {
    let stop = first_stop(readings, &limits(check, readings[0].domain.size_bytes));
    let got = stop.map_or_else(|| "continue".to_string(), |(_, reason)| reason.to_string());
    let name = format!("{} (min-mem {:?})", trace.file, check.min_mem);
    if got != check.expect {
        return Some(format!("{name}: got {got}, not {}", check.expect));
    }
    match (check.stop_between_s, stop) {
        (Some([from, to]), Some((at_ms, _))) if !(from * 1000..=to * 1000).contains(&at_ms) => {
            Some(format!(
                "{name}: stopped at {at_ms} ms, not within {from} to {to} s"
            ))
        }
        _ => None,
    }
}

#[test]
fn every_trace_gets_its_decision() {
    let index = index();
    let mut failures = Vec::new();
    for trace in &index.trace {
        assert!(LABELS.contains(&trace.label.as_str()), "{}", trace.label);
        assert!(!trace.recorded.is_empty() && !trace.check.is_empty());
        let readings = read_trace(&trace.file);
        assert!(readings.len() >= 10, "{} is too short", trace.file);
        failures.extend(
            trace
                .check
                .iter()
                .filter_map(|check| failure(trace, check, &readings)),
        );
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn every_trace_file_is_in_the_index() {
    let mut listed: Vec<String> = index().trace.into_iter().map(|trace| trace.file).collect();
    let mut found: Vec<String> = fs::read_dir(fixtures())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".jsonl"))
        .collect();
    listed.sort();
    found.sort();
    assert_eq!(listed, found);
}
