use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use super::{
    Collector, DEFAULT_TIMEOUT, KEEP, NoPosixShell, ShellTool, Spilled, accept_shell, posix_shell,
    tail,
};
// Only the unix-gated spill-failure test crosses the threshold on purpose
// and reaches into the collector's insides, so these travel with it rather
// than sit dead on windows.
#[cfg(unix)]
use super::{SPILL_THRESHOLD, Spill, assemble};
use crate::{
    Tool, ToolCtx, ToolError,
    job::{JobRead, JobStatus, Jobs, JobsError, State},
    truncate,
};

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
    ToolCtx::fixture(cwd)
}

/// A [`Jobs`] that records the one call it expects and hands back a
/// scripted status — enough to prove `run_in_background` really reaches
/// [`ToolCtx::jobs`] with a live child, without a whole job registry.
#[derive(Debug)]
struct Recording {
    started: std::sync::Mutex<Option<(String, tokio::process::Child)>>,
}

impl Recording {
    fn new() -> Self {
        Self {
            started: std::sync::Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl Jobs for Recording {
    async fn start(&self, command: String, child: tokio::process::Child) -> JobStatus {
        let status = JobStatus {
            id: "bash_1".to_owned(),
            command: command.clone(),
            state: State::Running,
        };
        *self.started.lock().expect("never poisoned") = Some((command, child));

        status
    }

    async fn output(&self, bash_id: &str) -> Result<JobRead, JobsError> {
        Err(JobsError::NotFound(bash_id.to_owned()))
    }

    async fn kill(&self, bash_id: &str) -> Result<JobStatus, JobsError> {
        Err(JobsError::NotFound(bash_id.to_owned()))
    }

    fn list(&self) -> Vec<JobStatus> {
        Vec::new()
    }
}

#[tokio::test]
async fn run_in_background_returns_immediately_naming_the_job_id() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let jobs: Arc<dyn Jobs> = Arc::new(Recording::new());

    let out = ShellTool::new()
        .run(
            serde_json::json!({ "command": "sleep 30", "run_in_background": true }),
            &ToolCtx {
                jobs: Some(Arc::clone(&jobs)),
                ..ctx(dir.path().to_owned())
            },
        )
        .await
        .expect("a background call still completes");

    assert!(
        out.output.contains("bash_1"),
        "the reply should name the job id: {:?}",
        out.output
    );
    assert_eq!(out.metadata["bash_id"], "bash_1");
    assert_eq!(out.metadata["status"], "running");
}

/// Proves the child handed to [`Jobs::start`] is a real, independently
/// running process — not a description of one — by reading a file it
/// writes after `spawn_background` has already returned.
#[tokio::test]
async fn run_in_background_hands_over_a_real_running_child() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let marker = dir.path().join("marker");
    let jobs = Arc::new(Recording::new());
    let command = format!("sleep 0.2; echo yes > {}", posix(&marker));

    ShellTool::new()
        .run(
            serde_json::json!({ "command": command, "run_in_background": true }),
            &ToolCtx {
                jobs: Some(Arc::clone(&jobs) as Arc<dyn Jobs>),
                ..ctx(dir.path().to_owned())
            },
        )
        .await
        .expect("a background call still completes");

    assert!(
        !marker.exists(),
        "the call returned before the delayed write, or nothing was backgrounded"
    );

    let mut child = jobs
        .started
        .lock()
        .expect("never poisoned")
        .take()
        .expect("the tool registered exactly one job")
        .1;
    child
        .wait()
        .await
        .expect("the backgrounded shell can still be waited on directly");

    assert!(
        marker.exists(),
        "the process kept running after the tool call returned"
    );
}

#[tokio::test]
async fn run_in_background_without_a_jobs_handle_is_refused_politely() {
    let dir = tempfile::tempdir().expect("a scratch directory");

    let refused = ShellTool::new()
        .run(
            serde_json::json!({ "command": "true", "run_in_background": true }),
            &ctx(dir.path().to_owned()),
        )
        .await
        .expect_err("nothing here can track a background job");

    assert!(
        matches!(&refused, ToolError::Failed(message) if message.contains("not available")),
        "got {refused:?}"
    );
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
    assert!(
        description.contains("run_in_background")
            && description.contains("bash_output")
            && description.contains("kill_shell"),
        "the background-execution addition should be appended: {description}"
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

    // Git Bash mounts the user's temp directory as its own `/tmp`, and a
    // tempdir inside it is answered as `/tmp/...` — an alias only that
    // shell can undo, which the lane proved when `pwd` said
    // "/tmp/.tmpD6oBQt" for a place no `C:` spelling reaches lexically.
    // `pwd -W` asks the same shell for the native spelling, which
    // `native` reads the way it reads any drive path. Unix keeps plain
    // `pwd`: `-W` is MSYS vocabulary.
    #[cfg(windows)]
    const PWD: &str = "pwd -W";
    #[cfg(not(windows))]
    const PWD: &str = "pwd";

    let rooted = tool
        .run(
            serde_json::json!({ "command": PWD }),
            &ctx(dir.path().to_owned()),
        )
        .await
        .expect("pwd runs");
    let relative = tool
        .run(
            serde_json::json!({ "command": PWD, "workdir": "nested" }),
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
        std::fs::canonicalize(native(text.trim())).unwrap_or_else(|error| {
            panic!(
                "the directory the shell reported exists — it said {:?}, \
                     read here as {:?}: {error}",
                text.trim(),
                native(text.trim()),
            )
        })
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
        out.output.contains("<shell_metadata>") && out.output.contains("exceeding timeout 1000 ms"),
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
/// Its one caller is the unix-gated spill-failure test, so the helper is
/// gated with it rather than left dead on windows.
#[cfg(unix)]
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
