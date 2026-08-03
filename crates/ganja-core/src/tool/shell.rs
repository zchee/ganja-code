//! The `bash` tool: runs a command in a shell.
//!
//! Spec: upstream `packages/opencode/src/tool/shell.ts`, the prompt beside it
//! in `tool/shell/`, and `packages/core/src/shell.ts` for shell selection and
//! the kill sequence. The tool id is `bash` because that is what upstream
//! registers — `ShellID.ToolID` is pinned to `bash` there for compatibility
//! with saved permissions, and a model calling `shell` would be calling a name
//! no upstream transcript contains.
//!
//! A command gets its own process group. Whatever it forks stays inside that
//! group, so a cancel or a timeout can end the whole tree instead of orphaning
//! the interesting half of it — the shell exiting says nothing about the
//! `make -j8` still running underneath it.

use std::{
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::{
    io::{AsyncRead, AsyncReadExt as _},
    process::{Child, Command},
};

use crate::tool::{Tool, ToolCtx, ToolError, ToolOutput, truncate};

/// How long a command runs before it is killed, when the call names no
/// timeout. Upstream's `2 * 60 * 1000`.
const DEFAULT_TIMEOUT: Duration = Duration::from_millis(2 * 60 * 1000);

/// How long the tree is given to wind itself up after `SIGTERM` before it is
/// killed outright. Upstream's `SIGKILL_TIMEOUT_MS`. Only a process group can
/// be asked to wind itself up, so only the unix path has a use for it.
#[cfg(unix)]
const KILL_GRACE: Duration = Duration::from_millis(200);

/// How long output is still collected after the command exits.
///
/// Whatever the command wrote last is already in the pipe by then, so this is
/// microseconds of work in practice. It is bounded rather than awaited to EOF
/// because a backgrounded grandchild inherits the pipe and can hold it open
/// long after the command itself is done — upstream returns on the exit code
/// for the same reason, and waiting for EOF would turn `make &` into a
/// two-minute timeout.
const DRAIN_GRACE: Duration = Duration::from_millis(100);

/// Longest command echoed in a one-line description.
const DESCRIBE_LIMIT: usize = 80;

/// What the model passes to `bash`.
#[derive(Debug, Deserialize, JsonSchema)]
struct Args {
    /// The command to execute
    command: String,
    /// Optional timeout in milliseconds
    #[serde(default)]
    timeout: Option<i64>,
    /// The working directory to run the command in. Defaults to the current directory. Use this instead of 'cd' commands.
    #[serde(default)]
    workdir: Option<String>,
}

/// How a command stopped running.
enum Ended {
    /// It finished on its own.
    Exit(ExitStatus),
    /// The turn was cancelled while it ran.
    Cancelled,
    /// It outlived its timeout.
    TimedOut,
}

/// Runs shell commands.
pub struct ShellTool {
    /// Rendered once, because it names the shell and the limits in force.
    description: String,
    /// The shell binary commands run under.
    shell: PathBuf,
}

impl ShellTool {
    /// Builds the tool around the shell this machine offers.
    #[must_use]
    pub fn new() -> Self {
        let shell = default_shell();
        let name = shell
            .file_name()
            .unwrap_or(shell.as_os_str())
            .to_string_lossy()
            .into_owned();

        Self {
            description: describe_tool(&name),
            shell,
        }
    }
}

impl Default for ShellTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn id(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    fn describe(&self, args: &serde_json::Value) -> String {
        let command = args
            .get("command")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();

        format!("shell: {}", shorten(command, DESCRIBE_LIMIT))
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;
        let timeout = match args.timeout {
            // Upstream's own wording, because the message is what the model
            // reads before it retries.
            Some(timeout) if timeout <= 0 => {
                return Err(ToolError::InvalidArgs(format!(
                    "Invalid timeout value: {timeout}. Timeout must be a positive number."
                )));
            }
            Some(timeout) => Duration::from_millis(timeout.unsigned_abs()),
            None => DEFAULT_TIMEOUT,
        };
        let cwd = match args.workdir.as_deref() {
            Some(workdir) if Path::new(workdir).is_absolute() => PathBuf::from(workdir),
            Some(workdir) => ctx.cwd.join(workdir),
            None => ctx.cwd.clone(),
        };

        let mut child = self.spawn(&args.command, &cwd)?;

        // Both pipes are drained the whole time the command runs: a command
        // that writes more than a pipe buffer holds would otherwise block on
        // its own output and never reach the exit this races for.
        let collected = Arc::new(Mutex::new(Vec::new()));
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let mut pumps = tokio::spawn({
            let out = Arc::clone(&collected);
            let err = Arc::clone(&collected);
            async move {
                tokio::join!(pump(stdout, out), pump(stderr, err));
            }
        });

        let ended = tokio::select! {
            status = child.wait() => match status {
                Ok(status) => Ended::Exit(status),
                Err(error) => {
                    kill_tree(&mut child).await;
                    return Err(ToolError::Failed(format!("the command could not be waited on: {error}")));
                }
            },
            () = ctx.cancel.cancelled() => Ended::Cancelled,
            () = tokio::time::sleep(timeout) => Ended::TimedOut,
        };

        if !matches!(ended, Ended::Exit(_)) {
            kill_tree(&mut child).await;
            // Reaping is what keeps a cancelled turn from leaving a zombie
            // behind for every command it interrupted.
            let _ = child.wait().await;
        }

        if tokio::time::timeout(DRAIN_GRACE, &mut pumps).await.is_err() {
            pumps.abort();
        }

        if matches!(ended, Ended::Cancelled) {
            return Err(ToolError::Cancelled);
        }

        let raw = collected
            .lock()
            .expect("the output buffer is never poisoned")
            .clone();
        // Lossy for the same reason upstream's `TextDecoder` is: a command is
        // free to write bytes that are not text, and losing a byte beats
        // failing a call that otherwise worked.
        let clamped = truncate::clamp(&String::from_utf8_lossy(&raw));
        let mut output = if clamped.text.is_empty() {
            "(no output)".to_owned()
        } else {
            clamped.text
        };

        if matches!(ended, Ended::TimedOut) {
            output.push_str(&format!(
                "\n\n<shell_metadata>\nshell tool terminated command after exceeding timeout {} ms. \
                 If this command is expected to take longer and is not waiting for interactive \
                 input, retry with a larger timeout value in milliseconds.\n</shell_metadata>",
                timeout.as_millis()
            ));
        }

        let exit = match ended {
            Ended::Exit(status) => status.code(),
            _ => None,
        };

        Ok(ToolOutput {
            title: args.command,
            output,
            metadata: serde_json::json!({
                "exit": exit,
                "truncated": clamped.truncated,
            }),
        })
    }
}

impl ShellTool {
    /// Starts `command` under the shell, in its own process group.
    fn spawn(&self, command: &str, cwd: &Path) -> Result<Child, ToolError> {
        let mut spawner = Command::new(&self.shell);
        spawner
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            // Nothing is ever typed at a tool call, and a command that waits
            // for input on an inherited terminal would hang until its timeout.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // The group is what makes the tree killable. `0` asks for a new group
        // led by the child, so its pid doubles as the group id and a negative
        // pid signals everything the command forked.
        #[cfg(unix)]
        spawner.process_group(0);

        spawner.spawn().map_err(|error| {
            ToolError::Failed(format!(
                "{} could not run the command in {}: {error}",
                self.shell.display(),
                cwd.display()
            ))
        })
    }
}

/// Ends the process tree the command started.
///
/// Upstream's `killTree` sequence: `SIGTERM` to the group, a short grace, then
/// `SIGKILL` to whatever ignored it. The standard library only ever kills one
/// process, so ending the group takes `killpg`.
#[cfg(unix)]
async fn kill_tree(child: &mut Child) {
    let Some(pid) = child.id() else {
        // Already reaped; there is no group left to name.
        return;
    };

    signal_group(pid, libc::SIGTERM);
    tokio::time::sleep(KILL_GRACE).await;

    if matches!(child.try_wait(), Ok(None)) {
        signal_group(pid, libc::SIGKILL);
    }
}

/// Sends `signal` to the process group led by `pid`.
#[cfg(unix)]
fn signal_group(pid: u32, signal: libc::c_int) {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        // No pid a kernel hands out is this large, so nothing sensible is
        // being asked for; signalling a truncated value would be worse than
        // signalling nothing.
        return;
    };

    // SAFETY: `killpg` reads no memory and owns no resource — it takes two
    // integers and returns one — so the invariant that matters is which group
    // gets signalled rather than anything about pointers.
    //
    // `pid` is the group. It comes from `Child::id`, and the child was spawned
    // with `process_group(0)`, which makes it the leader of a fresh group whose
    // id equals its pid; that group holds the shell and its descendants and
    // nothing else. `Child::id` returns `None` once the child has been reaped,
    // so reaching here means it has not been — and an unreaped pid cannot have
    // been recycled onto some unrelated process. The reap happens in `run`,
    // strictly after this returns.
    //
    // A failure is not worth reporting: `ESRCH` means the group is already
    // gone, which is the outcome being asked for, and `EPERM` cannot arise for
    // a group this process created.
    unsafe {
        libc::killpg(pid, signal);
    }
}

/// Windows has no process group to signal, so the direct child is all this can
/// reach until job objects are wired up.
#[cfg(not(unix))]
async fn kill_tree(child: &mut Child) {
    let _ = child.start_kill();
}

/// Appends everything `reader` produces to `sink`.
///
/// Both pipes share one buffer, so stdout and stderr interleave in the order
/// they arrived — which is what a terminal would have shown, and what
/// upstream's merged stream carries.
async fn pump<R: AsyncRead + Unpin>(mut reader: R, sink: Arc<Mutex<Vec<u8>>>) {
    let mut chunk = [0_u8; 8192];

    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => sink
                .lock()
                .expect("the output buffer is never poisoned")
                .extend_from_slice(&chunk[..read]),
        }
    }
}

/// The shell commands run under.
///
/// Upstream's `fallback()`: zsh on macOS, otherwise bash where it exists, and
/// `/bin/sh` where it does not. The configured shell is not consulted yet
/// because nothing in this port writes that config.
fn default_shell() -> PathBuf {
    if cfg!(target_os = "macos") {
        return PathBuf::from("/bin/zsh");
    }

    for candidate in ["/bin/bash", "/usr/bin/bash"] {
        if Path::new(candidate).is_file() {
            return PathBuf::from(candidate);
        }
    }

    PathBuf::from("/bin/sh")
}

/// The prompt the model is given, with the machine's own details filled in.
fn describe_tool(shell: &str) -> String {
    let command_section = render(
        include_str!("shell_posix.txt").trim_end(),
        &[
            ("chain", CHAIN),
            ("maxLines", &truncate::MAX_LINES.to_string()),
            ("maxBytes", &truncate::MAX_CHARS.to_string()),
            ("defaultTimeoutMs", &DEFAULT_TIMEOUT.as_millis().to_string()),
        ],
    );

    render(
        include_str!("shell.txt"),
        &[
            ("intro", INTRO),
            ("os", std::env::consts::OS),
            ("shell", shell),
            ("workdirSection", WORKDIR_SECTION),
            ("tmp", &std::env::temp_dir().display().to_string()),
            ("commandSection", &command_section),
        ],
    )
}

/// Upstream's `profile()` for a posix shell, whose fields are short enough to
/// live here rather than in a prompt file of their own.
const INTRO: &str = "Executes a given bash command in a persistent shell session with optional \
                     timeout, ensuring proper handling and security measures.";

/// How the prompt tells the model to change directory.
const WORKDIR_SECTION: &str = "All commands run in the current working directory by default. Use \
                               the `workdir` parameter if you need to run a command in a different \
                               directory. AVOID using `cd <directory> && <command>` patterns - use \
                               `workdir` instead.";

/// How the prompt tells the model to sequence dependent commands.
const CHAIN: &str = "If the commands depend on each other and must run sequentially, use a single \
                     Bash call with '&&' to chain them together (e.g., `git add . && git commit -m \
                     \"message\" && git push`). For instance, if one operation must complete before \
                     another starts (like mkdir before cp, Write before Bash for git operations, or \
                     git add before git commit), run these operations sequentially instead.";

/// Fills `${name}` placeholders in an upstream prompt.
///
/// The prompt files are byte-for-byte copies of upstream's, so the
/// substitution upstream does in TypeScript has to happen here instead.
fn render(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = template.to_owned();

    for (name, value) in values {
        out = out.replace(&format!("${{{name}}}"), value);
    }

    out
}

/// `text` cut to `limit` characters, saying so when anything was cut.
fn shorten(text: &str, limit: usize) -> String {
    let flattened = text.replace('\n', " ");

    if flattened.chars().count() <= limit {
        return flattened;
    }

    let kept: String = flattened.chars().take(limit).collect();

    format!("{kept}...")
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::Arc,
        time::{Duration, Instant},
    };

    use tokio_util::sync::CancellationToken;

    use super::{DEFAULT_TIMEOUT, ShellTool};
    use crate::tool::{FileTimes, Tool, ToolCtx, ToolError};

    /// A context rooted at `cwd`, with a cancel nobody has pulled.
    fn ctx(cwd: PathBuf) -> ToolCtx {
        ToolCtx {
            cwd,
            cancel: CancellationToken::new(),
            call_id: "call-1".to_owned(),
            files: Arc::new(FileTimes::default()),
        }
    }

    #[tokio::test]
    async fn the_prompt_names_the_shell_the_limits_and_the_default_timeout() {
        let tool = ShellTool::new();
        let description = tool.description();

        assert_eq!(tool.id(), "bash", "upstream registers this tool as `bash`");
        assert!(
            !description.contains("${"),
            "an unfilled placeholder reached the model: {description}"
        );
        assert!(
            description.contains(&format!("{}ms", DEFAULT_TIMEOUT.as_millis())),
            "the prompt should name the timeout it enforces: {description}"
        );
        assert!(
            description.contains(&crate::tool::truncate::MAX_LINES.to_string()),
            "the prompt should name the output budget it enforces: {description}"
        );
        assert!(
            description.contains("Be aware: OS:"),
            "the ported prompt should survive rendering intact: {description}"
        );
    }

    #[tokio::test]
    async fn both_streams_are_captured_in_the_order_they_arrived() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let tool = ShellTool::new();

        let out = tool
            .run(
                serde_json::json!({
                    "command": "printf 'to stdout\\n'; printf 'to stderr\\n' >&2",
                }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a command that runs has output");

        assert!(
            out.output.contains("to stdout") && out.output.contains("to stderr"),
            "both streams belong in one transcript, got {:?}",
            out.output
        );
        assert_eq!(out.metadata["exit"], 0);
        assert_eq!(
            out.title, "printf 'to stdout\\n'; printf 'to stderr\\n' >&2",
            "the title is the command, as upstream reports it"
        );
    }

    #[tokio::test]
    async fn a_command_that_says_nothing_says_so() {
        let dir = tempfile::tempdir().expect("a scratch directory");

        let out = ShellTool::new()
            .run(
                serde_json::json!({ "command": "true" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a silent command still succeeds");

        assert_eq!(out.output, "(no output)");
    }

    #[tokio::test]
    async fn a_nonzero_exit_is_reported_rather_than_swallowed() {
        let dir = tempfile::tempdir().expect("a scratch directory");

        let out = ShellTool::new()
            .run(
                serde_json::json!({ "command": "printf 'nope\\n' >&2; exit 3" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a failing command is still a completed call");

        assert_eq!(
            out.metadata["exit"], 3,
            "the exit code is how the model learns the command failed: {:?}",
            out.metadata
        );
        assert!(out.output.contains("nope"));
    }

    #[tokio::test]
    async fn commands_run_where_the_call_asked_them_to() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).expect("the fixture makes a directory");
        let tool = ShellTool::new();

        let rooted = tool
            .run(
                serde_json::json!({ "command": "pwd" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("pwd runs");
        let relative = tool
            .run(
                serde_json::json!({ "command": "pwd", "workdir": "nested" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("pwd runs");

        // The temporary directory is reached through a symlink on macOS, so
        // both sides are resolved before they are compared.
        let canonical = |text: &str| {
            std::fs::canonicalize(text.trim()).expect("the directory the shell reported exists")
        };
        assert_eq!(
            canonical(&rooted.output),
            std::fs::canonicalize(dir.path()).expect("the scratch directory exists")
        );
        assert_eq!(
            canonical(&relative.output),
            std::fs::canonicalize(&nested).expect("the nested directory exists")
        );
    }

    #[tokio::test]
    async fn a_timeout_kills_the_command_and_everything_it_forked() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let marker = dir.path().join("grandchild-was-here");
        // The backgrounded sleep is a grandchild of the tool: the shell forks
        // it and would leave it running if only the shell were killed. It
        // announces itself a second in, which is long after the timeout below.
        let command = format!("(sleep 1; touch {}) & sleep 30", marker.display());

        let started = Instant::now();
        let out = ShellTool::new()
            .run(
                serde_json::json!({ "command": command, "timeout": 200 }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a timeout is a completed call carrying what did arrive");
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(3),
            "the timeout should end the command promptly, took {elapsed:?}"
        );
        assert!(
            out.output.contains("<shell_metadata>")
                && out.output.contains("exceeding timeout 200 ms"),
            "the model has to be told why the output stops: {:?}",
            out.output
        );

        // Long enough that the grandchild would have written the marker had it
        // survived the group kill.
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert!(
            !marker.exists(),
            "a backgrounded grandchild outlived the timeout that killed its parent"
        );
    }

    #[tokio::test]
    async fn a_cancel_ends_the_command_and_reports_the_cancel() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let context = ctx(dir.path().to_owned());
        let cancel = context.cancel.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        });

        let started = Instant::now();
        let refused = ShellTool::new()
            .run(serde_json::json!({ "command": "sleep 30" }), &context)
            .await
            .expect_err("a cancelled call has no output to report");
        let elapsed = started.elapsed();

        assert!(matches!(refused, ToolError::Cancelled), "got {refused:?}");
        assert!(
            elapsed < Duration::from_secs(3),
            "a cancel should not wait out the command, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn a_timeout_that_is_not_a_duration_is_refused_with_the_remedy() {
        let dir = tempfile::tempdir().expect("a scratch directory");

        let refused = ShellTool::new()
            .run(
                serde_json::json!({ "command": "true", "timeout": -1 }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect_err("a negative timeout is not a timeout");

        assert!(
            matches!(&refused, ToolError::InvalidArgs(message) if message.contains("positive number")),
            "got {refused:?}"
        );
    }

    #[tokio::test]
    async fn a_call_without_a_command_is_refused_before_anything_runs() {
        let dir = tempfile::tempdir().expect("a scratch directory");

        let refused = ShellTool::new()
            .run(serde_json::json!({}), &ctx(dir.path().to_owned()))
            .await
            .expect_err("there is nothing to run");

        assert!(
            matches!(refused, ToolError::InvalidArgs(_)),
            "got {refused:?}"
        );
    }

    #[test]
    fn the_one_line_description_names_the_command_without_pasting_it_whole() {
        let tool = ShellTool::new();

        assert_eq!(
            tool.describe(&serde_json::json!({ "command": "git status" })),
            "shell: git status"
        );

        let long = tool.describe(&serde_json::json!({ "command": "x".repeat(500) }));
        assert!(long.starts_with("shell: xxx") && long.ends_with("..."));
        assert!(long.chars().count() < 100, "got {long}");
    }

    #[test]
    fn the_schema_asks_for_a_command_and_offers_the_optional_arguments() {
        let schema = serde_json::to_value(ShellTool::new().schema()).expect("a schema is JSON");

        assert_eq!(schema["required"], serde_json::json!(["command"]));
        for name in ["command", "timeout", "workdir"] {
            assert!(
                schema["properties"][name].is_object(),
                "the schema should offer {name}: {schema}"
            );
        }
        assert!(
            schema["properties"]["command"]["description"]
                .as_str()
                .is_some_and(|text| text.contains("execute")),
            "the argument descriptions are what the model reads: {schema}"
        );
    }
}
