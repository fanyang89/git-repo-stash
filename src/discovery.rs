use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env,
    ffi::OsStr,
    path::{Component, Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

const MAX_REPOSITORY_NAME_CHARS: usize = 100;

#[derive(Debug, Clone)]
pub struct Repository {
    pub path: PathBuf,
    pub relative_path: String,
    pub base_name: String,
    pub is_bare: bool,
    pub object_format: String,
}

#[derive(Debug, Default)]
pub struct Discovery {
    pub repositories: Vec<Repository>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct NamedRepository {
    pub repository: Repository,
    pub remote_name: String,
    pub is_mapped: bool,
}

pub fn discover(workdir: &Path) -> Discovery {
    let mut result = Discovery::default();
    let mut common_directories = HashSet::new();
    let mut entries = WalkDir::new(workdir)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter();

    while let Some(entry) = entries.next() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                result.errors.push(error.to_string());
                continue;
            }
        };
        if !entry.file_type().is_dir() {
            continue;
        }
        let candidate = entry.path();
        if candidate.file_name() == Some(OsStr::new(".git")) {
            entries.skip_current_dir();
            continue;
        }

        let has_dot_git = candidate.join(".git").exists();
        let looks_bare = candidate.join("HEAD").is_file()
            && candidate.join("objects").is_dir()
            && (candidate.join("refs").is_dir() || candidate.join("packed-refs").is_file());
        if !has_dot_git && !looks_bare {
            continue;
        }

        entries.skip_current_dir();
        match inspect_repository(workdir, candidate) {
            Ok((repository, common_directory)) => {
                if common_directories.insert(common_directory) {
                    result.repositories.push(repository);
                }
            }
            Err(error) => result
                .errors
                .push(format!("{}: {error:#}", candidate.display())),
        }
    }

    result
        .repositories
        .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    result
}

pub fn assign_names(
    repositories: Vec<Repository>,
    mappings: &BTreeMap<String, String>,
) -> Result<Vec<NamedRepository>> {
    validate_mapping_names(mappings)?;
    let reserved_names = mappings
        .values()
        .map(|name| canonical_name(name))
        .collect::<HashSet<_>>();
    let mut base_counts = HashMap::new();
    for repository in &repositories {
        *base_counts
            .entry(canonical_name(&repository.base_name))
            .or_insert(0usize) += 1;
    }

    let mut named = repositories
        .into_iter()
        .map(|repository| {
            if let Some(remote_name) = mappings.get(&repository.relative_path) {
                return Ok(NamedRepository {
                    repository,
                    remote_name: remote_name.clone(),
                    is_mapped: true,
                });
            }

            let duplicate = base_counts[&canonical_name(&repository.base_name)] > 1;
            let mut remote_name = if duplicate {
                encoded_relative_name(&repository)
            } else {
                generated_name(&repository.base_name, &repository.relative_path)
            };
            if reserved_names.contains(&canonical_name(&remote_name)) {
                remote_name =
                    unique_hashed_name(&remote_name, &repository.relative_path, &reserved_names)?;
            }
            Ok(NamedRepository {
                repository,
                remote_name,
                is_mapped: false,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    resolve_generated_collisions(&mut named, &reserved_names)?;
    Ok(named)
}

fn inspect_repository(workdir: &Path, candidate: &Path) -> Result<(Repository, PathBuf)> {
    let output = clean_git()
        .args(["-C"])
        .arg(candidate)
        .args([
            "rev-parse",
            "--is-bare-repository",
            "--git-common-dir",
            "--show-object-format",
        ])
        .output()
        .with_context(|| format!("failed to inspect {}", candidate.display()))?;
    if !output.status.success() {
        bail!("not a valid Git repository");
    }
    let stdout = String::from_utf8(output.stdout).context("Git returned a non-UTF-8 path")?;
    let mut lines = stdout.lines();
    let is_bare = lines.next() == Some("true");
    let common_directory = lines
        .next()
        .ok_or_else(|| anyhow!("Git did not return a common directory"))?;
    let object_format = lines
        .next()
        .ok_or_else(|| anyhow!("Git did not return an object format"))?;
    if !matches!(object_format, "sha1" | "sha256") {
        bail!("unsupported Git object format: {object_format}");
    }
    let common_directory = if Path::new(common_directory).is_absolute() {
        PathBuf::from(common_directory)
    } else {
        candidate.join(common_directory)
    };
    let common_directory = common_directory.canonicalize().with_context(|| {
        format!(
            "failed to resolve Git directory for {}",
            candidate.display()
        )
    })?;

    let relative = candidate
        .strip_prefix(workdir)
        .context("repository is outside the workdir")?;
    let relative_path = relative_path_string(relative)?;
    let file_name = candidate
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| anyhow!("repository directory name is not valid UTF-8"))?;
    let base_name = strip_dot_git(file_name).to_owned();
    if base_name.is_empty() {
        bail!("repository name is empty after removing .git");
    }

    Ok((
        Repository {
            path: candidate.to_owned(),
            relative_path,
            base_name,
            is_bare,
            object_format: object_format.to_owned(),
        },
        common_directory,
    ))
}

fn clean_git() -> Command {
    let mut command = Command::new("git");
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
        "GIT_SHALLOW_FILE",
        "GIT_QUARANTINE_PATH",
        "GIT_REPLACE_REF_BASE",
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    ] {
        command.env_remove(name);
    }
    for (name, _) in env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_CONFIG_") {
            command.env_remove(name);
        }
    }
    command
}

fn resolve_generated_collisions(
    repositories: &mut [NamedRepository],
    reserved_names: &HashSet<String>,
) -> Result<()> {
    for _ in 0..2 {
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, repository) in repositories.iter().enumerate() {
            by_name
                .entry(canonical_name(&repository.remote_name))
                .or_default()
                .push(index);
        }

        let mut changed = false;
        for indexes in by_name.values().filter(|indexes| indexes.len() > 1) {
            if indexes.iter().all(|index| repositories[*index].is_mapped) {
                let paths = indexes
                    .iter()
                    .map(|index| repositories[*index].repository.relative_path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!("multiple mapped repositories target the same remote: {paths}");
            }
            for index in indexes {
                if !repositories[*index].is_mapped {
                    let repository = &mut repositories[*index];
                    repository.remote_name = unique_hashed_name(
                        &repository.remote_name,
                        &repository.repository.relative_path,
                        reserved_names,
                    )?;
                    changed = true;
                }
            }
        }
        if !changed {
            return Ok(());
        }
    }
    bail!("could not generate unique remote repository names")
}

fn unique_hashed_name(
    name: &str,
    relative_path: &str,
    reserved_names: &HashSet<String>,
) -> Result<String> {
    for attempt in 0..100 {
        let identity = if attempt == 0 {
            relative_path.to_owned()
        } else {
            format!("{relative_path}#{attempt}")
        };
        let candidate = with_path_hash(name, &identity);
        if !reserved_names.contains(&canonical_name(&candidate)) {
            return Ok(candidate);
        }
    }
    bail!("could not generate an unused remote name for {relative_path}")
}

fn encoded_relative_name(repository: &Repository) -> String {
    let without_suffix = strip_dot_git(&repository.relative_path);
    let encoded = without_suffix
        .split('/')
        .map(slug_component)
        .collect::<Vec<_>>()
        .join("--");
    generated_name(&encoded, &repository.relative_path)
}

fn slug_component(component: &str) -> String {
    let mut result = String::new();
    let mut previous_separator = false;
    let mut previous_dot = false;
    for character in component.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
            result.push(character);
            previous_separator = false;
            previous_dot = false;
        } else if character == '.' {
            if previous_dot {
                result.push('-');
            }
            result.push(character);
            previous_separator = false;
            previous_dot = true;
        } else if !previous_separator {
            result.push('-');
            previous_separator = true;
            previous_dot = false;
        }
    }
    let result = result.trim_matches('-');
    if result.is_empty() {
        "repo".to_owned()
    } else {
        result.to_owned()
    }
}

pub fn generated_name(name: &str, identity: &str) -> String {
    let name = slug_component(strip_dot_git(name));
    let lower = name.to_ascii_lowercase();
    let reserved = matches!(lower.as_str(), "." | ".." | "-")
        || [".wiki", ".git", ".rss", ".atom"]
            .iter()
            .any(|suffix| lower.ends_with(suffix));
    if !reserved && name.chars().count() <= MAX_REPOSITORY_NAME_CHARS {
        return name;
    }
    with_path_hash(&name, identity)
}

fn with_path_hash(name: &str, relative_path: &str) -> String {
    let digest = Sha256::digest(relative_path.as_bytes());
    let suffix = format!(
        "--{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    );
    let keep = MAX_REPOSITORY_NAME_CHARS - suffix.chars().count();
    let prefix = name.chars().take(keep).collect::<String>();
    format!("{prefix}{suffix}")
}

fn relative_path_string(path: &Path) -> Result<String> {
    if path.as_os_str().is_empty() {
        return Ok(".".to_owned());
    }
    path.components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("repository path is not valid UTF-8")),
            _ => bail!("repository path is not relative"),
        })
        .collect::<Result<Vec<_>>>()
        .map(|components| components.join("/"))
}

fn canonical_name(name: &str) -> String {
    strip_dot_git(name).to_lowercase()
}

fn validate_mapping_names(mappings: &BTreeMap<String, String>) -> Result<()> {
    let mut remote_paths: HashMap<String, &str> = HashMap::new();
    for (path, remote_name) in mappings {
        if generated_name(remote_name, path) != *remote_name {
            bail!("mapped repository name is not valid for Gitea: {remote_name}");
        }
        if let Some(previous_path) = remote_paths.insert(canonical_name(remote_name), path) {
            bail!("mapping paths {previous_path} and {path} target the same remote {remote_name}");
        }
    }
    Ok(())
}

fn strip_dot_git(name: &str) -> &str {
    let Some(suffix) = name.get(name.len().saturating_sub(4)..) else {
        return name;
    };
    if !suffix.eq_ignore_ascii_case(".git") {
        return name;
    }
    name.get(..name.len() - 4).unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn repository(relative_path: &str) -> Repository {
        Repository {
            path: PathBuf::from(relative_path),
            relative_path: relative_path.into(),
            base_name: strip_dot_git(relative_path.rsplit('/').next().unwrap()).into(),
            is_bare: false,
            object_format: "sha1".into(),
        }
    }

    #[test]
    fn duplicate_names_use_relative_paths() {
        let repositories = vec![repository("team-a/foo"), repository("team-b/Foo.git")];

        let named = assign_names(repositories, &BTreeMap::new()).unwrap();

        assert_eq!(named[0].remote_name, "team-a--foo");
        assert_eq!(named[1].remote_name, "team-b--Foo");
    }

    #[test]
    fn existing_mapping_remains_stable_when_duplicate_is_added() {
        let repositories = vec![repository("old/foo"), repository("new/foo")];
        let mappings = BTreeMap::from([("old/foo".into(), "foo".into())]);

        let named = assign_names(repositories, &mappings).unwrap();

        let names = named
            .into_iter()
            .map(|repository| (repository.repository.relative_path, repository.remote_name))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(names["new/foo"], "new--foo");
        assert_eq!(names["old/foo"], "foo");
    }

    #[test]
    fn discovers_normal_and_bare_repositories_without_descending_into_them() {
        let directory = tempfile::tempdir().unwrap();
        let normal = directory.path().join("normal");
        let bare = directory.path().join("archive.git");
        fs::create_dir(&normal).unwrap();
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(&normal)
            .status()
            .unwrap();
        Command::new("git")
            .args(["init", "--quiet", "--bare"])
            .arg(&bare)
            .status()
            .unwrap();

        let discovered = discover(directory.path());

        assert!(discovered.errors.is_empty(), "{:?}", discovered.errors);
        assert_eq!(
            discovered
                .repositories
                .iter()
                .map(|repository| repository.relative_path.as_str())
                .collect::<Vec<_>>(),
            ["archive.git", "normal"]
        );
        assert!(discovered.repositories[0].is_bare);
    }

    #[test]
    fn generated_names_follow_gitea_rules() {
        assert_eq!(generated_name("my repo", "my repo"), "my-repo");
        assert_eq!(generated_name("项目", "项目"), "repo");
        assert!(generated_name("archive.wiki", "archive.wiki").starts_with("archive.wiki--"));
        assert!(!generated_name("a..b", "a..b").contains(".."));
    }

    #[test]
    fn stale_mappings_reserve_remote_names() {
        let repositories = vec![repository("new/foo")];
        let mappings = BTreeMap::from([("missing/foo".into(), "foo".into())]);

        let named = assign_names(repositories, &mappings).unwrap();

        assert!(named[0].remote_name.starts_with("foo--"));
    }

    #[test]
    fn generated_hashes_also_avoid_stale_mappings() {
        let repositories = vec![repository("new/foo")];
        let first_hash = with_path_hash("foo", "new/foo");
        let mappings = BTreeMap::from([
            ("missing/foo".into(), "foo".into()),
            ("missing/hashed".into(), first_hash.clone()),
        ]);

        let named = assign_names(repositories, &mappings).unwrap();

        assert_ne!(named[0].remote_name, "foo");
        assert_ne!(named[0].remote_name, first_hash);
    }
}
