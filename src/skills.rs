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

pub(crate) fn handle_if_requested() -> Option<i32> {
    let argv_os: Vec<_> = env::args_os().collect();
    if !argv_os.iter().any(|arg| arg == OsStr::new("--skill")) {
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
