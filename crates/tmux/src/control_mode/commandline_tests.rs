use super::*;

// Go's TestCommandLineInvalidUTF8 feeds an invalid-UTF-8 byte sequence
// (`string([]byte{0xff})`) as both the command token and an argument
// value. A Rust `&str`/`String` is a valid UTF-8 sequence by
// construction, so that input is unrepresentable here — the type
// system rejects it before any test could run. Waived (AC-4).

fn display_message() -> Command {
    Command::from_static("display-message")
}

fn refresh_client() -> Command {
    Command::from_static("refresh-client")
}

#[test]
fn bare_and_quoted_arguments_render_together() {
    let line = CommandLine::new(
        display_message(),
        [Arg::raw("-p"), Arg::string("#{session_name}")],
    );
    assert_eq!(
        line.render().unwrap(),
        "display-message -p '#{session_name}'"
    );
}

#[test]
fn spaces_are_quoted() {
    let line = CommandLine::new(display_message(), [Arg::string("hello world")]);
    assert_eq!(line.render().unwrap(), "display-message 'hello world'");
}

#[test]
fn semicolon_stays_argument_content() {
    let line = CommandLine::new(display_message(), [Arg::string("a;b")]);
    assert_eq!(line.render().unwrap(), "display-message 'a;b'");
}

#[test]
fn double_quotes_stay_inside_single_quotes() {
    let line = CommandLine::new(display_message(), [Arg::string(r#"say "hi""#)]);
    assert_eq!(line.render().unwrap(), r#"display-message 'say "hi"'"#);
}

#[test]
fn single_quote_switches_to_double_quotes() {
    let line = CommandLine::new(display_message(), [Arg::string("can't")]);
    assert_eq!(line.render().unwrap(), r#"display-message "can't""#);
}

#[test]
fn backslash_is_escaped_in_double_quotes() {
    let line = CommandLine::new(display_message(), [Arg::string(r"can't\stop")]);
    assert_eq!(line.render().unwrap(), r#"display-message "can't\\stop""#);
}

#[test]
fn dollar_is_escaped_in_double_quotes() {
    let line = CommandLine::new(display_message(), [Arg::string("can't $expand ${HOME}")]);
    assert_eq!(
        line.render().unwrap(),
        r#"display-message "can't \$expand \${HOME}""#
    );
}

#[test]
fn unicode_is_quoted_when_needed() {
    let line = CommandLine::new(display_message(), [Arg::string("hello 😀")]);
    assert_eq!(line.render().unwrap(), "display-message 'hello 😀'");
}

#[test]
fn raw_argument_is_passed_through() {
    let line = CommandLine::new(
        refresh_client(),
        [Arg::raw("-f"), Arg::raw("pause-after=30")],
    );
    assert_eq!(line.render().unwrap(), "refresh-client -f pause-after=30");
}

#[test]
fn empty_command_is_rejected() {
    let line = CommandLine::new(Command::from_static(""), []);
    let err = line.render().unwrap_err();
    assert!(err.to_string().contains("must not be empty"));
}

#[test]
fn command_token_with_space_is_rejected() {
    let line = CommandLine::new(Command::from_static("display message"), []);
    let err = line.render().unwrap_err();
    assert!(err.to_string().contains("plain token"));
}

#[test]
fn argument_newline_is_rejected() {
    let line = CommandLine::new(display_message(), [Arg::string("bad\n")]);
    let err = line.render().unwrap_err();
    assert!(err.to_string().contains("contains a newline"));
}

#[test]
fn empty_raw_argument_is_rejected() {
    let line = CommandLine::new(display_message(), [Arg::raw(" ")]);
    let err = line.render().unwrap_err();
    assert!(err.to_string().contains("raw argument must not be empty"));
}

#[test]
fn command_sequence_syntax_without_newline_is_valid() {
    assert!(validate_raw_line("display-message -p ok ; list-panes").is_ok());
}

#[test]
fn blank_raw_line_is_rejected() {
    let err = validate_raw_line("  ").unwrap_err();
    assert!(err.to_string().contains("must not be empty"));
}

#[test]
fn newline_in_raw_line_is_rejected() {
    let err = validate_raw_line("display-message\nlist-panes").unwrap_err();
    assert!(err.to_string().contains("contains a newline"));
}
