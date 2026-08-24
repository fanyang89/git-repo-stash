use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const MAPPING_FILE: &str = ".git-repo-stash.toml";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    #[serde(default = "mapping_version")]
    version: u32,
    #[serde(default)]
    pub repositories: BTreeMap<String, String>,
}

impl Mapping {
    pub fn load(workdir: &Path) -> Result<Self> {
        let path = path(workdir);
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        let mapping: Self = toml::from_str(&contents)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        if mapping.version != mapping_version() {
            bail!(
                "unsupported mapping version {} in {}",
                mapping.version,
                path.display()
            );
        }
        Ok(mapping)
    }

    pub fn save(&self, workdir: &Path) -> Result<()> {
        let destination = path(workdir);
        let temporary = workdir.join(format!("{MAPPING_FILE}.tmp"));
        let contents =
            toml::to_string_pretty(self).context("failed to serialize repository mapping")?;
        fs::write(&temporary, contents)
            .with_context(|| format!("failed to write {}", temporary.display()))?;
        fs::rename(&temporary, &destination).with_context(|| {
            format!(
                "failed to replace repository mapping {}",
                destination.display()
            )
        })?;
        Ok(())
    }
}

impl Default for Mapping {
    fn default() -> Self {
        Self {
            version: mapping_version(),
            repositories: BTreeMap::new(),
        }
    }
}

fn mapping_version() -> u32 {
    1
}

fn path(workdir: &Path) -> PathBuf {
    workdir.join(MAPPING_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let mut mapping = Mapping::default();
        mapping
            .repositories
            .insert("team/foo".into(), "team--foo".into());

        mapping.save(directory.path()).unwrap();
        let loaded = Mapping::load(directory.path()).unwrap();

        assert_eq!(loaded.repositories, mapping.repositories);
    }
}
