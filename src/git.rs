use std::{
    collections::BTreeMap,
    env,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
};

use anyhow::{Context, Result, bail};

use crate::discovery::Repository;

pub const ASKPASS_MODE_ENV: &str = "GIT_REPO_STASH_ASKPASS_MODE";
pub const ASKPASS_TOKEN_ENV: &str = "GIT_REPO_STASH_ASKPASS_TOKEN";
pub const ASKPASS_USERNAME_ENV: &str = "GIT_REPO_STASH_ASKPASS_USERNAME";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PushMode {
    BranchesAndTags,
    Mirror,
}

pub struct HttpsAuth<'a> {
    pub username: &'a str,
    pub token: &'a str,
}

#[derive(Debug)]
pub struct RepositoryState {
    pub dirty: bool,
    pub detached_unreferenced: bool,
    has_lfs_objects: bool,
    has_heads: bool,
    has_tags: bool,
    all_refs: Vec<String>,
    ref_values: BTreeMap<String, String>,
    lfs_roots: Vec<String>,
    has_non_commit_refs: bool,
}

#[derive(Debug)]
pub struct AdoptionGuard {
    expectations: Vec<(String, Option<String>)>,
    mirror_refspecs: Option<Vec<String>>,
}

impl RepositoryState {
    pub fn has_refs_for(&self, mode: PushMode) -> bool {
        match mode {
            PushMode::BranchesAndTags => self.has_heads || self.has_tags,
            PushMode::Mirror => !self.all_refs.is_empty(),
        }
    }
}

pub fn inspect_repository(repository: &Repository) -> Result<RepositoryState> {
    let dirty = is_dirty(repository)?;
    let detached_unreferenced = has_unreferenced_detached_head(repository)?;
    let ref_values = local_ref_values(repository, PushMode::Mirror)?;
    let all_refs = ref_values.keys().cloned().collect::<Vec<_>>();
    let (lfs_roots, has_non_commit_refs) =
        lfs_roots(repository, ref_values.keys().map(String::as_str))?;
    let lfs_available = command_succeeds(clean_git().args(["lfs", "version"]));
    let has_lfs_objects = if lfs_available {
        has_lfs_objects(repository)?
    } else if might_use_lfs(repository)? {
        bail!("repository uses Git LFS, but git-lfs is not installed")
    } else {
        false
    };
    if local_ref_values(repository, PushMode::Mirror)? != ref_values {
        bail!("repository refs changed while Git LFS objects were being inspected")
    }
    Ok(RepositoryState {
        dirty,
        detached_unreferenced,
        has_lfs_objects,
        has_heads: all_refs.iter().any(|name| name.starts_with("refs/heads/")),
        has_tags: all_refs.iter().any(|name| name.starts_with("refs/tags/")),
        all_refs,
        ref_values,
        lfs_roots,
        has_non_commit_refs,
    })
}

pub fn verify_safe_to_delete(repository: &Repository) -> Result<()> {
    if repository.is_bare {
        bail!("bare repositories are not deleted automatically")
    }
    if repository.path.join(".git").is_file() {
        bail!("linked worktrees and submodule working trees are not deleted automatically")
    }
    verify_repository_identity(repository)?;
    if has_unreferenced_detached_head(repository)? {
        bail!("detached HEAD commit is unreachable from refs and was not backed up")
    }
    verify_single_worktree(repository)?;
    verify_no_nested_repositories(repository)?;
    verify_no_filesystem_boundaries(repository)?;

    let status = git(repository)
        .args([
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignored=matching",
        ])
        .output()
        .context("failed to inspect files before deletion")?;
    ensure_success(status.status, "inspect files before deletion")?;
    if !status.stdout.is_empty() {
        bail!("working tree contains modified, untracked, or ignored files")
    }

    let index = git(repository)
        .args(["ls-files", "--stage"])
        .output()
        .context("failed to inspect submodules before deletion")?;
    ensure_success(index.status, "inspect submodules before deletion")?;
    if index
        .stdout
        .split(|byte| *byte == b'\n')
        .any(|line| line.starts_with(b"160000 "))
    {
        bail!("repository contains submodules that were not backed up independently")
    }

    let flags = git(repository)
        .args(["ls-files", "-v"])
        .output()
        .context("failed to inspect index flags before deletion")?;
    ensure_success(flags.status, "inspect index flags before deletion")?;
    if flags.stdout.split(|byte| *byte == b'\n').any(|line| {
        line.first()
            .is_some_and(|flag| *flag == b'S' || flag.is_ascii_lowercase())
    }) {
        bail!("index contains assume-unchanged or skip-worktree entries")
    }

    let unreachable = git(repository)
        .args(["fsck", "--unreachable", "--no-reflogs", "--no-progress"])
        .output()
        .context("failed to inspect unreachable Git objects before deletion")?;
    ensure_success(
        unreachable.status,
        "inspect unreachable Git objects before deletion",
    )?;
    if !unreachable.stdout.is_empty() {
        bail!("repository contains Git objects unreachable from refs")
    }
    Ok(())
}

pub fn verify_refs_unchanged(repository: &Repository, state: &RepositoryState) -> Result<()> {
    if state.has_non_commit_refs {
        bail!("repository has refs that do not point to commit history")
    }
    if local_ref_values(repository, PushMode::Mirror)? == state.ref_values {
        Ok(())
    } else {
        bail!("repository refs changed while the backup was running")
    }
}

pub fn verify_mirror_matches(
    repository: &Repository,
    remote_url: &str,
    auth: Option<&HttpsAuth<'_>>,
) -> Result<()> {
    verify_remote_rewrite(repository, remote_url)?;
    let local = local_ref_values(repository, PushMode::Mirror)?;
    let remote = remote_ref_values(repository, remote_url, PushMode::Mirror, auth)?;
    if local == remote {
        Ok(())
    } else {
        bail!("remote refs do not exactly match the local mirror")
    }
}

pub fn push_repository(
    repository: &Repository,
    state: &RepositoryState,
    remote_url: &str,
    mode: PushMode,
    auth: Option<&HttpsAuth<'_>>,
    adoption_guard: Option<&AdoptionGuard>,
) -> Result<()> {
    verify_remote_rewrite(repository, remote_url)?;
    if state.has_lfs_objects {
        reject_lfs_redirects(repository)?;
        push_lfs(repository, state, remote_url, auth)?;
    }

    let mut command = network_git(repository, auth)?;
    command.arg("push");
    match mode {
        PushMode::Mirror => {
            if let Some(guard) = adoption_guard {
                command.arg("--atomic");
                add_adoption_leases(&mut command, adoption_guard);
                command.arg(remote_url);
                command.args(
                    guard
                        .mirror_refspecs
                        .as_ref()
                        .context("mirror adoption guard has no refspecs")?,
                );
            } else {
                command.args(["--atomic", "--mirror", remote_url]);
            }
        }
        PushMode::BranchesAndTags => {
            if !state.has_heads && !state.has_tags {
                return Ok(());
            }
            command.arg("--atomic");
            add_adoption_leases(&mut command, adoption_guard);
            command.arg(remote_url);
            if state.has_heads {
                command.arg(if adoption_guard.is_some() {
                    "refs/heads/*:refs/heads/*"
                } else {
                    "+refs/heads/*:refs/heads/*"
                });
            }
            if state.has_tags {
                command.arg(if adoption_guard.is_some() {
                    "refs/tags/*:refs/tags/*"
                } else {
                    "+refs/tags/*:refs/tags/*"
                });
            }
        }
    }
    let status = command.status().context("failed to start git push")?;
    ensure_success(status, "push Git refs")
}

pub fn verify_adoption(
    repository: &Repository,
    remote_url: &str,
    mode: PushMode,
    auth: Option<&HttpsAuth<'_>>,
) -> Result<AdoptionGuard> {
    verify_remote_rewrite(repository, remote_url)?;
    let local = local_ref_values(repository, mode)?;
    let remote = remote_ref_values(repository, remote_url, mode, auth)?;
    let safe = match mode {
        PushMode::Mirror => remote.is_empty() || local == remote,
        PushMode::BranchesAndTags => remote.iter().all(|(name, value)| {
            local
                .get(name)
                .is_none_or(|local_value| local_value == value)
        }),
    };
    if safe {
        let mirror_refspecs = (mode == PushMode::Mirror).then(|| {
            local
                .keys()
                .map(|name| format!("{name}:{name}"))
                .chain(
                    remote
                        .keys()
                        .filter(|name| !local.contains_key(*name))
                        .map(|name| format!(":{name}")),
                )
                .collect()
        });
        let expectations = match mode {
            PushMode::BranchesAndTags => local
                .keys()
                .map(|name| (name.clone(), remote.get(name).cloned()))
                .collect(),
            PushMode::Mirror => local
                .keys()
                .chain(remote.keys())
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .map(|name| {
                    let expected = remote.get(&name).cloned();
                    (name, expected)
                })
                .collect(),
        };
        Ok(AdoptionGuard {
            expectations,
            mirror_refspecs,
        })
    } else {
        bail!(
            "remote repository has divergent refs; refusing to adopt it without manual reconciliation"
        )
    }
}

fn add_adoption_leases(command: &mut Command, guard: Option<&AdoptionGuard>) {
    let Some(guard) = guard else {
        return;
    };
    for (name, expected) in &guard.expectations {
        command.arg(format!(
            "--force-with-lease={name}:{}",
            expected.as_deref().unwrap_or_default()
        ));
    }
}

fn push_lfs(
    repository: &Repository,
    state: &RepositoryState,
    remote_url: &str,
    auth: Option<&HttpsAuth<'_>>,
) -> Result<()> {
    let mut command = network_git(repository, auth)?;
    command
        .args(["lfs", "push", "--all", remote_url, "--stdin"])
        .stdin(Stdio::piped());
    let mut child = command.spawn().context("failed to start git lfs push")?;
    let mut stdin = child
        .stdin
        .take()
        .context("failed to open git lfs push input")?;
    for object_id in &state.lfs_roots {
        writeln!(stdin, "{object_id}").context("failed to send refs to git lfs push")?;
    }
    drop(stdin);
    let status = child.wait().context("failed to wait for git lfs push")?;
    ensure_success(status, "push Git LFS objects")
}

fn is_dirty(repository: &Repository) -> Result<bool> {
    if repository.is_bare {
        return Ok(false);
    }
    let output = git(repository)
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .output()
        .with_context(|| format!("failed to inspect status of {}", repository.path.display()))?;
    ensure_success(output.status, "inspect repository status")?;
    Ok(!output.stdout.is_empty())
}

fn has_unreferenced_detached_head(repository: &Repository) -> Result<bool> {
    if repository.is_bare {
        return Ok(false);
    }
    let symbolic = git(repository)
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to inspect repository HEAD")?;
    if symbolic.success() {
        return Ok(false);
    }
    let output = git(repository)
        .args(["for-each-ref", "--contains", "HEAD", "--format=%(refname)"])
        .output()
        .context("failed to inspect detached HEAD reachability")?;
    ensure_success(output.status, "inspect detached HEAD reachability")?;
    Ok(output.stdout.is_empty())
}

fn might_use_lfs(repository: &Repository) -> Result<bool> {
    let output = git(repository)
        .args([
            "log",
            "--all",
            "-Gfilter=lfs",
            "--format=%H",
            "--",
            ".gitattributes",
            ":(glob)**/.gitattributes",
        ])
        .output()
        .context("failed to inspect Git LFS attributes")?;
    ensure_success(output.status, "inspect Git LFS attributes")?;
    Ok(!output.stdout.is_empty())
}

fn has_lfs_objects(repository: &Repository) -> Result<bool> {
    let output = git(repository)
        .args(["lfs", "ls-files", "--all"])
        .output()
        .context("failed to inspect Git LFS objects")?;
    ensure_success(output.status, "inspect Git LFS objects")?;
    Ok(!output.stdout.is_empty())
}

fn lfs_roots<'a>(
    repository: &Repository,
    references: impl Iterator<Item = &'a str>,
) -> Result<(Vec<String>, bool)> {
    let mut roots = std::collections::BTreeSet::new();
    let mut has_non_commit_refs = false;
    for reference in references {
        let output = git(repository)
            .args(["rev-parse", "--verify", &format!("{reference}^{{commit}}")])
            .output()
            .context("failed to resolve Git LFS root")?;
        if output.status.success() {
            roots.insert(
                String::from_utf8(output.stdout)
                    .context("Git returned a non-UTF-8 object ID")?
                    .trim()
                    .to_owned(),
            );
        } else {
            has_non_commit_refs = true;
        }
    }
    Ok((roots.into_iter().collect(), has_non_commit_refs))
}

fn verify_single_worktree(repository: &Repository) -> Result<()> {
    let output = git(repository)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .context("failed to inspect linked worktrees before deletion")?;
    ensure_success(output.status, "inspect linked worktrees before deletion")?;
    let worktree_count = String::from_utf8(output.stdout)
        .context("Git returned non-UTF-8 worktree paths")?
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .count();
    if worktree_count == 1 {
        Ok(())
    } else {
        bail!("repository has linked worktrees")
    }
}

fn verify_no_nested_repositories(repository: &Repository) -> Result<()> {
    let modules = repository.path.join(".git/modules");
    if modules.is_dir()
        && modules
            .read_dir()
            .context("failed to inspect Git submodule metadata")?
            .next()
            .is_some()
    {
        bail!("repository contains retained submodule metadata")
    }

    let mut entries = walkdir::WalkDir::new(&repository.path)
        .follow_links(false)
        .into_iter();
    while let Some(entry) = entries.next() {
        let entry = entry.context("failed to inspect nested repositories before deletion")?;
        let is_git_metadata = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(".git"));
        if entry.depth() == 1 && is_git_metadata {
            entries.skip_current_dir();
            continue;
        }
        if entry.depth() > 1 && is_git_metadata {
            bail!(
                "repository contains nested Git metadata at {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn verify_repository_identity(repository: &Repository) -> Result<()> {
    let output = git(repository)
        .args(["rev-parse", "--show-toplevel", "--absolute-git-dir"])
        .output()
        .context("failed to verify repository paths before deletion")?;
    ensure_success(output.status, "verify repository paths before deletion")?;
    let paths = String::from_utf8(output.stdout).context("Git returned non-UTF-8 paths")?;
    let mut paths = paths.lines();
    let top_level = paths.next().context("Git did not return a worktree path")?;
    let git_dir = paths.next().context("Git did not return a Git directory")?;
    let expected_top_level = repository
        .path
        .canonicalize()
        .with_context(|| format!("failed to resolve worktree {}", repository.path.display()))?;
    let expected_git_dir = repository
        .path
        .join(".git")
        .canonicalize()
        .with_context(|| {
            format!(
                "failed to resolve Git directory in {}",
                repository.path.display()
            )
        })?;
    if Path::new(top_level).canonicalize()? == expected_top_level
        && Path::new(git_dir).canonicalize()? == expected_git_dir
    {
        Ok(())
    } else {
        bail!("Git configuration redirects the repository outside its candidate directory")
    }
}

#[cfg(unix)]
fn verify_no_filesystem_boundaries(repository: &Repository) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let root_device = repository.path.metadata()?.dev();
    for entry in walkdir::WalkDir::new(&repository.path).follow_links(false) {
        let entry = entry.context("failed to inspect filesystem boundaries before deletion")?;
        if entry.file_type().is_dir() && entry.metadata()?.dev() != root_device {
            bail!(
                "repository crosses a filesystem boundary at {}",
                entry.path().display()
            )
        }
    }
    verify_linux_mount_points(repository)
}

#[cfg(not(unix))]
fn verify_no_filesystem_boundaries(_repository: &Repository) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_linux_mount_points(repository: &Repository) -> Result<()> {
    let root = repository.path.canonicalize()?;
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .context("failed to inspect Linux mount points before deletion")?;
    for line in mountinfo.lines() {
        let Some(encoded_path) = line.split_whitespace().nth(4) else {
            continue;
        };
        let mount_path = PathBuf::from(decode_mount_path(encoded_path));
        if mount_path == root || mount_path.starts_with(&root) {
            bail!(
                "repository contains a mount point at {}",
                mount_path.display()
            )
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn verify_linux_mount_points(_repository: &Repository) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn decode_mount_path(path: &str) -> String {
    path.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

fn verify_remote_rewrite(repository: &Repository, remote_url: &str) -> Result<()> {
    let output = git(repository)
        .args(["ls-remote", "--get-url", remote_url])
        .output()
        .context("failed to validate Git remote URL")?;
    ensure_success(output.status, "validate Git remote URL")?;
    let effective = String::from_utf8(output.stdout)
        .context("Git returned a non-UTF-8 remote URL")?
        .trim()
        .to_owned();
    if effective != remote_url {
        bail!("Git configuration rewrites remote URL to {effective}; refusing to send credentials")
    }

    let output = git(repository)
        .args(["config", "--get-regexp", r"^url\..*\.pushinsteadof$"])
        .output()
        .context("failed to inspect Git push URL rewrites")?;
    if !output.status.success() && output.status.code() != Some(1) {
        ensure_success(output.status, "inspect Git push URL rewrites")?;
    }
    let rewrites = String::from_utf8(output.stdout)
        .context("Git returned non-UTF-8 push URL configuration")?;
    for line in rewrites.lines() {
        let Some((_, prefix)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if remote_url.starts_with(prefix.trim()) {
            bail!(
                "Git configuration contains a matching pushInsteadOf URL; refusing to send credentials"
            )
        }
    }
    Ok(())
}

fn reject_lfs_redirects(repository: &Repository) -> Result<()> {
    let output = git(repository)
        .args([
            "config",
            "--get-regexp",
            r"^(lfs\.(url|pushurl)|remote\..*\.lfsurl)$",
        ])
        .output()
        .context("failed to inspect Git LFS configuration")?;
    if output.status.success() && !output.stdout.is_empty() {
        bail!("repository overrides the Git LFS destination; refusing to send credentials")
    }
    if !repository.is_bare && repository.path.join(".lfsconfig").exists() {
        bail!("repository contains .lfsconfig; refusing to use a repository-controlled LFS URL")
    }
    let tracked_config = command_succeeds(
        git(repository)
            .args(["cat-file", "-e", "HEAD:.lfsconfig"])
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );
    if tracked_config {
        bail!("repository tracks .lfsconfig; refusing to use a repository-controlled LFS URL")
    }
    Ok(())
}

fn local_ref_values(repository: &Repository, mode: PushMode) -> Result<BTreeMap<String, String>> {
    let output = git(repository)
        .args(["for-each-ref", "--format=%(refname)%09%(objectname)"])
        .output()
        .context("failed to enumerate local Git refs")?;
    ensure_success(output.status, "enumerate local Git refs")?;
    parse_ref_values(&output.stdout, mode)
}

fn remote_ref_values(
    repository: &Repository,
    remote_url: &str,
    mode: PushMode,
    auth: Option<&HttpsAuth<'_>>,
) -> Result<BTreeMap<String, String>> {
    let mut command = network_git(repository, auth)?;
    command.arg("ls-remote");
    if mode == PushMode::BranchesAndTags {
        command.args(["--heads", "--tags"]);
    }
    let output = command
        .arg(remote_url)
        .output()
        .context("failed to inspect remote Git refs")?;
    ensure_success(output.status, "inspect remote Git refs")?;
    parse_ref_values(&output.stdout, mode)
}

fn parse_ref_values(bytes: &[u8], mode: PushMode) -> Result<BTreeMap<String, String>> {
    let output = String::from_utf8(bytes.to_vec()).context("Git returned non-UTF-8 refs")?;
    let mut refs = BTreeMap::new();
    for line in output.lines() {
        let Some((first, second)) = line.split_once('\t') else {
            continue;
        };
        let (name, value) = if first.starts_with("refs/") {
            (first, second)
        } else {
            (second, first)
        };
        if !name.starts_with("refs/") || name.ends_with("^{}") {
            continue;
        }
        if mode == PushMode::BranchesAndTags
            && !name.starts_with("refs/heads/")
            && !name.starts_with("refs/tags/")
        {
            continue;
        }
        refs.insert(name.to_owned(), value.to_owned());
    }
    Ok(refs)
}

fn git(repository: &Repository) -> Command {
    let mut command = clean_git();
    command.arg("-C").arg(&repository.path);
    command
}

fn clean_git() -> Command {
    let mut command = Command::new("git");
    for name in [
        "GITEA_TOKEN",
        ASKPASS_MODE_ENV,
        ASKPASS_TOKEN_ENV,
        ASKPASS_USERNAME_ENV,
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

fn network_git(repository: &Repository, auth: Option<&HttpsAuth<'_>>) -> Result<Command> {
    let mut command = git(repository);
    command.args(["-c", "core.hooksPath=/dev/null", "-c", "credential.helper="]);
    let Some(auth) = auth else {
        return Ok(command);
    };
    let executable = env::current_exe().context("failed to locate askpass executable")?;
    command
        .env("GIT_ASKPASS", executable)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env(ASKPASS_MODE_ENV, "1")
        .env(ASKPASS_TOKEN_ENV, auth.token)
        .env(ASKPASS_USERNAME_ENV, auth.username);
    Ok(command)
}

fn command_succeeds(command: &mut Command) -> bool {
    command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn ensure_success(status: ExitStatus, action: &str) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        bail!("{action} failed with {status}")
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn pushes_branches_and_tags_to_a_bare_repository() {
        let directory = tempfile::tempdir().unwrap();
        let (repository, remote) = repositories(directory.path());
        let state = inspect_repository(&repository).unwrap();

        push_repository(
            &repository,
            &state,
            remote.to_str().unwrap(),
            PushMode::BranchesAndTags,
            None,
            None,
        )
        .unwrap();

        let output = Command::new("git")
            .args(["--git-dir"])
            .arg(&remote)
            .args(["for-each-ref", "--format=%(refname)"])
            .output()
            .unwrap();
        let refs = String::from_utf8(output.stdout).unwrap();
        assert!(refs.lines().any(|name| name.starts_with("refs/heads/")));
        assert!(refs.lines().any(|name| name == "refs/tags/v1"));
    }

    #[test]
    fn adoption_rejects_divergent_remote_refs() {
        let directory = tempfile::tempdir().unwrap();
        let (repository, remote) = repositories(directory.path());
        let state = inspect_repository(&repository).unwrap();
        push_repository(
            &repository,
            &state,
            remote.to_str().unwrap(),
            PushMode::BranchesAndTags,
            None,
            None,
        )
        .unwrap();
        fs::write(repository.path.join("README.md"), "changed\n").unwrap();
        commit(&repository.path, "changed");

        let error = verify_adoption(
            &repository,
            remote.to_str().unwrap(),
            PushMode::BranchesAndTags,
            None,
        )
        .unwrap_err();

        assert!(error.to_string().contains("divergent refs"));
    }

    #[test]
    fn adoption_lease_rejects_a_remote_race() {
        let directory = tempfile::tempdir().unwrap();
        let (repository, remote) = repositories(directory.path());
        let state = inspect_repository(&repository).unwrap();
        let guard = verify_adoption(
            &repository,
            remote.to_str().unwrap(),
            PushMode::BranchesAndTags,
            None,
        )
        .unwrap();
        let intruder = directory.path().join("intruder");
        fs::create_dir(&intruder).unwrap();
        run(Command::new("git")
            .args(["init", "--quiet", "--initial-branch=master"])
            .arg(&intruder));
        fs::write(intruder.join("other.txt"), "other\n").unwrap();
        commit(&intruder, "other");
        run(Command::new("git")
            .args(["-C"])
            .arg(&intruder)
            .args(["push"])
            .arg(remote.to_str().unwrap())
            .arg("HEAD:refs/heads/master"));

        let error = push_repository(
            &repository,
            &state,
            remote.to_str().unwrap(),
            PushMode::BranchesAndTags,
            None,
            Some(&guard),
        )
        .unwrap_err();

        assert!(error.to_string().contains("push Git refs failed"));
    }

    #[test]
    fn repository_url_rewrites_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let (repository, _) = repositories(directory.path());
        run(Command::new("git")
            .args(["-C"])
            .arg(&repository.path)
            .args([
                "config",
                "url.https://evil.example/.insteadOf",
                "https://gitea.example/",
            ]));

        let error = verify_remote_rewrite(&repository, "https://gitea.example/backup/source.git")
            .unwrap_err();

        assert!(error.to_string().contains("rewrites remote URL"));

        run(Command::new("git")
            .args(["-C"])
            .arg(&repository.path)
            .args([
                "config",
                "--unset-all",
                "url.https://evil.example/.insteadOf",
            ]));
        run(Command::new("git")
            .args(["-C"])
            .arg(&repository.path)
            .args([
                "config",
                "url.https://evil.example/.pushInsteadOf",
                "https://gitea.example/",
            ]));

        let error = verify_remote_rewrite(&repository, "https://gitea.example/backup/source.git")
            .unwrap_err();

        assert!(error.to_string().contains("pushInsteadOf"));
    }

    #[test]
    fn mirror_push_uses_a_valid_atomic_command() {
        let directory = tempfile::tempdir().unwrap();
        let (repository, remote) = repositories(directory.path());
        let state = inspect_repository(&repository).unwrap();

        push_repository(
            &repository,
            &state,
            remote.to_str().unwrap(),
            PushMode::Mirror,
            None,
            None,
        )
        .unwrap();
    }

    #[test]
    fn mirror_adoption_does_not_delete_a_racing_remote_ref() {
        let directory = tempfile::tempdir().unwrap();
        let (repository, remote) = repositories(directory.path());
        let state = inspect_repository(&repository).unwrap();
        let guard = verify_adoption(
            &repository,
            remote.to_str().unwrap(),
            PushMode::Mirror,
            None,
        )
        .unwrap();
        let intruder = directory.path().join("intruder-mirror");
        fs::create_dir(&intruder).unwrap();
        run(Command::new("git")
            .args(["init", "--quiet", "--initial-branch=raced"])
            .arg(&intruder));
        fs::write(intruder.join("other.txt"), "other\n").unwrap();
        commit(&intruder, "other");
        run(Command::new("git")
            .args(["-C"])
            .arg(&intruder)
            .args(["push"])
            .arg(remote.to_str().unwrap())
            .arg("HEAD:refs/heads/raced"));

        push_repository(
            &repository,
            &state,
            remote.to_str().unwrap(),
            PushMode::Mirror,
            None,
            Some(&guard),
        )
        .unwrap();

        let error = verify_adoption(
            &repository,
            remote.to_str().unwrap(),
            PushMode::Mirror,
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("divergent refs"));
    }

    #[test]
    fn ignored_files_prevent_local_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let (repository, _) = repositories(directory.path());
        fs::write(repository.path.join(".gitignore"), "generated.log\n").unwrap();
        commit(&repository.path, "ignore generated file");
        fs::write(repository.path.join("generated.log"), "not backed up\n").unwrap();

        let error = verify_safe_to_delete(&repository).unwrap_err();

        assert!(error.to_string().contains("ignored files"));
        assert!(repository.path.join("generated.log").exists());
    }

    #[test]
    fn modified_files_prevent_local_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let (repository, _) = repositories(directory.path());
        fs::write(repository.path.join("README.md"), "modified\n").unwrap();

        let error = verify_safe_to_delete(&repository).unwrap_err();

        assert!(error.to_string().contains("modified"));
        assert!(repository.path.exists());
    }

    #[test]
    fn hidden_index_changes_prevent_local_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let (repository, _) = repositories(directory.path());
        run(Command::new("git")
            .args(["-C"])
            .arg(&repository.path)
            .args(["update-index", "--assume-unchanged", "README.md"]));
        fs::write(repository.path.join("README.md"), "hidden change\n").unwrap();

        let error = verify_safe_to_delete(&repository).unwrap_err();

        assert!(error.to_string().contains("assume-unchanged"));
    }

    #[test]
    fn linked_worktrees_prevent_local_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let (repository, _) = repositories(directory.path());
        let linked = directory.path().join("linked");
        run(Command::new("git")
            .args(["-C"])
            .arg(&repository.path)
            .args(["worktree", "add", "--quiet", "--detach"])
            .arg(&linked));

        let error = verify_safe_to_delete(&repository).unwrap_err();

        assert!(error.to_string().contains("linked worktrees"));
    }

    #[test]
    fn unreachable_objects_prevent_local_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let (repository, _) = repositories(directory.path());
        fs::write(repository.path.join("temporary.txt"), "temporary\n").unwrap();
        commit(&repository.path, "temporary commit");
        run(Command::new("git")
            .args(["-C"])
            .arg(&repository.path)
            .args(["reset", "--hard", "--quiet", "HEAD~1"]));

        let error = verify_safe_to_delete(&repository).unwrap_err();

        assert!(error.to_string().contains("unreachable from refs"));
    }

    #[test]
    fn ref_changes_are_detected_before_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let (repository, _) = repositories(directory.path());
        let state = inspect_repository(&repository).unwrap();
        run(Command::new("git")
            .args(["-C"])
            .arg(&repository.path)
            .args(["tag", "added-during-backup"]));

        let error = verify_refs_unchanged(&repository, &state).unwrap_err();

        assert!(error.to_string().contains("refs changed"));
    }

    #[test]
    fn non_commit_refs_prevent_local_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let (repository, _) = repositories(directory.path());
        let output = Command::new("git")
            .args(["-C"])
            .arg(&repository.path)
            .args(["hash-object", "-w", "README.md"])
            .output()
            .unwrap();
        let object_id = String::from_utf8(output.stdout).unwrap();
        run(Command::new("git")
            .args(["-C"])
            .arg(&repository.path)
            .args(["update-ref", "refs/data/blob", object_id.trim()]));
        let state = inspect_repository(&repository).unwrap();

        let error = verify_refs_unchanged(&repository, &state).unwrap_err();

        assert!(error.to_string().contains("do not point to commit"));
    }

    fn repositories(root: &std::path::Path) -> (Repository, std::path::PathBuf) {
        let source = root.join("source");
        let remote = root.join("remote.git");
        fs::create_dir(&source).unwrap();
        run(Command::new("git")
            .args(["init", "--quiet", "--initial-branch=master"])
            .arg(&source));
        run(Command::new("git")
            .args(["init", "--quiet", "--bare"])
            .arg(&remote));
        fs::write(source.join("README.md"), "test\n").unwrap();
        run(Command::new("git")
            .args(["-C"])
            .arg(&source)
            .args(["add", "README.md"]));
        commit(&source, "initial");
        run(Command::new("git")
            .args(["-C"])
            .arg(&source)
            .args(["tag", "v1"]));
        (
            Repository {
                path: source,
                relative_path: "source".into(),
                base_name: "source".into(),
                is_bare: false,
                object_format: "sha1".into(),
            },
            remote,
        )
    }

    fn commit(repository: &std::path::Path, message: &str) {
        run(Command::new("git")
            .args(["-C"])
            .arg(repository)
            .args(["add", "."]));
        run(Command::new("git").args(["-C"]).arg(repository).args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--quiet",
            "-m",
            message,
        ]));
    }

    fn run(command: &mut Command) {
        assert!(command.status().unwrap().success());
    }
}
