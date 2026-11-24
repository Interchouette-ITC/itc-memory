//! Deterministic Rust review inputs and Clippy diagnostics.

use std::{
    collections::BTreeSet,
    fmt::Write as _,
    io::Read,
    path::{Component, Path},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, bail};
use serde::Serialize;
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_COMMAND_OUTPUT: usize = 8 * 1024 * 1024;
const MAX_REVIEW_BYTES: usize = 50_000;
const MAX_REVIEW_FILES: usize = 30;

/// Clippy output, including a nonzero exit status and any compiler diagnostics.
#[derive(Debug, Serialize)]
pub struct LintResult {
    /// Whether Cargo completed successfully.
    pub success: bool,
    /// Process exit code, if the process exited normally.
    pub exit_code: Option<i32>,
    /// Cargo compiler-message objects.
    pub diagnostics: Vec<Value>,
    /// Cargo's diagnostic output outside the JSON message stream.
    pub stderr: String,
}

/// Line-numbered source and, when requested, the current Git diff.
#[derive(Debug, Serialize)]
pub struct ReviewSource {
    /// Repository-relative source paths and their line-numbered contents.
    pub files: Vec<SourceFile>,
    /// Changed paths absent from the working tree; their changes remain in the diff.
    pub missing_files: Vec<String>,
    /// Separate staged and unstaged Git diffs.
    pub diff: Option<String>,
}

/// One source file selected for review.
#[derive(Debug, Serialize)]
pub struct SourceFile {
    /// Repository-relative path.
    pub path: String,
    /// Line-numbered source text.
    pub content: String,
}

/// Runs Clippy for the checkout's Cargo workspace.
///
/// # Errors
/// Returns an error when the command cannot be started, is cancelled, times out,
/// exceeds output limits, or returns malformed compiler messages.
pub async fn lint(root: &Path, cancellation: &CancellationToken) -> anyhow::Result<LintResult> {
    let output = run_command(
        root,
        "cargo",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--message-format=json",
        ],
        cancellation,
    )
    .await?;
    let stdout = std::str::from_utf8(&output.stdout).context("Clippy output is not UTF-8")?;
    let mut diagnostics = Vec::new();
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let message: Value = serde_json::from_str(line).context("invalid Cargo JSON message")?;
        if message.get("reason").and_then(Value::as_str) == Some("compiler-message") {
            diagnostics.push(
                message
                    .get("message")
                    .context("Cargo compiler message is missing its diagnostic")?
                    .clone(),
            );
        }
    }
    Ok(LintResult {
        success: output.status.success(),
        exit_code: output.status.code(),
        diagnostics,
        stderr: String::from_utf8(output.stderr).context("Clippy stderr is not UTF-8")?,
    })
}

/// Retrieves explicitly named Rust files from a checkout.
///
/// # Errors
/// Returns an error for invalid, inaccessible, out-of-scope, or oversized files.
pub fn explicit_files(root: &Path, paths: &[String]) -> anyhow::Result<ReviewSource> {
    if paths.is_empty() {
        bail!("files scope requires at least one Rust file");
    }
    build_source(root, paths.iter().map(String::as_str), None)
}

/// Retrieves changed Rust files and separate staged and unstaged Git diffs.
///
/// # Errors
/// Returns an error if Git cannot read the changes or selected files are invalid.
pub async fn changed_files(
    root: &Path,
    cancellation: &CancellationToken,
) -> anyhow::Result<ReviewSource> {
    let unstaged_names = git(
        root,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--name-only",
            "--diff-filter=ACMRTU",
            "-z",
            "--",
            "*.rs",
        ],
        cancellation,
    )
    .await?;
    let staged_names = git(
        root,
        &[
            "diff",
            "--cached",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--name-only",
            "--diff-filter=ACMRTU",
            "-z",
            "--",
            "*.rs",
        ],
        cancellation,
    )
    .await?;
    let untracked = git(
        root,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            "*.rs",
        ],
        cancellation,
    )
    .await?;
    let unstaged_diff = git(
        root,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--",
            "*.rs",
        ],
        cancellation,
    )
    .await?;
    let staged_diff = git(
        root,
        &[
            "diff",
            "--cached",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--",
            "*.rs",
        ],
        cancellation,
    )
    .await?;
    let unstaged_names =
        std::str::from_utf8(&unstaged_names).context("changed file paths are not UTF-8")?;
    let staged_names =
        std::str::from_utf8(&staged_names).context("staged file paths are not UTF-8")?;
    let untracked =
        std::str::from_utf8(&untracked).context("untracked file paths are not UTF-8")?;
    let unstaged_diff =
        String::from_utf8(unstaged_diff).context("unstaged Git diff is not UTF-8")?;
    let staged_diff = String::from_utf8(staged_diff).context("staged Git diff is not UTF-8")?;
    let paths = unstaged_names
        .split('\0')
        .chain(staged_names.split('\0'))
        .chain(untracked.split('\0'))
        .filter(|path| !path.is_empty())
        .collect::<BTreeSet<_>>();
    if paths.is_empty() && unstaged_diff.is_empty() && staged_diff.is_empty() {
        bail!("no Rust changes found in the checkout");
    }
    let diff = format!("Staged changes:\n{staged_diff}\nUnstaged changes:\n{unstaged_diff}");
    build_source(root, paths, Some(diff))
}

fn build_source<'a>(
    root: &Path,
    paths: impl IntoIterator<Item = &'a str>,
    diff: Option<String>,
) -> anyhow::Result<ReviewSource> {
    let root = root.canonicalize().context("resolve repository root")?;
    let mut files = Vec::new();
    let mut missing_files = Vec::new();
    let mut total = diff.as_ref().map_or(0, String::len);
    let unique_paths = paths.into_iter().collect::<BTreeSet<_>>();
    for path in unique_paths {
        if files.len() + missing_files.len() == MAX_REVIEW_FILES {
            bail!("review scope exceeds {MAX_REVIEW_FILES} files");
        }
        let relative = Path::new(path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
            || relative
                .extension()
                .is_none_or(|extension| extension != "rs")
        {
            bail!("review path must be a repository-relative Rust file: {path}");
        }
        let mut ancestor = root.clone();
        if let Some(parent) = relative.parent() {
            for component in parent.components() {
                ancestor.push(component);
                if ancestor.join(".git").exists() {
                    bail!("review path belongs to a nested checkout: {path}");
                }
            }
        }
        if diff.is_some() {
            match std::fs::symlink_metadata(root.join(relative)) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    total = total.saturating_add(path.len());
                    missing_files.push(path.to_owned());
                    continue;
                }
                Err(error) => return Err(error).with_context(|| format!("inspect {path}")),
                Ok(_) => {}
            }
        }
        let resolved = root
            .join(relative)
            .canonicalize()
            .with_context(|| format!("resolve {path}"))?;
        if !resolved.starts_with(&root) || !resolved.is_file() {
            bail!("review path must resolve to a file inside the checkout: {path}");
        }
        let mut canonical_parent = resolved.parent();
        while let Some(parent) = canonical_parent {
            if parent == root {
                break;
            }
            if parent.join(".git").exists() {
                bail!("review path belongs to a nested checkout: {path}");
            }
            canonical_parent = parent.parent();
        }
        let mut bytes = Vec::new();
        std::fs::File::open(&resolved)
            .with_context(|| format!("open {path}"))?
            .take(u64::try_from(MAX_REVIEW_BYTES + 1)?)
            .read_to_end(&mut bytes)
            .with_context(|| format!("read {path}"))?;
        if bytes.len() > MAX_REVIEW_BYTES {
            bail!("review file exceeds {MAX_REVIEW_BYTES} bytes: {path}");
        }
        let source = String::from_utf8(bytes).with_context(|| format!("{path} is not UTF-8"))?;
        let mut content = String::new();
        for (index, line) in source.lines().enumerate() {
            writeln!(content, "{}: {line}", index + 1)?;
        }
        total = total
            .saturating_add(path.len())
            .saturating_add(content.len());
        if total > MAX_REVIEW_BYTES {
            bail!("review scope exceeds {MAX_REVIEW_BYTES} bytes; select fewer files");
        }
        files.push(SourceFile {
            path: path.to_owned(),
            content,
        });
    }
    if total > MAX_REVIEW_BYTES {
        bail!("review scope exceeds {MAX_REVIEW_BYTES} bytes; select fewer changes");
    }
    Ok(ReviewSource {
        files,
        missing_files,
        diff,
    })
}

async fn git(
    root: &Path,
    args: &[&str],
    cancellation: &CancellationToken,
) -> anyhow::Result<Vec<u8>> {
    let output = run_command(root, "git", args, cancellation).await?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout)
}

struct CommandOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn read_bounded(mut reader: impl AsyncRead + Unpin) -> anyhow::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = vec![0; 8192];
    let mut overflow = false;
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        if output.len().saturating_add(count) <= MAX_COMMAND_OUTPUT {
            output.extend_from_slice(&buffer[..count]);
        } else {
            overflow = true;
        }
    }
    if overflow {
        bail!("command output exceeded {MAX_COMMAND_OUTPUT} bytes");
    }
    Ok(output)
}

async fn run_command(
    root: &Path,
    program: &str,
    args: &[&str],
    cancellation: &CancellationToken,
) -> anyhow::Result<CommandOutput> {
    if cancellation.is_cancelled() {
        bail!("{program} was cancelled");
    }
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(root)
        .env("CARGO_TERM_COLOR", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if program == "git" {
        command.env("GIT_CONFIG_NOSYSTEM", "1");
        command.env("GIT_OPTIONAL_LOCKS", "0");
    }
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("start {program}"))?;
    let child_id = child.id().context("spawned process has no ID")?;
    let stdout = child.stdout.take().context("missing command stdout")?;
    let stderr = child.stderr.take().context("missing command stderr")?;
    let finished = tokio::select! {
        biased;
        () = cancellation.cancelled() => None,
        result = tokio::time::timeout(COMMAND_TIMEOUT, async {
            let (status, stdout, stderr) = tokio::try_join!(
                async { child.wait().await.map_err(anyhow::Error::from) },
                read_bounded(stdout),
                read_bounded(stderr),
            )?;
            Ok::<_, anyhow::Error>(CommandOutput { status, stdout, stderr })
        }) => Some(result),
    };
    match finished {
        Some(Ok(Ok(output))) => Ok(output),
        other => {
            terminate_process_group(child_id)?;
            let _ = child.kill().await;
            match other {
                None => bail!("{program} was cancelled"),
                Some(Err(_)) => bail!(
                    "{program} timed out after {} seconds",
                    COMMAND_TIMEOUT.as_secs()
                ),
                Some(Ok(Err(error))) => {
                    Err(error.context(format!("{program} failed while collecting output")))
                }
                Some(Ok(Ok(_))) => unreachable!(),
            }
        }
    }
}

#[cfg(unix)]
fn terminate_process_group(id: u32) -> anyhow::Result<()> {
    use nix::{
        errno::Errno,
        sys::signal::{Signal, killpg},
        unistd::Pid,
    };
    let id = i32::try_from(id).context("process ID is out of range")?;
    match killpg(Pid::from_raw(id), Signal::SIGKILL) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(unix))]
fn terminate_process_group(_id: u32) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    struct Fixture(PathBuf);

    use std::path::PathBuf;

    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("itc-rust-review-{}", Uuid::new_v4()));
            fs::create_dir_all(path.join("src")).unwrap();
            fs::write(
                path.join("Cargo.toml"),
                "[package]\nname = \"review-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            )
            .unwrap();
            Self(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn explicit_scope_numbers_lines_and_rejects_escape_and_nested_checkouts() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("src/lib.rs"), "fn one() {}\nfn two() {}\n").unwrap();
        let source = explicit_files(&fixture.0, &["src/lib.rs".to_owned()]).unwrap();
        assert_eq!(source.files[0].content, "1: fn one() {}\n2: fn two() {}\n");
        assert!(explicit_files(&fixture.0, &["../outside.rs".to_owned()]).is_err());
        fs::create_dir_all(fixture.0.join("nested/.git")).unwrap();
        fs::write(fixture.0.join("nested/code.rs"), "fn nested() {}\n").unwrap();
        assert!(explicit_files(&fixture.0, &["nested/code.rs".to_owned()]).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", fixture.0.join("src/outside.rs")).unwrap();
            assert!(explicit_files(&fixture.0, &["src/outside.rs".to_owned()]).is_err());
        }
        fs::write(
            fixture.0.join("src/large.rs"),
            "x".repeat(MAX_REVIEW_BYTES + 1),
        )
        .unwrap();
        let error = explicit_files(&fixture.0, &["src/large.rs".to_owned()]).unwrap_err();
        assert!(error.to_string().contains("exceeds"));
    }

    #[tokio::test]
    async fn clippy_reports_success_and_compiler_errors() {
        let fixture = Fixture::new();
        let token = CancellationToken::new();
        fs::write(
            fixture.0.join("src/lib.rs"),
            "pub fn answer() -> u32 { 42 }\n",
        )
        .unwrap();
        let success = lint(&fixture.0, &token).await.unwrap();
        assert!(success.success, "{}", success.stderr);
        fs::write(
            fixture.0.join("src/lib.rs"),
            "pub fn answer() -> u32 { \"not a number\" }\n",
        )
        .unwrap();
        let failure = lint(&fixture.0, &token).await.unwrap();
        assert!(!failure.success);
        assert!(
            failure
                .diagnostics
                .iter()
                .any(|message| { message.get("level").and_then(Value::as_str) == Some("error") })
        );
    }

    #[tokio::test]
    async fn changed_scope_includes_staged_unstaged_and_untracked_rust() {
        let fixture = Fixture::new();
        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&fixture.0)
                .status()
                .unwrap();
            assert!(status.success());
        };
        run_git(&["init", "--quiet"]);
        fs::write(fixture.0.join("src/lib.rs"), "pub fn staged() {}\n").unwrap();
        run_git(&["add", "src/lib.rs"]);
        fs::write(fixture.0.join("src/lib.rs"), "pub fn unstaged() {}\n").unwrap();
        fs::write(fixture.0.join("src/new.rs"), "pub fn untracked() {}\n").unwrap();

        let source = changed_files(&fixture.0, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            source
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/lib.rs", "src/new.rs"]
        );
        let diff = source.diff.unwrap();
        assert!(diff.contains("pub fn staged()"));
        assert!(diff.contains("pub fn unstaged()"));
        fs::remove_file(fixture.0.join("src/lib.rs")).unwrap();
        let deleted = changed_files(&fixture.0, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(deleted.missing_files, vec!["src/lib.rs"]);
        assert!(deleted.diff.unwrap().contains("pub fn staged()"));
    }

    #[tokio::test]
    async fn command_cancellation_is_reported() {
        let fixture = Fixture::new();
        let token = CancellationToken::new();
        token.cancel();
        let error = run_command(&fixture.0, "cargo", &["--version"], &token)
            .await
            .err()
            .expect("cancelled command must fail");
        assert!(error.to_string().contains("cancelled"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_stops_a_running_process_group() {
        let fixture = Fixture::new();
        let token = CancellationToken::new();
        let task_token = token.clone();
        let root = fixture.0.clone();
        let task = tokio::spawn(async move {
            run_command(&root, "sh", &["-c", "sleep 30"], &task_token).await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        token.cancel();
        let error = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("cancelled command did not stop")
            .unwrap()
            .err()
            .expect("cancelled command must fail");
        assert!(error.to_string().contains("cancelled"));
    }
}
