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
    collections::VecDeque,
    io::Write as _,
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
    sync::mpsc,
};

use crate::{Tool, ToolCtx, ToolError, ToolOutput, truncate};

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
    /// Only a test ever sets this, through [`ShellTool::spilling_into`]: a
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
            Ok(shell) => shell
                .file_name()
                .unwrap_or(shell.as_os_str())
                .to_string_lossy()
                .into_owned(),
            // The prompt is rendered once, and where there is no shell there is
            // nothing truthful to name: printing the refused one would tell the
            // model its commands run under PowerShell, which is exactly what
            // will not happen. Every call answers with the reason anyway.
            Err(_) => "unavailable".to_owned(),
        };

        Self {
            description: describe_tool(&name),
            shell,
            spill_dir: None,
        }
    }

    /// Spills into `dir` rather than the resolved data directory. See
    /// [`ShellTool::spill_dir`].
    #[cfg(test)]
    fn spilling_into(dir: &Path) -> Self {
        Self {
            spill_dir: Some(dir.to_owned()),
            ..Self::new()
        }
    }

    /// The tool as it stands on a machine that offers no shell it may use.
    ///
    /// Built rather than discovered, so the refusal can be asserted on wherever
    /// the tests run: the machines where the probe actually answers this way
    /// are the ones nobody develops on.
    #[cfg(test)]
    fn refusing(why: NoPosixShell) -> Self {
        Self {
            description: String::new(),
            shell: Err(why),
            spill_dir: None,
        }
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
        let command = args
            .get("command")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();

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

        let (window, dropped, spilled) = collected
            .lock()
            .expect("the output buffer is never poisoned")
            .finish();
        let Assembled {
            mut output,
            truncated,
            spill,
        } = assemble(&window, dropped, spilled, self.spill_dir.as_deref());

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

        Ok(ToolOutput {
            title: args.command,
            output,
            metadata,
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
        Self {
            spill_dir,
            ..Self::default()
        }
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
        None if clipped => open_spill(spill_dir, window).map(|(path, _)| {
            if dropped {
                Spilled::Partial(path)
            } else {
                Spilled::Whole(path)
            }
        }),
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

    Assembled {
        output,
        truncated,
        spill: spill.map(|spilled| spilled.path().to_owned()),
    }
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
        let shell = self
            .shell
            .as_ref()
            .map_err(|why| ToolError::Failed(why.to_string()))?;
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
#[cfg(not(unix))]
async fn kill_tree(child: &mut Child) {
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
                sink.lock()
                    .expect("the output buffer is never poisoned")
                    .push(&chunk[..read]);
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

    !matches!(
        stem.to_ascii_lowercase().as_str(),
        "pwsh" | "powershell" | "cmd" | "command"
    )
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
        path::{Path, PathBuf},
        sync::Arc,
        time::{Duration, Instant},
    };

    use tokio_util::sync::CancellationToken;

    use super::{
        Collector, DEFAULT_TIMEOUT, KEEP, NoPosixShell, SPILL_THRESHOLD, ShellTool, Spill, Spilled,
        accept_shell, assemble, posix_shell, tail,
    };
    use crate::{FileTimes, Tool, ToolCtx, ToolError, truncate};

    /// `text`, which a shell printed, as this platform spells a path.
    ///
    /// A POSIX shell on Windows answers `pwd` with `/c/Users/...` where the
    /// native spelling is `C:\Users\...`; Cygwin writes `/cygdrive/c/...` and
    /// WSL `/mnt/c/...` for the same place. All of them name one directory and
    /// only one of them is a path anything else here can open.
    ///
    /// Gated to Windows rather than merely documented as Windows-only: on unix
    /// `/c/Users` *is* the path, and rewriting it would invent a drive that
    /// does not exist. A single letter is the whole test, so `/usr/bin` keeps
    /// its meaning.
    #[cfg(windows)]
    fn native(text: &str) -> PathBuf {
        let rest = text.strip_prefix('/').unwrap_or(text);
        let rest = rest
            .strip_prefix("cygdrive/")
            .or_else(|| rest.strip_prefix("mnt/"))
            .unwrap_or(rest);
        let (head, tail) = rest.split_once('/').unwrap_or((rest, ""));

        match head.strip_suffix(':').unwrap_or(head).as_bytes() {
            [drive] if drive.is_ascii_alphabetic() => PathBuf::from(format!(
                "{}:\\{}",
                drive.to_ascii_uppercase() as char,
                tail.replace('/', "\\")
            )),
            _ => PathBuf::from(text),
        }
    }

    /// Nothing to translate where a shell and the filesystem already agree.
    #[cfg(not(windows))]
    fn native(text: &str) -> PathBuf {
        PathBuf::from(text)
    }

    /// `path` as it has to be written *inside a command string*.
    ///
    /// The other direction, and the reason both exist: a command is POSIX shell
    /// text, so a native Windows path interpolated into one loses its
    /// separators to the shell's own escaping and names somewhere nobody meant.
    /// Windows opens a forward-slash path perfectly well, so this is what a
    /// fixture writes. A no-op on unix.
    fn posix(path: &Path) -> String {
        path.display().to_string().replace('\\', "/")
    }

    /// A context rooted at `cwd`, with a cancel nobody has pulled.
    fn ctx(cwd: PathBuf) -> ToolCtx {
        ToolCtx {
            cwd,
            cancel: CancellationToken::new(),
            call_id: "call-1".to_owned(),
            files: Arc::new(FileTimes::default()),
            credentials: crate::Credentials::Unguarded,
            spawn: None,
            ask: None,
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
            description.contains(&crate::truncate::MAX_LINES.to_string()),
            "the prompt should name the output budget it enforces: {description}"
        );
        assert!(
            description.contains("Be aware: OS:"),
            "the ported prompt should survive rendering intact: {description}"
        );
    }

    /// The refusal the Windows posture rests on. A POSIX command line handed to
    /// PowerShell is not the command `ganja-permission` tokenized to decide
    /// which files it would touch, so a shell that reads another grammar is
    /// refused rather than used — the one place where falling back would open
    /// the location gate rather than narrow it.
    ///
    /// Deliberately not gated to Windows: the judgement is text about a name,
    /// it is what a configured shell will be put through when this port grows
    /// one, and a rule that only runs where nobody can watch it is a rule that
    /// rots.
    #[test]
    fn a_powershell_or_cmd_shell_is_refused_rather_than_handed_posix_text() {
        for refused in [
            r"C:\Program Files\PowerShell\7\pwsh.exe",
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
            r"C:\Windows\System32\cmd.exe",
            "/usr/local/bin/PWSH.EXE",
            "cmd",
            "command.com",
        ] {
            let verdict = accept_shell(PathBuf::from(refused));

            assert_eq!(
                verdict,
                Err(NoPosixShell::NotPosix(PathBuf::from(refused))),
                "{refused} parses a command line by rules this port does not write"
            );
        }

        for allowed in [
            "sh",
            "/bin/bash",
            "/bin/zsh",
            r"C:\Program Files\Git\bin\sh.exe",
            r"C:\Program Files\Git\usr\bin\bash.exe",
        ] {
            assert_eq!(
                accept_shell(PathBuf::from(allowed)),
                Ok(PathBuf::from(allowed)),
                "{allowed} is a shell this port's command lines are written for"
            );
        }
    }

    /// What a machine with no usable shell answers: a tool result naming the
    /// remedy, not a panic and not a silent success. The message is the model's
    /// only way to learn that nothing it runs here will work.
    #[tokio::test]
    async fn a_call_with_no_posix_shell_to_run_it_is_refused_with_the_remedy() {
        let dir = tempfile::tempdir().expect("a scratch directory");

        for why in [
            NoPosixShell::Missing,
            NoPosixShell::NotPosix(PathBuf::from(r"C:\Windows\System32\cmd.exe")),
        ] {
            let tool = ShellTool::refusing(why.clone());

            let refused = tool
                .run(
                    serde_json::json!({ "command": "echo hello" }),
                    &ctx(dir.path().to_owned()),
                )
                .await
                .expect_err("a machine with no shell runs nothing");

            let ToolError::Failed(message) = refused else {
                panic!("a missing shell is a failure the model reads, got {refused:?}");
            };
            assert_eq!(message, why.to_string());
            assert!(
                message.contains("Git Bash"),
                "the refusal should name what to install: {message}"
            );
        }
    }

    /// The machine the suite is running on offers a shell this port may use —
    /// which is what every other test in this module has been assuming since it
    /// was written.
    #[test]
    fn this_machine_offers_a_shell_a_posix_command_line_may_be_handed_to() {
        let shell = posix_shell().expect("a development machine has a POSIX shell");

        // On Windows the probe's whole job is to answer with something
        // spawnable, and a bare name is exactly what does not survive
        // `CreateProcess` from a non-Git-Bash environment.
        #[cfg(windows)]
        assert!(
            shell.is_file(),
            "the probe must resolve to a binary that is there: {}",
            shell.display()
        );
        #[cfg(unix)]
        assert_eq!(shell, PathBuf::from("sh"));
    }

    /// The two streams alternate, with enough of a pause between writes that
    /// the arrival order is not in question. Reading one pipe to exhaustion
    /// and then the other would answer `one three two four` — a transcript
    /// that reads as though the command did something it never did.
    #[tokio::test]
    async fn both_streams_are_captured_in_the_order_they_arrived() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let tool = ShellTool::new();
        let command = "printf 'one\\n'; sleep 0.1; printf 'two\\n' >&2; \
                       sleep 0.1; printf 'three\\n'; sleep 0.1; printf 'four\\n' >&2";

        let out = tool
            .run(
                serde_json::json!({ "command": command }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a command that runs has output");

        assert_eq!(
            out.output.lines().collect::<Vec<_>>(),
            vec!["one", "two", "three", "four"],
            "got {:?}",
            out.output
        );
        assert_eq!(out.metadata["exit"], 0);
        assert_eq!(
            out.title, command,
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
        // both sides are resolved before they are compared — and the shell's
        // answer is put into this platform's alphabet first, because a POSIX
        // shell on Windows prints `/c/Users/...` for a place spelled
        // `C:\Users\...` and `canonicalize` knows only the second one.
        let canonical = |text: &str| {
            std::fs::canonicalize(native(text.trim()))
                .expect("the directory the shell reported exists")
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
        let forked = dir.path().join("grandchild-was-forked");
        let survived = dir.path().join("grandchild-was-here");
        // The backgrounded sleep is a grandchild of the tool: the shell forks
        // it and would leave it running if only the shell were killed.
        //
        // It writes twice, and the first write is why. The claim under test is
        // that a file does NOT appear, and an assertion of that shape passes
        // for every reason a file might fail to be written — a path the shell
        // could not parse, a shell that never ran at all — not only the one it
        // means. So the grandchild announces itself the moment it exists, and
        // that announcement is asserted *present*: without it this test would
        // have gone on passing on a platform where the command was never
        // running in the first place.
        //
        // The paths are interpolated POSIX-spelled, because a command string is
        // POSIX shell text by contract and a native Windows path written into
        // one is eaten by its own backslashes.
        let command = format!(
            "( echo yes > {forked}; sleep 3; echo yes > {survived} ) & sleep 30",
            forked = posix(&forked),
            survived = posix(&survived),
        );

        let started = Instant::now();
        let out = ShellTool::new()
            .run(
                serde_json::json!({ "command": command, "timeout": 1_000 }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a timeout is a completed call carrying what did arrive");
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(5),
            "the timeout should end the command promptly, took {elapsed:?}"
        );
        assert!(
            out.output.contains("<shell_metadata>")
                && out.output.contains("exceeding timeout 1000 ms"),
            "the model has to be told why the output stops: {:?}",
            out.output
        );

        // Long enough that the grandchild would have written its second marker
        // had it survived the kill.
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            forked.exists(),
            "the grandchild never ran, so nothing below proves anything about killing it"
        );
        assert!(
            !survived.exists(),
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

    /// A command may write more than this machine has memory for, and the
    /// two-minute default timeout is long enough for `yes` to prove it. What
    /// bounds it is the window: the newest [`KEEP`] bytes stay in memory,
    /// everything else has already gone to the spill file, and what the model
    /// reads is the end of the output rather than the beginning of it.
    #[tokio::test]
    async fn a_flood_of_output_is_bounded_in_memory_and_the_tail_is_what_survives() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let spill = dir.path().join("tool-output");
        // ~230 KB over 40,000 lines: past the line budget, past the byte
        // budget, and past [`KEEP`], so the spill really opens mid-run.
        let out = ShellTool::spilling_into(&spill)
            .run(
                serde_json::json!({ "command": "seq 1 40000" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a command that floods still completes");

        assert_eq!(out.metadata["truncated"], true);
        assert!(
            out.output
                .starts_with("...output truncated...\n\nFull output saved to: "),
            "the model has to read why the output starts mid-stream, got {:?}",
            &out.output[..out.output.len().min(120)]
        );
        assert!(
            out.output.contains("\n40000"),
            "the end of the output is what a shell result is for"
        );
        assert!(
            !out.output.contains("\n1\n"),
            "the head should have been cut, not the tail"
        );
        assert!(
            out.output.len() < truncate::MAX_CHARS + 1_024,
            "what the model reads must fit the budget, got {} bytes",
            out.output.len()
        );

        let path = out.metadata["outputPath"]
            .as_str()
            .expect("a truncated command names where its output went");
        let full = std::fs::read_to_string(path).expect("the spill is readable");
        assert!(
            full.starts_with("1\n") && full.trim_end().ends_with("\n40000"),
            "the spill holds everything, not just what was shown"
        );
        assert!(
            full.len() > KEEP,
            "the spill should hold more than was ever in memory at once, got {} bytes",
            full.len()
        );
    }

    /// The overwhelmingly common case: nothing is cut, so nothing is spilled
    /// and no file is named.
    #[tokio::test]
    async fn output_that_fits_the_budget_names_no_file_and_says_it_was_not_truncated() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let spill = dir.path().join("tool-output");

        let out = ShellTool::spilling_into(&spill)
            .run(
                serde_json::json!({ "command": "seq 1 10" }),
                &ctx(dir.path().to_owned()),
            )
            .await
            .expect("a small command runs");

        assert_eq!(out.output, "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n");
        assert_eq!(out.metadata["truncated"], false);
        assert!(
            out.metadata.get("outputPath").is_none(),
            "a call that kept everything must not name a file: {:?}",
            out.metadata
        );
        assert!(
            !spill.exists(),
            "nothing should have been written to disk at all"
        );
    }

    /// The window is what bounds memory, so it is worth proving directly:
    /// ten megabytes through the collector leaves [`KEEP`] bytes behind it,
    /// and the file holds every one of them.
    #[test]
    fn the_collector_holds_a_bounded_window_however_much_is_pushed_through_it() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let spill = dir.path().join("tool-output");
        let mut collector = Collector::new(Some(spill.clone()));

        let chunk = vec![b'x'; 8192];
        let pushes = (10 * 1024 * 1024) / chunk.len();
        for _ in 0..pushes {
            collector.push(&chunk);
        }

        let (window, dropped, spilled) = collector.finish();
        assert!(
            window.len() <= KEEP,
            "the window is the memory bound, got {} bytes for {} pushed",
            window.len(),
            pushes * chunk.len()
        );
        assert!(dropped, "a flood that big cannot have been kept whole");
        let spilled = spilled.expect("everything past the threshold goes to a file");
        assert!(
            matches!(spilled, Spilled::Whole(_)),
            "every write succeeded, so the file holds the whole output"
        );
        assert_eq!(
            std::fs::metadata(spilled.path())
                .expect("the spill exists")
                .len()
                .try_into(),
            Ok(pushes * chunk.len()),
            "the spill holds every byte that was pushed"
        );
    }

    /// The files sitting in `dir`, so a test can say how many were written.
    fn spill_files(dir: &Path) -> Vec<PathBuf> {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .expect("the spill directory exists")
            .map(|entry| entry.expect("a readable directory entry").path())
            .collect();
        entries.sort();

        entries
    }

    /// A spill that stops accepting writes part-way leaves a real file holding
    /// everything up to that point — far more than the window does. Forgetting
    /// its name would orphan it and send the assembly off to write a second
    /// file from the window alone, which the model would then be told is the
    /// "full output": less output, advertised as more.
    ///
    /// The failure is produced by putting a read-only descriptor where the
    /// writable one was, so every later write fails with `EBADF` on demand —
    /// no full disk and no resource limit needed to reach the state.
    #[cfg(unix)]
    #[test]
    fn a_spill_that_stops_accepting_writes_keeps_the_file_it_already_wrote() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let spill = dir.path().join("tool-output");
        let mut collector = Collector::new(Some(spill.clone()));

        // Past the threshold, so the file is opened and seeded with the head.
        collector.push(&vec![b'a'; SPILL_THRESHOLD + 1]);
        let Spill::Open(opened, _) = std::mem::take(&mut collector.spill) else {
            panic!("crossing the threshold opens the spill");
        };
        let written = std::fs::metadata(&opened).expect("the spill exists").len();
        collector.spill = Spill::Open(
            opened.clone(),
            std::fs::File::open(&opened).expect("the spill re-opens read-only"),
        );

        collector.push(b"this chunk cannot land");

        let (window, dropped, spilled) = collector.finish();
        assert!(dropped, "output that could not be written is output lost");
        assert!(
            matches!(&spilled, Some(Spilled::Partial(path)) if *path == opened),
            "the partial file keeps its name"
        );

        let assembled = assemble(&window, dropped, spilled, Some(&spill));

        assert!(
            assembled.output.contains("Partial output saved to:"),
            "a file that stops short must not be introduced as the whole \
             output: {:?}",
            &assembled.output[..assembled.output.len().min(120)]
        );
        assert!(
            !assembled.output.contains("Full output saved to:"),
            "got {:?}",
            &assembled.output[..assembled.output.len().min(120)]
        );
        assert_eq!(
            assembled.spill.as_deref(),
            Some(opened.as_path()),
            "the notice and the metadata must name the file that has the output"
        );
        assert_eq!(
            spill_files(&spill),
            vec![opened.clone()],
            "a second file was written, orphaning the first"
        );
        assert_eq!(
            std::fs::metadata(&opened)
                .expect("the spill still exists")
                .len(),
            written,
            "the partial file should be left exactly as the failure left it"
        );
    }

    /// A single chunk larger than the whole window is still the only thing
    /// there is to show, which is why upstream's `list.length > 1` guard is
    /// part of the port.
    #[test]
    fn one_chunk_larger_than_the_window_is_kept_rather_than_dropped() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let mut collector = Collector::new(Some(dir.path().join("tool-output")));

        collector.push(&vec![b'x'; KEEP * 2]);

        let (window, _, _) = collector.finish();
        assert_eq!(
            window.len(),
            KEEP * 2,
            "dropping the only chunk would leave the model reading nothing"
        );
    }

    #[test]
    fn the_tail_keeps_the_end_of_the_output_and_says_it_cut() {
        let short = "one\ntwo\nthree";
        assert_eq!(tail(short), (short.to_owned(), false));

        let many = (1..=truncate::MAX_LINES + 10)
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let (kept, cut) = tail(&many);

        assert!(cut);
        assert_eq!(
            kept.lines().count(),
            truncate::MAX_LINES,
            "the budget is a line count, not a suggestion"
        );
        assert!(
            kept.ends_with(&(truncate::MAX_LINES + 10).to_string()),
            "the last line of the output is the one that matters"
        );
        assert!(
            kept.starts_with("11\n"),
            "and the lines before the budget are the ones that go: {:?}",
            &kept[..kept.len().min(20)]
        );
    }

    #[test]
    fn one_line_longer_than_the_budget_keeps_its_tail_without_splitting_a_character() {
        // Every character is four bytes, so a byte-indexed cut lands inside
        // one unless the boundary is walked forward.
        let line = "\u{1F980}".repeat(truncate::MAX_CHARS);
        let (kept, cut) = tail(&line);

        assert!(cut);
        assert!(
            kept.len() <= truncate::MAX_CHARS,
            "the tail still has to fit the budget, got {} bytes",
            kept.len()
        );
        assert!(
            kept.chars().all(|character| character == '\u{1F980}'),
            "a cut inside a character would have left a replacement behind"
        );
        assert!(
            line.ends_with(&kept),
            "what survives is the end of the line"
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
