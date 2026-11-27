use clap::{CommandFactory as _, Parser as _, ValueEnum as _};
use clap_complete::Shell;

use super::script;
use crate::{Cli, Command};

/// The subcommands a person can see, read from the command line itself
/// rather than listed here: a subcommand added later is then covered by the
/// test below on the day it is added.
fn visible_subcommands() -> Vec<String> {
    Cli::command()
        .get_subcommands()
        .filter(|subcommand| !subcommand.is_hide_set())
        .map(|subcommand| subcommand.get_name().to_owned())
        .collect()
}

#[test]
fn every_shell_gets_a_script_that_offers_each_visible_subcommand() {
    let subcommands = visible_subcommands();
    assert!(
        subcommands.iter().any(|name| name == "completions"),
        "the listing is read from the real command line: {subcommands:?}"
    );

    for shell in Shell::value_variants() {
        let script = String::from_utf8(script(*shell, Cli::command()))
            .unwrap_or_else(|error| panic!("the {shell} script is not UTF-8: {error}"));

        assert!(script.contains("ganja"), "the {shell} script names the binary:\n{script}");
        for name in &subcommands {
            assert!(
                script.contains(name.as_str()),
                "the {shell} script offers `{name}`:\n{script}"
            );
        }
    }
}

/// A nested subcommand and a flag of its own are the two things a script
/// generated from only the top level would be missing.
#[test]
fn the_script_reaches_below_the_top_level() {
    for shell in [Shell::Bash, Shell::Zsh] {
        let script = String::from_utf8(script(shell, Cli::command()))
            .unwrap_or_else(|error| panic!("the {shell} script is not UTF-8: {error}"));

        for expected in ["import-opencode", "--refresh", "--dry-run"] {
            assert!(script.contains(expected), "the {shell} script offers `{expected}`:\n{script}");
        }
    }
}

#[test]
fn the_shell_is_named_and_must_be_one_a_script_exists_for() {
    for shell in ["bash", "zsh"] {
        let cli = Cli::try_parse_from(["ganja", "completions", shell])
            .unwrap_or_else(|error| panic!("`completions {shell}` parses: {error}"));
        assert!(
            matches!(cli.command, Some(Command::Completions(_))),
            "`completions {shell}` is the completions subcommand: {cli:?}"
        );
    }

    let missing = Cli::try_parse_from(["ganja", "completions"])
        .expect_err("a script is for one shell, so none named is a usage error");
    assert_eq!(missing.kind(), clap::error::ErrorKind::MissingRequiredArgument, "{missing}");

    let unknown = Cli::try_parse_from(["ganja", "completions", "tcsh"])
        .expect_err("a shell with no generator is refused at parse");
    assert_eq!(unknown.kind(), clap::error::ErrorKind::InvalidValue, "{unknown}");
    let unknown = unknown.to_string();
    for offered in ["bash", "zsh"] {
        assert!(unknown.contains(offered), "the refusal names `{offered}`:\n{unknown}");
    }
}
