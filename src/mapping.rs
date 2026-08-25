use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

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
        let contents =
            toml::to_string_pretty(self).context("failed to serialize repository mapping")?;
        let mut temporary = NamedTempFile::new_in(workdir).with_context(|| {
            format!("failed to create a temporary file in {}", workdir.display())
        })?;
        temporary
            .write_all(contents.as_bytes())
            .context("failed to write repository mapping")?;
        temporary
            .as_file()
            .sync_all()
            .context("failed to sync repository mapping")?;
        temporary
            .persist(&destination)
            .map_err(|error| error.error)
            .with_context(|| {
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

    #[cfg(unix)]
    #[test]
    fn mapping_save_replaces_symlink_without_following_it() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        fs::write(&target, "do not overwrite\n").unwrap();
        symlink(&target, directory.path().join(MAPPING_FILE)).unwrap();
        let mapping = Mapping::default();

        mapping.save(directory.path()).unwrap();

        assert_eq!(fs::read_to_string(target).unwrap(), "do not overwrite\n");
        assert!(
            !fs::symlink_metadata(directory.path().join(MAPPING_FILE))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
