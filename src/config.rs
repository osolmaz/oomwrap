//! The profile table: which commands belong to which profile, and the
//! policy that each profile carries.

use std::path::Path;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

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
    pub(crate) requires_earlyoom: bool,
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
        requires_earlyoom: true,
    },
    ProfileSpec {
        profile: Profile::LlamaCpp,
        executables: &[shim("llama-server"), shim("llama-cli"), shim("llama-bench")],
        python_modules: &[],
        path_markers: &[],
        requires_earlyoom: true,
    },
    ProfileSpec {
        profile: Profile::Sglang,
        executables: &[shim("sglang")],
        python_modules: &["sglang"],
        path_markers: &[],
        requires_earlyoom: true,
    },
    ProfileSpec {
        profile: Profile::Trtllm,
        executables: &[shim("trtllm-serve")],
        python_modules: &[],
        path_markers: &["TensorRT-LLM"],
        requires_earlyoom: true,
    },
    ProfileSpec {
        profile: Profile::Tgi,
        executables: &[shim("text-generation-launcher")],
        python_modules: &[],
        path_markers: &[],
        requires_earlyoom: true,
    },
];

/// Memory-heavy programs that belong to no profile; `doctor` lists them too.
pub(crate) const OTHER_HEAVY_PROCESSES: &[&str] = &["ollama", "lmstudio", "torchrun"];

impl Profile {
    pub(crate) fn requires_earlyoom(self) -> bool {
        PROFILES
            .iter()
            .any(|spec| spec.profile == self && spec.requires_earlyoom)
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

pub(crate) fn effective_profile(profile: Profile, command: &[String]) -> Profile {
    if profile != Profile::Auto {
        return profile;
    }
    detect_profile(command).unwrap_or(Profile::Generic)
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
        assert!(Profile::Vllm.requires_earlyoom());
        assert!(Profile::LlamaCpp.requires_earlyoom());
        assert!(Profile::Sglang.requires_earlyoom());
        assert!(Profile::Trtllm.requires_earlyoom());
        assert!(Profile::Tgi.requires_earlyoom());
        assert!(!Profile::Generic.requires_earlyoom());
        assert_eq!(
            effective_profile(Profile::Generic, &["vllm".into()]),
            Profile::Generic
        );
        assert_eq!(
            effective_profile(Profile::Auto, &["vllm".into()]),
            Profile::Vllm
        );
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
