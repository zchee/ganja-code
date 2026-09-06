use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::time::Duration;

use ganja_testkit::tmux::PrivateServer;

use super::{
    Closed, Killed, LAUNCH_HEAD, LIVENESS_FORMAT, Launch, Listed, PANE_FORMAT, Placement,
    REFUSED_NO_TMUX, Server, TmuxError, after_kill, buffer_name, environment, parse_listing,
    parse_pane, shell_quote, socket_of,
};
use crate::reaper::Pane;

/// The recording of grok's own TUI probe. The refusal sentence a dead
/// grok pane shows is read off it rather than restated here, so the
/// capture test below exercises the very line a fixture comparison will
/// look for.
const GROK_TUI_PROBE: &str = include_str!("../tests/fixtures/grok-tui-probe.txt");

/// The refusal has to say which variable, because that is the whole of what
/// somebody reading it can act on.
#[test]
fn the_refusal_names_the_variable_and_the_way_out() {
    assert!(REFUSED_NO_TMUX.contains("$TMUX"));
    assert!(REFUSED_NO_TMUX.contains("in-process"));
}

/// `$TMUX` is `socket,pid,index`, and only the socket is wanted — read the
/// way the client reads it, up to the first comma.
#[test]
fn the_socket_is_the_value_up_to_the_first_comma() {
    assert_eq!(
        socket_of(OsStr::new("/private/tmp/tmux-501/default,4242,0")),
        std::path::Path::new("/private/tmp/tmux-501/default")
    );
    assert_eq!(socket_of(OsStr::new("/tmp/sock")), std::path::Path::new("/tmp/sock"));
    assert!(socket_of(OsStr::new(",1,2")).as_os_str().is_empty());
}

/// The pair is spelled by one format and read by one parser, and the
/// parser refuses what is not a pane id beside a pid.
#[test]
fn a_format_line_reads_back_as_the_pair() {
    assert_eq!(PANE_FORMAT, "#{pane_id} #{pane_pid}");
    let pane = parse_pane("%17 48213").expect("a pane line parses");
    assert_eq!(pane.id, "%17");
    assert_eq!(pane.birth, "48213");

    assert!(parse_pane("%17").is_none(), "no second half");
    assert!(parse_pane("17 48213").is_none(), "not a pane id");
    assert!(parse_pane("%17 forty").is_none(), "not a pid");
    assert!(parse_pane("").is_none());
}

/// A word is quoted so the shell on the other end reads it back byte for
/// byte — and only when it must be: a safe word rides bare, a backslash
/// never rides inside single quotes (fish reads it as an escape there),
/// and the NUL byte no shell word can carry is refused rather than sent.
#[test]
fn a_shell_word_survives_quoting() {
    let quote = |text: &str| {
        super::shell_quote(OsStr::new(text))
            .expect("no NUL rides these words")
            .into_string()
            .expect("ascii")
    };

    assert_eq!(quote("plain"), "plain");
    assert_eq!(quote("/with space/a;b"), "'/with space/a;b'");
    assert_eq!(quote("it's"), "\"it's\"");
    assert_eq!(quote("a\\b"), "\"a\\\\b\"");
    assert_eq!(quote(""), "''");

    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

        let quoted = super::shell_quote(OsStr::from_bytes(b"a\x80b"))
            .expect("bytes outside UTF-8 still quote");
        assert_eq!(quoted.into_vec(), b"'a\x80b'".to_vec());

        let refused = super::shell_quote(OsStr::from_bytes(b"a\0b"));
        assert!(matches!(refused, Err(super::TmuxError::Unquotable { .. })));
    }
}

/// Exactly one pane of the window is where typing goes, and it is the one
/// tmux itself calls active — a fresh split's answer is read off tmux
/// rather than assumed, since whether a split takes the focus is tmux's
/// choice.
#[tokio::test]
async fn focused_answers_for_the_active_pane_of_the_current_window_and_no_other() {
    let server = PrivateServer::start(&["sleep", "3600"], &[], &[]);
    let at = Server::at(server.socket(), Some(server.first_pane().to_owned()));
    let other = server.split(None, &[], &["sleep", "3600"]);

    let first = at.focused(server.first_pane()).await.expect("tmux answers");
    let second = at.focused(&other).await.expect("tmux answers");
    assert_ne!(first, second, "one pane of the window has the focus");
    let active = server.run(&["display-message", "-p", "-t", &other, "#{pane_active}"]);
    assert_eq!(second, active.trim() == "1");
}

/// The composed line opens on the wipe, then quotes only the words that
/// need it; the head is exactly the three sequences (**D554**), ends on the
/// `; ` that makes the `exec` a second command, and is absent from the
/// head-less composition a reader matches on; and a word no quoting can
/// carry refuses the whole line before tmux is handed anything.
#[test]
fn a_launch_line_wipes_then_quotes_what_needs_it_and_refuses_a_nul() {
    let binary = std::path::Path::new("/opt/ganja builds/ganja");
    let argv = [OsString::from("--agent-name"), OsString::from("it's")];
    let line = super::launch_line(binary, &argv)
        .expect("no NUL rides these words")
        .into_string()
        .expect("ascii");
    assert_eq!(line, format!("{LAUNCH_HEAD}exec '/opt/ganja builds/ganja' --agent-name \"it's\""));

    // `ED 2`, `ED 3`, `CUP`, as the bytes a shell's `printf` decodes them
    // from — and nothing else, so a fallback is one edit of one constant.
    assert_eq!(LAUNCH_HEAD, "printf '\\033[2J\\033[3J\\033[H'; ");
    assert!(LAUNCH_HEAD.ends_with("; "), "the head is a command of its own before the exec");

    let exec = super::exec_line(binary, &argv)
        .expect("no NUL rides these words")
        .into_string()
        .expect("ascii");
    assert_eq!(exec, "exec '/opt/ganja builds/ganja' --agent-name \"it's\"");
    assert_eq!(line, format!("{LAUNCH_HEAD}{exec}"), "the line is the head, then the exec");

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;

        let word = OsString::from_vec(b"a\0b".to_vec());
        let refused = super::launch_line(std::path::Path::new("/bin/ganja"), &[word]);
        assert!(matches!(refused, Err(super::TmuxError::Unquotable { .. })));
    }
}

/// The environment helper renders exactly the names it is given that are
/// set, and never a name it was not given — the D502 mechanism.
///
/// Reads two variables every process has and one no process has, rather
/// than setting any: this module's tests share a process.
#[test]
fn only_the_named_variables_travel_and_only_when_set() {
    let path = std::env::var_os("PATH").expect("every process has a PATH");
    let mut expected = OsString::from("PATH=");
    expected.push(&path);

    let carried = environment(["PATH", "GANJA_TMUX_TEST_NOBODY_SETS_THIS"]);
    assert_eq!(carried, vec![expected]);

    assert!(environment(std::iter::empty()).is_empty());
}

/// The liveness listing is the pair with tmux's verdict in front — pinned
/// as that composition, so the split's format and the listing's cannot
/// drift apart — and dead is that verdict's word alone: a `1` is skipped
/// whatever follows the id (measured: nothing, the pid of a process the
/// pane no longer has), while a `0` over a pane with no pid is
/// **unreadable**, never dead, because a pane this parser drops is one a
/// kill answers `AlreadyGone` for and the reaper deletes the record of.
#[test]
fn a_listing_line_is_live_dead_or_unreadable_on_tmuxs_own_verdict() {
    assert_eq!(
        LIVENESS_FORMAT.strip_prefix("#{pane_dead} "),
        Some(PANE_FORMAT),
        "the tail is the pair, spelled once"
    );

    assert_eq!(
        parse_listing("0 %2 48213"),
        Some(Listed::Live(Pane { id: "%2".to_owned(), birth: "48213".to_owned() }))
    );
    assert_eq!(parse_listing("1 %2"), Some(Listed::Dead), "the measured shape");
    assert_eq!(
        parse_listing("1 %2 48213"),
        Some(Listed::Dead),
        "tmux's word, not the tail, says dead"
    );

    assert!(parse_listing("0 %2").is_none(), "a live pane with no pid stays loud");
    assert!(parse_listing("0 %2 forty").is_none(), "not a pid");
    assert!(parse_listing("%2 48213").is_none(), "no verdict");
    assert!(parse_listing("2 %2 48213").is_none(), "not a verdict");
    assert!(parse_listing("1 2").is_none(), "dead, but not a pane id");
    assert!(parse_listing("1").is_none(), "a verdict over nothing");
    assert!(parse_listing("").is_none());
}

/// After the kill, `Alive` is for a pane the listing shows running and
/// for nothing else: gone is closed, running is left alone (it was
/// respawned under the kill), and still-there-still-dead is the honest
/// fourth answer — a kill that did not take is never reported as a live
/// member. Pinned here without a server because a real tmux cannot be
/// cheaply made to refuse a `kill-pane`; the real-server test below
/// covers the three answers it can produce.
#[test]
fn after_the_kill_only_a_running_pane_answers_alive() {
    assert_eq!(after_kill(None), Closed::Yes);
    assert_eq!(after_kill(Some(false)), Closed::Alive);
    assert_eq!(
        after_kill(Some(true)),
        Closed::Refused,
        "a pane still listed dead after the kill is not alive"
    );
}

/// Every delivery loads its own buffer, named for this process and a
/// counter, so two deliveries on one server cannot paste each other's
/// text — which is the whole of what the name promises.
#[test]
fn every_delivery_gets_a_buffer_name_of_its_own() {
    let first = buffer_name();
    let second = buffer_name();
    assert_ne!(first, second);

    let prefix = format!("ganja-{}-", std::process::id());
    assert!(first.starts_with(&prefix), "{first}");
    assert!(second.starts_with(&prefix), "{second}");
}

// ---- Against a real tmux, on a private server of the test's own --------
//
// `PrivateServer` hard-fails without tmux and kills its server when it
// drops, panics included, so no pane or process outlives a test. Nothing
// below touches the process environment: the server is reached through
// `Server::at`, never `$TMUX`.

/// A pane on `at` running `argv` in `cwd`, beside the lead's.
async fn split(at: &Server, cwd: &Path, argv: &[&str]) -> Pane {
    let argv: Vec<OsString> = argv.iter().map(OsString::from).collect();
    at.split(Launch {
        cwd,
        environment: &[],
        argv: &argv,
        placement: Placement::Beside { share: crate::pane::DEFAULT_SHARE },
    })
    .await
    .expect("the private server splits a pane")
}

/// The column takes the share it is handed, and the lead keeps the rest:
/// on a 200-column window a 65 splits off a pane about 130 wide (tmux
/// rounds, and a border takes a column), and a 35 one about 70.
#[tokio::test]
async fn a_column_beside_the_lead_takes_the_share_it_is_given() {
    for (share, expected) in [(65u8, 130usize), (35, 70)] {
        let server = PrivateServer::start(&["sleep", "3600"], &[], &[]);
        let at = Server::at(server.socket(), Some(server.first_pane().to_owned()));
        let argv: Vec<OsString> = ["sleep", "3600"].iter().map(OsString::from).collect();
        let pane = at
            .split(Launch {
                cwd: Path::new("/"),
                environment: &[],
                argv: &argv,
                placement: Placement::Beside { share },
            })
            .await
            .expect("the private server splits a pane");
        let width: usize = server
            .run(&["display-message", "-p", "-t", &pane.id, "#{pane_width}"])
            .trim()
            .parse()
            .expect("a pane width");
        assert!(
            width.abs_diff(expected) <= 2,
            "a share of {share} on 200 columns gave the column {width} columns"
        );
    }
}

/// Polls `probe` until it is satisfied, or fails naming `what` and the
/// last state it saw once the bound a real server is held to has passed.
async fn eventually(what: &str, mut probe: impl AsyncFnMut() -> Result<(), String>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let Err(state) = probe().await else {
            return;
        };
        assert!(
            tokio::time::Instant::now() < deadline,
            "gave up waiting for {what}; last seen: {state}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// What a pane shows comes back as text, and a line the pane wrapped at
/// its width comes back as the one line it was — grok's recorded refusal
/// sentence is longer than a narrow pane is wide, and whoever compares a
/// dead pane's words against that recording has to see it whole.
#[tokio::test]
async fn a_capture_reads_what_the_pane_shows_with_wrapped_lines_rejoined() {
    // The sentence is the recording's, not a restatement of it: the very
    // bytes a fixture comparison will look for in a dead grok pane.
    let sentence = GROK_TUI_PROBE
        .lines()
        .find_map(|line| line.strip_prefix("error: "))
        .expect("the grok recording carries the vendor's refusal verbatim");
    let cwd = ganja_testkit::temp_dir();
    // 80 columns, of which the teammate's column takes 70%: narrower than
    // the sentence, so the pane has to wrap it.
    let server = PrivateServer::start_in(cwd.path(), (80, 24), &["sleep", "3600"], &[], &[]);
    let at = Server::at(server.socket(), Some(server.first_pane().to_owned()));
    let quoted = shell_quote(OsStr::new(sentence))
        .expect("no NUL in the sentence")
        .into_string()
        .expect("ascii");
    let script = format!("printf '%s\\n' {quoted}; exec sleep 3600");
    let pane = split(&at, cwd.path(), &["sh", "-c", &script]).await;

    let width: usize = server
        .run(&["display-message", "-p", "-t", &pane.id, "#{pane_width}"])
        .trim()
        .parse()
        .expect("a pane width");
    assert!(
        width < sentence.chars().count(),
        "the premise: the sentence wraps in a {width}-column pane"
    );

    eventually("the sentence to show in the pane, whole", async || {
        let shown = at.capture(&pane.id).await.map_err(|error| error.to_string())?;
        if shown.lines().any(|line| line == sentence) { Ok(()) } else { Err(format!("{shown:?}")) }
    })
    .await;
}

/// What `pane_id` shows **and** everything scrolled above it: the visible
/// rows and the whole history, joined the way [`Server::capture`] joins —
/// the reader a person scrolling up in the pane is.
fn screen_and_history(server: &PrivateServer, pane_id: &str) -> String {
    server.run(&["capture-pane", "-p", "-J", "-S", "-", "-E", "-", "-t", pane_id])
}

/// **D554.** The launch line wipes the pane before it execs: what the idle
/// shell put on the screen before the line — a rc file's banner, its
/// prompt, the echo of the line itself — and the scrollback behind it are
/// gone by the time the CLI's first row shows, under the default `sh` and
/// under `bash` alike, each run with no startup files so the pane shell is
/// exactly what the test typed into.
///
/// The banner is made to **overflow** the pane so part of it is in the
/// history before the line is typed — a banner that never scrolled would
/// prove the screen wiped and say nothing about the scrollback, which is
/// the sequence `ED 3` is in the head for (an inline TUI's scrollback is the
/// pane's). The stand-in for the CLI is a `sh -c` that prints its first row
/// and sleeps, which is enough: the wipe is the shell's own act, ordered
/// before the `exec`, and the CLI never sees the screen it inherits.
#[tokio::test]
async fn a_launch_line_leaves_nothing_of_the_shells_on_the_screen_or_in_the_history() {
    let shells: [&[&str]; 2] = [&["/bin/sh", "-s"], &["/bin/bash", "--norc", "--noprofile", "-s"]];
    for (index, shell) in shells.into_iter().enumerate() {
        let banner = format!("BANNER-{}-{index}", std::process::id());
        let cwd = ganja_testkit::temp_dir();
        // A short window, so thirty banner rows overflow the pane.
        let server = PrivateServer::start_in(cwd.path(), (80, 12), &["sleep", "3600"], &[], &[]);
        let at = Server::at(server.socket(), Some(server.first_pane().to_owned()));
        let pane = split(&at, cwd.path(), shell).await;

        // What a rc file would have printed, standing in for `direnv`'s rows.
        let rows = format!("i=0; while [ $i -lt 30 ]; do i=$((i+1)); echo {banner}-$i; done");
        at.type_line(&pane.id, OsStr::new(&rows)).await.expect("the shell hears its banner");
        eventually("the banner to overflow into the history", async || {
            let size = server.run(&["display-message", "-p", "-t", &pane.id, "#{history_size}"]);
            let all = screen_and_history(&server, &pane.id);
            if all.contains(&banner) && size.trim() != "0" {
                Ok(())
            } else {
                Err(format!("history_size={} shown={all:?}", size.trim()))
            }
        })
        .await;

        let line = super::launch_line(
            Path::new("/bin/sh"),
            &[OsString::from("-c"), OsString::from("printf 'MARK\\n'; exec sleep 3600")],
        )
        .expect("no NUL rides these words");
        at.type_line(&pane.id, &line).await.expect("the shell hears its launch line");
        eventually("the CLI's first row to show", async || {
            let shown = at.capture(&pane.id).await.map_err(|error| error.to_string())?;
            if shown.lines().any(|row| row == "MARK") { Ok(()) } else { Err(format!("{shown:?}")) }
        })
        .await;

        let shown = at.capture(&pane.id).await.expect("the pane captures");
        let all = screen_and_history(&server, &pane.id);
        for (what, text) in [("screen", &shown), ("screen and history", &all)] {
            assert!(
                !text.contains(&banner),
                "{shell:?}: the {what} still holds the banner: {text:?}"
            );
            assert!(
                !text.contains("exec /bin/sh"),
                "{shell:?}: the {what} still holds the exec: {text:?}"
            );
            assert!(
                !text.contains(LAUNCH_HEAD.trim_end()),
                "{shell:?}: the {what} still holds the head's echo: {text:?}"
            );
            assert!(
                text.lines().any(|row| row == "MARK"),
                "{shell:?}: the {what} lost the CLI's row"
            );
        }
        assert_eq!(
            server.run(&["display-message", "-p", "-t", &pane.id, "#{history_size}"]).trim(),
            "0",
            "{shell:?}: the scrollback was emptied, not merely scrolled"
        );
    }
}

/// Multi-line text reaches the pane's program whole — its newlines, its
/// quotes and its non-ASCII as given — followed by the one Enter that
/// submits it; an empty text delivers nothing, not even that Enter; and
/// the buffer the text travelled in is gone once it has been pasted.
///
/// The stub is a `cat` writing the pane's input to a file, so what the
/// pane's program received is read back as bytes rather than as a
/// screen: the cooked pty turns the paste's `\r` separators back into
/// the newlines that were loaded, which is what a TUI's bracketed paste
/// does on its own side.
#[tokio::test]
async fn a_pasted_text_reaches_the_pane_whole_with_its_newlines_and_is_submitted() {
    const TEXT: &str = "line one\nline two; with 'quotes' and \"doubles\"\n\tindented ünïcödé";
    let dir = ganja_testkit::temp_dir();
    let server = PrivateServer::start(&["sleep", "3600"], &[], &[]);
    let at = Server::at(server.socket(), Some(server.first_pane().to_owned()));
    let received = dir.path().join("received.txt");
    let pane = split(
        &at,
        dir.path(),
        &["sh", "-c", "exec cat > \"$0\"", received.to_str().expect("a utf-8 temp path")],
    )
    .await;
    // The redirection opens the file before `cat` runs: the stub is
    // listening once the file exists.
    eventually("the stub to open its file", async || {
        if received.exists() { Ok(()) } else { Err("no file yet".to_owned()) }
    })
    .await;

    at.paste_submit(&pane.id, "").await.expect("an empty text is no message, and no error");
    at.paste_submit(&pane.id, TEXT).await.expect("the text is delivered");

    // Exactly the text and the one Enter after it: had the empty paste
    // pressed Enter, the file would open with a newline of its own.
    let expected = format!("{TEXT}\n");
    eventually("the pasted text to reach the stub's file", async || {
        let got = std::fs::read_to_string(&received).map_err(|error| error.to_string())?;
        if got == expected { Ok(()) } else { Err(format!("{got:?}")) }
    })
    .await;
    assert_eq!(server.run(&["list-buffers"]).trim(), "", "the buffer was freed by the paste");
}

/// A delivery dropped mid-flight — the shim runtime's `select!` letting
/// go of it — leaves no buffer on the server, pastes nothing and presses
/// no Enter, and the pane takes the next delivery as if the dropped one
/// had never been asked for.
///
/// Mid-flight by construction rather than by timing: the text is well
/// past what a pipe holds and the future is dropped after its **first**
/// poll, which spawned the client and filled the pipe and then had to
/// wait — so the writer itself had not handed the text over, and the
/// client cannot have seen it end. What the drop then does is the thing
/// under test: kill the client before closing its stdin, so
/// the cut is never read as the text's end and tmux, which loads a
/// buffer only from a read it saw end, sets nothing. `biased;` is what
/// makes the first branch the one polled first; without it the select
/// might never poll the delivery at all and prove nothing.
#[tokio::test]
async fn a_delivery_dropped_mid_flight_leaves_no_buffer_and_presses_no_enter() {
    let dir = ganja_testkit::temp_dir();
    let server = PrivateServer::start(&["sleep", "3600"], &[], &[]);
    let at = Server::at(server.socket(), Some(server.first_pane().to_owned()));
    let received = dir.path().join("received.txt");
    let pane = split(
        &at,
        dir.path(),
        &["sh", "-c", "exec cat > \"$0\"", received.to_str().expect("a utf-8 temp path")],
    )
    .await;
    eventually("the stub to open its file", async || {
        if received.exists() { Ok(()) } else { Err("no file yet".to_owned()) }
    })
    .await;

    // ~4 MiB: two orders of magnitude past any pipe buffer (64 KiB at
    // most), so one poll of the writer ends in a full pipe, not in EOF.
    let dropped: String = (0..65536)
        .map(|line| format!("line {line:05}: the quick brown fox jumps over the lazy dog\n"))
        .collect();
    tokio::select! {
        biased;
        outcome = at.paste_submit(&pane.id, &dropped) => {
            panic!("a delivery this size cannot finish in one poll: {outcome:?}");
        }
        () = std::future::ready(()) => {}
    }
    // The select dropped the delivery, and with it the client. No await
    // has passed since: whatever the server holds for that client, it
    // is not a buffer — one is set only from a read that ended.
    assert_eq!(
        server.run(&["list-buffers"]).trim(),
        "",
        "a dropped delivery left no buffer on the server"
    );

    // The pane is untouched, and the next delivery is the first thing it
    // hears: had the dropped one pasted, the file would open with its
    // text; had it pressed Enter, with a newline.
    at.paste_submit(&pane.id, "after\n").await.expect("the next delivery lands");
    eventually("the next delivery to reach the stub's file", async || {
        let got = std::fs::read_to_string(&received).map_err(|error| error.to_string())?;
        if got == "after\n\n" {
            Ok(())
        } else {
            Err(format!("{:?}", got.chars().take(120).collect::<String>()))
        }
    })
    .await;
    assert_eq!(
        server.run(&["list-buffers"]).trim(),
        "",
        "and the server's buffer stack is as the test found it"
    );
}

/// Text larger than a pipe holds is handed to tmux whole through stdin
/// — the write and the wait run together, so neither side wedges the
/// other — and none of it is on argv. The buffer is named the way every
/// delivery's is and freed the way every delivery's is, so the test
/// bypasses neither rule the module establishes.
#[tokio::test]
async fn a_large_text_is_handed_to_tmux_through_stdin_whole() {
    let dir = ganja_testkit::temp_dir();
    let server = PrivateServer::start(&["sleep", "3600"], &[], &[]);
    let at = Server::at(server.socket(), None);
    // ~230 KiB: past any pipe buffer this machine hands out (64 KiB at
    // most), so a write that waited for the read to finish would stall.
    let text: String = (0..4096)
        .map(|line| format!("line {line:04}: the quick brown fox jumps over the lazy dog\n"))
        .collect();
    let buffer = buffer_name();

    let mut load = at.command();
    load.arg("load-buffer").arg("-b").arg(&buffer).arg("-");
    super::feed("load-buffer", load, text.as_bytes()).await.expect("tmux takes the whole text");

    let saved = dir.path().join("buffer.txt");
    server.run(&["save-buffer", "-b", &buffer, saved.to_str().expect("a utf-8 temp path")]);
    assert_eq!(std::fs::read_to_string(&saved).expect("the saved buffer reads"), text);

    server.run(&["delete-buffer", "-b", &buffer]);
    assert_eq!(
        server.run(&["list-buffers"]).trim(),
        "",
        "the test leaves the server's buffer stack as it found it"
    );
}

/// A pane kept on exit is still there after its process ended — dead,
/// its last words readable through `capture` — while the liveness
/// listing neither lists it (tmux's own word is that it is dead, and a
/// pane with no process has no pair) nor refuses because of it, so an
/// identity-checked kill of the recorded pair finds it already gone and
/// leaves the dead pane on screen for a person to read. Against a real
/// server, this is also where the listing format's dead shape is pinned:
/// a line the parser could not read would fail the listing here.
#[tokio::test]
async fn a_pane_kept_on_exit_stays_readable_after_its_process_dies() {
    let dir = ganja_testkit::temp_dir();
    let server = PrivateServer::start(&["sleep", "3600"], &[], &[]);
    let at = Server::at(server.socket(), Some(server.first_pane().to_owned()));
    let pane = split(
        &at,
        dir.path(),
        &["sh", "-c", "read line; printf 'last words: %s\\n' \"$line\"; exit 1"],
    )
    .await;

    at.remain_on_exit(&pane.id, true).await.expect("the pane option is set");
    assert_eq!(
        server.run(&["show-options", "-p", "-v", "-t", &pane.id, "remain-on-exit"]).trim(),
        "on"
    );

    at.type_line(&pane.id, OsStr::new("refused by the vendor"))
        .await
        .expect("the stub hears its line");
    eventually("the pane's process to die", async || {
        let dead = server.run(&["display-message", "-p", "-t", &pane.id, "#{pane_dead}"]);
        if dead.trim() == "1" { Ok(()) } else { Err(format!("pane_dead={}", dead.trim())) }
    })
    .await;

    assert!(server.panes().contains(&pane.id), "the dead pane is still on the server");
    let shown = at.capture(&pane.id).await.expect("a dead pane still captures");
    assert!(
        shown.lines().any(|line| line == "last words: refused by the vendor"),
        "its last words are readable: {shown:?}"
    );
    // The tmux fact `capture_with_history` exists for (D554): the dead-pane
    // notice's own linefeed scrolled the pane's top row — here the tty's
    // echo of the typed line — into the history, off the visible screen and
    // out of a plain capture, and the history read still has it.
    assert!(
        !shown.lines().any(|line| line == "refused by the vendor"),
        "the top row was scrolled off by the dead notice: {shown:?}"
    );
    let whole = at.capture_with_history(&pane.id).await.expect("the history reads too");
    assert!(
        whole.lines().any(|line| line == "refused by the vendor"),
        "and the history still holds it: {whole:?}"
    );
    assert!(
        whole.lines().any(|line| line == "last words: refused by the vendor"),
        "beside the words themselves: {whole:?}"
    );

    let live = at.panes().await.expect("a dead pane does not make the listing unreadable");
    assert!(
        !live.iter().any(|listed| listed.id == pane.id),
        "and it is not in the liveness listing: {live:?}"
    );
    assert_eq!(at.kill(&pane).await.expect("the kill reads the listing"), Killed::AlreadyGone);
    assert!(server.panes().contains(&pane.id), "the kill left the dead pane where it was");
}

/// Turned back off, the option is really off: the pane closes with its
/// process, and there is nothing left to capture.
#[tokio::test]
async fn a_pane_no_longer_kept_on_exit_closes_with_its_process() {
    let dir = ganja_testkit::temp_dir();
    let server = PrivateServer::start(&["sleep", "3600"], &[], &[]);
    let at = Server::at(server.socket(), Some(server.first_pane().to_owned()));
    let pane = split(&at, dir.path(), &["sh", "-c", "read line; exit 1"]).await;

    at.remain_on_exit(&pane.id, true).await.expect("on");
    at.remain_on_exit(&pane.id, false).await.expect("and off again");
    at.type_line(&pane.id, OsStr::new("bye")).await.expect("the stub hears its line");

    eventually("the pane to close", async || {
        let listed = server.panes();
        if listed.contains(&pane.id) { Err(format!("still listed: {listed:?}")) } else { Ok(()) }
    })
    .await;
    assert!(
        matches!(
            at.capture(&pane.id).await,
            Err(TmuxError::Failed { command: "capture-pane", .. })
        ),
        "a closed pane has nothing to capture"
    );
}

/// `close_dead` ends a pane only once its process has: asked of a live
/// pane it answers `Alive` and touches nothing, asked of the same pane
/// dead it closes it, asked again it finds nothing by that id — and an
/// id the listing does not print never reaches the server's command
/// string, which is what keeps a recorded id from carrying a second
/// command in. The three answers a real server produces; the fourth,
/// `Refused`, needs a `kill-pane` that does not take, which a real
/// server cannot cheaply be made to do, and is pinned on `after_kill`
/// without one.
#[tokio::test]
async fn close_dead_closes_a_dead_pane_and_leaves_a_live_one_alone() {
    let dir = ganja_testkit::temp_dir();
    let server = PrivateServer::start(&["sleep", "3600"], &[], &[]);
    let at = Server::at(server.socket(), Some(server.first_pane().to_owned()));
    let pane = split(&at, dir.path(), &["sh", "-c", "read line; exit 1"]).await;
    at.remain_on_exit(&pane.id, true).await.expect("kept on exit");

    assert_eq!(
        at.close_dead(&pane.id).await.expect("a live pane is classified, not an error"),
        Closed::Alive
    );
    assert!(server.panes().contains(&pane.id), "and was left where it was");
    assert_eq!(
        server.run(&["display-message", "-p", "-t", &pane.id, "#{pane_dead}"]).trim(),
        "0",
        "with its process still running"
    );

    at.type_line(&pane.id, OsStr::new("bye")).await.expect("the stub hears its line");
    eventually("the pane's process to die", async || {
        let dead = server.run(&["display-message", "-p", "-t", &pane.id, "#{pane_dead}"]);
        if dead.trim() == "1" { Ok(()) } else { Err(format!("pane_dead={}", dead.trim())) }
    })
    .await;

    assert_eq!(at.close_dead(&pane.id).await.expect("a dead pane closes"), Closed::Yes);
    assert!(!server.panes().contains(&pane.id), "the dead pane is gone: {:?}", server.panes());
    assert_eq!(
        at.close_dead(&pane.id).await.expect("nothing by that id is not an error"),
        Closed::AlreadyGone
    );

    // An id that is not one tmux printed — here one that would read as a
    // second command inside `if-shell`'s string — is answered off the
    // listing alone, and the server is still standing afterwards.
    let forged = format!("{}; kill-server", server.first_pane());
    assert_eq!(
        at.close_dead(&forged).await.expect("an unknown id is not an error"),
        Closed::AlreadyGone
    );
    assert!(
        server.panes().contains(&server.first_pane().to_owned()),
        "and nothing reached the server"
    );
}
