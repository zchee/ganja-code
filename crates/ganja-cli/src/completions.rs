//! `ganja completions <shell>` — the script a shell loads to complete this
//! binary's own command line.
//!
//! The script is generated, when it is asked for, from the [`clap::Command`]
//! the binary parses with, so it cannot offer a subcommand or a flag this
//! build does not have, and a flag added tomorrow is in tomorrow's script
//! without anyone remembering to add it.
//!
//! Two limits follow from generating it that way, and both are accepted:
//!
//! - **A flag hidden from `--help` is still offered on Tab.** Hiding is a
//!   property of the rendered help; the argument is still in the `Command`,
//!   and the generators write every argument they find there. Clap has no
//!   way to take an argument back out of a built `Command`, so keeping them
//!   out would mean filtering the generated text per shell.
//! - **Only what the `Command` states is completed**: subcommands, flags,
//!   the values of an enumerated flag, and paths. A provider id, a model, a
//!   session id or an MCP server name is read from config or storage at run
//!   time, and a static script has nothing to ask.

use std::io::{self, Write as _};

use anyhow::{Context as _, Result};
use clap::Args;
use clap_complete::Shell;

/// `ganja completions`: the shell the script is written for.
#[derive(Debug, Args)]
pub(crate) struct CompletionsArgs {
    /// The shell to write the script for.
    shell: Shell,
}

/// Writes the completion script for the named shell to standard output.
///
/// `command` is the binary's own command line, handed in rather than built
/// here so that this module completes whatever `main` parses and has no
/// second opinion about what that is.
///
/// # Errors
///
/// Fails when standard output cannot be written — except by a reader that
/// closed the pipe, which is `ganja completions zsh | head -1` having read
/// what it wanted, and is not a failure to report.
pub(crate) fn completions(args: &CompletionsArgs, command: clap::Command) -> Result<()> {
    let script = script(args.shell, command);
    let mut stdout = io::stdout().lock();

    match stdout.write_all(&script).and_then(|()| stdout.flush()) {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        written => written.context("failed to write the completion script"),
    }
}

/// The script as bytes — separated from the printing so a test reads exactly
/// what a shell would.
fn script(shell: Shell, mut command: clap::Command) -> Vec<u8> {
    // The name the script registers itself under has to be the name a person
    // types, and the command already knows it.
    let name = command.get_name().to_owned();
    // Into memory rather than straight to standard output: the generators
    // answer a failed write by panicking, and a closed pipe is an ordinary
    // thing to happen to this output.
    let mut script = Vec::new();
    clap_complete::generate(shell, &mut command, name, &mut script);

    script
}

#[cfg(test)]
#[path = "completions_tests.rs"]
mod tests;
