//! Sizes and durations of the guard policy, parsed once at the border.

use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

/// A memory size in bytes.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Size(u64);

impl Size {
    pub(crate) fn bytes(self) -> u64 {
        self.0
    }
}

impl FromStr for Size {
    type Err = anyhow::Error;

    fn from_str(input: &str) -> Result<Self> {
        parse_size(input).map(Self)
    }
}

/// Parses a size with an optional unit: `B`, `K`, `M`, `G` or `T`, with or
/// without a `B` or `iB` suffix, all binary.
pub(crate) fn parse_size(input: &str) -> Result<u64> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("size cannot be empty");
    }

    let split_at = trimmed
        .char_indices()
        .find_map(|(idx, ch)| {
            if ch.is_ascii_digit() || matches!(ch, '.' | '+' | '-' | 'e' | 'E') {
                None
            } else {
                Some(idx)
            }
        })
        .unwrap_or(trimmed.len());

    let (number, unit) = trimmed.split_at(split_at);
    let value: f64 = number
        .parse()
        .with_context(|| format!("invalid numeric size: {number:?}"))?;
    let multiplier = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        other => bail!("unknown size unit: {other}"),
    };
    if !value.is_finite() || value < 0.0 {
        bail!("size must be a non-negative finite number");
    }
    Ok((value * multiplier).round() as u64)
}

/// Parses a duration: a number of seconds, or a number with `ms`, `s` or `m`.
pub(crate) fn parse_duration(input: &str) -> Result<Duration> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("duration cannot be empty");
    }
    if let Some(number) = trimmed.strip_suffix("ms") {
        let millis: f64 = number
            .parse()
            .with_context(|| format!("invalid millisecond duration: {number:?}"))?;
        return duration_from_secs(millis / 1000.0);
    }
    if let Some(number) = trimmed.strip_suffix('s') {
        let seconds: f64 = number
            .parse()
            .with_context(|| format!("invalid second duration: {number:?}"))?;
        return duration_from_secs(seconds);
    }
    if let Some(number) = trimmed.strip_suffix('m') {
        let minutes: f64 = number
            .parse()
            .with_context(|| format!("invalid minute duration: {number:?}"))?;
        return duration_from_secs(minutes * 60.0);
    }
    let seconds: f64 = trimmed
        .parse()
        .with_context(|| format!("invalid duration: {trimmed:?}"))?;
    duration_from_secs(seconds)
}

fn duration_from_secs(seconds: f64) -> Result<Duration> {
    Duration::try_from_secs_f64(seconds)
        .map_err(|_| anyhow!("duration must be a non-negative finite number"))
}

/// Formats a byte count for messages, in GiB, MiB or bytes.
pub(crate) fn format_bytes(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}GiB", bytes as f64 / GIB)
    } else if bytes >= 1024 * 1024 {
        format!("{:.1}MiB", bytes as f64 / MIB)
    } else {
        format!("{bytes}B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("1").unwrap(), 1);
        assert_eq!(parse_size("1K").unwrap(), 1024);
        assert_eq!(parse_size("1.5M").unwrap(), 1_572_864);
        assert_eq!(parse_size("2G").unwrap(), 2_147_483_648);
        assert_eq!(parse_size("1T").unwrap(), 1_099_511_627_776);
        assert_eq!(parse_size("1TB").unwrap(), 1_099_511_627_776);
        assert!(parse_size("12Z").is_err());
        assert!(parse_size("").is_err());
        assert!(parse_size("-1G").is_err());
        assert!(parse_size("NaNG").is_err());
        assert!(parse_size("infG").is_err());
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("2s").unwrap(), Duration::from_secs(2));
        assert_eq!(parse_duration("1m").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration("0.5").unwrap(), Duration::from_millis(500));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("-1").is_err());
        assert!(parse_duration("NaN").is_err());
        assert!(parse_duration("inf").is_err());
        assert!(parse_duration("-1ms").is_err());
        assert!(parse_duration("NaNs").is_err());
        assert!(parse_duration("infm").is_err());
    }
}
