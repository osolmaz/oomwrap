//! PATH shims and in-place wrappers that run a tool through oomwrap.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::config::default_shim_tools;
use crate::host::Host;

pub(crate) const PATH_SHIM_MARKER: &str = "oomwrap managed path shim";
pub(crate) const ABSOLUTE_WRAPPER_MARKER: &str = "oomwrap managed absolute wrapper";

/// The policy that a generated script passes to `oomwrap run`. It stays text:
/// the script quotes it as data, and `oomwrap run` parses it.
pub(crate) struct ScriptFloors<'a> {
    /// `None` leaves the floor to the default of `oomwrap run`.
    pub(crate) min_mem: Option<&'a str>,
}

/// The end of every generated script: the flags of `oomwrap run` and the exec.
const RUN_TAIL: &str = r#"args=(run --profile "$profile")
if [[ -n "$min_mem" ]]; then
  args+=(--min-mem "$min_mem")
fi
if [[ "${OOMWRAP_ALLOW_NO_MACHINE_GUARD:-}" == "1" ]]; then
  args+=(--allow-no-machine-guard)
fi
if [[ -n "${OOMWRAP_EVENT_LOG:-}" ]]; then
  args+=(--event-log "$OOMWRAP_EVENT_LOG")
fi

exec "$oomwrap" "${args[@]}" -- "$real" "$@"
"#;

pub(crate) fn install_shims(
    bin_dir: &Path,
    tools: &[String],
    floors: &ScriptFloors<'_>,
    force: bool,
) -> Result<()> {
    fs::create_dir_all(bin_dir)
        .with_context(|| format!("failed to create {}", bin_dir.display()))?;
    let oomwrap_bin = current_oomwrap_bin()?;

    for tool in selected_tools(tools) {
        validate_tool_name(&tool)?;
        let path = bin_dir.join(&tool);
        if path_entry_exists(&path)? && !is_managed_path_shim(&path)? && !force {
            bail!("refusing to replace non-managed file: {}", path.display());
        }
        write_executable(&path, &path_shim_script(&tool, &oomwrap_bin, floors))?;
        println!("installed shim: {}", path.display());
    }
    Ok(())
}

pub(crate) fn uninstall_shims(bin_dir: &Path, tools: &[String]) -> Result<()> {
    for tool in selected_tools(tools) {
        validate_tool_name(&tool)?;
        let path = bin_dir.join(&tool);
        if is_managed_path_shim(&path)? {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
            println!("removed shim: {}", path.display());
        }
    }
    Ok(())
}

pub(crate) fn wrap(target: &Path, floors: &ScriptFloors<'_>, force: bool) -> Result<()> {
    if !target.exists() {
        bail!("cannot wrap missing path: {}", target.display());
    }
    if is_managed_absolute_wrapper(target)? {
        println!("already wrapped: {}", target.display());
        return Ok(());
    }
    if is_managed_path_shim(target)? {
        bail!("cannot wrap PATH shim: {}", target.display());
    }
    if !target.is_file() {
        bail!("cannot wrap non-file path: {}", target.display());
    }
    if !is_executable(target)? {
        bail!("cannot wrap non-executable path: {}", target.display());
    }

    let real = real_path_for(target);
    if path_entry_exists(&real)? && !force {
        bail!(
            "refusing to overwrite existing real path: {}",
            real.display()
        );
    }

    fs::rename(target, &real)
        .with_context(|| format!("failed to move {} to {}", target.display(), real.display()))?;
    let oomwrap_bin = current_oomwrap_bin()?;
    if let Err(error) = write_executable(target, &absolute_wrapper_script(&oomwrap_bin, floors)) {
        let _ = fs::rename(&real, target);
        return Err(error);
    }
    println!("wrapped executable: {}", target.display());
    Ok(())
}

pub(crate) fn unwrap(path: &Path) -> Result<()> {
    let target =
        fs::canonicalize(path).with_context(|| format!("failed to resolve {}", path.display()))?;
    let real = real_path_for(&target);
    if !is_managed_absolute_wrapper(&target)? {
        bail!("not an oomwrap managed wrapper: {}", target.display());
    }
    if !path_entry_exists(&real)? {
        bail!("missing real executable: {}", real.display());
    }
    fs::remove_file(&target).with_context(|| format!("failed to remove {}", target.display()))?;
    fs::rename(&real, &target)
        .with_context(|| format!("failed to restore {}", target.display()))?;
    println!("unwrapped executable: {}", target.display());
    Ok(())
}

pub(crate) fn selected_tools(tools: &[String]) -> Vec<String> {
    if tools.is_empty() {
        return default_shim_tools();
    }
    tools.to_vec()
}

/// Accepts only a name that is one plain path component, written as it is.
pub(crate) fn validate_tool_name(tool: &str) -> Result<()> {
    let mut components = Path::new(tool).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) if name == tool => Ok(()),
        _ => bail!("tool name must be a bare executable name: {tool}"),
    }
}

pub(crate) fn default_bin_dir(host: &Host) -> Result<PathBuf> {
    Ok(host.home()?.join(".local/bin"))
}

pub(crate) fn current_oomwrap_bin() -> Result<PathBuf> {
    env::current_exe().context("failed to resolve current oomwrap binary")
}

fn is_executable(path: &Path) -> Result<bool> {
    Ok(path.metadata()?.permissions().mode() & 0o111 != 0)
}

// Thin filesystem adapter; path edge cases include equivalent mutations for
// non-files because read_to_string also returns false-like behavior.
#[cfg_attr(test, mutants::skip)]
pub(crate) fn is_managed_path_shim(path: &Path) -> Result<bool> {
    file_contains_marker(path, PATH_SHIM_MARKER)
}

pub(crate) fn is_managed_absolute_wrapper(path: &Path) -> Result<bool> {
    file_contains_marker(path, ABSOLUTE_WRAPPER_MARKER)
}

fn file_contains_marker(path: &Path, marker: &str) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let content = fs::read_to_string(path).unwrap_or_default();
    Ok(content.contains(marker))
}

fn path_entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn write_executable(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("missing file name for {}", path.display()))?
        .to_string_lossy();

    for attempt in 0..100 {
        let tmp = parent.join(format!(
            ".{file_name}.oomwrap-tmp-{}-{attempt}",
            std::process::id()
        ));
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to create {}", tmp.display()));
            }
        };

        let result = (|| -> Result<()> {
            file.write_all(content.as_bytes())
                .with_context(|| format!("failed to write {}", tmp.display()))?;
            file.sync_all()
                .with_context(|| format!("failed to sync {}", tmp.display()))?;
            drop(file);

            let mut permissions = fs::metadata(&tmp)?.permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&tmp, permissions)?;
            fs::rename(&tmp, path).with_context(|| {
                format!(
                    "failed to replace {} with generated executable",
                    path.display()
                )
            })?;
            Ok(())
        })();

        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        return result;
    }

    bail!(
        "failed to create temporary executable next to {}",
        path.display()
    )
}

pub(crate) fn real_path_for(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".real");
    PathBuf::from(os)
}

fn path_shim_script(tool: &str, oomwrap_bin: &Path, floors: &ScriptFloors<'_>) -> String {
    let env_name = format!("OOMWRAP_REAL_{}", env_suffix(tool));
    format!(
        r#"#!/usr/bin/env bash
# {PATH_SHIM_MARKER}
set -euo pipefail

tool={tool_q}
real_env={env_q}
default_oomwrap={bin_q}
default_min_mem={min_mem_q}
oomwrap="${{OOMWRAP_BIN:-$default_oomwrap}}"
min_mem="${{OOMWRAP_MIN_MEM:-$default_min_mem}}"
profile="${{OOMWRAP_PROFILE:-auto}}"
real="${{!real_env:-}}"

if [[ -z "$real" ]]; then
  shim_dir="$(cd "$(dirname "${{BASH_SOURCE[0]}}")" && pwd)"
  IFS=: read -r -a path_parts <<< "$PATH"
  for dir in "${{path_parts[@]}}"; do
    [[ -z "$dir" ]] && dir="."
    [[ "$dir" == "$shim_dir" ]] && continue
    candidate="$dir/$tool"
    [[ -x "$candidate" && ! -d "$candidate" ]] || continue
    if grep -q "{PATH_SHIM_MARKER}" "$candidate" 2>/dev/null; then
      continue
    fi
    real="$candidate"
    break
  done
fi

if [[ -z "$real" ]]; then
  echo "oomwrap: could not resolve real executable for $tool" >&2
  echo "set $real_env=/path/to/$tool or put the real binary later in PATH" >&2
  exit 127
fi

{RUN_TAIL}"#,
        tool_q = sh_quote(tool),
        env_q = sh_quote(&env_name),
        bin_q = sh_quote(&oomwrap_bin.display().to_string()),
        min_mem_q = sh_quote(floors.min_mem.unwrap_or_default()),
    )
}

fn absolute_wrapper_script(oomwrap_bin: &Path, floors: &ScriptFloors<'_>) -> String {
    format!(
        r#"#!/usr/bin/env bash
# {ABSOLUTE_WRAPPER_MARKER}
set -euo pipefail

default_oomwrap={bin_q}
default_min_mem={min_mem_q}
oomwrap="${{OOMWRAP_BIN:-$default_oomwrap}}"
min_mem="${{OOMWRAP_MIN_MEM:-$default_min_mem}}"
profile="${{OOMWRAP_PROFILE:-auto}}"
real="$(readlink -f "${{BASH_SOURCE[0]}}").real"

{RUN_TAIL}"#,
        bin_q = sh_quote(&oomwrap_bin.display().to_string()),
        min_mem_q = sh_quote(floors.min_mem.unwrap_or_default()),
    )
}

fn env_suffix(tool: &str) -> String {
    tool.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::host::{Host, disk_available};

    const FLOORS: ScriptFloors<'static> = ScriptFloors {
        min_mem: Some("24G"),
    };

    #[test]
    fn misc_helpers_cover_boundaries() {
        let host = Host::from_env();
        assert_eq!(selected_tools(&[]), default_shim_tools());
        assert_eq!(selected_tools(&["custom".to_string()]), vec!["custom"]);
        assert!(validate_tool_name("vllm").is_ok());
        assert!(validate_tool_name("llama-server").is_ok());
        assert!(validate_tool_name("").is_err());
        assert!(validate_tool_name("../vllm").is_err());
        assert!(validate_tool_name("/tmp/vllm").is_err());
        assert!(validate_tool_name(".").is_err());
        assert!(validate_tool_name("vllm/").is_err());
        assert!(validate_tool_name("..").is_err());
        assert!(validate_tool_name("/").is_err());
        assert!(validate_tool_name("./vllm").is_err());
        assert!(disk_available(Path::new(".")).unwrap() > 1024 * 1024 * 1024);
        let default_bin = default_bin_dir(&host).unwrap();
        assert!(default_bin.ends_with(".local/bin"));
        assert!(default_bin.starts_with(host.home().unwrap()));

        let dir = tempfile::tempdir().unwrap();
        assert!(!is_managed_path_shim(&dir.path().join("missing")).unwrap());
        assert!(!is_managed_path_shim(dir.path()).unwrap());
        let shim = dir.path().join("shim");
        fs::write(&shim, format!("# {PATH_SHIM_MARKER}\n")).unwrap();
        assert!(is_managed_path_shim(&shim).unwrap());
        assert!(!is_managed_absolute_wrapper(&shim).unwrap());

        let wrapper = dir.path().join("wrapper");
        fs::write(&wrapper, format!("# {ABSOLUTE_WRAPPER_MARKER}\n")).unwrap();
        assert!(is_managed_absolute_wrapper(&wrapper).unwrap());
        assert!(!is_managed_path_shim(&wrapper).unwrap());
    }

    #[test]
    fn path_entry_errors_other_than_missing_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, "").unwrap();
        assert!(path_entry_exists(&file).unwrap());
        assert!(!path_entry_exists(&dir.path().join("missing")).unwrap());
        // A path below a regular file fails with ENOTDIR, not with NotFound.
        assert!(path_entry_exists(&file.join("child")).is_err());
    }

    #[test]
    fn write_executable_skips_a_taken_temporary_name() {
        let dir = tempfile::tempdir().unwrap();
        let taken = dir
            .path()
            .join(format!(".tool.oomwrap-tmp-{}-0", std::process::id()));
        fs::write(&taken, "other").unwrap();
        let path = dir.path().join("tool");
        write_executable(&path, "#!/bin/sh\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "#!/bin/sh\n");
        assert!(is_executable(&path).unwrap());
        assert_eq!(fs::read_to_string(&taken).unwrap(), "other");
    }

    #[test]
    fn write_executable_reports_a_missing_folder_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let error = write_executable(&dir.path().join("missing/tool"), "").unwrap_err();
        let cause = error.root_cause().downcast_ref::<io::Error>();
        assert_eq!(cause.map(io::Error::kind), Some(io::ErrorKind::NotFound));
        assert!(format!("{error}").contains("failed to create"));
    }

    #[test]
    fn generated_path_shim_has_marker_and_env_escape() {
        let script = path_shim_script("vllm", Path::new("/tmp/oomwrap"), &FLOORS);
        assert!(script.contains(PATH_SHIM_MARKER));
        assert!(script.contains("oomwrap managed"));
        assert!(script.contains("OOMWRAP_REAL_VLLM"));
        assert!(script.contains("OOMWRAP_ALLOW_NO_MACHINE_GUARD"));
        assert!(script.contains("default_min_mem='24G'"));
        assert!(script.ends_with(RUN_TAIL));
        let without_floor = path_shim_script(
            "vllm",
            Path::new("/tmp/oomwrap"),
            &ScriptFloors { min_mem: None },
        );
        assert!(without_floor.contains("default_min_mem=''"));
    }

    #[test]
    fn generated_absolute_wrapper_has_marker_and_resolves_real_target() {
        let script = absolute_wrapper_script(Path::new("/tmp/oomwrap"), &FLOORS);
        assert!(script.contains(ABSOLUTE_WRAPPER_MARKER));
        assert!(script.contains("readlink -f"));
        assert!(script.contains(".real"));
        assert!(script.contains("default_min_mem='24G'"));
        assert!(script.ends_with(RUN_TAIL));
    }

    #[test]
    fn real_path_appends_real_suffix() {
        assert_eq!(
            real_path_for(Path::new("/tmp/vllm")),
            PathBuf::from("/tmp/vllm.real")
        );
    }
}
