use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant},
};

#[test]
fn creates_resources_records_mapping_and_pushes_repository() {
    let directory = tempfile::tempdir().unwrap();
    let (workdir, _source, remote) = create_repositories(directory.path());

    let (server_url, server) = start_gitea_server(&remote, 5);
    let output = Command::new(env!("CARGO_BIN_EXE_git-repo-stash"))
        .arg(&workdir)
        .args([
            "--gitea-url",
            &server_url,
            "--org",
            "backup",
            "--allow-insecure-http",
        ])
        .env("GITEA_TOKEN", "secret")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
    let mapping = fs::read_to_string(workdir.join(".git-repo-stash.toml")).unwrap();
    assert!(mapping.contains("source = \"source\""));
    let refs = Command::new("git")
        .args(["--git-dir"])
        .arg(&remote)
        .args(["for-each-ref", "--format=%(refname)"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8(refs.stdout)
            .unwrap()
            .lines()
            .any(|name| name.starts_with("refs/heads/"))
    );
}

#[test]
fn deletes_a_clean_repository_after_a_successful_mirror_backup() {
    let directory = tempfile::tempdir().unwrap();
    let (workdir, source, remote) = create_repositories(directory.path());
    let (server_url, server) = start_gitea_server(&remote, 5);

    let output = Command::new(env!("CARGO_BIN_EXE_git-repo-stash"))
        .arg(&workdir)
        .args([
            "--gitea-url",
            &server_url,
            "--org",
            "backup",
            "--allow-insecure-http",
            "--mirror",
            "--delete-after-backup",
        ])
        .env("GITEA_TOKEN", "secret")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
    assert!(!source.exists());
    let mapping = fs::read_to_string(workdir.join(".git-repo-stash.toml")).unwrap();
    assert!(!mapping.contains("source = \"source\""));
    let refs = Command::new("git")
        .args(["--git-dir"])
        .arg(&remote)
        .args(["for-each-ref", "--format=%(refname)"])
        .output()
        .unwrap();
    assert!(!refs.stdout.is_empty());
}

#[test]
fn dry_run_previews_deletion_without_removing_the_repository() {
    let directory = tempfile::tempdir().unwrap();
    let (workdir, source, remote) = create_repositories(directory.path());
    let (server_url, server) = start_gitea_server(&remote, 2);

    let output = Command::new(env!("CARGO_BIN_EXE_git-repo-stash"))
        .arg(&workdir)
        .args([
            "--gitea-url",
            &server_url,
            "--org",
            "backup",
            "--allow-insecure-http",
            "--mirror",
            "--delete-after-backup",
            "--dry-run",
        ])
        .env("GITEA_TOKEN", "secret")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
    assert!(source.exists());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("Would delete")
    );
    assert!(!workdir.join(".git-repo-stash.toml").exists());
}

#[test]
fn reports_repository_failures_in_the_final_summary() {
    let directory = tempfile::tempdir().unwrap();
    let (workdir, source, remote) = create_repositories(directory.path());
    fs::write(source.join("README.md"), "uncommitted change\n").unwrap();
    let (server_url, server) = start_gitea_server(&remote, 3);

    let output = Command::new(env!("CARGO_BIN_EXE_git-repo-stash"))
        .arg(&workdir)
        .args([
            "--gitea-url",
            &server_url,
            "--org",
            "backup",
            "--allow-insecure-http",
            "--mirror",
            "--delete-after-backup",
        ])
        .env("GITEA_TOKEN", "secret")
        .output()
        .unwrap();

    assert!(!output.status.success());
    server.join().unwrap();
    assert!(source.exists());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Backup summary: 0/1 repositories succeeded; 1 failure(s)"));
    assert!(stderr.contains("Failures:\n  - source: source is not safe to delete"));
    assert!(stderr.contains("working tree contains modified, untracked, or ignored files"));
}

fn create_repositories(
    root: &Path,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let workdir = root.join("workdir");
    let source = workdir.join("source");
    let remote = root.join("remote.git");
    fs::create_dir_all(&source).unwrap();
    run(Command::new("git").args(["init", "--quiet"]).arg(&source));
    run(Command::new("git")
        .args(["init", "--quiet", "--bare"])
        .arg(&remote));
    fs::write(source.join("README.md"), "backup test\n").unwrap();
    run(Command::new("git")
        .args(["-C"])
        .arg(&source)
        .args(["add", "README.md"]));
    run(Command::new("git").args(["-C"]).arg(&source).args([
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "--quiet",
        "-m",
        "initial",
    ]));
    (workdir, source, remote)
}

fn start_gitea_server(remote: &Path, expected_requests: usize) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let remote = remote.to_string_lossy().into_owned();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut handled = 0;
        while handled < expected_requests && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let request = read_request(&mut stream);
                    assert!(
                        request
                            .to_ascii_lowercase()
                            .contains("authorization: token secret")
                    );
                    let first_line = request.lines().next().unwrap_or_default();
                    let (status, body) = match first_line {
                        "GET /api/v1/user HTTP/1.1" => (200, r#"{"login":"tester"}"#.to_owned()),
                        "GET /api/v1/orgs/backup HTTP/1.1" => (404, "{}".to_owned()),
                        "POST /api/v1/orgs HTTP/1.1" => (201, "{}".to_owned()),
                        "GET /api/v1/repos/backup/source HTTP/1.1" => (404, "{}".to_owned()),
                        "POST /api/v1/orgs/backup/repos HTTP/1.1" => (
                            {
                                assert!(request.contains(r#""object_format_name":"sha1""#));
                                201
                            },
                            serde_json::json!({
                                "name": "source",
                                "ssh_url": remote,
                                "clone_url": "https://example.com/backup/source.git",
                                "object_format_name": "sha1",
                            })
                            .to_string(),
                        ),
                        other => panic!("unexpected request: {other}"),
                    };
                    write_response(&mut stream, status, &body);
                    handled += 1;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("test server failed: {error}"),
            }
        }
        assert_eq!(handled, expected_requests, "test server timed out");
    });
    (format!("http://{address}"), handle)
}

#[test]
fn askpass_reads_the_scoped_token_environment() {
    let output = Command::new(env!("CARGO_BIN_EXE_git-repo-stash"))
        .arg("Password for HTTPS")
        .env("GIT_REPO_STASH_ASKPASS_MODE", "1")
        .env("GIT_REPO_STASH_ASKPASS_TOKEN", "scoped-secret")
        .env_remove("GITEA_TOKEN")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "scoped-secret\n");
}

fn read_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = Vec::new();
    let mut buffer = [0; 4096];
    let mut expected_length = None;
    loop {
        let count = stream.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..count]);
        if let Some(header_end) = find_bytes(&request, b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            expected_length = Some(header_end + 4 + content_length);
        }
        if expected_length.is_some_and(|length| request.len() >= length) {
            break;
        }
    }
    String::from_utf8(request).unwrap()
}

fn write_response(stream: &mut TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        404 => "Not Found",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    stream.flush().unwrap();
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn run(command: &mut Command) {
    assert!(command.status().unwrap().success());
}
