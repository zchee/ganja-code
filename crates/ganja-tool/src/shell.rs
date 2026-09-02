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

use std::collections::VecDeque;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::{Tool, ToolCtx, ToolError, ToolOutput, job, truncate};

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
///
/// `pub(crate)` because a shell command is no longer the only free text a
/// description line is built from: [`crate::tasklist`] titles a call on a
/// subject another member wrote, which is under no cap at all until it reaches
/// one. One limit rather than two, for [`crate::list_sessions::neutralize`]'s
/// reason — a second number here would be a spelling to keep in step for no
/// boundary at all.
pub(crate) const DESCRIBE_LIMIT: usize = 80;

/// How much of a running command's output is held in memory.
///
/// Upstream's `keep = limits.maxBytes * 2` (`tool/shell.ts`, `run`). A
/// command is free to write more than a machine has memory for — `yes` under
/// the two-minute default timeout is gigabytes — and what the model is
/// eventually shown is the *end* of the output anyway, so only the most
/// recent chunks are worth keeping. Everything else has already gone to the
/// spill file by the time it is dropped.
const KEEP: usize = truncate::MAX_CHARS * 2;

/// How much output accumulates before it starts going to disk instead.
///
/// Upstream's `if (Buffer.byteLength(full, "utf-8") > limits.maxBytes)` — one
/// result's whole budget. Below it a command's output never touches the disk
/// at all, which is the overwhelmingly common case.
const SPILL_THRESHOLD: usize = truncate::MAX_CHARS;

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
    /// Set to true to run this command in the background. Returns immediately
    /// with a shell id instead of waiting for the command to finish; poll
    /// bash_output with that id for new output. timeout is ignored when this
    /// is true — see [`ShellTool::run_reporting`]'s background branch.
    #[serde(default)]
    run_in_background: Option<bool>,
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
    /// The shell binary commands run under, or why this machine offers none.
    ///
    /// Held as the refusal rather than resolved to one at the call, so that a
    /// machine with no POSIX shell says so once, in the same words, to every
    /// call — and says it as a tool result the model reads and can act on,
    /// which is what every other failure in this crate does.
    shell: Result<PathBuf, NoPosixShell>,
    /// Where an overflowing command's output is spilled, when it must not go
    /// where [`truncate::open_spill`] would put it.
    ///
    /// Only a test ever sets this, through `ShellTool::spilling_into` — gated
    /// `#[cfg(test)]`, so there is no item here to link: a
    /// test that spilled into the resolved data directory would fill a real
    /// person's `~/.local/share` with fixtures, which `tests/AGENTS.md`
    /// forbids in as many words. Every other build leaves it empty and the
    /// location is resolved per call.
    spill_dir: Option<PathBuf>,
}

impl ShellTool {
    /// The registry id, which is also the permission key. `bash`, not `shell`:
    /// upstream pins it for compatibility with saved permissions, and so does
    /// this. Spelled as a constant because the `!` passthrough builds a part
    /// carrying it without going through the registry.
    pub const ID: &'static str = "bash";

    /// Builds the tool around the shell this machine offers.
    #[must_use]
    pub fn new() -> Self {
        let shell = default_shell();
        let name = match &shell {
            Ok(shell) => {
                shell.file_name().unwrap_or(shell.as_os_str()).to_string_lossy().into_owned()
            }
            // The prompt is rendered once, and where there is no shell there is
            // nothing truthful to name: printing the refused one would tell the
            // model its commands run under PowerShell, which is exactly what
            // will not happen. Every call answers with the reason anyway.
            Err(_) => "unavailable".to_owned(),
        };

        Self { description: describe_tool(&name), shell, spill_dir: None }
    }

    /// Spills into `dir` rather than the resolved data directory. See
    /// [`ShellTool::spill_dir`].
    #[cfg(test)]
    fn spilling_into(dir: &Path) -> Self {
        Self { spill_dir: Some(dir.to_owned()), ..Self::new() }
    }

    /// The tool as it stands on a machine that offers no shell it may use.
    ///
    /// Built rather than discovered, so the refusal can be asserted on wherever
    /// the tests run: the machines where the probe actually answers this way
    /// are the ones nobody develops on.
    #[cfg(test)]
    fn refusing(why: NoPosixShell) -> Self {
        Self { description: String::new(), shell: Err(why), spill_dir: None }
    }
}

/// Opens a spill file seeded with `head`, in `dir` when one was named and
/// wherever [`truncate::open_spill`] resolves when none was. See
/// [`ShellTool::spill_dir`] for why the choice exists at all.
fn open_spill(dir: Option<&Path>, head: &[u8]) -> Option<(PathBuf, std::fs::File)> {
    match dir {
        Some(dir) => truncate::open_spill_in(dir, head),
        None => truncate::open_spill(head),
    }
}

impl Default for ShellTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn id(&self) -> &str {
        Self::ID
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    fn describe(&self, args: &serde_json::Value) -> String {
        let command = args.get("command").and_then(serde_json::Value::as_str).unwrap_or_default();

        format!("shell: {}", shorten(command, DESCRIBE_LIMIT))
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        self.run_reporting(args, ctx, None).await
    }
}

/// Where a running command's output is reported as it arrives, chunk by chunk
/// and undecoded.
///
/// Unbounded on purpose: this sits on the drain path, and a bounded channel
/// whose reader fell behind would block the pump, which would block the command
/// on its own pipe, which is the deadlock the drain exists to prevent. What
/// bounds memory is the reader, not the channel.
pub type Progress = mpsc::UnboundedSender<Vec<u8>>;

impl ShellTool {
    /// Runs the command, reporting each chunk of output to `progress` as it
    /// arrives.
    ///
    /// [`Tool::run`] is this with nothing to report to. The `!` passthrough is
    /// the caller that has something: it renders the output into a transcript
    /// row while the command is still running, which is what upstream's
    /// `shellImpl` republishes on the part it faked.
    pub async fn run_reporting(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
        progress: Option<Progress>,
    ) -> Result<ToolOutput, ToolError> {
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

        // Background execution branches before any of the foreground
        // machinery below runs — the same spawn this tool has always used
        // (**D454**), handed to whoever tracks it from here. Nothing past
        // this block is reachable on this path, and nothing in it is
        // touched by this path: the foreground body stays exactly what it
        // was.
        if args.run_in_background == Some(true) {
            return self.spawn_background(args.command, &cwd, ctx).await;
        }

        let mut child = self.spawn(&args.command, &cwd)?;

        // Both pipes are drained the whole time the command runs: a command
        // that writes more than a pipe buffer holds would otherwise block on
        // its own output and never reach the exit this races for. What it
        // drains into is bounded — see [`Collector`].
        let collected = Arc::new(Mutex::new(Collector::new(self.spill_dir.clone())));
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let mut pumps = tokio::spawn({
            let out = Arc::clone(&collected);
            let err = Arc::clone(&collected);
            let out_progress = progress.clone();
            async move {
                tokio::join!(pump(stdout, out, out_progress), pump(stderr, err, progress));
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

        let (window, dropped, spilled) =
            collected.lock().expect("the output buffer is never poisoned").finish();
        let Assembled { mut output, truncated, spill } =
            assemble(&window, dropped, spilled, self.spill_dir.as_deref());

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

        let mut metadata = serde_json::json!({
            "exit": exit,
            "truncated": truncated,
        });
        // Upstream spreads `outputPath` in only for `cut && file`, so a call
        // that kept everything names no file rather than naming an absent one.
        // A partial spill is still named: the model can read what did land.
        if let Some(path) = &spill
            && truncated
        {
            metadata["outputPath"] = serde_json::json!(path);
        }

        Ok(ToolOutput { title: args.command, output, metadata })
    }

    /// Runs `command` in `cwd` as a background job: the same spawn
    /// [`ShellTool::spawn`] gives the foreground path, handed to
    /// [`ToolCtx::jobs`] instead of waited on inline. Returns as soon as
    /// registration completes, naming the id `bash_output` and `kill_shell`
    /// read (**D454**).
    ///
    /// `timeout` is never consulted here — **D455**: the foreground default
    /// exists so a hung command cannot wedge a turn waiting on it, and a
    /// background job blocks nothing, so applying the same clock would kill
    /// the long-running work backgrounding exists for the moment it passed
    /// two minutes with no override. `kill_shell` is the one way a
    /// background job ends before it exits on its own.
    async fn spawn_background(
        &self,
        command: String,
        cwd: &Path,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let Some(jobs) = ctx.jobs.as_ref() else {
            return Err(ToolError::Failed(job::NO_JOBS.to_owned()));
        };

        let child = self.spawn(&command, cwd)?;
        let status = jobs.start(command.clone(), child).await;

        Ok(ToolOutput {
            title: command,
            output: format!(
                "Command is running in the background with id \"{}\". Nothing \
                 here notifies you of new output or completion — call \
                 bash_output with this id to poll for it, and kill_shell to \
                 end it early.",
                status.id
            ),
            metadata: serde_json::json!({
                "bash_id": status.id,
                "status": "running",
            }),
        })
    }
}

/// Where a running command's output goes.
///
/// Three things at once, which is what upstream's `run` keeps in `list`,
/// `full` and `sink` (`tool/shell.ts`): a bounded window of the most recent
/// chunks, the head held in memory until there is enough of it to be worth a
/// file, and that file once there is. The invariant the whole type exists for
/// is that none of them grows without limit — a command that writes forever
/// costs [`KEEP`] bytes of memory and however much disk it fills, never more
/// memory than that.
#[derive(Default)]
struct Collector {
    /// The most recent chunks, oldest first.
    window: VecDeque<Vec<u8>>,
    /// Bytes [`Collector::window`] holds, so the bound is a subtraction
    /// rather than a walk.
    used: usize,
    /// Whether anything has fallen out of the window.
    dropped: bool,
    /// Where the whole output goes.
    spill: Spill,
    /// Which directory that file is opened in; see [`ShellTool::spill_dir`].
    spill_dir: Option<PathBuf>,
}

/// The file a command's whole output is kept in, and how far along it is.
enum Spill {
    /// No file yet, and this is everything the command has written. Bounded
    /// by [`SPILL_THRESHOLD`], which is the point at which one is worth
    /// opening.
    Holding(Vec<u8>),
    /// Open, with everything since appended to it as it arrives.
    Open(PathBuf, std::fs::File),
    /// Nothing more can be written, carrying the file that was open when
    /// writing stopped — [`None`] when there never was one.
    ///
    /// The path outliving the failure is the point. A write that fails
    /// part-way leaves a real file holding everything up to that chunk, and
    /// forgetting it would send [`Collector::finish`] off to open a second
    /// one from the window alone: strictly less output, and advertised as
    /// more. Only the path is kept and never the bytes, so the memory bound
    /// is exactly what it was.
    ///
    /// [`None`] is the one case where output is lost outright — nowhere
    /// writable was ever found — and the alternative there is holding it in
    /// memory, which is the defect this type exists to prevent.
    Refused(Option<PathBuf>),
}

/// The file a finished command's output was spilled to, and how much of the
/// output actually reached it.
///
/// The distinction is what the notice is allowed to claim. A file the model
/// is told holds the "full output" had better hold it.
enum Spilled {
    /// Everything the command wrote.
    Whole(PathBuf),
    /// Everything up to the point the spill could no longer be written.
    Partial(PathBuf),
}

impl Spilled {
    /// Where the file is, whichever it is.
    fn path(&self) -> &Path {
        match self {
            Self::Whole(path) | Self::Partial(path) => path,
        }
    }

    /// How the notice introduces it. "Full" is upstream's wording
    /// (`tool/shell.ts`, `run`); "Partial" is this port's, for a state
    /// upstream does not distinguish — it dies on a failed spill write where
    /// this one keeps the call alive and has to say what survived.
    fn label(&self) -> &'static str {
        match self {
            Self::Whole(_) => "Full",
            Self::Partial(_) => "Partial",
        }
    }
}

impl Default for Spill {
    fn default() -> Self {
        Self::Holding(Vec::new())
    }
}

impl Collector {
    /// A collector that spills into `spill_dir`, or wherever
    /// [`truncate::open_spill`] resolves when it is empty.
    fn new(spill_dir: Option<PathBuf>) -> Self {
        Self { spill_dir, ..Self::default() }
    }

    /// Takes one chunk of the command's output.
    fn push(&mut self, chunk: &[u8]) {
        // The window first: whatever else happens, the end of the output is
        // what the model is shown, so the newest bytes are the ones to keep.
        self.used += chunk.len();
        self.window.push_back(chunk.to_vec());
        // `len() > 1` is upstream's guard, and it matters: a single chunk
        // larger than the whole budget is still the only thing there is to
        // show, and dropping it would leave the model reading nothing at all.
        while self.used > KEEP && self.window.len() > 1 {
            let Some(oldest) = self.window.pop_front() else {
                break;
            };
            self.used -= oldest.len();
            self.dropped = true;
        }

        // Taken by value so each arm can simply hand back the next state,
        // rather than assigning into a field it is holding a borrow of.
        let next = match std::mem::take(&mut self.spill) {
            // Past the threshold the file is the record, and memory is not.
            // The write is synchronous inside the lock on purpose: these are
            // pipe-sized appends to a local file, and the alternative is
            // holding a lock across an await in the one path that must never
            // stall a producer — the same trade `session::Persist` makes, for
            // the same reason.
            Spill::Open(path, mut file) => {
                if file.write_all(chunk).is_ok() {
                    Spill::Open(path, file)
                } else {
                    // A disk that stopped accepting writes does not fail the
                    // command: the window still holds the end of the output,
                    // which is what the model reads. The file keeps its name
                    // — everything written before this chunk is still in it,
                    // and that is more than the window has.
                    self.dropped = true;
                    Spill::Refused(Some(path))
                }
            }
            Spill::Refused(path) => Spill::Refused(path),
            Spill::Holding(mut bytes) => {
                bytes.extend_from_slice(chunk);
                if bytes.len() <= SPILL_THRESHOLD {
                    Spill::Holding(bytes)
                } else {
                    match open_spill(self.spill_dir.as_deref(), &bytes) {
                        Some((path, file)) => Spill::Open(path, file),
                        // Nowhere to spill to, so the head is let go rather
                        // than kept growing. It counts as dropped because it
                        // is: the model must not read a partial output as if
                        // it were the whole thing.
                        None => {
                            self.dropped = true;
                            Spill::Refused(None)
                        }
                    }
                }
            }
        };
        self.spill = next;
    }

    /// The window as one buffer, whether anything was cut on the way, and the
    /// spill file if one was opened — saying whether it holds everything.
    fn finish(&mut self) -> (Vec<u8>, bool, Option<Spilled>) {
        let mut window = Vec::with_capacity(self.used);
        for chunk in &self.window {
            window.extend_from_slice(chunk);
        }
        let spilled = match &self.spill {
            Spill::Open(path, _) => Some(Spilled::Whole(path.clone())),
            // Seeded with everything up to the threshold and appended to
            // since, so what is in it is the output up to the failure.
            Spill::Refused(Some(path)) => Some(Spilled::Partial(path.clone())),
            Spill::Holding(_) | Spill::Refused(None) => None,
        };

        (window, self.dropped, spilled)
    }
}

/// What a finished command hands back: what the model reads, whether anything
/// was cut, and the file the rest of it is in.
struct Assembled {
    output: String,
    truncated: bool,
    spill: Option<PathBuf>,
}

/// Turns what the collector kept into what the model reads.
///
/// Split out of [`ShellTool::run`] so the shapes that are awkward to reach
/// through a real command — a spill that failed part-way, above all — can be
/// exercised directly.
fn assemble(
    window: &[u8],
    dropped: bool,
    spilled: Option<Spilled>,
    spill_dir: Option<&Path>,
) -> Assembled {
    // Lossy for the same reason upstream's `TextDecoder` is: a command is
    // free to write bytes that are not text, and losing a byte beats failing
    // a call that otherwise worked. Decoding the window whole, rather than
    // chunk by chunk, keeps a code point split across two reads intact.
    let (mut output, clipped) = tail(&String::from_utf8_lossy(window));
    let truncated = dropped || clipped;

    // Everything cut has to be somewhere the model can still reach, which is
    // what this tool's own prompt promises.
    let spill = match spilled {
        // A file opened while the command ran already holds more than the
        // window does. Opening a second one from the window would orphan it
        // and hand the model strictly less.
        Some(spilled) => Some(spilled),
        // No file yet, so one is written now from the window. It is the whole
        // output precisely when nothing was dropped on the way — a command
        // under [`SPILL_THRESHOLD`] never lost anything, whereas one whose
        // spill could not be opened at all did.
        None if clipped => open_spill(spill_dir, window)
            .map(|(path, _)| if dropped { Spilled::Partial(path) } else { Spilled::Whole(path) }),
        None => None,
    };

    if output.is_empty() {
        output = "(no output)".to_owned();
    }
    // Upstream's marker, verbatim and in its position: a prefix, so the model
    // reads why the output starts mid-stream before it reads the output
    // (`tool/shell.ts`, `run`). Deliberately not `truncate`'s own `hint`,
    // which the other tools use — this one names the whole file rather than a
    // way to search it, because what was cut is the head.
    if let Some(spilled) = &spill
        && truncated
    {
        output = format!(
            "...output truncated...\n\n{} output saved to: {}\n\n{output}",
            spilled.label(),
            spilled.path().display()
        );
    }

    Assembled { output, truncated, spill: spill.map(|spilled| spilled.path().to_owned()) }
}

/// The last [`truncate::MAX_LINES`] lines of `text` that fit in
/// [`truncate::MAX_CHARS`], and whether anything was cut.
///
/// Upstream's `tail` (`tool/shell.ts`), and the direction is the point: every
/// other tool in this crate clamps to the *head* through [`truncate::clamp`],
/// because the beginning of a file or a search is what matters. The end of a
/// command's output is what matters — the error it failed with, the summary
/// it printed — so this one keeps the end, exactly as upstream does.
fn tail(text: &str) -> (String, bool) {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() <= truncate::MAX_LINES && text.len() <= truncate::MAX_CHARS {
        return (text.to_owned(), false);
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0_usize;
    for line in lines.iter().rev() {
        if kept.len() >= truncate::MAX_LINES {
            break;
        }
        // The newline that would rejoin this line to the one after it, which
        // the first line kept does not need.
        let size = line.len() + usize::from(!kept.is_empty());
        if bytes + size > truncate::MAX_CHARS {
            if kept.is_empty() {
                // One line longer than the entire budget. Upstream keeps its
                // tail rather than nothing, walking forward off any byte that
                // continues a character so what survives is still text.
                return (last_bytes(line, truncate::MAX_CHARS).to_owned(), true);
            }
            break;
        }
        kept.push(line);
        bytes += size;
    }
    kept.reverse();

    (kept.join("\n"), true)
}

/// The last `budget` bytes of `line`, moved forward to the nearest character
/// boundary — upstream's `while ((buf[start] & 0xc0) === 0x80) start++`, which
/// is the same question `is_char_boundary` answers.
fn last_bytes(line: &str, budget: usize) -> &str {
    let mut start = line.len().saturating_sub(budget);
    while start < line.len() && !line.is_char_boundary(start) {
        start += 1;
    }

    &line[start..]
}

impl ShellTool {
    /// Starts `command` under the shell, in its own process group.
    fn spawn(&self, command: &str, cwd: &Path) -> Result<Child, ToolError> {
        // A machine with no shell this port may use refuses here rather than at
        // construction, so the reason travels back as a tool result the model
        // reads instead of taking down the session that built the registry.
        let shell = self.shell.as_ref().map_err(|why| ToolError::Failed(why.to_string()))?;
        let mut spawner = Command::new(shell);
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
                shell.display(),
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
///
/// Public — not private to this module — because `ganja-core`'s
/// background-job registry ends a killed or shut-down job's tree the exact
/// same way a foreground command's cancel or timeout does; re-deriving the
/// `SIGTERM`/grace/`SIGKILL` sequence a second time would be the sequence a
/// security review has to read twice instead of once, for no behavioral
/// difference between the two callers.
#[cfg(unix)]
pub async fn kill_tree(child: &mut Child) {
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
///
/// Public because shell commands, MCP servers, and language servers own
/// different shutdown ladders but all need this one unsafe system call. Keeping
/// only the call here lets each owner retain its own grace and liveness policy.
#[cfg(unix)]
pub fn signal_group(pid: u32, signal: libc::c_int) {
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
    // Every owner supplies an id obtained from `Child::id` after spawning that
    // child with `process_group(0)`, or retains that same id as its group
    // handle. The child is therefore the leader of a fresh group whose id
    // equals its pid; the group holds that child and its descendants and cannot
    // name a group this process did not create.
    //
    // `Child::id` returns `None` once the child has been reaped, so an id that
    // reaches this function still names an unreaped child; an unreaped pid
    // cannot have been recycled onto an unrelated process. Every owner but
    // `Servers::shutdown` holds that invariant; the one that cannot — rmcp
    // reaps its own children — records its residual at its own resend site.
    //
    // A failure is not worth reporting: `ESRCH` means the group is already
    // gone, which is the outcome being asked for, and `EPERM` cannot arise for
    // a group this process created.
    unsafe {
        libc::killpg(pid, signal);
    }
}

/// Ends the process tree on a platform with no process group to signal.
///
/// Upstream's own answer on Windows (`packages/core/src/shell.ts`, `killTree`):
/// hand the pid to `taskkill /T`, which walks the parent chain the kernel keeps
/// and ends every descendant. `Child::start_kill` reaches the shell and nothing
/// else, so a cancelled `cargo build` would go on building with its pipes still
/// open — the orphaning this module's process groups exist to prevent, left
/// unprevented on the one platform that has no groups.
///
/// There is no `SIGTERM` half to this: `taskkill /F` is the only termination
/// Windows offers a process that is not cooperating, so [`KILL_GRACE`] has
/// nothing to grant here and is a unix constant.
///
/// The call is awaited rather than fired and forgotten, so a caller returning
/// straight afterwards cannot outrun it. What it reports is not read — it fails
/// for a tree that has already exited, which is the outcome being asked for —
/// and the direct child is killed regardless, so a machine whose `taskkill` is
/// missing or refused is no worse off than before.
///
/// Public for the same reason the unix twin above is: `ganja-core`'s
/// background-job registry reuses this sequence rather than re-deriving it.
#[cfg(not(unix))]
pub async fn kill_tree(child: &mut Child) {
    if let Some(pid) = child.id() {
        let _ = Command::new("taskkill")
            .args(["/pid", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status()
            .await;
    }

    let _ = child.start_kill();
}

/// Hands everything `reader` produces to `sink`.
///
/// Both pipes share one collector, so stdout and stderr interleave in the
/// order they arrived — which is what a terminal would have shown, and what
/// upstream's merged stream carries.
async fn pump<R: AsyncRead + Unpin>(
    mut reader: R,
    sink: Arc<Mutex<Collector>>,
    progress: Option<Progress>,
) {
    let mut chunk = [0_u8; 8192];

    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => {
                sink.lock().expect("the output buffer is never poisoned").push(&chunk[..read]);
                // Outside the lock, and unbounded, because this arrives on the
                // path that must never stall a producer: a command blocked on
                // its own output is a command that never reaches its exit.
                // A watcher that has gone away simply stops being told.
                if let Some(progress) = &progress {
                    let _ = progress.send(chunk[..read].to_vec());
                }
            }
        }
    }
}

/// Why this machine offers no shell a command may be handed to.
///
/// Every command that reaches this tool is POSIX shell text: upstream writes it
/// that way, and `ganja-permission` reads it back with a POSIX tokenizer to
/// decide which files the call would touch. A shell that parses that text by
/// other rules is therefore not a substitute for the one this port expects but
/// a hazard — which is why the absence of a POSIX shell is an error the model
/// is told about rather than a fallback taken quietly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoPosixShell {
    /// The shell that was found parses another grammar entirely — PowerShell,
    /// or `cmd`.
    NotPosix(PathBuf),
    /// Nothing POSIX-shaped is installed, or nothing this process can see.
    Missing,
}

impl std::fmt::Display for NoPosixShell {
    /// Read by a person and by the model alike — this is the text a refused
    /// call answers with — so it names the remedy and not only the fault.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPosix(shell) => write!(
                formatter,
                "{} is a PowerShell or cmd shell, and every command this tool \
                 runs is POSIX shell text. Refusing to run it: PowerShell reads \
                 quoting, `&&` and `$` by rules of its own, so what ran would \
                 not be what the permission gate was asked about. Install Git \
                 Bash, or run under WSL, and put its sh on PATH.",
                shell.display()
            ),
            Self::Missing => write!(
                formatter,
                "no POSIX shell was found on this machine, and every command \
                 this tool runs is POSIX shell text. Install Git Bash — which \
                 provides sh.exe and bash.exe — or run under WSL, and make sure \
                 the shell it installs is on PATH."
            ),
        }
    }
}

impl std::error::Error for NoPosixShell {}

/// The shell a POSIX command line may be handed to on this machine, or why
/// there is none.
///
/// Public because `ganja-tui` hands `$EDITOR` to a shell the same way, and the
/// probe below — and the refusal beside it — are worth having exactly once.
///
/// Unix answers with `sh`, which POSIX requires to be on PATH: resolving it
/// further would change which shell a command line has been running under since
/// this port had one, for no gain on the platform where nothing was broken.
///
/// # Errors
///
/// Returns [`NoPosixShell`] when this machine offers only a PowerShell-family
/// shell, or no shell at all.
pub fn posix_shell() -> Result<PathBuf, NoPosixShell> {
    #[cfg(unix)]
    let found = PathBuf::from("sh");
    #[cfg(not(unix))]
    let found = probe()?;

    accept_shell(found)
}

/// `shell` if a POSIX command line means on it what it was written to mean, and
/// a refusal otherwise.
///
/// Applied to every candidate whatever found it — the probe below today, a
/// configured shell whenever this port grows one — because this is the one
/// place the answer can be made to fail closed. Falling back to PowerShell
/// would be the fail-open case: the command that ran would not be the text
/// `ganja-permission` tokenized, and the location gate's whole claim is that
/// those two are the same string.
fn accept_shell(shell: PathBuf) -> Result<PathBuf, NoPosixShell> {
    if speaks_posix(&shell) {
        return Ok(shell);
    }

    Err(NoPosixShell::NotPosix(shell))
}

/// Whether `shell` reads a command line by POSIX rules.
///
/// Judged by the binary's own name, which is the only thing knowable without
/// running it. The list is what would otherwise be reached for on Windows:
/// `cmd` is what `%ComSpec%` names, `pwsh` what a current PowerShell install
/// puts on PATH, `powershell` what every Windows already carries, and
/// `command` its DOS-era ancestor. A name nobody here recognises is taken at
/// its word — an unknown shell that cannot run the command fails at the spawn,
/// with the error the system gives, which says more than a guess would.
///
/// The name is cut out by text, on both separators, rather than through
/// [`Path::file_stem`]: a `\` is an ordinary character in a unix path, so
/// `file_stem` would hand back a whole Windows path unshortened and every
/// judgement about one would have to be made on Windows to mean anything. A
/// unix file genuinely named for a Windows path is refused by this, and that is
/// a trade worth making — nobody's shell is called that, and the direction of
/// the mistake is refusal rather than a command run under the wrong grammar.
fn speaks_posix(shell: &Path) -> bool {
    let text = shell.to_string_lossy();
    let name = text.rsplit(['/', '\\']).next().unwrap_or(&text);
    let stem = name.split('.').next().unwrap_or(name);

    !matches!(stem.to_ascii_lowercase().as_str(), "pwsh" | "powershell" | "cmd" | "command")
}

/// The shell commands run under.
///
/// Upstream's `fallback()`: zsh on macOS, otherwise bash where it exists, and
/// `/bin/sh` where it does not. The configured shell is not consulted yet
/// because nothing in this port writes that config.
///
/// Windows has no such convention to inherit, so it takes the shared probe in
/// [`posix_shell`] and the same refusal with it.
fn default_shell() -> Result<PathBuf, NoPosixShell> {
    #[cfg(unix)]
    let found = unix_default();
    #[cfg(not(unix))]
    let found = probe()?;

    accept_shell(found)
}

/// Upstream's `fallback()`, which is a statement about which shell a person on
/// this machine would have typed the command into themselves.
#[cfg(unix)]
fn unix_default() -> PathBuf {
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

/// Where a POSIX shell lives on Windows.
///
/// Naming the bare `sh` and leaving it to the spawn is not enough here.
/// `CreateProcess` searches for a bare name by rules of its own — the
/// application directory first, `PATH` last — and Git Bash's `sh.exe` sits in a
/// directory that is on `PATH` for a Git Bash session and absent from the
/// environment a desktop shortcut hands over. So the binary is resolved to a
/// full path before it is ever spawned, and the conventional Git for Windows
/// layouts are searched after `PATH` for the case where ganja was not launched
/// from a Git Bash session at all.
///
/// Finding is all this does. Whether what it found may be *used* is
/// [`accept_shell`]'s question, asked once by each caller — so that a
/// configured shell, which this will never have looked at, passes the same gate
/// as a probed one.
#[cfg(not(unix))]
fn probe() -> Result<PathBuf, NoPosixShell> {
    // Best first: `sh` is the shell this crate's command lines are written
    // against, `bash` the one Git for Windows is certain to have installed.
    const NAMES: [&str; 2] = ["sh.exe", "bash.exe"];

    for name in NAMES {
        if let Some(found) = on_path(name) {
            return Ok(found);
        }
    }

    for root in git_roots() {
        // Git for Windows has kept the shell in both places across its
        // versions, and which one an install wrote depends on its vintage.
        for directory in ["bin", r"usr\bin"] {
            for name in NAMES {
                let candidate = root.join(directory).join(name);
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
    }

    Err(NoPosixShell::Missing)
}

/// The first directory on `PATH` holding `name`.
#[cfg(not(unix))]
fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;

    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

/// Where Git for Windows is conventionally installed, likeliest first.
///
/// Read from the environment rather than written out, because the program
/// directory is not `C:\Program Files` on every machine — a localised Windows,
/// a relocated install, or a 32-bit Git on a 64-bit box each move it — and the
/// literal is kept only as the last thing to try.
#[cfg(not(unix))]
fn git_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(|directory| PathBuf::from(directory).join("Git"))
        .collect();
    roots.push(PathBuf::from(r"C:\Program Files\Git"));

    roots
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

    let mut description = render(
        include_str!("shell.txt"),
        &[
            ("intro", INTRO),
            ("os", std::env::consts::OS),
            ("shell", shell),
            ("workdirSection", WORKDIR_SECTION),
            ("tmp", &std::env::temp_dir().display().to_string()),
            ("commandSection", &command_section),
        ],
    );
    // Ganja's own addition, with no upstream counterpart to port (**D454**):
    // appended after the ported prompt above, which stays a byte-exact copy
    // of upstream opencode's.
    description.push_str("\n\n");
    description.push_str(BACKGROUND_SECTION);

    description
}

/// Documents `run_in_background`, `bash_output` and `kill_shell` — Claude
/// Code's contract, which this prompt states plainly rather than promising
/// something this build does not do: there is no push notification when a
/// background shell produces output or finishes, only `bash_output` to poll.
const BACKGROUND_SECTION: &str = "\
# Running in the background
Set run_in_background to true to start a long-running command without \
waiting for it to finish; the call returns immediately, naming the shell's \
id. Use the bash_output tool with that id to check on it — output arrives \
only when you ask for it, never automatically, so poll when you need to \
know — and kill_shell to end it early. A timeout given alongside \
run_in_background is ignored: a backgrounded command is not waited on, so \
nothing here would apply it.";

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
///
/// `pub(crate)` for [`DESCRIBE_LIMIT`]'s reason, and with it: the flattening of
/// newlines is half of what makes a description one line, so a second caller
/// wanting the same one-line title wants this function rather than its own
/// arithmetic.
pub(crate) fn shorten(text: &str, limit: usize) -> String {
    let flattened = text.replace('\n', " ");

    if flattened.chars().count() <= limit {
        return flattened;
    }

    let kept: String = flattened.chars().take(limit).collect();

    format!("{kept}...")
}

#[cfg(test)]
#[path = "shell_tests.rs"]
mod tests;
