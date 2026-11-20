//! The profile table: which commands belong to which profile, and the
//! policy that each profile carries.

mod file;

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use anyhow::bail;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

pub(crate) use file::{Policy, Settings, Source};

/// The backend that `--backend` or the policy file asks for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BackendRequest {
    /// `cgroup` when its checks pass, `poll` otherwise.
    Auto,
    Cgroup,
    Poll,
}

impl FromStr for BackendRequest {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> anyhow::Result<Self> {
        match text {
            "auto" => Ok(Self::Auto),
            "cgroup" => Ok(Self::Cgroup),
            "poll" => Ok(Self::Poll),
            _ => bail!("unknown backend {text:?}; use auto, cgroup or poll"),
        }
    }
}

impl fmt::Display for BackendRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Auto => "auto",
            Self::Cgroup => "cgroup",
            Self::Poll => "poll",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, ValueEnum)]
pub(crate) enum Profile {
    Auto,
    Vllm,
    LlamaCpp,
    Sglang,
    Trtllm,
    Tgi,
    Generic,
}

/// One executable name of a profile.
pub(crate) struct Executable {
    pub(crate) name: &'static str,
    /// `install-shims` makes a PATH shim for it when no tool is named.
    pub(crate) default_shim: bool,
}

/// What oomwrap knows about one inference engine.
pub(crate) struct ProfileSpec {
    pub(crate) profile: Profile,
    pub(crate) executables: &'static [Executable],
    /// Modules that `python -m <module>` runs.
    pub(crate) python_modules: &'static [&'static str],
    /// Text that marks the engine anywhere in the program path.
    pub(crate) path_markers: &'static [&'static str],
    /// A large model load can also cause driver-level allocation pressure,
    /// so the profile refuses to launch without a machine-wide guard.
    pub(crate) requires_machine_guard: bool,
}

const fn shim(name: &'static str) -> Executable {
    Executable {
        name,
        default_shim: true,
    }
}

const fn no_shim(name: &'static str) -> Executable {
    Executable {
        name,
        default_shim: false,
    }
}

/// The inference profiles, in the order that detection tries them.
pub(crate) const PROFILES: &[ProfileSpec] = &[
    ProfileSpec {
        profile: Profile::Vllm,
        executables: &[shim("vllm"), no_shim("api_server"), no_shim("gpu_worker")],
        python_modules: &["vllm"],
        path_markers: &[],
        requires_machine_guard: true,
    },
    ProfileSpec {
        profile: Profile::LlamaCpp,
        executables: &[shim("llama-server"), shim("llama-cli"), shim("llama-bench")],
        python_modules: &[],
        path_markers: &[],
        requires_machine_guard: true,
    },
    ProfileSpec {
        profile: Profile::Sglang,
        executables: &[shim("sglang")],
        python_modules: &["sglang"],
        path_markers: &[],
        requires_machine_guard: true,
    },
    ProfileSpec {
        profile: Profile::Trtllm,
        executables: &[shim("trtllm-serve")],
        python_modules: &[],
        path_markers: &["TensorRT-LLM"],
        requires_machine_guard: true,
    },
    ProfileSpec {
        profile: Profile::Tgi,
        executables: &[shim("text-generation-launcher")],
        python_modules: &[],
        path_markers: &[],
        requires_machine_guard: true,
    },
];

/// Memory-heavy programs that belong to no profile; `doctor` lists them too.
pub(crate) const OTHER_HEAVY_PROCESSES: &[&str] = &["ollama", "lmstudio", "torchrun"];

impl Profile {
    /// The name as `--profile` spells it.
    pub(crate) fn name(self) -> String {
        self.to_possible_value()
            .map(|value| value.get_name().to_string())
            .unwrap_or_default()
    }

    /// The names of `[profile.<name>]` sections: every profile but `auto`.
    pub(crate) fn section_names() -> Vec<String> {
        Self::value_variants()
            .iter()
            .filter(|profile| **profile != Self::Auto)
            .map(|profile| profile.name())
            .collect()
    }

    pub(crate) fn requires_machine_guard(self) -> bool {
        PROFILES
            .iter()
            .any(|spec| spec.profile == self && spec.requires_machine_guard)
    }
}

/// The executables that get a PATH shim by default.
pub(crate) fn default_shim_tools() -> Vec<String> {
    PROFILES
        .iter()
        .flat_map(|spec| spec.executables)
        .filter(|executable| executable.default_shim)
        .map(|executable| executable.name.to_string())
        .collect()
}

/// Every name that marks an engine process in a command line.
pub(crate) fn engine_process_names() -> impl Iterator<Item = &'static str> {
    PROFILES
        .iter()
        .flat_map(|spec| {
            spec.executables
                .iter()
                .map(|executable| executable.name)
                .chain(spec.python_modules.iter().copied())
        })
        .chain(OTHER_HEAVY_PROCESSES.iter().copied())
}

/// The profile of a job: the one that `--profile` names, or the one that
/// the command shows, or the one that the shim's tool name shows.
pub(crate) fn effective_profile(
    profile: Profile,
    command: &[String],
    shim: Option<&str>,
) -> Profile {
    if profile != Profile::Auto {
        return profile;
    }
    detect_profile(command)
        .or_else(|| shim.and_then(|tool| detect_profile(&[tool.to_string()])))
        .unwrap_or(Profile::Generic)
}

pub(crate) fn detect_profile(command: &[String]) -> Option<Profile> {
    let first = command.first()?;
    let name = Path::new(first)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(first);
    let name = name.strip_suffix(".real").unwrap_or(name);
    let python_module = is_python_executable(name);

    PROFILES
        .iter()
        .find(|spec| {
            spec.executables
                .iter()
                .any(|executable| executable.name == name)
                || (python_module
                    && spec
                        .python_modules
                        .iter()
                        .any(|module| command_runs_module(command, module)))
                || spec
                    .path_markers
                    .iter()
                    .any(|marker| first.contains(marker))
        })
        .map(|spec| spec.profile)
}

fn is_python_executable(name: &str) -> bool {
    if name == "python" {
        return true;
    }
    let Some(suffix) = name.strip_prefix("python") else {
        return false;
    };
    !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
}

fn command_runs_module(command: &[String], module: &str) -> bool {
    command.windows(2).any(|window| {
        window[0] == "-m"
            && (window[1] == module
                || window[1]
                    .strip_prefix(module)
                    .is_some_and(|suffix| suffix.starts_with('.')))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_profiles() {
        assert_eq!(detect_profile(&["vllm".into()]), Some(Profile::Vllm));
        assert_eq!(detect_profile(&["api_server".into()]), Some(Profile::Vllm));
        assert_eq!(detect_profile(&["gpu_worker".into()]), Some(Profile::Vllm));
        assert_eq!(
            detect_profile(&["python".into(), "-m".into(), "vllm".into()]),
            Some(Profile::Vllm)
        );
        assert_eq!(
            detect_profile(&["python3.11".into(), "-m".into(), "vllm".into()]),
            Some(Profile::Vllm)
        );
        assert_eq!(
            detect_profile(&[
                "python3".into(),
                "-m".into(),
                "vllm.entrypoints.openai.api_server".into()
            ]),
            Some(Profile::Vllm)
        );
        assert_eq!(
            detect_profile(&["/tmp/vllm.real".into()]),
            Some(Profile::Vllm)
        );
        assert_eq!(
            detect_profile(&["/usr/bin/llama-server".into()]),
            Some(Profile::LlamaCpp)
        );
        assert_eq!(
            detect_profile(&["llama-cli".into()]),
            Some(Profile::LlamaCpp)
        );
        assert_eq!(
            detect_profile(&["llama-bench".into()]),
            Some(Profile::LlamaCpp)
        );
        assert_eq!(detect_profile(&["sglang".into()]), Some(Profile::Sglang));
        assert_eq!(
            detect_profile(&["python3".into(), "-m".into(), "sglang".into()]),
            Some(Profile::Sglang)
        );
        assert_eq!(
            detect_profile(&["python3".into(), "-m".into(), "sglang.launch_server".into()]),
            Some(Profile::Sglang)
        );
        assert_eq!(
            detect_profile(&["python3".into(), "-m".into(), "other".into()]),
            None
        );
        assert_eq!(
            detect_profile(&["python3".into(), "script.py".into(), "vllm".into()]),
            None
        );
        assert_eq!(
            detect_profile(&["trtllm-serve".into()]),
            Some(Profile::Trtllm)
        );
        assert_eq!(
            detect_profile(&["/opt/TensorRT-LLM/server".into()]),
            Some(Profile::Trtllm)
        );
        assert_eq!(
            detect_profile(&["text-generation-launcher".into()]),
            Some(Profile::Tgi)
        );
        assert_eq!(detect_profile(&["echo".into()]), None);
    }

    #[test]
    fn python_executables_are_python_with_a_version() {
        for name in ["python", "python3", "python3.12"] {
            assert!(is_python_executable(name), "{name} is Python");
        }
        for name in ["node", "python3x", "pythonw", "py"] {
            assert!(!is_python_executable(name), "{name} is not Python");
        }
        assert_eq!(
            detect_profile(&["node".into(), "-m".into(), "vllm".into()]),
            None
        );
    }

    #[test]
    fn profile_requirements_are_conservative() {
        assert!(Profile::Vllm.requires_machine_guard());
        assert!(Profile::LlamaCpp.requires_machine_guard());
        assert!(Profile::Sglang.requires_machine_guard());
        assert!(Profile::Trtllm.requires_machine_guard());
        assert!(Profile::Tgi.requires_machine_guard());
        assert!(!Profile::Generic.requires_machine_guard());
        assert_eq!(Profile::LlamaCpp.name(), "llama-cpp");
        assert_eq!(Profile::Vllm.name(), "vllm");
        assert_eq!(
            effective_profile(Profile::Generic, &["vllm".into()], None),
            Profile::Generic
        );
        assert_eq!(
            effective_profile(Profile::Auto, &["vllm".into()], None),
            Profile::Vllm
        );
        // A wrapper runs `<path>.real`; the shim's tool name tells the rest.
        let wrapped = ["/opt/run.real".to_string()];
        assert_eq!(
            effective_profile(Profile::Auto, &wrapped, Some("sglang")),
            Profile::Sglang
        );
        assert_eq!(
            effective_profile(Profile::Auto, &wrapped, Some("tool")),
            Profile::Generic
        );
        assert_eq!(
            effective_profile(Profile::Auto, &["vllm".into()], Some("sglang")),
            Profile::Vllm
        );
        assert_eq!(Profile::section_names()[0], "vllm");
        assert!(!Profile::section_names().contains(&"auto".to_string()));
        assert_eq!(
            "cgroup".parse::<BackendRequest>().unwrap(),
            BackendRequest::Cgroup
        );
        assert_eq!(BackendRequest::Auto.to_string(), "auto");
        assert_eq!(BackendRequest::Poll.to_string(), "poll");
        assert!("x".parse::<BackendRequest>().is_err());
    }

    #[test]
    fn table_lists_default_shims_and_engine_names() {
        assert_eq!(
            default_shim_tools(),
            [
                "vllm",
                "llama-server",
                "llama-cli",
                "llama-bench",
                "sglang",
                "trtllm-serve",
                "text-generation-launcher",
            ]
        );
        let names: Vec<_> = engine_process_names().collect();
        for name in ["vllm", "api_server", "gpu_worker", "llama-server", "ollama"] {
            assert!(names.contains(&name), "{name} missing");
        }
    }
}
