//! The policy file and its merge with the flags.
//!
//! oomwrap reads `/etc/oomwrap.toml`, then the user's
//! `${XDG_CONFIG_HOME:-~/.config}/oomwrap/config.toml`. A file holds general
//! settings at its top and a `[profile.<name>]` section for each profile.
//! The keys are the flag names without their dashes in front. For each
//! setting, the first of these that sets it wins: a flag, the user file's
//! profile section, the user file's general settings, the system file's
//! profile section, the system file's general settings. A setting that none
//! of them sets takes its default.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Deserializer, Serialize, de};

use super::{BackendRequest, Profile};
use crate::events::EventLog;
use crate::limits::{Pressure, Size, format_duration, parse_duration};

/// The section that holds one setting for each profile.
const PROFILE_KEY: &str = "profile";
/// The list of extra machine-wide guards, by process name.
const MACHINE_GUARDS_KEY: &str = "machine-guards";

/// The settings that a flag or a policy file can give. `None` leaves a
/// setting to the next layer, and in the end to its default.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub(crate) struct Settings {
    #[serde(default, deserialize_with = "parsed")]
    pub(crate) backend: Option<BackendRequest>,
    #[serde(default, deserialize_with = "parsed")]
    pub(crate) memory_max: Option<Size>,
    #[serde(default, deserialize_with = "parsed")]
    pub(crate) memory_high: Option<Size>,
    #[serde(default, deserialize_with = "parsed")]
    pub(crate) memory_swap_max: Option<Size>,
    #[serde(default, deserialize_with = "parsed")]
    pub(crate) min_mem: Option<Size>,
    #[serde(default, deserialize_with = "parsed")]
    pub(crate) domain_pressure: Option<Pressure>,
    #[serde(default, deserialize_with = "duration")]
    pub(crate) domain_pressure_for: Option<Duration>,
    #[serde(default, deserialize_with = "parsed")]
    pub(crate) job_pressure: Option<Pressure>,
    #[serde(default, deserialize_with = "duration")]
    pub(crate) job_pressure_for: Option<Duration>,
    #[serde(default, deserialize_with = "duration")]
    pub(crate) poll: Option<Duration>,
    #[serde(default, deserialize_with = "duration")]
    pub(crate) term_grace: Option<Duration>,
    pub(crate) require_machine_guard: Option<bool>,
    #[serde(default, deserialize_with = "parsed")]
    pub(crate) event_log: Option<EventLog>,
}

impl Settings {
    /// Each setting by its key, with its value as a flag spells it.
    pub(crate) fn entries(&self) -> Vec<(&'static str, Option<String>)> {
        vec![
            ("backend", self.backend.map(|value| value.to_string())),
            ("memory-max", self.memory_max.map(|size| size.to_string())),
            ("memory-high", self.memory_high.map(|size| size.to_string())),
            (
                "memory-swap-max",
                self.memory_swap_max.map(|size| size.to_string()),
            ),
            ("min-mem", self.min_mem.map(|size| size.to_string())),
            (
                "domain-pressure",
                self.domain_pressure.map(|pressure| pressure.to_string()),
            ),
            (
                "domain-pressure-for",
                self.domain_pressure_for.map(format_duration),
            ),
            (
                "job-pressure",
                self.job_pressure.map(|pressure| pressure.to_string()),
            ),
            (
                "job-pressure-for",
                self.job_pressure_for.map(format_duration),
            ),
            ("poll", self.poll.map(format_duration)),
            ("term-grace", self.term_grace.map(format_duration)),
            (
                "require-machine-guard",
                self.require_machine_guard
                    .map(|required| required.to_string()),
            ),
            (
                "event-log",
                self.event_log.as_ref().map(ToString::to_string),
            ),
        ]
    }
}

/// A value written as text, or as a bare number such as `0` or `30`.
#[derive(Deserialize)]
#[serde(untagged)]
enum Text {
    Text(String),
    Number(u64),
}

impl Text {
    fn into_string(self) -> String {
        match self {
            Self::Text(text) => text,
            Self::Number(number) => number.to_string(),
        }
    }
}

fn parsed<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: fmt::Display,
{
    let text = Text::deserialize(deserializer)?.into_string();
    text.parse()
        .map(Some)
        .map_err(|error| de::Error::custom(format!("{error:#}")))
}

fn duration<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Duration>, D::Error> {
    let text = Text::deserialize(deserializer)?.into_string();
    parse_duration(&text)
        .map(Some)
        .map_err(|error| de::Error::custom(format!("{error:#}")))
}

/// One policy file, read and checked.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PolicyFile {
    pub(crate) path: PathBuf,
    general: Settings,
    profiles: Vec<(Profile, Settings)>,
    /// Extra machine-wide guards, by process name.
    pub(crate) machine_guards: Vec<String>,
}

impl PolicyFile {
    /// Reads one policy file; a file that does not exist gives `None`.
    pub(crate) fn read(path: &Path) -> Result<Option<Self>> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read the policy file {}", path.display()));
            }
        };
        Self::parse(path, &text)
            .map(Some)
            .with_context(|| format!("invalid policy file {}", path.display()))
    }

    fn parse(path: &Path, text: &str) -> Result<Self> {
        let mut table: toml::Table = text.parse()?;
        let profiles = match table.remove(PROFILE_KEY) {
            Some(value) => profile_sections(value)?,
            None => Vec::new(),
        };
        let machine_guards = match table.remove(MACHINE_GUARDS_KEY) {
            Some(value) => guard_names(value)?,
            None => Vec::new(),
        };
        let general = toml::Value::Table(table).try_into::<Settings>()?;
        Ok(Self {
            path: path.to_path_buf(),
            general,
            profiles,
            machine_guards,
        })
    }

    fn profile(&self, profile: Profile) -> Option<&Settings> {
        self.profiles
            .iter()
            .find(|(name, _)| *name == profile)
            .map(|(_, settings)| settings)
    }
}

fn profile_sections(value: toml::Value) -> Result<Vec<(Profile, Settings)>> {
    let toml::Value::Table(table) = value else {
        bail!("{PROFILE_KEY} must hold [{PROFILE_KEY}.<name>] sections");
    };
    table
        .into_iter()
        .map(|(name, section)| {
            let profile = profile_named(&name)?;
            let settings = section
                .try_into::<Settings>()
                .with_context(|| format!("[{PROFILE_KEY}.{name}]"))?;
            Ok((profile, settings))
        })
        .collect()
}

fn profile_named(name: &str) -> Result<Profile> {
    match Profile::from_str(name, false) {
        Ok(profile) if profile != Profile::Auto => Ok(profile),
        _ => bail!(
            "unknown profile [{PROFILE_KEY}.{name}]; use one of {}",
            Profile::section_names().join(", ")
        ),
    }
}

fn guard_names(value: toml::Value) -> Result<Vec<String>> {
    let names = value
        .try_into::<Vec<String>>()
        .with_context(|| format!("{MACHINE_GUARDS_KEY} must be a list of process names"))?;
    for name in &names {
        if name.is_empty() || name.contains('/') || name.contains(char::is_whitespace) {
            bail!("{MACHINE_GUARDS_KEY}: {name:?} is not a process name");
        }
    }
    Ok(names)
}

/// Where a setting came from.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub(crate) enum Source {
    Flag,
    File {
        path: PathBuf,
        /// The profile section, or `None` for the general settings.
        #[serde(skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },
    Default,
}

impl fmt::Display for Source {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Flag => formatter.write_str("flag"),
            Self::File {
                path,
                profile: Some(profile),
            } => write!(formatter, "{} [{PROFILE_KEY}.{profile}]", path.display()),
            Self::File {
                path,
                profile: None,
            } => write!(formatter, "{}", path.display()),
            Self::Default => formatter.write_str("default"),
        }
    }
}

/// The policy files that exist, in the order system, then user.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Policy {
    pub(crate) files: Vec<PolicyFile>,
}

/// The merged settings of one run, and where each came from.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Resolved {
    pub(crate) settings: Settings,
    /// The source of each entry of [`Settings::entries`], in its order.
    pub(crate) sources: Vec<Source>,
}

impl Policy {
    pub(crate) fn load(paths: &[PathBuf]) -> Result<Self> {
        let mut files = Vec::new();
        for path in paths {
            files.extend(PolicyFile::read(path)?);
        }
        Ok(Self { files })
    }

    /// The extra machine-wide guards that any file names.
    pub(crate) fn machine_guards(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .files
            .iter()
            .flat_map(|file| file.machine_guards.iter().cloned())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// The layers for `profile`, the first that sets a value wins.
    fn layers<'a>(&'a self, flags: &'a Settings, profile: Profile) -> Vec<(Source, &'a Settings)> {
        let mut layers = vec![(Source::Flag, flags)];
        for file in self.files.iter().rev() {
            if let Some(section) = file.profile(profile) {
                let source = Source::File {
                    path: file.path.clone(),
                    profile: Some(profile.name()),
                };
                layers.push((source, section));
            }
            let source = Source::File {
                path: file.path.clone(),
                profile: None,
            };
            layers.push((source, &file.general));
        }
        layers
    }

    /// Merges the flags with the files for one profile.
    pub(crate) fn resolve(&self, flags: &Settings, profile: Profile) -> Resolved {
        let layers = self.layers(flags, profile);
        let mut settings = Settings::default();
        let mut sources = Vec::new();
        macro_rules! merge {
            ($($field:ident),* $(,)?) => {
                $(
                    let (value, source) = first(&layers, |layer| layer.$field.clone());
                    settings.$field = value;
                    sources.push(source);
                )*
            };
        }
        // The order of `Settings::entries`.
        merge!(
            backend,
            memory_max,
            memory_high,
            memory_swap_max,
            min_mem,
            domain_pressure,
            domain_pressure_for,
            job_pressure,
            job_pressure_for,
            poll,
            term_grace,
            require_machine_guard,
            event_log,
        );
        Resolved { settings, sources }
    }
}

fn first<T>(
    layers: &[(Source, &Settings)],
    get: impl Fn(&Settings) -> Option<T>,
) -> (Option<T>, Source) {
    layers
        .iter()
        .find_map(|(source, layer)| get(layer).map(|value| (Some(value), source.clone())))
        .unwrap_or((None, Source::Default))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(text: &str) -> PolicyFile {
        PolicyFile::parse(Path::new("/etc/oomwrap.toml"), text).unwrap()
    }

    fn error(text: &str) -> String {
        format!(
            "{:#}",
            PolicyFile::parse(Path::new("p.toml"), text).unwrap_err()
        )
    }

    #[test]
    fn a_file_holds_general_settings_profiles_and_guards() {
        let policy = file(
            r#"
min-mem = "2G"
memory-swap-max = 0
poll = 2
term-grace = "500ms"
domain-pressure = "25%"
backend = "poll"
require-machine-guard = false
event-log = "none"
machine-guards = ["myguard"]

[profile.vllm]
memory-max = "80%"
job-pressure-for = "1m"
"#,
        );
        assert_eq!(policy.general.min_mem, Some("2G".parse().unwrap()));
        assert_eq!(policy.general.memory_swap_max, Some(Size::Bytes(0)));
        assert_eq!(policy.general.poll, Some(Duration::from_secs(2)));
        assert_eq!(policy.general.term_grace, Some(Duration::from_millis(500)));
        assert_eq!(policy.general.backend, Some(BackendRequest::Poll));
        assert_eq!(policy.general.require_machine_guard, Some(false));
        assert_eq!(policy.general.event_log, Some(EventLog::Off));
        assert_eq!(policy.machine_guards, ["myguard"]);
        let vllm = policy.profile(Profile::Vllm).unwrap();
        assert_eq!(vllm.memory_max, Some(Size::Percent(80)));
        assert_eq!(vllm.job_pressure_for, Some(Duration::from_secs(60)));
        assert!(policy.profile(Profile::Generic).is_none());
    }

    #[test]
    fn a_bad_file_names_the_key_and_the_section() {
        assert!(error("min-swap = \"1G\"").contains("unknown field `min-swap`"));
        assert!(error("min-mem = \"1GB\"").contains("unknown size unit"));
        assert!(error("[profile.vllm]\nmin-mem = \"x\"").contains("[profile.vllm]"));
        assert!(error("[profile.auto]\n").contains("unknown profile [profile.auto]"));
        assert!(error("[profile.nope]\n").contains("use one of vllm, llama-cpp"));
        assert!(error("profile = 1").contains("[profile.<name>] sections"));
        assert!(error("machine-guards = \"x\"").contains("list of process names"));
        assert!(error("machine-guards = [\"a/b\"]").contains("is not a process name"));
        assert!(error("machine-guards = [\"\"]").contains("is not a process name"));
        assert!(error("machine-guards = [\"a b\"]").contains("is not a process name"));
        assert!(error("poll = \"-1s\"").contains("non-negative"));
        assert!(error("min-mem =").contains("TOML"));
    }

    #[test]
    fn a_missing_file_is_no_policy_and_an_unreadable_one_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            PolicyFile::read(&dir.path().join("none.toml")).unwrap(),
            None
        );
        assert!(PolicyFile::read(dir.path()).is_err());
        let path = dir.path().join("bad.toml");
        fs::write(&path, "min-mem = 1.5").unwrap();
        let message = format!("{:#}", PolicyFile::read(&path).unwrap_err());
        assert!(message.starts_with("invalid policy file "), "{message}");
        let good = dir.path().join("good.toml");
        fs::write(&good, "min-mem = \"1G\"").unwrap();
        let policy = Policy::load(&[dir.path().join("none.toml"), good.clone()]).unwrap();
        assert_eq!(policy.files.len(), 1);
        assert_eq!(policy.files[0].path, good);
    }

    #[test]
    fn the_closest_setting_wins() {
        let system = PolicyFile {
            path: PathBuf::from("/etc/oomwrap.toml"),
            ..file(
                "min-mem = \"1G\"\npoll = \"3s\"\nmachine-guards = [\"b\", \"a\"]\n[profile.vllm]\nmin-mem = \"8G\"\nterm-grace = \"20s\"",
            )
        };
        let user = PolicyFile {
            path: PathBuf::from("/home/u/.config/oomwrap/config.toml"),
            ..file(
                "min-mem = \"2G\"\nmachine-guards = [\"a\"]\n[profile.vllm]\nterm-grace = \"30s\"",
            )
        };
        let policy = Policy {
            files: vec![system, user],
        };
        assert_eq!(policy.machine_guards(), ["a", "b"]);
        let flags = Settings {
            poll: Some(Duration::from_secs(5)),
            ..Settings::default()
        };
        let vllm = policy.resolve(&flags, Profile::Vllm);
        // The user's general setting comes before the system's profile section.
        assert_eq!(vllm.settings.min_mem, Some(Size::Bytes(2 << 30)));
        assert_eq!(vllm.settings.term_grace, Some(Duration::from_secs(30)));
        assert_eq!(vllm.settings.poll, Some(Duration::from_secs(5)));
        assert_eq!(vllm.settings.backend, None);
        let sources: Vec<String> = vllm.sources.iter().map(ToString::to_string).collect();
        let keys: Vec<&str> = vllm
            .settings
            .entries()
            .iter()
            .map(|(key, _)| *key)
            .collect();
        let source_of = |key: &str| sources[keys.iter().position(|k| *k == key).unwrap()].clone();
        assert_eq!(source_of("min-mem"), "/home/u/.config/oomwrap/config.toml");
        assert_eq!(
            source_of("term-grace"),
            "/home/u/.config/oomwrap/config.toml [profile.vllm]"
        );
        assert_eq!(source_of("poll"), "flag");
        assert_eq!(source_of("backend"), "default");
        assert_eq!(sources.len(), keys.len());
        let generic = Policy {
            files: vec![policy.files[0].clone()],
        }
        .resolve(&Settings::default(), Profile::Generic);
        assert_eq!(generic.settings.min_mem, Some(Size::Bytes(1 << 30)));
        assert_eq!(generic.settings.poll, Some(Duration::from_secs(3)));
        let json = serde_json::to_value(&generic.sources[4]).unwrap();
        assert_eq!(json["kind"], "file");
        assert!(json.get("profile").is_none());
    }

    #[test]
    fn every_setting_has_an_entry() {
        let settings = file(
            r#"
backend = "cgroup"
memory-max = "1G"
memory-high = "900M"
memory-swap-max = "infinity"
min-mem = "15%"
domain-pressure = "20%"
domain-pressure-for = "10s"
job-pressure = "60%"
job-pressure-for = "30s"
poll = "1s"
term-grace = "1500ms"
require-machine-guard = true
event-log = "e.jsonl"
"#,
        )
        .general;
        let entries = settings.entries();
        assert_eq!(entries.len(), 13);
        assert!(
            entries.iter().all(|(_, value)| value.is_some()),
            "{entries:?}"
        );
        assert_eq!(entries[0], ("backend", Some("cgroup".to_string())));
        assert_eq!(entries[10], ("term-grace", Some("1500ms".to_string())));
        assert_eq!(entries[12], ("event-log", Some("e.jsonl".to_string())));
    }
}
