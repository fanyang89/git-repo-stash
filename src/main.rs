mod cli;
mod discovery;
mod git;
mod gitea;
mod mapping;

use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use sha2::{Digest, Sha256};
use url::Url;

use crate::{
    cli::{Cli, Protocol},
    discovery::{NamedRepository, assign_names, discover, generated_name},
    git::{
        ASKPASS_MODE_ENV, ASKPASS_TOKEN_ENV, ASKPASS_USERNAME_ENV, HttpsAuth, PushMode,
        RepositoryState, inspect_repository, push_repository, verify_adoption,
        verify_mirror_matches, verify_refs_unchanged, verify_safe_to_delete,
    },
    gitea::{GiteaClient, Lookup, RemoteRepository},
    mapping::Mapping,
};

fn main() -> ExitCode {
    if handle_askpass() {
        return ExitCode::SUCCESS;
    }
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn handle_askpass() -> bool {
    if env::var_os(ASKPASS_MODE_ENV).is_none() {
        return false;
    }
    let prompt = env::args_os().nth(1).unwrap_or_default();
    if prompt
        .to_string_lossy()
        .to_ascii_lowercase()
        .contains("username")
    {
        print_env(ASKPASS_USERNAME_ENV);
    } else {
        print_env(ASKPASS_TOKEN_ENV);
    }
    true
}

fn print_env(name: &str) {
    let value = env::var_os(name).unwrap_or_else(|| OsString::from(""));
    println!("{}", value.to_string_lossy());
}

fn run(cli: Cli) -> Result<()> {
    let workdir = cli
        .workdir
        .canonicalize()
        .with_context(|| format!("failed to open workdir {}", cli.workdir.display()))?;
    if !workdir.is_dir() {
        bail!("workdir is not a directory: {}", workdir.display());
    }
    let org = match &cli.org {
        Some(org) if valid_organization_name(org) => org.clone(),
        Some(org) => bail!("organization name is not valid for Gitea: {org}"),
        None => generated_organization_name(&default_org(&workdir), &workdir.to_string_lossy()),
    };
    if org.is_empty() {
        bail!("organization name cannot be empty");
    }
    let token = env::var("GITEA_TOKEN").context("GITEA_TOKEN is not set")?;
    let gitea = GiteaClient::new(&cli.gitea_url, &token, cli.allow_insecure_http)?;
    let user = gitea
        .current_user()
        .context("failed to authenticate with Gitea")?;

    let discovered = discover(&workdir);
    let mut failures = discovered.errors.len();
    for error in &discovered.errors {
        eprintln!("Scan error: {error}");
    }
    println!("Found {} repositories", discovered.repositories.len());

    let mut mapping = Mapping::load(&workdir)?;
    let repositories = assign_names(discovered.repositories, &mapping.repositories)?;
    let org_exists = match gitea.organization(&org)? {
        Lookup::Found(()) => true,
        Lookup::Missing if cli.dry_run => {
            println!("Would create organization {org}");
            false
        }
        Lookup::Missing => {
            println!("Creating organization {org}");
            gitea.create_organization(&org, cli.public)?;
            true
        }
    };

    for repository in repositories {
        if let Err(error) = back_up_repository(
            &cli,
            &workdir,
            &org,
            org_exists,
            &user.login,
            &token,
            &gitea,
            &mut mapping,
            &repository,
        ) {
            failures += 1;
            eprintln!("Failed {}: {error:#}", repository.repository.relative_path);
        }
    }

    if failures > 0 {
        bail!("{failures} operation(s) failed");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn back_up_repository(
    cli: &Cli,
    workdir: &Path,
    org: &str,
    org_exists: bool,
    username: &str,
    token: &str,
    gitea: &GiteaClient,
    mapping: &mut Mapping,
    named: &NamedRepository,
) -> Result<()> {
    let local = &named.repository;
    let qualified_name = format!("{org}/{}", named.remote_name);
    let mode = if cli.mirror {
        PushMode::Mirror
    } else {
        PushMode::BranchesAndTags
    };
    let state = inspect_repository(local)?;
    if cli.delete_after_backup {
        verify_deletion_candidate(workdir, local)?;
        verify_refs_unchanged(local, &state)?;
    }
    if state.dirty {
        eprintln!(
            "Warning: {} has uncommitted or untracked files",
            local.relative_path
        );
    }
    if !state.has_refs_for(mode) {
        eprintln!(
            "Warning: {} has no refs selected by the push mode",
            local.relative_path
        );
    }
    if state.detached_unreferenced {
        eprintln!(
            "Warning: {} has a detached HEAD commit unreachable from refs",
            local.relative_path
        );
    }
    let mut adoption_guard = None;
    let mut record_after_push = false;
    let remote = if !org_exists {
        println!("Would create repository {qualified_name}");
        None
    } else {
        match gitea.repository(org, &named.remote_name)? {
            Lookup::Found(remote) if named.is_mapped => {
                verify_object_format(local, &remote)?;
                Some(remote)
            }
            Lookup::Found(remote) if cli.adopt_existing => {
                verify_object_format(local, &remote)?;
                let (remote_url, https) =
                    select_remote(&remote, cli.protocol, cli.allow_insecure_http)?;
                let auth = https.then_some(HttpsAuth { username, token });
                adoption_guard = Some(verify_adoption(local, remote_url, mode, auth.as_ref())?);
                if cli.dry_run {
                    println!("Would adopt repository {qualified_name}");
                } else {
                    record_after_push = true;
                    println!("Adopting repository {qualified_name}");
                }
                Some(remote)
            }
            Lookup::Found(_) => bail!(
                "remote repository {qualified_name} already exists without a local mapping; use --adopt-existing to associate it"
            ),
            Lookup::Missing if cli.dry_run => {
                println!("Would create repository {qualified_name}");
                None
            }
            Lookup::Missing => {
                println!("Creating repository {qualified_name}");
                let remote = gitea.create_repository(
                    org,
                    &named.remote_name,
                    cli.public,
                    &local.object_format,
                )?;
                verify_object_format(local, &remote)?;
                if !named.is_mapped {
                    record_mapping(workdir, mapping, named)?;
                }
                Some(remote)
            }
        }
    };

    if cli.dry_run {
        if let Some(remote) = &remote {
            select_remote(remote, cli.protocol, cli.allow_insecure_http)?;
        }
        println!("Would push {} -> {qualified_name}", local.relative_path);
        if cli.delete_after_backup {
            println!("Would delete {}", local.path.display());
        }
        return Ok(());
    }
    let remote = remote.ok_or_else(|| anyhow!("repository URL is unavailable"))?;
    let (remote_url, https) = select_remote(&remote, cli.protocol, cli.allow_insecure_http)?;
    let auth = https.then_some(HttpsAuth { username, token });
    println!("Pushing {} -> {qualified_name}", local.relative_path);
    push_repository(
        local,
        &state,
        remote_url,
        mode,
        auth.as_ref(),
        adoption_guard.as_ref(),
    )?;
    if record_after_push {
        if mode == PushMode::Mirror {
            verify_adoption(local, remote_url, mode, auth.as_ref())?;
        }
        record_mapping(workdir, mapping, named)?;
        println!("Adopted repository {qualified_name}");
    }
    if cli.delete_after_backup {
        delete_local_repository(workdir, mapping, named, &state, remote_url, auth.as_ref())?;
    }
    Ok(())
}

fn verify_deletion_candidate(workdir: &Path, local: &discovery::Repository) -> Result<()> {
    if local.path == workdir {
        bail!("refusing to delete a repository that is also the workdir")
    }
    verify_safe_to_delete(local)
        .with_context(|| format!("{} is not safe to delete", local.relative_path))
}

fn delete_local_repository(
    workdir: &Path,
    mapping: &mut Mapping,
    named: &NamedRepository,
    state: &RepositoryState,
    remote_url: &str,
    auth: Option<&HttpsAuth<'_>>,
) -> Result<()> {
    let original_path = &named.repository.path;
    verify_deletion_candidate(workdir, &named.repository)?;
    verify_refs_unchanged(&named.repository, state)?;
    verify_mirror_matches(&named.repository, remote_url, auth)?;

    let quarantine_path = quarantine_path(&named.repository)?;
    fs::rename(original_path, &quarantine_path).with_context(|| {
        format!(
            "failed to isolate {} before deletion",
            original_path.display()
        )
    })?;
    let mut quarantined = named.repository.clone();
    quarantined.path = quarantine_path.clone();
    let quarantine_check = verify_safe_to_delete(&quarantined)
        .and_then(|()| verify_refs_unchanged(&quarantined, state))
        .and_then(|()| verify_mirror_matches(&quarantined, remote_url, auth));
    if let Err(error) = quarantine_check {
        restore_quarantined_repository(original_path, &quarantine_path)?;
        return Err(error).context("repository changed while it was being isolated for deletion");
    }

    let relative_path = &named.repository.relative_path;
    let Some(remote_name) = mapping.repositories.remove(relative_path) else {
        restore_quarantined_repository(original_path, &quarantine_path)?;
        bail!("repository mapping disappeared before local deletion")
    };
    if let Err(error) = mapping.save(workdir) {
        mapping
            .repositories
            .insert(relative_path.clone(), remote_name);
        restore_quarantined_repository(original_path, &quarantine_path)?;
        return Err(error).context("failed to remove mapping before local deletion");
    }

    if let Err(delete_error) = fs::remove_dir_all(&quarantine_path) {
        mapping
            .repositories
            .insert(relative_path.clone(), remote_name);
        let mapping_restore = mapping.save(workdir);
        let path_restore = restore_quarantined_repository(original_path, &quarantine_path);
        mapping_restore.with_context(|| {
            format!("failed to restore mapping after local deletion failed: {delete_error}")
        })?;
        path_restore?;
        return Err(delete_error)
            .with_context(|| format!("failed to delete {}", original_path.display()));
    }
    println!("Deleted {}", original_path.display());
    Ok(())
}

fn quarantine_path(repository: &discovery::Repository) -> Result<PathBuf> {
    let parent = repository
        .path
        .parent()
        .ok_or_else(|| anyhow!("repository has no parent directory"))?;
    for attempt in 0..1000 {
        let candidate = parent.join(format!(
            ".git-repo-stash-deleting-{}-{}-{attempt}",
            repository.base_name,
            std::process::id()
        ));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    bail!("could not allocate a quarantine path for deletion")
}

fn restore_quarantined_repository(original: &Path, quarantine: &Path) -> Result<()> {
    if !quarantine.exists() {
        return Ok(());
    }
    if original.exists() {
        bail!(
            "repository was retained at {} because its original path is occupied",
            quarantine.display()
        )
    }
    fs::rename(quarantine, original).with_context(|| {
        format!(
            "repository was retained at {}, but restoring its original path failed",
            quarantine.display()
        )
    })
}

fn verify_object_format(local: &discovery::Repository, remote: &RemoteRepository) -> Result<()> {
    if remote.object_format_name == local.object_format
        || (remote.object_format_name.is_empty() && local.object_format == "sha1")
    {
        Ok(())
    } else {
        bail!(
            "object format mismatch: local is {}, remote is {}",
            local.object_format,
            remote.object_format_name
        )
    }
}

fn record_mapping(workdir: &Path, mapping: &mut Mapping, named: &NamedRepository) -> Result<()> {
    if let Some((path, _)) = mapping.repositories.iter().find(|(path, remote)| {
        *path != &named.repository.relative_path && remote.eq_ignore_ascii_case(&named.remote_name)
    }) {
        bail!("remote {} is already mapped from {path}", named.remote_name);
    }
    mapping.repositories.insert(
        named.repository.relative_path.clone(),
        named.remote_name.clone(),
    );
    mapping.save(workdir)
}

fn select_remote(
    repository: &RemoteRepository,
    protocol: Protocol,
    allow_insecure_http: bool,
) -> Result<(&str, bool)> {
    match protocol {
        Protocol::Auto if !repository.ssh_url.is_empty() => Ok((&repository.ssh_url, false)),
        Protocol::Auto => {
            validate_http_clone_url(&repository.clone_url, allow_insecure_http)?;
            Ok((&repository.clone_url, true))
        }
        Protocol::Ssh if repository.ssh_url.is_empty() => {
            bail!("repository {} has no SSH URL", repository.name)
        }
        Protocol::Ssh => Ok((&repository.ssh_url, false)),
        Protocol::Https => {
            validate_http_clone_url(&repository.clone_url, allow_insecure_http)?;
            Ok((&repository.clone_url, true))
        }
    }
}

fn validate_http_clone_url(clone_url: &str, allow_insecure_http: bool) -> Result<()> {
    let url = Url::parse(clone_url).context("Gitea returned an invalid HTTP clone URL")?;
    match url.scheme() {
        "https" => Ok(()),
        "http" if allow_insecure_http => Ok(()),
        "http" => bail!("HTTP clone URL is insecure; pass --allow-insecure-http to allow it"),
        scheme => bail!("Gitea returned an unsupported HTTP clone URL scheme: {scheme}"),
    }
}

fn default_org(workdir: &Path) -> String {
    workdir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned()
}

fn generated_organization_name(name: &str, identity: &str) -> String {
    let mut generated =
        generated_name(name, identity)
            .chars()
            .fold(String::new(), |mut result, character| {
                let previous_is_separator = result
                    .chars()
                    .next_back()
                    .is_some_and(|previous| !previous.is_ascii_alphanumeric());
                if character.is_ascii_alphanumeric()
                    || (matches!(character, '-' | '_' | '.') && !previous_is_separator)
                {
                    result.push(character);
                }
                result
            });
    while generated
        .chars()
        .next_back()
        .is_some_and(|character| !character.is_ascii_alphanumeric())
    {
        generated.pop();
    }
    if generated.is_empty() {
        generated.push_str("backup");
    }
    if generated.chars().count() <= 40 && !reserved_organization_name(&generated) {
        return generated;
    }

    let digest = Sha256::digest(identity.as_bytes());
    let suffix = format!(
        "-{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    );
    let mut prefix = generated
        .chars()
        .take(40 - suffix.len())
        .collect::<String>();
    while prefix
        .chars()
        .next_back()
        .is_some_and(|character| !character.is_ascii_alphanumeric())
    {
        prefix.pop();
    }
    format!("{prefix}{suffix}")
}

fn valid_organization_name(name: &str) -> bool {
    let characters = name.chars().collect::<Vec<_>>();
    !characters.is_empty()
        && characters.len() <= 40
        && characters
            .first()
            .is_some_and(|character| character.is_ascii_alphanumeric())
        && characters
            .last()
            .is_some_and(|character| character.is_ascii_alphanumeric())
        && characters.iter().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
        && characters
            .windows(2)
            .all(|pair| pair[0].is_ascii_alphanumeric() || pair[1].is_ascii_alphanumeric())
        && !reserved_organization_name(name)
}

fn reserved_organization_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "admin"
            | "api"
            | "assets"
            | "attachments"
            | "avatar"
            | "explore"
            | "issues"
            | "milestones"
            | "notifications"
            | "org"
            | "pulls"
            | "repo"
            | "robots.txt"
            | "user"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote() -> RemoteRepository {
        RemoteRepository {
            name: "repo".into(),
            ssh_url: "git@example.com:org/repo.git".into(),
            clone_url: "https://example.com/org/repo.git".into(),
            object_format_name: "sha1".into(),
        }
    }

    #[test]
    fn automatic_protocol_prefers_ssh() {
        assert_eq!(
            select_remote(&remote(), Protocol::Auto, false).unwrap(),
            ("git@example.com:org/repo.git", false)
        );
    }

    #[test]
    fn explicit_https_uses_clone_url() {
        assert_eq!(
            select_remote(&remote(), Protocol::Https, false).unwrap(),
            ("https://example.com/org/repo.git", true)
        );
    }

    #[test]
    fn organization_names_are_bounded_and_safe() {
        assert!(valid_organization_name("backup-org"));
        assert!(!valid_organization_name("api"));
        assert!(!valid_organization_name("bad__name"));
        let generated = generated_organization_name(
            "a-very-long-work-directory-name-that-exceeds-forty-characters",
            "/tmp/workdir",
        );
        assert!(valid_organization_name(&generated));
    }
}
