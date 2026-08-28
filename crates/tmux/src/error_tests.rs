use super::*;
use crate::control_mode::protocol::BlockMarker;

fn marker() -> BlockMarker {
    BlockMarker { time: 1, command: 2, flags: 0 }
}

#[test]
fn a_command_error_with_no_output_lines_names_only_the_command() {
    let err = CommandError {
        line: "list-panes".to_string(),
        response: Response { begin: marker(), end: marker(), lines: Vec::new(), error: true },
    };
    assert_eq!(err.to_string(), r#"tmux: command "list-panes" failed"#);
}

#[test]
fn a_command_error_with_output_lines_joins_them_after_a_colon() {
    let err = CommandError {
        line: "bogus".to_string(),
        response: Response {
            begin: marker(),
            end: marker(),
            lines: vec!["unknown command".to_string(), "near bogus".to_string()],
            error: true,
        },
    };
    assert_eq!(err.to_string(), "tmux: command \"bogus\" failed: unknown command\nnear bogus");
}

#[test]
fn a_protocol_error_without_a_line_omits_the_on_clause() {
    let err = ProtocolError {
        line: None,
        message: "unexpected EOF after %begin for command 2".to_string(),
    };
    assert_eq!(err.to_string(), "tmux: protocol error: unexpected EOF after %begin for command 2");
}

#[test]
fn a_protocol_error_with_a_line_names_it() {
    let err = ProtocolError {
        line: Some("garbage".to_string()),
        message: "unexpected non-control line outside response block".to_string(),
    };
    assert_eq!(
        err.to_string(),
        "tmux: protocol error on \"garbage\": unexpected non-control line outside response block"
    );
}

#[test]
fn an_exit_error_with_no_reason_stays_terse() {
    let err = Error::Exit { reason: String::new() };
    assert_eq!(err.to_string(), "tmux: control client exited");
}

#[test]
fn an_exit_error_with_a_reason_names_it() {
    let err = Error::Exit { reason: "detached".to_string() };
    assert_eq!(err.to_string(), "tmux: control client exited: detached");
}

#[test]
fn a_command_error_converts_into_the_top_level_error_by_from() {
    let source = CommandError {
        line: "x".to_string(),
        response: Response { begin: marker(), end: marker(), lines: Vec::new(), error: true },
    };
    let err: Error = source.clone().into();
    assert_eq!(err.to_string(), source.to_string());
}

#[test]
fn a_protocol_error_converts_into_the_top_level_error_by_from() {
    let source = ProtocolError { line: None, message: "bad".to_string() };
    let err: Error = source.clone().into();
    assert_eq!(err.to_string(), source.to_string());
}

#[test]
fn a_client_that_will_not_start_names_the_word_that_would_have_run() {
    let err = Error::ClientStart {
        command: Some("new-session".to_string()),
        source: Arc::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No such file or directory",
        )),
    };
    assert_eq!(err.to_string(), "tmux: new-session could not be run: No such file or directory");
}

/// A refusal is worth carrying only if it still says what tmux said, so
/// this pins the stderr passthrough — and the fallback for the rare call
/// that fails silently.
#[cfg(unix)]
#[test]
fn a_refused_client_carries_tmuxs_words_or_else_its_status() {
    use std::os::unix::process::ExitStatusExt as _;

    let status = std::process::ExitStatus::from_raw(1 << 8);
    let spoke = Error::ClientRefused {
        command: Some("list-panes".to_string()),
        status,
        stderr: "no server running on /tmp/x".to_string(),
    };
    assert_eq!(spoke.to_string(), "tmux: list-panes failed: no server running on /tmp/x");

    let silent = Error::ClientRefused { command: None, status, stderr: String::new() };
    let message = silent.to_string();
    let prefix = "tmux: the client failed: ";
    assert!(message.starts_with(prefix), "{message:?}");
    assert!(
        message.len() > prefix.len(),
        "a silent refusal must still say how the client ended: {message:?}"
    );
}
