use super::{command, program, read_back, seed};

#[test]
fn the_editor_is_whatever_the_environment_names_and_vi_when_it_names_nothing() {
    let cases = [
        (Some("nvim"), "nvim"),
        (Some("code -w"), "code -w"),
        (Some("  hx  "), "hx"),
        (Some(""), "vi"),
        (Some("   "), "vi"),
        (None, "vi"),
    ];

    for (configured, expected) in cases {
        assert_eq!(
            program(configured.map(str::to_owned)),
            expected,
            "{configured:?}"
        );
    }
}

/// What the whole feature rests on: the buffer goes out and comes back
/// unchanged when the editor changes nothing.
#[test]
fn a_seeded_buffer_reads_back_verbatim() {
    let directory = tempfile::tempdir().expect("a temporary directory is creatable");

    for text in [
        "one line",
        "first\nsecond\nthird",
        "trailing spaces   ",
        "",
        "unicode: 日本語 and an emoji",
    ] {
        let path = seed(directory.path(), text).expect("the seed writes");

        assert_eq!(read_back(&path).expect("the seed reads back"), text);
    }
}

/// Editors end a file with a newline. Carrying it back would leave the
/// composer's cursor on a line the user never typed.
#[test]
fn one_trailing_newline_is_the_editors_and_is_dropped() {
    let directory = tempfile::tempdir().expect("a temporary directory is creatable");
    let cases = [
        ("what the editor wrote\n", "what the editor wrote"),
        (
            "a blank line the user left\n\n",
            "a blank line the user left\n",
        ),
        ("no newline at all", "no newline at all"),
    ];

    for (written, expected) in cases {
        let path = seed(directory.path(), written).expect("the seed writes");

        assert_eq!(
            read_back(&path).expect("it reads back"),
            expected,
            "{written:?}"
        );
    }
}

/// The path is an argument, not part of the command string, so a directory
/// nobody would name by hand cannot become part of what runs.
///
/// The program is asserted against the resolved shell rather than against
/// the literal `sh`. A literal passed on Windows, where nothing spawns a
/// bare `sh`: the old assertion held while `/editor` could not launch an
/// editor at all.
#[test]
fn the_path_reaches_the_editor_as_an_argument_rather_than_as_text() {
    let path = std::path::Path::new("/tmp/a dir; rm -rf ~/ganja-prompt.md");
    let shell = ganja_tool::shell::posix_shell().expect("a machine with a POSIX shell");
    let command = command(&shell, "code -w", path);

    let arguments: Vec<&std::ffi::OsStr> = command.get_args().collect();

    assert_eq!(command.get_program(), shell.as_os_str());
    assert_eq!(arguments[0], "-c");
    assert_eq!(arguments[1], "code -w \"$@\"");
    assert_eq!(arguments[3], path.as_os_str());

    // On Windows a program name only means something if it resolves, and
    // the whole point of the probe is that a bare name does not.
    #[cfg(windows)]
    assert!(
        shell.is_file(),
        "the editor's shell has to be a binary that is there: {}",
        shell.display()
    );

    // And the shell that was resolved is the shell that runs, which on the
    // one platform where the two could differ is the whole fix. Asserted
    // with a shell the probe would never answer, because on unix its answer
    // is `sh` and a hardcoded `sh` would pass without meaning anything.
    let elsewhere = std::path::Path::new("/opt/somewhere/dash");

    assert_eq!(
        super::command(elsewhere, "code -w", path).get_program(),
        elsewhere.as_os_str(),
        "the editor runs under the shell it was handed"
    );
}
