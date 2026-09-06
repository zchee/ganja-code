use buffa::Message as _;
use serde_json::json;

use super::{Answer, Outcome, READ_FIRST, answer, decode, proto, redirect};
use crate::protocol::ToolState;

/// The roster a bridged turn declares in these tests: every tool the redirect
/// table can reach.
fn everything() -> Vec<String> {
    ["read", "bash", "grep", "glob", "write", "webfetch"].into_iter().map(str::to_owned).collect()
}

fn ran(output: &str) -> Outcome {
    Outcome::Ran { output: output.to_owned(), metadata: json!({}) }
}

fn ran_with(output: &str, metadata: serde_json::Value) -> Outcome {
    Outcome::Ran { output: output.to_owned(), metadata }
}

/// The `ExecResponse` one answer put on the wire, decoded back off its bytes.
///
/// Off the bytes rather than off the value, because a field *number* is what
/// each row below pins and only the bytes carry one: a struct field can be
/// read back correctly while the `.proto` has renumbered underneath it.
fn sent(shape: &Answer, outcome: &Outcome) -> Vec<proto::ExecResponse> {
    let messages = answer(shape, Some(4), Some("exec-abc"), outcome);
    let (results, close) = messages.split_at(messages.len() - 1);

    assert!(
        close[0]
            .exec_control
            .as_option()
            .and_then(|control| control.stream_close.as_option())
            .and_then(|close| close.id)
            == Some(4),
        "every exec ends with the close that says it is over"
    );

    results
        .iter()
        .map(|message| {
            let bytes = message.encode_to_vec();
            let decoded =
                proto::ClientMessage::decode_from_slice(&bytes).expect("what was sent decodes");
            let response =
                decoded.exec_response.as_option().expect("a result rides the exec channel").clone();
            assert_eq!(response.id, Some(4), "the id the server minted comes back");
            assert_eq!(response.exec_id.as_deref(), Some("exec-abc"), "and so does the exec id");

            response
        })
        .collect()
}

/// **AC-15, the read row.** `path`, `offset` and `limit` reach ganja's own
/// `read` under its own names, and what it produced comes back on
/// `read_result = 7` with the window reported as applied.
#[test]
fn a_read_exec_runs_ganjas_read_and_answers_on_the_read_result() {
    let bridged = redirect(
        &decode::ExecArgs::Read {
            path: "/repo/src/lib.rs".to_owned(),
            offset: Some(12),
            limit: Some(40),
        },
        &everything(),
    )
    .expect("read is on the roster");

    assert_eq!(bridged.tool, "read");
    assert_eq!(
        bridged.input,
        json!({ "filePath": "/repo/src/lib.rs", "offset": 12, "limit": 40 }),
        "the window passes through unchanged; nothing here re-bases it"
    );

    let outcome = ran_with(
        "<path>/repo/src/lib.rs</path>\n<type>file</type>\n<content>\n12: fn main() {}\n</content>",
        json!({ "truncated": false, "display": { "totalLines": 91 } }),
    );
    let [response] = sent(&bridged.answer, &outcome).try_into().expect("one result");
    let success = response
        .read_result
        .as_option()
        .and_then(|result| result.success.as_option())
        .expect("the read succeeded");

    assert_eq!(success.path.as_deref(), Some("/repo/src/lib.rs"));
    assert!(
        success.content.as_deref().is_some_and(|content| content.contains("12: fn main() {}")),
        "the tool's own output travels verbatim: {:?}",
        success.content
    );
    assert_eq!(success.total_lines, Some(91));
    assert_eq!(success.truncated, Some(false));
    assert_eq!(success.range_applied, Some(true), "a window was asked for");
    assert!(
        !response.redacted_read_result.is_set(),
        "a plain read answers at field 7, never at 29"
    );
}

/// A read with no window says so, rather than reporting a range it did not
/// apply.
#[test]
fn a_read_without_a_window_reports_no_range_applied() {
    let bridged = redirect(
        &decode::ExecArgs::Read { path: "/f".to_owned(), offset: None, limit: None },
        &everything(),
    )
    .expect("read is on the roster");

    assert_eq!(bridged.input, json!({ "filePath": "/f" }));
    let [response] = sent(&bridged.answer, &ran("body")).try_into().expect("one result");
    assert_eq!(
        response
            .read_result
            .as_option()
            .and_then(|result| result.success.as_option())
            .and_then(|success| success.range_applied),
        Some(false)
    );
}

/// `ReadArgs.offset` is an `int32` and `read`'s is unsigned, so a negative one
/// is dropped rather than handed over: passed on, it fails the tool's own
/// deserializer with "invalid type: integer `-3`", which the model reads as a
/// broken client instead of a window that does not exist.
#[test]
fn a_negative_offset_is_dropped_rather_than_handed_to_a_tool_that_cannot_read_it() {
    let bridged = redirect(
        &decode::ExecArgs::Read { path: "/f".to_owned(), offset: Some(-3), limit: Some(10) },
        &everything(),
    )
    .expect("read is on the roster");

    assert_eq!(
        bridged.input,
        json!({ "filePath": "/f", "limit": 10 }),
        "no offset key at all, which is what `read` reads as 'from the top'"
    );
    let [response] = sent(&bridged.answer, &ran("body")).try_into().expect("one result");
    assert_eq!(
        response
            .read_result
            .as_option()
            .and_then(|result| result.success.as_option())
            .and_then(|success| success.range_applied),
        Some(true),
        "the limit is still a window, and the answer says so"
    );
}

/// The redacted read is the same tool and the same message at a **second
/// field**, which is the one thing that separates the two kinds.
#[test]
fn a_redacted_read_answers_at_field_29() {
    let bridged = redirect(
        &decode::ExecArgs::RedactedRead { path: "/f".to_owned(), offset: None, limit: None },
        &everything(),
    )
    .expect("read is on the roster");

    let [response] = sent(&bridged.answer, &ran("body")).try_into().expect("one result");
    assert!(response.redacted_read_result.is_set(), "the redacted kind answers at 29");
    assert!(!response.read_result.is_set(), "and never at 7");
}

/// A read that failed answers on the error arm carrying the tool's own
/// sentence, and a denied one on the rejected arm — which is what tells the
/// model on the other end whether to try something else or to stop asking.
#[test]
fn a_failed_read_is_an_error_and_a_denied_read_is_a_rejection() {
    let shape = Answer::Read { redacted: false, path: "/f".to_owned(), ranged: false };

    let [failed] = sent(&shape, &Outcome::Failed("File not found: /f".to_owned()))
        .try_into()
        .expect("one result");
    let error = failed
        .read_result
        .as_option()
        .and_then(|result| result.error.as_option())
        .expect("the error arm");
    assert_eq!(error.path.as_deref(), Some("/f"));
    assert_eq!(error.error.as_deref(), Some("File not found: /f"));

    let [refused] =
        sent(&shape, &Outcome::Refused("no".to_owned())).try_into().expect("one result");
    assert_eq!(
        refused
            .read_result
            .as_option()
            .and_then(|result| result.rejected.as_option())
            .and_then(|rejected| rejected.reason.as_deref()),
        Some("no")
    );
}

/// **AC-15, the shell row.** The command and its directory reach `bash` under
/// that tool's own names, and the answer is the two events a running client
/// would have written: the output, then the exit.
#[test]
fn a_shell_stream_exec_runs_bash_and_answers_with_stdout_then_exit() {
    let bridged = redirect(
        &decode::ExecArgs::ShellStream {
            command: "ls -a".to_owned(),
            working_directory: "/repo".to_owned(),
        },
        &everything(),
    )
    .expect("bash is on the roster");

    assert_eq!(bridged.tool, "bash");
    assert_eq!(bridged.input, json!({ "command": "ls -a", "workdir": "/repo" }));

    let outcome = ran_with("a\nb\n", json!({ "exit": 0, "truncated": false }));
    let responses = sent(&bridged.answer, &outcome);
    assert_eq!(responses.len(), 2, "the output, then the exit");

    assert_eq!(
        responses[0]
            .shell_stream
            .as_option()
            .and_then(|event| event.stdout.as_option())
            .and_then(|out| out.data.as_deref()),
        Some("a\nb\n")
    );
    assert_eq!(
        responses[1]
            .shell_stream
            .as_option()
            .and_then(|event| event.exit.as_option())
            .and_then(|exit| exit.code),
        Some(0)
    );
}

/// A command that was killed reports no exit code at all, and claiming a clean
/// zero would tell the model it succeeded.
#[test]
fn a_shell_that_reports_no_exit_code_does_not_claim_a_clean_one() {
    let outcome = ran_with("", json!({ "exit": serde_json::Value::Null }));
    let responses = sent(&Answer::Shell, &outcome);

    assert_eq!(
        responses[1]
            .shell_stream
            .as_option()
            .and_then(|event| event.exit.as_option())
            .and_then(|exit| exit.code),
        Some(1)
    );
}

/// A failed shell writes its complaint where a shell's complaints go, and a
/// refused one is the single rejected event D550 already sends.
#[test]
fn a_failed_shell_writes_stderr_and_a_refused_one_writes_only_the_rejection() {
    let responses = sent(&Answer::Shell, &Outcome::Failed("no shell on this machine".to_owned()));
    assert_eq!(responses.len(), 2);
    assert_eq!(
        responses[0]
            .shell_stream
            .as_option()
            .and_then(|event| event.stderr.as_option())
            .and_then(|err| err.data.as_deref()),
        Some("no shell on this machine")
    );
    assert_eq!(
        responses[1]
            .shell_stream
            .as_option()
            .and_then(|event| event.exit.as_option())
            .and_then(|exit| exit.code),
        Some(1)
    );

    let refused = sent(&Answer::Shell, &Outcome::Refused("denied".to_owned()));
    assert_eq!(refused.len(), 1, "a refusal is one event and then the close");
    assert!(
        refused[0].shell_stream.as_option().is_some_and(|event| event.rejected.is_set()),
        "the streamed kind's own rejected event"
    );
}

/// **AC-15, the grep row.** The pattern, path and glob reach `grep` under its
/// own names, and the tool's own output is read back into the match tree the
/// result frame carries.
#[test]
fn a_grep_exec_runs_grep_and_its_output_becomes_the_match_tree() {
    let bridged = redirect(
        &decode::ExecArgs::Grep {
            pattern: "fn main".to_owned(),
            path: "/repo".to_owned(),
            glob: "*.rs".to_owned(),
            case_insensitive: false,
        },
        &everything(),
    )
    .expect("grep is on the roster");

    assert_eq!(bridged.tool, "grep");
    assert_eq!(bridged.input, json!({ "pattern": "fn main", "path": "/repo", "include": "*.rs" }));

    let output = "Found 3 matches\n/repo/a.rs:\n  Line 3: fn main() {}\n  Line 9: fn main2() {}\n\
                  \n\n/repo/b.rs:\n  Line 1: fn main() {}";
    let [response] = sent(&bridged.answer, &ran(output)).try_into().expect("one result");
    let success = response
        .grep_result
        .as_option()
        .and_then(|result| result.success.as_option())
        .expect("the search succeeded");

    assert_eq!(success.pattern.as_deref(), Some("fn main"));
    assert_eq!(success.path.as_deref(), Some("/repo"));
    assert_eq!(success.output_mode.as_deref(), Some("content"));

    let content = success.workspace_results[0]
        .value
        .as_option()
        .and_then(|union| union.content.as_option())
        .expect("matching lines");
    assert_eq!(content.total_matched_lines, Some(3));
    assert_eq!(content.matches.len(), 2, "two files matched");
    assert_eq!(content.matches[0].file.as_deref(), Some("/repo/a.rs"));
    assert_eq!(content.matches[0].matches[0].line_number, Some(3));
    assert_eq!(content.matches[0].matches[0].content.as_deref(), Some("fn main() {}"));
    assert_eq!(content.matches[1].file.as_deref(), Some("/repo/b.rs"));
    assert_eq!(content.matches[1].matches.len(), 1);
}

/// Ganja's `grep` has no case-insensitivity argument, so the member is folded
/// into the pattern its own engine reads. Decoding it and ignoring it would
/// answer a different search than the one that was asked for.
#[test]
fn a_case_insensitive_grep_becomes_an_inline_flag_on_the_pattern() {
    let bridged = redirect(
        &decode::ExecArgs::Grep {
            pattern: "todo".to_owned(),
            path: String::new(),
            glob: String::new(),
            case_insensitive: true,
        },
        &everything(),
    )
    .expect("grep is on the roster");

    assert_eq!(bridged.input, json!({ "pattern": "(?i)todo" }));
}

/// Grep has no rejected arm at all, so a refusal has one place to go and the
/// model reads it there.
#[test]
fn a_refused_grep_travels_on_the_only_arm_it_has() {
    let shape = Answer::Grep { pattern: "x".to_owned(), path: "/repo".to_owned() };
    let [response] =
        sent(&shape, &Outcome::Refused("denied".to_owned())).try_into().expect("one result");

    assert_eq!(
        response
            .grep_result
            .as_option()
            .and_then(|result| result.error.as_option())
            .and_then(|error| error.error.as_deref()),
        Some("denied")
    );
}

/// **AC-15, the ls row**, over a listing written here: what the frame says,
/// given lines of the shape `glob` produces — the files directly in the
/// directory are named, and a child directory is named because a file inside
/// it showed up, which is why the node says its children were **not**
/// processed.
///
/// The listing is *chosen* rather than measured, so this pins the answer
/// builder's field numbers and nothing about the tool. What `glob` really
/// lists — dotfiles included — is measured against a real directory by the
/// test below.
#[test]
fn an_ls_exec_becomes_one_glob_level_and_says_what_it_did_not_walk() {
    let bridged = redirect(&decode::ExecArgs::Ls { path: "/repo".to_owned() }, &everything())
        .expect("glob is on the roster");

    assert_eq!(bridged.tool, "glob");
    assert_eq!(
        bridged.input,
        json!({ "pattern": "{*,*/*}", "path": "/repo" }),
        "one level of files, plus the files that reveal a child directory"
    );

    // The shape a `glob` answer has: absolute paths, one per line. Which
    // paths is this test's own choice.
    let output = "/repo/Cargo.toml\n/repo/README.md\n/repo/src/lib.rs\n/repo/src/main.rs";
    let [response] = sent(&bridged.answer, &ran(output)).try_into().expect("one result");
    let root = response
        .ls_result
        .as_option()
        .and_then(|result| result.success.as_option())
        .and_then(|success| success.directory_tree_root.as_option())
        .expect("the tree");

    assert_eq!(root.abs_path.as_deref(), Some("/repo"));
    assert_eq!(
        root.children_files.iter().filter_map(|file| file.name.as_deref()).collect::<Vec<_>>(),
        vec!["Cargo.toml", "README.md"]
    );
    assert_eq!(root.num_files, Some(2));
    assert_eq!(
        root.children_dirs.iter().filter_map(|dir| dir.abs_path.as_deref()).collect::<Vec<_>>(),
        vec!["/repo/src"],
        "named once, however many of its files showed up"
    );
    assert_eq!(
        root.children_were_processed,
        Some(false),
        "one glob is not a walk, and claiming otherwise would be a lie about the listing"
    );
}

/// **AC-15, the ls row, over a real directory** — the AC's own condition, and
/// the only form of it a change in `glob`'s behaviour can redden.
///
/// The test above hands the answer builder a hand-written listing, which pins
/// the frame's field numbers and nothing about the tool: if `glob` stopped
/// listing dotfiles, or started walking deeper than one level, that string
/// would keep saying otherwise. So this one builds a real directory holding a
/// file, a **dotfile**, a subdirectory with a file in it and a grandchild
/// directory deeper still, runs the redirect's own arguments through the
/// **real** [`ganja_tool::glob::GlobTool`], and asserts the frame that came
/// out of what the tool actually found.
///
/// The dotfile **is** listed, which is measured here rather than assumed: the
/// walker's `hidden(true)` default only applies to an entry the glob override
/// did not match, and `{*,*/*}` is gitignore-glob syntax, where a leading `*`
/// matches a leading dot. The one level the row promises is real too — the
/// grandchild's file matches neither arm of the pattern, so neither it nor its
/// directory appears anywhere in the tree.
#[tokio::test]
async fn an_ls_exec_over_a_real_directory_lists_what_glob_really_finds() {
    let directory = tempfile::tempdir().expect("a temp directory");
    let root = directory.path().to_string_lossy().into_owned();
    std::fs::write(directory.path().join("Cargo.toml"), "[package]\n").expect("the fixture writes");
    std::fs::write(directory.path().join(".hidden"), "a dotfile\n").expect("the fixture writes");
    std::fs::create_dir(directory.path().join("src")).expect("the fixture makes a child");
    std::fs::write(directory.path().join("src/lib.rs"), "// lib\n").expect("the fixture writes");
    std::fs::create_dir(directory.path().join("src/deep")).expect("and a grandchild");
    std::fs::write(directory.path().join("src/deep/buried.rs"), "// deep\n")
        .expect("the fixture writes");

    let bridged = redirect(&decode::ExecArgs::Ls { path: root.clone() }, &everything())
        .expect("glob is on the roster");
    assert_eq!(bridged.tool, "glob");

    // The real tool, with the redirect's own arguments and nothing rewritten:
    // what this asserts about is exactly what a bridged listing would run.
    let context = ganja_tool::ToolCtx {
        cwd: directory.path().to_path_buf(),
        cancel: tokio_util::sync::CancellationToken::new(),
        call_id: "call".to_owned(),
        files: std::sync::Arc::new(ganja_tool::FileTimes::default()),
        credentials: ganja_tool::Credentials::Unguarded,
        spawn: None,
        postbox: None,
        tasks: None,
        ask: None,
        switch: None,
        jobs: None,
    };
    let listed =
        ganja_tool::Tool::run(&ganja_tool::glob::GlobTool, bridged.input.clone(), &context)
            .await
            .expect("a real directory lists");

    let [response] = sent(&bridged.answer, &ran(&listed.output)).try_into().expect("one result");
    let node = response
        .ls_result
        .as_option()
        .and_then(|result| result.success.as_option())
        .and_then(|success| success.directory_tree_root.as_option())
        .expect("the tree");

    assert_eq!(node.abs_path.as_deref(), Some(root.as_str()));
    let files: Vec<&str> =
        node.children_files.iter().filter_map(|file| file.name.as_deref()).collect();
    assert_eq!(
        files,
        vec![".hidden", "Cargo.toml"],
        "the files directly in it, the dotfile among them, sorted as `glob` sorts them"
    );
    assert_eq!(node.num_files, Some(2), "counted, not guessed");
    assert_eq!(
        node.children_dirs.iter().filter_map(|dir| dir.abs_path.as_deref()).collect::<Vec<_>>(),
        vec![format!("{root}/src").as_str()],
        "the child directory, named once because a file inside it showed up"
    );
    assert!(
        !files.iter().any(|name| name.contains("lib.rs")),
        "a file one level down is what named the directory, never a file of this one: {files:?}"
    );
    assert!(
        !listed.output.contains("buried.rs"),
        "and `{{*,*/*}}` never reached the grandchild at all: {}",
        listed.output
    );
    assert_eq!(
        node.children_were_processed,
        Some(false),
        "one glob is not a walk, and claiming otherwise would be a lie about the listing"
    );
}

/// A listing this build cannot match is **refused**, not answered empty.
///
/// `glob` answers with absolute paths and the tree is built by stripping the
/// directory off each of them, so a relative path matches nothing and an
/// absent one — which decodes to `""` — matches *everything* with the root
/// left in front, fabricating a child directory called `/Users` and reporting
/// zero files. Both are refused before the redirect, under a reason naming
/// what is required rather than the kind-level "ganja does not run ls_args".
#[test]
fn a_listing_whose_path_is_not_absolute_is_refused_and_says_what_it_needed() {
    for path in ["", "src", "./src", "~/repo"] {
        let args = decode::ExecArgs::Ls { path: path.to_owned() };

        assert!(
            redirect(&args, &everything()).is_none(),
            "a listing of {path:?} names no directory glob's own output could be matched against"
        );
        assert_eq!(
            super::argument_refusal(&args),
            Some(super::ABSOLUTE_LISTING),
            "and the refusal names the requirement rather than the kind: {path:?}"
        );
    }

    let absolute = decode::ExecArgs::Ls { path: "/repo".to_owned() };
    assert!(redirect(&absolute, &everything()).is_some(), "an absolute path is still redirected");
    assert_eq!(
        super::argument_refusal(&absolute),
        None,
        "and has no argument refusal to answer with"
    );
}

/// A listing whose entries lie somewhere else answers on the **error** arm.
///
/// The distinction is the whole point: `glob` says "No files found" for a
/// directory that really is empty, which leaves the tree honestly empty, while
/// a listing of absolute paths none of which lie under the directory asked
/// about is a search that resolved somewhere else — and reporting *that* as an
/// empty directory would state a fact nobody established.
#[test]
fn a_listing_of_a_different_directory_is_an_error_rather_than_an_empty_one() {
    let bridged = redirect(&decode::ExecArgs::Ls { path: "/repo".to_owned() }, &everything())
        .expect("glob is on the roster");

    let elsewhere = "/somewhere/else/a.rs\n/somewhere/else/b.rs";
    let [response] = sent(&bridged.answer, &ran(elsewhere)).try_into().expect("one result");
    let result = response.ls_result.as_option().expect("a listing answers on ls_result");
    assert!(
        result.success.is_unset(),
        "an empty directory and a listing of somewhere else are different answers: {result:?}"
    );
    let error = result.error.as_option().expect("the kind's own error arm");
    assert_eq!(error.path.as_deref(), Some("/repo"), "named for the path that was asked about");
    assert!(
        error.error.as_deref().is_some_and(|error| error.contains("different directory")),
        "{error:?}"
    );

    // And the case it must not swallow: a directory that really is empty.
    let [empty] = sent(&bridged.answer, &ran("No files found")).try_into().expect("one result");
    let tree = empty
        .ls_result
        .as_option()
        .and_then(|result| result.success.as_option())
        .and_then(|success| success.directory_tree_root.as_option())
        .expect("an empty directory is still a directory");
    assert_eq!(tree.num_files, Some(0));
    assert!(tree.children_dirs.is_empty());
}

/// **AC-15, the write row.** The path and the text reach `write` under its own
/// names, and the counts the result reports are computed from that text —
/// which is exactly what landed on disk.
#[test]
fn a_write_exec_runs_write_and_reports_what_it_wrote() {
    let bridged = redirect(
        &decode::ExecArgs::Write {
            path: "/repo/new.txt".to_owned(),
            file_text: "one\ntwo\n".to_owned(),
        },
        &everything(),
    )
    .expect("write is on the roster");

    assert_eq!(bridged.tool, "write");
    assert_eq!(bridged.input, json!({ "filePath": "/repo/new.txt", "content": "one\ntwo\n" }));

    let [response] =
        sent(&bridged.answer, &ran("Wrote file successfully.")).try_into().expect("one result");
    let success = response
        .write_result
        .as_option()
        .and_then(|result| result.success.as_option())
        .expect("the write succeeded");

    assert_eq!(success.path.as_deref(), Some("/repo/new.txt"));
    assert_eq!(success.lines_created, Some(2));
    assert_eq!(success.file_size, Some(8));
}

/// **AC-16.** Ganja's read-before-write rule applies to a bridged write
/// unchanged, and its refusal rides the **rejected** arm rather than the error
/// one — because it is a refusal the model can act on by asking for a read
/// first, which is what that arm means to the loop on the other end.
///
/// The sentence is built through the real `FileTimes` rather than typed here,
/// so this test fails the day that sentence changes — which is the whole
/// reason the constant it is matched against may be a copy at all.
#[test]
fn a_write_to_a_file_this_session_never_read_is_rejected_with_the_read_first_reason() {
    let refusal = ganja_tool::FileTimes::default()
        .check_fresh_stat(std::path::Path::new("/repo/untouched.txt"), None)
        .expect_err("a file nobody read is refused");
    let sentence = refusal.to_string();
    assert!(
        sentence.contains(READ_FIRST),
        "this module's copy of the read-first sentence has drifted from the tool's: {sentence}"
    );

    let shape = Answer::Write { path: "/repo/untouched.txt".to_owned(), lines: 1, size: 4 };
    let [response] =
        sent(&shape, &Outcome::Failed(sentence.clone())).try_into().expect("one result");
    let rejected = response
        .write_result
        .as_option()
        .and_then(|result| result.rejected.as_option())
        .expect("a read-first refusal is a rejection, not a failure");

    assert_eq!(rejected.path.as_deref(), Some("/repo/untouched.txt"));
    assert_eq!(rejected.reason.as_deref(), Some(sentence.as_str()));
}

/// Any *other* failure is a failure: a write that could not reach the disk is
/// not something asking for a read first would fix.
#[test]
fn a_write_that_failed_for_another_reason_is_an_error() {
    let shape = Answer::Write { path: "/repo/x".to_owned(), lines: 0, size: 0 };
    let [response] = sent(&shape, &Outcome::Failed("could not write /repo/x: no space".to_owned()))
        .try_into()
        .expect("one result");

    assert!(response.write_result.as_option().is_some_and(|result| result.error.is_set()));
}

/// **AC-15, the fetch row.** The url reaches `webfetch` and comes back with
/// what was fetched — and with the two members `webfetch` cannot report left
/// absent rather than invented.
#[test]
fn a_fetch_exec_runs_webfetch_and_reports_only_what_that_tool_knows() {
    let bridged =
        redirect(&decode::ExecArgs::Fetch { url: "https://example.com".to_owned() }, &everything())
            .expect("webfetch is on the roster");

    assert_eq!(bridged.tool, "webfetch");
    assert_eq!(bridged.input, json!({ "url": "https://example.com" }));

    let [response] = sent(&bridged.answer, &ran("# Example")).try_into().expect("one result");
    let success = response
        .fetch_result
        .as_option()
        .and_then(|result| result.success.as_option())
        .expect("the fetch succeeded");

    assert_eq!(success.url.as_deref(), Some("https://example.com"));
    assert_eq!(success.content.as_deref(), Some("# Example"));
}

/// **The roster is the gate.** A seat that is not offering `bash` on this
/// request does not run a shell because the server asked for one, and the
/// exec falls back to the typed refusal it got before this table existed.
#[test]
fn an_exec_whose_tool_is_not_on_this_requests_roster_is_not_redirected() {
    let readonly = vec!["read".to_owned()];

    assert!(
        redirect(
            &decode::ExecArgs::ShellStream {
                command: "rm -rf /".to_owned(),
                working_directory: String::new(),
            },
            &readonly,
        )
        .is_none(),
        "a turn not offering bash does not run a shell"
    );
    assert!(
        redirect(
            &decode::ExecArgs::Read { path: "/f".to_owned(), offset: None, limit: None },
            &readonly
        )
        .is_some(),
        "and the tool it does offer still works"
    );
    assert!(
        redirect(&decode::ExecArgs::Read { path: "/f".to_owned(), offset: None, limit: None }, &[])
            .is_none(),
        "an empty roster redirects nothing at all"
    );
}

/// The kinds with no ganja equivalent are not in the table at any roster: a
/// delete has no tool here, the non-streamed shell is not the kind this build
/// answers, and an `mcp_args` is matched by name rather than redirected.
#[test]
fn the_kinds_outside_the_table_are_never_redirected() {
    for args in [
        decode::ExecArgs::Delete { path: "/f".to_owned() },
        decode::ExecArgs::Shell { command: "ls".to_owned(), working_directory: String::new() },
        decode::ExecArgs::Unmodelled,
    ] {
        assert!(redirect(&args, &everything()).is_none(), "{args:?}");
    }
}

/// **The Dv-8 constants, both of them.** A wire cannot see the permission
/// engine, so the only thing that separates a call somebody refused from a
/// call that ran and broke is the text of the `Error` part — and getting that
/// wrong would report every denial to the model as a broken tool.
#[test]
fn a_permission_refusal_is_told_from_a_failure_by_the_sentence_the_engine_renders() {
    let rejected = ToolState::Error {
        input: json!({}),
        error: ganja_tool::permission_text::REJECTED.to_owned(),
        started: 0,
        completed: 0,
    };
    let denied = ToolState::Error {
        input: json!({}),
        error: format!("{}[]", ganja_tool::permission_text::DENIED_PREFIX),
        started: 0,
        completed: 0,
    };
    let broke = ToolState::Error {
        input: json!({}),
        error: "File not found: /f".to_owned(),
        started: 0,
        completed: 0,
    };

    assert!(matches!(Outcome::of(&rejected), Some(Outcome::Refused(_))));
    assert!(matches!(Outcome::of(&denied), Some(Outcome::Refused(_))));
    assert!(matches!(Outcome::of(&broke), Some(Outcome::Failed(_))));
}

/// A call still running has no outcome, which is what stops a resume from
/// answering the server about a tool that has not finished.
#[test]
fn an_unfinished_call_has_no_outcome_yet() {
    assert_eq!(Outcome::of(&ToolState::Pending { input: None }), None);
    assert_eq!(
        Outcome::of(&ToolState::Running {
            input: json!({}),
            metadata: serde_json::Value::Null,
            started: 0,
        }),
        None
    );
}

/// **The `mcp_result` shapes.** A tool that ran answers on the success arm; a
/// tool that ran and *failed* answers on the same arm with `is_error` set,
/// which is cursor's own shape for it; and only a refusal is a rejection.
#[test]
fn a_bridged_tool_call_answers_success_failure_and_refusal_differently() {
    let [ran] = sent(&Answer::Mcp, &ran("the answer")).try_into().expect("one result");
    let success = ran
        .mcp_result
        .as_option()
        .and_then(|result| result.success.as_option())
        .expect("a tool that ran");
    assert_eq!(
        success.content[0].text.as_option().and_then(|text| text.text.as_deref()),
        Some("the answer")
    );
    assert_eq!(success.is_error, Some(false));

    let [failed] =
        sent(&Answer::Mcp, &Outcome::Failed("it broke".to_owned())).try_into().expect("one result");
    let failure = failed
        .mcp_result
        .as_option()
        .and_then(|result| result.success.as_option())
        .expect("a failed tool is still a tool that ran");
    assert_eq!(failure.is_error, Some(true), "cursor's own shape for a failed tool");

    let [refused] =
        sent(&Answer::Mcp, &Outcome::Refused("denied".to_owned())).try_into().expect("one result");
    assert_eq!(
        refused
            .mcp_result
            .as_option()
            .and_then(|result| result.rejected.as_option())
            .and_then(|rejected| rejected.reason.as_deref()),
        Some("denied")
    );
}
