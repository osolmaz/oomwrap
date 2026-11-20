use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use skillflag::{Options, maybe_handle_skillflag};
use tempfile::TempDir;

const SKILL_ID: &str = "memory-safe-launch";
const EMBEDDED_FILES: [(&str, &[u8]); 3] = [
    (
        "SKILL.md",
        include_bytes!("../.agents/skills/memory-safe-launch/SKILL.md"),
    ),
    (
        "agents/openai.yaml",
        include_bytes!("../.agents/skills/memory-safe-launch/agents/openai.yaml"),
    ),
    (
        "references/unified-memory-inference-recovery.md",
        include_bytes!(
            "../.agents/skills/memory-safe-launch/references/unified-memory-inference-recovery.md"
        ),
    ),
];

/// Serves the embedded skill for `oomwrap --skill ...`. Only a first argument
/// of `--skill` asks for it: a `--skill` in a guarded command is the
/// command's own argument.
pub(crate) fn handle_if_requested() -> Option<i32> {
    let argv_os: Vec<_> = env::args_os().collect();
    if !asks_for_skill(&argv_os) {
        return None;
    }

    let argv: Vec<String> = argv_os
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();

    match EmbeddedSkills::materialize() {
        Ok(skills) => {
            let options = Options {
                skills_roots: vec![skills.root().to_path_buf()],
                include_bundled_skill: false,
                ..Options::default()
            };
            maybe_handle_skillflag(&argv, &options)
        }
        Err(error) => {
            eprintln!("error: failed to prepare embedded skills: {error:#}");
            Some(2)
        }
    }
}

fn asks_for_skill(argv: &[std::ffi::OsString]) -> bool {
    argv.get(1).is_some_and(|arg| arg == OsStr::new("--skill"))
}

struct EmbeddedSkills {
    _temp_dir: TempDir,
    root: PathBuf,
}

impl EmbeddedSkills {
    fn materialize() -> Result<Self> {
        let temp_dir = tempfile::tempdir().context("create temporary skill directory")?;
        let root = temp_dir.path().join(".agents/skills");
        let skill_dir = root.join(SKILL_ID);

        for (relative_path, contents) in EMBEDDED_FILES {
            let destination = skill_dir.join(relative_path);
            let parent = destination
                .parent()
                .context("embedded skill file has no parent directory")?;
            fs::create_dir_all(parent)
                .with_context(|| format!("create embedded skill directory {}", parent.display()))?;
            fs::write(&destination, contents)
                .with_context(|| format!("write embedded skill file {}", destination.display()))?;
        }

        Ok(Self {
            _temp_dir: temp_dir,
            root,
        })
    }

    fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_first_skill_flag_asks_for_the_skill() {
        let argv = |args: &[&str]| -> Vec<std::ffi::OsString> {
            args.iter().map(std::ffi::OsString::from).collect()
        };
        assert!(asks_for_skill(&argv(&["oomwrap", "--skill", "list"])));
        assert!(!asks_for_skill(&argv(&[
            "oomwrap", "run", "--", "tool", "--skill"
        ])));
        assert!(!asks_for_skill(&argv(&["oomwrap"])));
    }
}
