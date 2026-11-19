//! Sizes, pressures and durations of the guard policy, parsed once at the
//! border, and their resolution against the memory domain. Pure: no IO.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const KIB: u64 = 1024;
const PERCENT_SCALE: u64 = 100;

/// A memory size as written: bytes with an optional binary unit (`512M`,
/// `24G`), a whole-number share of the memory domain's size (`85%`), or
/// `infinity`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Size {
    Bytes(u64),
    /// A share of the domain's size, in whole percent from 0 to 100.
    Percent(u8),
    Infinity,
}

impl Size {
    /// The size in bytes for a domain of `domain_bytes`; `None` for `infinity`.
    pub(crate) fn resolve(self, domain_bytes: u64) -> Option<u64> {
        match self {
            Self::Bytes(bytes) => Some(bytes),
            Self::Percent(percent) => Some(share_of(domain_bytes, percent)),
            Self::Infinity => None,
        }
    }
}

fn share_of(domain_bytes: u64, percent: u8) -> u64 {
    let share = u128::from(domain_bytes) * u128::from(percent) / u128::from(PERCENT_SCALE);
    // A share of at most 100% never exceeds the domain size, which is a u64.
    u64::try_from(share).unwrap_or(domain_bytes)
}

impl FromStr for Size {
    type Err = anyhow::Error;

    fn from_str(input: &str) -> Result<Self> {
        let text = input.trim();
        if text == "infinity" {
            return Ok(Self::Infinity);
        }
        if let Some(number) = text.strip_suffix('%') {
            let percent: u8 = number
                .parse()
                .with_context(|| format!("invalid percentage: {text:?}"))?;
            if u64::from(percent) > PERCENT_SCALE {
                bail!("a share cannot exceed 100%: {text:?}");
            }
            return Ok(Self::Percent(percent));
        }
        let (number, multiplier) = match text.char_indices().last() {
            Some((index, unit)) if unit.is_ascii_alphabetic() => {
                (&text[..index], unit_multiplier(unit, text)?)
            }
            _ => (text, 1),
        };
        let value: u64 = number.parse().with_context(|| {
            format!("invalid size: {text:?}; write a whole number with K, M, G or T, a share such as 85%, or infinity")
        })?;
        value
            .checked_mul(multiplier)
            .map(Self::Bytes)
            .ok_or_else(|| anyhow!("size is too large: {text:?}"))
    }
}

/// The binary units that systemd reads: `K`, `M`, `G` and `T`.
fn unit_multiplier(unit: char, text: &str) -> Result<u64> {
    let exponent = match unit {
        'K' => 1,
        'M' => 2,
        'G' => 3,
        'T' => 4,
        _ => bail!("unknown size unit in {text:?}; use K, M, G or T (binary units)"),
    };
    Ok(KIB.pow(exponent))
}

impl fmt::Display for Size {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bytes(bytes) => formatter.write_str(&format_size_text(*bytes)),
            Self::Percent(percent) => write!(formatter, "{percent}%"),
            Self::Infinity => formatter.write_str("infinity"),
        }
    }
}

/// Writes a byte count in the largest binary unit that divides it exactly.
fn format_size_text(bytes: u64) -> String {
    for (unit, exponent) in [("T", 4), ("G", 3), ("M", 2), ("K", 1)] {
        let multiplier = KIB.pow(exponent);
        if bytes != 0 && bytes.is_multiple_of(multiplier) {
            return format!("{}{unit}", bytes / multiplier);
        }
    }
    bytes.to_string()
}

/// A default size with absolute bounds: a share alone is too small on a
/// small board and too large on a large server.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BoundedSize {
    pub(crate) size: Size,
    pub(crate) at_least_bytes: u64,
    pub(crate) at_most_bytes: u64,
}

impl BoundedSize {
    pub(crate) fn resolve(self, domain_bytes: u64) -> Option<u64> {
        self.size
            .resolve(domain_bytes)
            .map(|bytes| bytes.clamp(self.at_least_bytes, self.at_most_bytes))
    }
}

impl fmt::Display for BoundedSize {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}, at least {}, at most {}",
            self.size,
            format_size_text(self.at_least_bytes),
            format_size_text(self.at_most_bytes)
        )
    }
}

/// A size that the user gave, or the default with its bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SizeSetting {
    Given(Size),
    Default(BoundedSize),
}

impl SizeSetting {
    pub(crate) fn resolve(self, domain_bytes: u64) -> Option<u64> {
        match self {
            Self::Given(size) => size.resolve(domain_bytes),
            Self::Default(bounded) => bounded.resolve(domain_bytes),
        }
    }
}

impl fmt::Display for SizeSetting {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Given(size) => size.fmt(formatter),
            Self::Default(bounded) => bounded.fmt(formatter),
        }
    }
}

/// A share of time that tasks stalled on memory, as PSI reports it, in
/// hundredths of a percent: `12.34%` is 1234.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Pressure(u32);

impl Pressure {
    const FULL: u32 = 100 * 100;

    pub(crate) fn from_hundredths(hundredths: u32) -> Option<Self> {
        (hundredths <= Self::FULL).then_some(Self(hundredths))
    }

    #[cfg(test)]
    pub(crate) fn hundredths(self) -> u32 {
        self.0
    }

    /// Parses a decimal percentage with at most two decimals, such as the
    /// `avg10=12.34` of a PSI file.
    pub(crate) fn parse_decimal(text: &str) -> Result<Self> {
        let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
        if fraction.len() > 2 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
            bail!("invalid pressure: {text:?}");
        }
        let whole: u32 = whole
            .parse()
            .with_context(|| format!("invalid pressure: {text:?}"))?;
        let fraction: u32 = format!("{fraction:0<2}").parse().unwrap_or(0);
        whole
            .checked_mul(100)
            .and_then(|hundredths| hundredths.checked_add(fraction))
            .and_then(Self::from_hundredths)
            .ok_or_else(|| anyhow!("pressure above 100%: {text:?}"))
    }
}

impl FromStr for Pressure {
    type Err = anyhow::Error;

    /// Parses a flag value such as `20%`.
    fn from_str(input: &str) -> Result<Self> {
        let text = input.trim();
        let number = text
            .strip_suffix('%')
            .ok_or_else(|| anyhow!("write a pressure as a percentage, such as 20%: {text:?}"))?;
        Self::parse_decimal(number)
    }
}

impl fmt::Display for Pressure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{:02}%", self.0 / 100, self.0 % 100)
    }
}

impl Serialize for Pressure {
    /// Serializes as the percentage that PSI prints, such as `12.34`.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_f64(f64::from(self.0) / 100.0)
    }
}

impl<'de> Deserialize<'de> for Pressure {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let percent = f64::deserialize(deserializer)?;
        let hundredths = (percent * 100.0).round();
        if !(0.0..=f64::from(Self::FULL)).contains(&hundredths) {
            return Err(serde::de::Error::custom(format!(
                "pressure out of range: {percent}"
            )));
        }
        // The range check above keeps the value inside u32.
        Ok(Self(hundredths as u32))
    }
}

/// A pressure stop condition: PSI `full avg10` at or above `at_least` for
/// at least `lasting`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PressureLimit {
    pub(crate) at_least: Pressure,
    pub(crate) lasting: Duration,
}

/// The default floor for the domain's free room: 15%, at least 256M, at most 16G.
pub(crate) const DEFAULT_MIN_MEM: BoundedSize = BoundedSize {
    size: Size::Percent(15),
    at_least_bytes: 256 * KIB * KIB,
    at_most_bytes: 16 * KIB * KIB * KIB,
};

/// The published defaults of `systemd-oomd` for one unit: 60% for 30 s.
pub(crate) const DEFAULT_JOB_PRESSURE: PressureLimit = PressureLimit {
    at_least: Pressure(60 * 100),
    lasting: Duration::from_secs(30),
};

/// Lower and shorter than the job's, because 60% for 30 s at the root of a
/// machine is already a frozen desktop. Provisional until the machine
/// pressure traces of phase 6.
pub(crate) const DEFAULT_DOMAIN_PRESSURE: PressureLimit = PressureLimit {
    at_least: Pressure(20 * 100),
    lasting: Duration::from_secs(10),
};

/// The stop policy of one run, before it meets a domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GuardPolicy {
    pub(crate) min_mem: SizeSetting,
    pub(crate) job_pressure: PressureLimit,
    pub(crate) domain_pressure: PressureLimit,
}

impl Default for GuardPolicy {
    fn default() -> Self {
        Self {
            min_mem: SizeSetting::Default(DEFAULT_MIN_MEM),
            job_pressure: DEFAULT_JOB_PRESSURE,
            domain_pressure: DEFAULT_DOMAIN_PRESSURE,
        }
    }
}

/// The resolved stop conditions of one run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StopLimits {
    /// The floor for the domain's free room (`--min-mem`), in bytes.
    pub(crate) min_free_bytes: u64,
    pub(crate) job_pressure: PressureLimit,
    pub(crate) domain_pressure: PressureLimit,
}

impl GuardPolicy {
    /// Resolves the shares against a domain of `domain_bytes` and checks
    /// that the floor leaves room for a job.
    pub(crate) fn resolve(&self, domain_bytes: u64) -> Result<StopLimits> {
        let Some(min_free_bytes) = self.min_mem.resolve(domain_bytes) else {
            bail!("--min-mem cannot be infinity");
        };
        if min_free_bytes >= domain_bytes {
            bail!(
                "--min-mem {} ({}) is not below the memory domain's size {}",
                self.min_mem,
                format_bytes(min_free_bytes),
                format_bytes(domain_bytes)
            );
        }
        Ok(StopLimits {
            min_free_bytes,
            job_pressure: self.job_pressure,
            domain_pressure: self.domain_pressure,
        })
    }
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

    const GIB: u64 = 1024 * 1024 * 1024;

    fn size(text: &str) -> Size {
        text.parse().unwrap()
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(size("1"), Size::Bytes(1));
        assert_eq!(size("1K"), Size::Bytes(1024));
        assert_eq!(size("512M"), Size::Bytes(512 * 1024 * 1024));
        assert_eq!(size("2G"), Size::Bytes(2 * GIB));
        assert_eq!(size("1T"), Size::Bytes(1024 * GIB));
        assert_eq!(size(" 85% "), Size::Percent(85));
        assert_eq!(size("0%"), Size::Percent(0));
        assert_eq!(size("100%"), Size::Percent(100));
        assert_eq!(size("infinity"), Size::Infinity);
        for bad in [
            "",
            "1TB",
            "1GB",
            "1GiB",
            "1g",
            "1.5M",
            "-1G",
            "12Z",
            "101%",
            "1.5%",
            "%",
            "G",
            "NaNG",
            "inf",
            "20000000T",
        ] {
            assert!(bad.parse::<Size>().is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn formats_sizes_as_written() {
        for text in ["512M", "24G", "1T", "85%", "infinity", "1000", "1536K"] {
            assert_eq!(size(text).to_string(), text);
        }
        assert_eq!(Size::Bytes(0).to_string(), "0");
    }

    #[test]
    fn resolves_shares_against_the_domain() {
        assert_eq!(size("85%").resolve(1000), Some(850));
        assert_eq!(size("100%").resolve(u64::MAX), Some(u64::MAX));
        assert_eq!(size("15%").resolve(16 * GIB), Some(16 * GIB * 15 / 100));
        assert_eq!(size("2G").resolve(1), Some(2 * GIB));
        assert_eq!(size("infinity").resolve(1), None);
    }

    #[test]
    fn bounds_clamp_a_default_share() {
        let floor = BoundedSize {
            size: Size::Percent(15),
            at_least_bytes: 256 * 1024 * 1024,
            at_most_bytes: 16 * GIB,
        };
        assert_eq!(floor.resolve(GIB), Some(256 * 1024 * 1024));
        assert_eq!(floor.resolve(64 * GIB), Some(64 * GIB * 15 / 100));
        assert_eq!(floor.resolve(2048 * GIB), Some(16 * GIB));
        assert_eq!(floor.to_string(), "15%, at least 256M, at most 16G");
        let setting = SizeSetting::Default(floor);
        assert_eq!(setting.resolve(GIB), Some(256 * 1024 * 1024));
        assert_eq!(setting.to_string(), floor.to_string());
        let given = SizeSetting::Given(Size::Bytes(5));
        assert_eq!(given.resolve(GIB), Some(5));
        assert_eq!(given.to_string(), "5");
    }

    #[test]
    fn the_policy_resolves_against_the_domain() {
        let policy = GuardPolicy::default();
        let limits = policy.resolve(16 * GIB).unwrap();
        assert_eq!(limits.min_free_bytes, 16 * GIB * 15 / 100);
        assert_eq!(limits.job_pressure, DEFAULT_JOB_PRESSURE);
        assert_eq!(limits.domain_pressure, DEFAULT_DOMAIN_PRESSURE);
        assert_eq!(DEFAULT_JOB_PRESSURE.at_least.hundredths(), 6000);
        assert_eq!(DEFAULT_DOMAIN_PRESSURE.at_least.hundredths(), 2000);
        // The bounds keep a floor on a small board and cap it on a large server.
        assert_eq!(
            policy.resolve(GIB).unwrap().min_free_bytes,
            256 * 1024 * 1024
        );
        assert_eq!(policy.resolve(2048 * GIB).unwrap().min_free_bytes, 16 * GIB);
        // A floor at or above the domain's size leaves no room.
        assert!(policy.resolve(256 * 1024 * 1024).is_err());
        let given = |size: &str| GuardPolicy {
            min_mem: SizeSetting::Given(size.parse().unwrap()),
            ..GuardPolicy::default()
        };
        assert!(given("100%").resolve(GIB).is_err());
        assert!(given("infinity").resolve(GIB).is_err());
        assert_eq!(given("99%").resolve(100).unwrap().min_free_bytes, 99);
        assert_eq!(given("1G").resolve(GIB + 1).unwrap().min_free_bytes, GIB);
    }

    #[test]
    fn parses_pressures() {
        assert_eq!("20%".parse::<Pressure>().unwrap().hundredths(), 2000);
        assert_eq!("12.5%".parse::<Pressure>().unwrap().hundredths(), 1250);
        assert_eq!(Pressure::parse_decimal("0.07").unwrap().hundredths(), 7);
        assert_eq!(
            Pressure::parse_decimal("100.00").unwrap().hundredths(),
            10000
        );
        for bad in ["20", "-1%", "100.01%", "1.234%", "a%", "1.a%"] {
            assert!(bad.parse::<Pressure>().is_err(), "{bad:?} must be refused");
        }
        assert_eq!(
            Pressure::from_hundredths(1234).unwrap().to_string(),
            "12.34%"
        );
        assert_eq!(Pressure::from_hundredths(10001), None);
    }

    #[test]
    fn pressures_serialize_as_psi_percentages() {
        let pressure = Pressure::from_hundredths(1234).unwrap();
        assert_eq!(serde_json::to_string(&pressure).unwrap(), "12.34");
        let back: Pressure = serde_json::from_str("12.34").unwrap();
        assert_eq!(back, pressure);
        assert!(serde_json::from_str::<Pressure>("100.5").is_err());
        assert!(serde_json::from_str::<Pressure>("-1").is_err());
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

    #[test]
    fn formats_bytes_for_messages() {
        assert_eq!(format_bytes(7), "7B");
        assert_eq!(format_bytes(4096), "4096B");
        assert_eq!(format_bytes(1024 * 1024), "1.0MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0MiB");
        assert_eq!(format_bytes(GIB), "1.0GiB");
        assert_eq!(format_bytes(3 * GIB), "3.0GiB");
    }
}
