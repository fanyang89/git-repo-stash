# git-repo-stash

`git-repo-stash` recursively discovers local Git repositories, creates matching
repositories in a Gitea organization, and pushes committed Git data as a backup.

## Requirements

- Git
- A Gitea access token with `write:organization` and `write:repository` scopes
- An SSH key registered with Gitea, or HTTPS access using the token
- Git LFS when any discovered repository uses LFS

## Install

```sh
cargo install --path .
```

For a static x86_64 Linux binary, install Zig and `cargo-zigbuild`, add the
musl target, then use the project shortcut:

```sh
cargo install cargo-zigbuild
rustup target add x86_64-unknown-linux-musl
cargo musl
```

The binary is written to
`target/x86_64-unknown-linux-musl/release/git-repo-stash`.

## Usage

```sh
export GITEA_URL=https://git.example.com
export GITEA_TOKEN=your-token

git-repo-stash /path/to/workdir
```

The organization defaults to the workdir directory name. Override it when
needed:

```sh
git-repo-stash /path/to/workdir --org source-backups
```

Important options:

```text
--protocol auto|ssh|https  Prefer SSH by default or select a protocol
--mirror                   Mirror every ref and delete missing remote refs
--public                   Create public organizations and repositories
--dry-run                  Query Gitea without creating or pushing
--adopt-existing           Associate an existing untracked remote repository
--allow-insecure-http      Allow tokens over unencrypted HTTP
```

The Gitea URL must be the instance root, including any installation subpath,
not its `/api/v1` URL.

## Repository Names

A unique local directory name is used directly. Duplicate names are encoded
from their workdir-relative paths:

```text
team-a/foo -> team-a--foo
team-b/foo -> team-b--foo
```

Names are compared case-insensitively and bare repository names lose the
`.git` suffix. Unsupported Gitea name characters are replaced with `-`. A short
path hash resolves any remaining collision, reserved name, or excessive name
length.

Associations are stored in `WORKDIR/.git-repo-stash.toml`. Existing mappings
remain stable when new duplicate repositories appear. If a generated remote
name already exists in Gitea without a mapping, the command refuses to push
until `--adopt-existing` explicitly associates it.

Adoption is allowed only when overlapping branches and tags are identical. In
mirror mode, every advertised ref must match. Divergent remotes must be
reconciled manually before adoption. The mapping is saved only after the first
adopted push succeeds, and ref leases prevent changes between validation and
push from being overwritten.

## Backup Semantics

The default mode force-updates all local branches and tags but leaves other
remote refs untouched. `--mirror` makes the remote refs exactly match the local
repository and can delete remote data.

Remote-tracking refs, stashes, notes, and replace refs are outside the default
mode. Use `--mirror` when these refs are part of the backup. Commits reachable
only from a detached `HEAD` are not refs and are not backed up in either mode.

Git LFS objects are uploaded with `git lfs push --all` before refs are pushed.
Repositories without branches or tags are still created, but default mode has
no refs to push.

The command warns about dirty repositories. Uncommitted and untracked files
are not Git objects and are not backed up. Submodule repositories are backed up
only when they are independently discovered under the workdir. Scanning stops
below each discovered repository, so nested repositories are intentionally not
treated as separate backups.

The command does not add or modify local Git remotes. A failed repository does
not stop later repositories; any failure produces a non-zero exit status.

## Authentication

API and HTTPS credentials are read from `GITEA_TOKEN`. HTTPS pushes use a
temporary Git askpass invocation and do not store the token in a remote URL,
Git configuration, or process arguments. Automatic protocol selection uses the
Gitea-provided SSH URL when available and does not retry with HTTPS after an
SSH failure. HTTP is rejected unless `--allow-insecure-http` is provided.
