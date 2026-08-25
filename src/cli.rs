use std::path::PathBuf;

use clap::{Parser, ValueEnum};

#[derive(Debug, Parser)]
#[command(version, about = "Back up local Git repositories to Gitea")]
pub struct Cli {
    /// Directory to scan recursively
    pub workdir: PathBuf,

    /// Gitea instance root URL
    #[arg(long, env = "GITEA_URL")]
    pub gitea_url: String,

    /// Target organization name (defaults to the workdir name)
    #[arg(long)]
    pub org: Option<String>,

    /// Protocol used for Git pushes
    #[arg(long, value_enum, default_value_t = Protocol::Auto)]
    pub protocol: Protocol,

    /// Mirror all refs and delete remote refs missing locally
    #[arg(long)]
    pub mirror: bool,

    /// Create public organizations and repositories
    #[arg(long)]
    pub public: bool,

    /// Show intended changes without creating or pushing
    #[arg(long)]
    pub dry_run: bool,

    /// Associate untracked existing remote repositories with local repositories
    #[arg(long)]
    pub adopt_existing: bool,

    /// Allow tokens to be sent over unencrypted HTTP
    #[arg(long)]
    pub allow_insecure_http: bool,

    /// Delete clean local repositories after a successful mirror backup
    #[arg(long, requires = "mirror")]
    pub delete_after_backup: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum Protocol {
    #[default]
    Auto,
    Ssh,
    Https,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletion_requires_mirror_mode() {
        let error = Cli::try_parse_from([
            "git-repo-stash",
            "workdir",
            "--gitea-url",
            "https://example.com",
            "--delete-after-backup",
        ])
        .unwrap_err();

        assert!(error.to_string().contains("--mirror"));
    }
}
