use super::*;

// AC-4 note: Go's "environment entries must be KEY=VALUE" case
// (`TestOptionsValidation`) has no counterpart — see the module doc's
// `Env` divergence. Go's `TestOptionsCloneEnv` (defensive-copy proof for
// a `[]string` field) also has no counterpart: `Options` here has no
// `clone_env` accessor to defend — `Options: Clone` already makes a
// caller's copy independent, and `env()` returns a borrowed slice a
// caller cannot mutate through at all. Go's "stderr limit must be
// non-negative" case (also `TestOptionsValidation`) has no counterpart
// either — see the module doc's `stderr_line_limit` divergence: `usize`
// makes a negative value unrepresentable.

fn valid() -> Options {
    Options::new().with_session_name("safe")
}

#[test]
fn an_explicit_initial_command_is_valid() {
    Options::new().with_initial_command(["new-session", "-A", "-s", "safe"]).validate().unwrap();
}

#[test]
fn an_explicit_session_target_is_valid() {
    valid().validate().unwrap();
}

#[test]
fn an_implicit_default_attach_is_rejected() {
    let err = Options::new().validate().unwrap_err();
    assert!(err.to_string().contains("initial_command or session_name"));
}

#[test]
fn socket_name_and_path_conflict_is_rejected() {
    let err = Options::new()
        .with_socket_name("a")
        .with_socket_path("/tmp/a")
        .with_session_name("safe")
        .validate()
        .unwrap_err();
    assert!(err.to_string().contains("mutually exclusive"));
}

#[test]
fn a_zero_event_buffer_is_rejected() {
    let err = valid().with_event_buffer(0).validate().unwrap_err();
    assert!(err.to_string().contains("event_buffer must be > 0"));
}

#[test]
fn a_zero_shutdown_timeout_is_rejected() {
    let err = valid().with_shutdown_timeout(Duration::ZERO).validate().unwrap_err();
    assert!(err.to_string().contains("shutdown_timeout must be > 0"));
}

#[test]
fn an_initial_command_argument_with_a_newline_is_rejected() {
    let err = Options::new().with_initial_command(["new-session\n"]).validate().unwrap_err();
    assert!(err.to_string().contains("contains a newline"));
}

#[test]
fn a_session_name_with_a_newline_is_rejected() {
    let err = Options::new().with_session_name("bad\n").validate().unwrap_err();
    assert!(err.to_string().contains("session_name contains a newline"));
}

#[test]
fn initial_command_and_session_name_conflict_is_rejected() {
    let err = Options::new()
        .with_initial_command(["new-session", "-A", "-s", "safe"])
        .with_session_name("safe")
        .validate()
        .unwrap_err();
    assert!(err.to_string().contains("mutually exclusive"));
}

#[test]
fn launch_args_attach_explicit_session() {
    let opts = Options::new().with_session_name("safe");
    assert_eq!(opts.launch_args(), vec!["-C", "attach-session", "-t", "safe"]);
}

#[test]
fn launch_args_create_explicit_session() {
    let opts = Options::new().with_session_name("safe").with_create_session(true);
    assert_eq!(opts.launch_args(), vec!["-C", "new-session", "-A", "-s", "safe"]);
}

#[test]
fn launch_args_socket_name_and_config() {
    let opts = Options::new()
        .with_socket_name("sock")
        .with_config_file("/dev/null")
        .with_session_name("safe");
    assert_eq!(
        opts.launch_args(),
        vec!["-L", "sock", "-f", "/dev/null", "-C", "attach-session", "-t", "safe"]
    );
}

#[test]
fn launch_args_socket_path_and_initial_command() {
    let opts = Options::new().with_socket_path("/tmp/tmux.sock").with_initial_command([
        "new-session",
        "-A",
        "-s",
        "safe",
    ]);
    assert_eq!(
        opts.launch_args(),
        vec!["-S", "/tmp/tmux.sock", "-C", "new-session", "-A", "-s", "safe"]
    );
}

#[test]
fn initial_command_line_renders_the_explicit_command() {
    let opts = Options::new().with_initial_command(["new-session", "-A", "-s", "test"]);
    assert_eq!(opts.initial_command_line(), "new-session -A -s test");
}

#[test]
fn initial_command_line_renders_the_default_attach() {
    let opts = Options::new().with_session_name("test");
    assert_eq!(opts.initial_command_line(), "attach-session -t test");
}

#[test]
fn initial_command_line_renders_the_default_create() {
    let opts = Options::new().with_session_name("test").with_create_session(true);
    assert_eq!(opts.initial_command_line(), "new-session -A -s test");
}
