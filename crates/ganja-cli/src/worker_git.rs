//! Git runs before engine assembly, in a dedicated process group and a temporary
//! checkout. A failed clone can therefore be cleaned without touching other files.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt as _;
use tokio::process::Command;
use tokio::time::{Instant, timeout, timeout_at};
use tokio_util::sync::CancellationToken;

const OUTPUT_CAP: usize = 4096;
const REAP_BOUND: Duration = Duration::from_secs(5);

#[derive(Clone, Default)]
pub(crate) struct GitOptions {
    pub cancel: CancellationToken,
    pub global_config: Option<PathBuf>,
    pub ssl_ca: Option<PathBuf>,
}

/// The validated values that become git arguments, never a shell command.
pub(crate) struct Checkout {
    pub repository: String,
    pub reference: String,
    pub branch: String,
    pub author_name: String,
    pub author_email: String,
    pub git: GitOptions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stage {
    Dispatch,
    Clone,
    Switch,
    Config,
    Deadline,
    Cleanup,
}

#[derive(Debug, thiserror::Error)]
#[error("git bootstrap failed during {stage:?}")]
pub(crate) struct Failure {
    pub stage: Stage,
}

/// Creates the clone and installs it into an otherwise empty worker directory.
pub(crate) async fn prepare(
    root: PathBuf,
    request: Checkout,
    deadline: Instant,
) -> Result<String, Failure> {
    let parsed =
        url::Url::parse(&request.repository).map_err(|_| Failure { stage: Stage::Dispatch })?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || request.reference.is_empty()
        || request.reference.starts_with('-')
        || request.reference.chars().any(char::is_control)
        || request.branch.is_empty()
        || request.branch.starts_with('-')
        || request.author_name.chars().any(char::is_control)
        || request.author_email.chars().any(char::is_control)
        || request.author_name.is_empty()
        || request.author_email.is_empty()
    {
        return Err(Failure { stage: Stage::Dispatch });
    }
    let mut entries =
        tokio::fs::read_dir(&root).await.map_err(|_| Failure { stage: Stage::Dispatch })?;
    if entries.next_entry().await.map_err(|_| Failure { stage: Stage::Dispatch })?.is_some() {
        return Err(Failure { stage: Stage::Dispatch });
    }
    let temporary = tempfile::Builder::new()
        .prefix(".ganja-bootstrap-")
        .tempdir_in(&root)
        .map_err(|_| Failure { stage: Stage::Dispatch })?;
    let directory = temporary.path();
    let header = format!(
        "http.{}/.extraHeader=Authorization: placeholder",
        parsed.origin().ascii_serialization()
    );
    let result = async {
        git(
            directory,
            &["check-ref-format", "--branch", &request.branch],
            deadline,
            Stage::Dispatch,
            &request.git,
        )
        .await?;
        git(
            directory,
            &["-c", &header, "clone", "--no-checkout", "--", &request.repository, "."],
            deadline,
            Stage::Clone,
            &request.git,
        )
        .await?;
        git(
            directory,
            &["-c", &header, "fetch", "--no-tags", "--", "origin", &request.reference],
            deadline,
            Stage::Switch,
            &request.git,
        )
        .await?;
        git(
            directory,
            &["switch", "--create", &request.branch, "FETCH_HEAD"],
            deadline,
            Stage::Switch,
            &request.git,
        )
        .await?;
        for (key, value) in [
            ("user.name", request.author_name.as_str()),
            ("user.email", request.author_email.as_str()),
            ("commit.gpgsign", "false"),
        ] {
            git(
                directory,
                &["config", "--local", key, value],
                deadline,
                Stage::Config,
                &request.git,
            )
            .await?;
        }
        let head =
            git(directory, &["rev-parse", "HEAD"], deadline, Stage::Config, &request.git).await?;
        let head = String::from_utf8(head).map_err(|_| Failure { stage: Stage::Config })?;
        let head = head.trim();
        if !matches!(head.len(), 40 | 64) || !head.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Failure { stage: Stage::Config });
        }
        Ok(head.to_owned())
    }
    .await;
    let head = match result {
        Ok(head) => head,
        Err(error) => {
            temporary.close().map_err(|_| Failure { stage: Stage::Cleanup })?;
            return Err(error);
        }
    };
    let mut installed = Vec::new();
    let mut entries =
        tokio::fs::read_dir(directory).await.map_err(|_| Failure { stage: Stage::Cleanup })?;
    while let Some(entry) =
        entries.next_entry().await.map_err(|_| Failure { stage: Stage::Cleanup })?
    {
        if Instant::now() >= deadline || request.git.cancel.is_cancelled() {
            rollback(&installed).await?;
            temporary.close().map_err(|_| Failure { stage: Stage::Cleanup })?;
            return Err(Failure { stage: Stage::Deadline });
        }
        let destination = root.join(entry.file_name());
        if tokio::fs::rename(entry.path(), &destination).await.is_err() {
            rollback(&installed).await?;
            temporary.close().map_err(|_| Failure { stage: Stage::Cleanup })?;
            return Err(Failure { stage: Stage::Cleanup });
        }
        installed.push(destination);
    }
    temporary.close().map_err(|_| Failure { stage: Stage::Cleanup })?;
    Ok(head)
}

async fn rollback(paths: &[PathBuf]) -> Result<(), Failure> {
    for path in paths {
        let metadata = tokio::fs::symlink_metadata(path)
            .await
            .map_err(|_| Failure { stage: Stage::Cleanup })?;
        let removed = if metadata.is_dir() {
            tokio::fs::remove_dir_all(path).await
        } else {
            tokio::fs::remove_file(path).await
        };
        removed.map_err(|_| Failure { stage: Stage::Cleanup })?;
    }
    Ok(())
}

/// A cancellation guard kills the whole group, not just git's parent process.
struct Group(Option<i32>);

impl Group {
    fn kill(&mut self) {
        if let Some(id) = self.0.take() {
            // SAFETY: this positive pid came from a child launched with process_group(0),
            // so it identifies our own isolated group, never the caller's group.
            unsafe { libc::killpg(id, libc::SIGKILL) };
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
    }
}

async fn git(
    directory: &Path,
    arguments: &[&str],
    deadline: Instant,
    stage: Stage,
    options: &GitOptions,
) -> Result<Vec<u8>, Failure> {
    if Instant::now() >= deadline || options.cancel.is_cancelled() {
        return Err(Failure { stage: Stage::Deadline });
    }
    let mut command = Command::new("git");
    command
        .args(arguments.iter().map(OsString::from))
        .current_dir(directory)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    if let Some(path) = &options.global_config {
        command.env("GIT_CONFIG_GLOBAL", path);
    }
    if let Some(path) = &options.ssl_ca {
        command.env("GIT_SSL_CAINFO", path);
    }
    let mut child = command.spawn().map_err(|_| Failure { stage })?;
    let id = child.id().and_then(|pid| i32::try_from(pid).ok()).ok_or(Failure { stage })?;
    let mut group = Group(Some(id));
    let mut stdout = child.stdout.take().ok_or(Failure { stage })?;
    let mut reader = tokio::spawn(async move {
        let mut result = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let count = stdout.read(&mut buffer).await?;
            if count == 0 {
                break;
            }
            let keep = count.min(OUTPUT_CAP.saturating_sub(result.len()));
            result.extend_from_slice(&buffer[..keep]);
        }
        Ok::<_, std::io::Error>(result)
    });
    let completion = tokio::select! {
        () = options.cancel.cancelled() => None,
        result = timeout_at(deadline, child.wait()) => result.ok(),
    };
    let status = match completion {
        Some(Ok(status)) => status,
        Some(Err(_)) => {
            group.kill();
            reader.abort();
            let _ = reader.await;
            return Err(Failure { stage });
        }
        None => {
            group.kill();
            let reaped = timeout(REAP_BOUND, child.wait()).await;
            reader.abort();
            let _ = reader.await;
            return Err(Failure {
                stage: if matches!(reaped, Ok(Ok(_))) { Stage::Deadline } else { Stage::Cleanup },
            });
        }
    };
    // A child of git may retain the pipe after git exits. End it before joining
    // the reader, otherwise a completed parent could leave this await unbounded.
    group.kill();
    let output = match timeout(REAP_BOUND, &mut reader).await {
        Ok(output) => output.map_err(|_| Failure { stage })?.map_err(|_| Failure { stage })?,
        Err(_) => {
            reader.abort();
            let _ = reader.await;
            return Err(Failure { stage: Stage::Cleanup });
        }
    };
    if !status.success() {
        return Err(Failure { stage });
    }
    Ok(output)
}

#[cfg(test)]
#[path = "worker_git_tests.rs"]
mod tests;
