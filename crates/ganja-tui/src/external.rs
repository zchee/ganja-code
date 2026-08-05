//! Composing the prompt in the user's own editor.
//!
//! Spec: upstream `packages/tui/src/component/prompt/index.tsx:422-512`, which
//! seeds a temporary file with the buffer, hands the terminal to `$EDITOR`,
//! and puts back whatever came out with the cursor at the end.
//!
//! The module is split so that the part worth testing can be. [`program`],
//! [`seed`], [`read_back`] and [`command`] are pure enough to assert on;
//! [`edit`] is the one function that takes the terminal away from ratatui and
//! gives it back, and it is exercised by hand rather than by a test — a test
//! that spawned a real editor would be testing the machine it ran on. What a
//! test *can* pin is that the seed a session hands over comes back verbatim,
//! and that is what the round-trip test below does.

use std::{
    io::{self, Write as _},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context as _, Result};
use ratatui::crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};

/// What is run when the environment names no editor. Upstream falls back the
/// same way, and `vi` is the one editor POSIX requires to be there.
const FALLBACK: &str = "vi";

/// Name of the file the buffer is handed over in. The extension is what makes
/// an editor open it in whatever mode it uses for prose rather than in none.
const SEED_NAME: &str = "ganja-prompt.md";

/// The editor `configured` names, or the fallback when it names nothing.
///
/// Takes the value rather than reading the environment so that the choice can
/// be asserted on without a test mutating the process it runs in.
#[must_use]
pub fn program(configured: Option<String>) -> String {
    configured
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| FALLBACK.to_owned())
}

/// The editor this run would launch, from the environment.
#[must_use]
pub fn configured_program() -> String {
    program(std::env::var("EDITOR").ok())
}

/// Writes `text` into a file under `directory` and answers with its path.
///
/// # Errors
///
/// Returns an error if the file cannot be created or written.
pub fn seed(directory: &Path, text: &str) -> io::Result<PathBuf> {
    let path = directory.join(SEED_NAME);
    let mut file = std::fs::File::create(&path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;

    Ok(path)
}

/// Reads back what the editor left in `path`.
///
/// One trailing newline is dropped: every editor worth the name ends a file
/// with one, and carrying it into the composer would leave the cursor on an
/// empty line the user never typed. A blank line the user *did* leave — two
/// newlines — survives, because only the last one is the editor's.
///
/// # Errors
///
/// Returns an error if the file cannot be read.
pub fn read_back(path: &Path) -> io::Result<String> {
    let text = std::fs::read_to_string(path)?;

    Ok(text.strip_suffix('\n').map_or(text.clone(), str::to_owned))
}

/// The child process that opens `path` in `program`, run under `shell`.
///
/// Run through a shell rather than split here, because `$EDITOR` is a *command
/// line* and carries flags often enough to matter — `code -w`,
/// `emacsclient -nw`. The path arrives as an argument rather than inside the
/// string, so a directory with a space or a quote in its name cannot become
/// part of the command.
///
/// The shell is passed in rather than named here so that this stays a pure
/// function, and so that the one machine where naming it would be wrong — a
/// Windows box, where a bare `sh` reaches nothing and PowerShell would read
/// `"$@"` as something else entirely — gets the same answer the `bash` tool
/// resolved. See [`ganja_tool::shell::posix_shell`].
#[must_use]
pub fn command(shell: &Path, program: &str, path: &Path) -> Command {
    let mut command = Command::new(shell);
    command
        .arg("-c")
        .arg(format!("{program} \"$@\""))
        .arg(program)
        .arg(path);

    command
}

/// Hands the terminal to the user's editor, seeded with `text`, and answers
/// with what it left behind.
///
/// The terminal is given back on every path out, including the ones that fail:
/// an editor that could not be launched must not leave the shell in raw mode
/// with the alternate screen still up.
///
/// # Errors
///
/// Returns an error if the temporary file cannot be written or read, or if the
/// editor cannot be launched.
pub fn edit(text: &str) -> Result<String> {
    let directory = tempfile::tempdir().context("failed to make room for the prompt")?;
    let path = seed(directory.path(), text).context("failed to write the prompt out")?;

    // Resolved before the terminal is handed over, so a machine with no shell
    // to run the editor under says so on an intact screen rather than after
    // leaving and re-entering the alternate one for nothing.
    let shell = ganja_tool::shell::posix_shell()
        .context("failed to find a shell to run the editor under")?;

    let released = release();
    let status = command(&shell, &configured_program(), &path)
        .status()
        .context("failed to run the editor");
    let taken = take();

    // The terminal comes first: a refusal the user cannot read is worse than
    // the refusal itself.
    released?;
    taken?;
    let status = status?;
    if !status.success() {
        anyhow::bail!("the editor exited with {status}");
    }

    read_back(&path).context("failed to read the prompt back")
}

/// Gives the terminal back to the shell, so a child process can use it.
fn release() -> Result<()> {
    disable_raw_mode().context("failed to leave raw mode")?;
    execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)
        .context("failed to leave the alternate screen")
}

/// Takes the terminal back, once the child is done with it.
fn take() -> Result<()> {
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)
        .context("failed to re-enter the alternate screen")?;
    enable_raw_mode().context("failed to re-enter raw mode")
}

#[cfg(test)]
mod tests {
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
}
