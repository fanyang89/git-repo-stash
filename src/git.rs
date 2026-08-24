use std::{
    collections::BTreeMap,
    env,
    io::Write,
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
    let lfs_available = command_succeeds(clean_git().args(["lfs", "version"]));
    let has_lfs_objects = if lfs_available {
        has_lfs_objects(repository)?
    } else if might_use_lfs(repository)? {
        bail!("repository uses Git LFS, but git-lfs is not installed")
    } else {
        false
    };
    let all_refs = ref_names(repository)?;
    Ok(RepositoryState {
        dirty,
        detached_unreferenced,
        has_lfs_objects,
        has_heads: all_refs.iter().any(|name| name.starts_with("refs/heads/")),
        has_tags: all_refs.iter().any(|name| name.starts_with("refs/tags/")),
        all_refs,
    })
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
        push_lfs(repository, state, remote_url, mode, auth)?;
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
    mode: PushMode,
    auth: Option<&HttpsAuth<'_>>,
) -> Result<()> {
    let mut command = network_git(repository, auth)?;
    command.args(["lfs", "push", "--all", remote_url]);
    if mode == PushMode::Mirror {
        command.arg("--stdin").stdin(Stdio::piped());
        let mut child = command.spawn().context("failed to start git lfs push")?;
        let mut stdin = child
            .stdin
            .take()
            .context("failed to open git lfs push input")?;
        for reference in &state.all_refs {
            writeln!(stdin, "{reference}").context("failed to send refs to git lfs push")?;
        }
        drop(stdin);
        let status = child.wait().context("failed to wait for git lfs push")?;
        ensure_success(status, "push Git LFS objects")
    } else {
        let status = command.status().context("failed to start git lfs push")?;
        ensure_success(status, "push Git LFS objects")
    }
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

fn has_lfs_objects(repository: &Repository) -> Result<bool> {
    let output = git(repository)
        .args(["lfs", "ls-files", "--all"])
        .output()
        .context("failed to inspect Git LFS objects")?;
    ensure_success(output.status, "inspect Git LFS objects")?;
    Ok(!output.stdout.is_empty())
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

fn ref_names(repository: &Repository) -> Result<Vec<String>> {
    let output = git(repository)
        .args(["for-each-ref", "--format=%(refname)"])
        .output()
        .context("failed to enumerate Git refs")?;
    ensure_success(output.status, "enumerate Git refs")?;
    let refs = String::from_utf8(output.stdout).context("Git returned non-UTF-8 refs")?;
    Ok(refs.lines().map(str::to_owned).collect())
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
            "--local",
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
    command
        .env_remove("GITEA_TOKEN")
        .env_remove(ASKPASS_MODE_ENV)
        .env_remove(ASKPASS_TOKEN_ENV)
        .env_remove(ASKPASS_USERNAME_ENV);
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
