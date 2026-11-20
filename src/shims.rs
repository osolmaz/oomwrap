//! PATH shims and in-place wrappers that run a tool through oomwrap.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

use crate::config::default_shim_tools;
use crate::host::Host;

pub(crate) const PATH_SHIM_MARKER: &str = "oomwrap managed path shim";
pub(crate) const ABSOLUTE_WRAPPER_MARKER: &str = "oomwrap managed absolute wrapper";
/// The mark of a script without policy, which ends in `oomwrap run --shim`.
/// A managed script without it came from an earlier version and passes
/// flags of its own.
pub(crate) const SCRIPT_VERSION_MARKER: &str = "oomwrap script 2";
/// The markers sit at the top of a script; a file is read this far.
const SCRIPT_HEAD_BYTES: u64 = 4096;
/// The file in the state folder that lists the wrapped paths.
const WRAPPED_RECORD: &str = "oomwrap/wrapped-paths";

/// The end of every generated script. It carries no policy: `oomwrap run`
/// reads the policy file, so a later change of flags or defaults never
/// breaks an installed script.
const RUN_TAIL: &str = r#"exec "$oomwrap" run --shim "$tool" -- "$real" "$@"
"#;

/// The two kinds of generated script.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ScriptKind {
    /// A PATH shim that runs the real tool found later in `PATH`.
    PathShim,
    /// A wrapper in place of a file that moved to `<file>.real`.
    Wrapper,
}

pub(crate) fn install_shims(bin_dir: &Path, tools: &[String], force: bool) -> Result<()> {
    fs::create_dir_all(bin_dir)
        .with_context(|| format!("failed to create {}", bin_dir.display()))?;
    let oomwrap_bin = current_oomwrap_bin()?;

    for tool in selected_tools(tools) {
        validate_tool_name(&tool)?;
        let path = bin_dir.join(&tool);
        if path_entry_exists(&path)? && !is_managed_path_shim(&path)? && !force {
            bail!("refusing to replace non-managed file: {}", path.display());
        }
        write_executable(&path, &script(ScriptKind::PathShim, &tool, &oomwrap_bin))?;
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

/// Wraps an executable in place, or writes the wrapper of an earlier
/// `wrap` again.
pub(crate) fn wrap(host: &Host, target: &Path, force: bool) -> Result<()> {
    if !target.exists() {
        bail!("cannot wrap missing path: {}", target.display());
    }
    if is_managed_absolute_wrapper(target)? {
        let oomwrap_bin = current_oomwrap_bin()?;
        let wrapper = script(ScriptKind::Wrapper, &tool_name(target), &oomwrap_bin);
        write_executable(target, &wrapper)?;
        record_wrapped(host, target);
        println!("rewrote wrapper: {}", target.display());
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
    let wrapper = script(ScriptKind::Wrapper, &tool_name(target), &oomwrap_bin);
    if let Err(error) = write_executable(target, &wrapper) {
        let _ = fs::rename(&real, target);
        return Err(error);
    }
    record_wrapped(host, target);
    println!("wrapped executable: {}", target.display());
    Ok(())
}

pub(crate) fn unwrap(host: &Host, path: &Path) -> Result<()> {
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
    forget_wrapped(host, &[path, &target]);
    println!("unwrapped executable: {}", target.display());
    Ok(())
}

/// The name that a wrapper passes as `--shim`: the file name of the
/// wrapped path.
fn tool_name(target: &Path) -> String {
    target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn record_path(host: &Host) -> Result<PathBuf> {
    Ok(host.state_dir()?.join(WRAPPED_RECORD))
}

/// The paths that `wrap` recorded, so `doctor` finds wrappers outside
/// `PATH`.
pub(crate) fn wrapped_paths(host: &Host) -> Vec<PathBuf> {
    record_path(host)
        .and_then(|path| Ok(fs::read_to_string(path)?))
        .map(|text| text.lines().map(PathBuf::from).collect())
        .unwrap_or_default()
}

/// Adds a wrapped path to the record. A failure only warns: the wrapper
/// works, but `doctor` may miss it.
fn record_wrapped(host: &Host, target: &Path) {
    let Ok(path) = std::path::absolute(target) else {
        return;
    };
    let mut paths = wrapped_paths(host);
    if !paths.contains(&path) {
        paths.push(path);
    }
    write_record(host, &paths);
}

fn forget_wrapped(host: &Host, targets: &[&Path]) {
    let forget: Vec<PathBuf> = targets
        .iter()
        .filter_map(|target| std::path::absolute(target).ok())
        .collect();
    let paths: Vec<PathBuf> = wrapped_paths(host)
        .into_iter()
        .filter(|path| !forget.contains(path))
        .collect();
    write_record(host, &paths);
}

fn write_record(host: &Host, paths: &[PathBuf]) {
    let written = record_path(host).and_then(|record| {
        let dir = record.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
        let mut text = String::new();
        for path in paths {
            text.push_str(&path.to_string_lossy());
            text.push('\n');
        }
        write_file(&record, text.as_bytes(), 0o644)
    });
    if let Err(error) = written {
        eprintln!(
            "warning: failed to record the wrapped paths: {error:#}; doctor may miss a wrapper"
        );
    }
}

/// A script that oomwrap made, found by `doctor`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct FoundScript {
    pub(crate) path: PathBuf,
    pub(crate) kind: ScriptKind,
    /// Made by an earlier version: it passes flags of its own.
    pub(crate) outdated: bool,
    /// The command that writes it again.
    pub(crate) fix: String,
}

/// The scripts that oomwrap made in `dirs` and at the recorded paths.
pub(crate) fn find_scripts(dirs: &[PathBuf], wrapped: &[PathBuf]) -> Vec<FoundScript> {
    let shims = dirs
        .iter()
        .flat_map(|dir| dir_entries(dir))
        .filter_map(|path| found_script(&path, ScriptKind::PathShim));
    let wrappers = wrapped
        .iter()
        .filter_map(|path| found_script(path, ScriptKind::Wrapper));
    let mut found: Vec<FoundScript> = shims.chain(wrappers).collect();
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found.dedup_by(|a, b| a.path == b.path);
    found
}

fn dir_entries(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect()
}

fn found_script(path: &Path, kind: ScriptKind) -> Option<FoundScript> {
    let marker = match kind {
        ScriptKind::PathShim => PATH_SHIM_MARKER,
        ScriptKind::Wrapper => ABSOLUTE_WRAPPER_MARKER,
    };
    // A symbolic link is not a script that oomwrap wrote there.
    if !fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file()) {
        return None;
    }
    let head = script_head(path);
    if !head.contains(marker) {
        return None;
    }
    Some(FoundScript {
        path: path.to_path_buf(),
        kind,
        outdated: !head.contains(SCRIPT_VERSION_MARKER),
        fix: fix_command(path, kind),
    })
}

/// The command that writes a script again with this version.
fn fix_command(path: &Path, kind: ScriptKind) -> String {
    match kind {
        ScriptKind::PathShim => format!(
            "oomwrap install-shims --bin-dir {} --tool {}",
            sh_quote(
                &path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .display()
                    .to_string()
            ),
            sh_quote(&tool_name(path))
        ),
        ScriptKind::Wrapper => format!("oomwrap wrap {}", sh_quote(&path.display().to_string())),
    }
}

/// The first bytes of a file, where the markers of a script sit.
fn script_head(path: &Path) -> String {
    let mut head = Vec::new();
    let read =
        fs::File::open(path).and_then(|file| file.take(SCRIPT_HEAD_BYTES).read_to_end(&mut head));
    match read {
        Ok(_) => String::from_utf8_lossy(&head).into_owned(),
        Err(_) => String::new(),
    }
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
    Ok(script_head(path).contains(marker))
}

fn path_entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn write_executable(path: &Path, content: &str) -> Result<()> {
    write_file(path, content.as_bytes(), 0o755)
}

/// Writes a file in one step: a new file next to it, then a rename.
fn write_file(path: &Path, content: &[u8], mode: u32) -> Result<()> {
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
            file.write_all(content)
                .with_context(|| format!("failed to write {}", tmp.display()))?;
            file.sync_all()
                .with_context(|| format!("failed to sync {}", tmp.display()))?;
            drop(file);

            let mut permissions = fs::metadata(&tmp)?.permissions();
            permissions.set_mode(mode);
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

/// The one generator of PATH shims and wrappers. Only the way to find the
/// real program differs; neither kind carries policy.
fn script(kind: ScriptKind, tool: &str, oomwrap_bin: &Path) -> String {
    let (marker, find_real) = match kind {
        ScriptKind::PathShim => (PATH_SHIM_MARKER, path_search(tool)),
        ScriptKind::Wrapper => (
            ABSOLUTE_WRAPPER_MARKER,
            r#"real="$(readlink -f "${BASH_SOURCE[0]}").real"
"#
            .to_string(),
        ),
    };
    format!(
        r#"#!/usr/bin/env bash
# {marker}
# {SCRIPT_VERSION_MARKER}: no policy here; oomwrap run reads the policy file.
set -euo pipefail

tool={tool_q}
default_oomwrap={bin_q}
oomwrap="${{OOMWRAP_BIN:-$default_oomwrap}}"
{find_real}
{RUN_TAIL}"#,
        tool_q = sh_quote(tool),
        bin_q = sh_quote(&oomwrap_bin.display().to_string()),
    )
}

/// Finds the real tool later in `PATH`, past this shim and other shims, or
/// takes it from `OOMWRAP_REAL_<TOOL>`.
fn path_search(tool: &str) -> String {
    let env_name = format!("OOMWRAP_REAL_{}", env_suffix(tool));
    format!(
        r#"real_env={env_q}
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
"#,
        env_q = sh_quote(&env_name),
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
    fn both_kinds_of_script_carry_no_policy_and_end_in_run_shim() {
        let shim = script(ScriptKind::PathShim, "vllm", Path::new("/tmp/oomwrap"));
        assert!(shim.contains(PATH_SHIM_MARKER));
        assert!(shim.contains(SCRIPT_VERSION_MARKER));
        assert!(shim.contains("OOMWRAP_REAL_VLLM"));
        assert!(shim.contains("tool='vllm'"));
        assert!(shim.ends_with(RUN_TAIL));
        let wrapper = script(ScriptKind::Wrapper, "it's", Path::new("/tmp/oomwrap"));
        assert!(wrapper.contains(ABSOLUTE_WRAPPER_MARKER));
        assert!(wrapper.contains("readlink -f"));
        assert!(wrapper.contains(r"tool='it'\''s'"));
        assert!(wrapper.ends_with(RUN_TAIL));
        for text in [&shim, &wrapper] {
            for policy in [
                "min-mem",
                "MIN_MEM",
                "OOMWRAP_PROFILE",
                "MACHINE_GUARD",
                "EVENT_LOG",
            ] {
                assert!(!text.contains(policy), "{policy} in {text}");
            }
        }
    }

    #[test]
    fn doctor_finds_scripts_and_names_the_outdated_ones() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let bin_path = Path::new("/x/oomwrap");
        fs::write(
            bin.join("vllm"),
            script(ScriptKind::PathShim, "vllm", bin_path),
        )
        .unwrap();
        fs::write(
            bin.join("sglang"),
            format!("#!/bin/sh\n# {PATH_SHIM_MARKER}\n"),
        )
        .unwrap();
        fs::write(bin.join("other"), "#!/bin/sh\n").unwrap();
        std::os::unix::fs::symlink(bin.join("vllm"), bin.join("link")).unwrap();
        let wrapped = dir.path().join("tool");
        fs::write(&wrapped, format!("# {ABSOLUTE_WRAPPER_MARKER}\n")).unwrap();
        let found = find_scripts(
            &[bin.clone(), dir.path().join("missing")],
            &[wrapped.clone(), dir.path().join("gone"), bin.join("vllm")],
        );
        let summary: Vec<(String, ScriptKind, bool)> = found
            .iter()
            .map(|script| (tool_name(&script.path), script.kind, script.outdated))
            .collect();
        assert_eq!(
            summary,
            [
                ("sglang".to_string(), ScriptKind::PathShim, true),
                ("vllm".to_string(), ScriptKind::PathShim, false),
                ("tool".to_string(), ScriptKind::Wrapper, true),
            ]
        );
        assert_eq!(
            found[0].fix,
            format!(
                "oomwrap install-shims --bin-dir '{}' --tool 'sglang'",
                bin.display()
            )
        );
        assert_eq!(
            found[2].fix,
            format!("oomwrap wrap '{}'", wrapped.display())
        );
    }

    #[test]
    fn the_record_of_wrapped_paths_adds_and_forgets() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = Host::from_env();
        host.state_home = Some(dir.path().join("state"));
        assert!(wrapped_paths(&host).is_empty());
        let a = dir.path().join("a");
        record_wrapped(&host, &a);
        record_wrapped(&host, &a);
        record_wrapped(&host, &dir.path().join("b"));
        assert_eq!(wrapped_paths(&host), [a.clone(), dir.path().join("b")]);
        forget_wrapped(&host, &[&a]);
        assert_eq!(wrapped_paths(&host), [dir.path().join("b")]);
        // A state folder that cannot be made only warns.
        host.state_home = Some(dir.path().join("b").join("state"));
        fs::write(dir.path().join("b"), "").unwrap();
        record_wrapped(&host, &a);
        assert!(wrapped_paths(&host).is_empty());
    }

    #[test]
    fn real_path_appends_real_suffix() {
        assert_eq!(
            real_path_for(Path::new("/tmp/vllm")),
            PathBuf::from("/tmp/vllm.real")
        );
    }
}
