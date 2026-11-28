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

/// The top level's long flags that are not also the name of a subcommand:
/// `model` and `session` beside the subcommands `models` and `sessions`.
fn flags_naming_no_subcommand(subcommands: &[String]) -> Vec<String> {
    Cli::command()
        .get_arguments()
        .filter_map(clap::Arg::get_long)
        .filter(|flag| !subcommands.iter().any(|name| name == flag))
        .map(str::to_owned)
        .collect()
}

/// The part of `script` that follows `opens`, up to the first `closes` after
/// it.
fn between<'a>(script: &'a str, opens: &str, closes: &str) -> &'a str {
    script
        .split_once(opens)
        .and_then(|(_, rest)| rest.split_once(closes))
        .map(|(listing, _)| listing)
        .unwrap_or_else(|| panic!("nothing lies between {opens:?} and {closes:?} in:\n{script}"))
}

/// The candidate each line of `listing` carries: what follows `before`, up to
/// `after`. A line that does not start with `before` carries none.
fn candidates<'a>(listing: &'a str, before: &str, after: char) -> Vec<&'a str> {
    listing
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix(before)?.split_once(after))
        .map(|(candidate, _)| candidate)
        .collect()
}

/// What a shell's script offers for the word after `ganja`, a flag spelled
/// with its dashes.
///
/// Read from the one place each script lists the top level, because looking
/// for a name anywhere in the script proves less than it seems to. `config`
/// is a subcommand, the `--config` flag and a word in several help texts; and
/// every subcommand is listed a second time under `ganja help`, so a script
/// that no longer offered one after `ganja` would still contain its name.
fn offered_after_ganja(shell: Shell, script: &str) -> Vec<&str> {
    match shell {
        Shell::Bash => between(script, "        ganja)\n            opts=\"", "\"")
            .split_whitespace()
            .collect(),
        Shell::Zsh => candidates(between(script, "\n_ganja_commands() {\n", "\n}"), "'", ':'),
        Shell::Fish => {
            candidates(script, "complete -c ganja -n \"__fish_ganja_needs_command\" -f -a \"", '"')
        }
        Shell::Elvish => candidates(between(script, "&'ganja'= {\n", "\n        }"), "cand ", ' '),
        Shell::PowerShell => candidates(
            between(script, "'ganja' {\n", "\n            break\n"),
            "[CompletionResult]::new('",
            '\'',
        ),
        // `Shell` is `#[non_exhaustive]`: the crate may add a variant without
        // a major version, so a match outside it has to have this arm. A new
        // shell stops the test here until its listing is written down.
        other => panic!("where the {other} script lists the top level is not known"),
    }
}

#[test]
fn every_shell_gets_a_script_that_offers_each_visible_subcommand() {
    let subcommands = visible_subcommands();
    assert!(
        subcommands.iter().any(|name| name == "completions"),
        "the listing is read from the real command line: {subcommands:?}"
    );
    let flags = flags_naming_no_subcommand(&subcommands);
    assert!(
        flags.iter().any(|flag| flag == "model"),
        "the flags are read from the real command line: {flags:?}"
    );

    for shell in Shell::value_variants() {
        let script = String::from_utf8(script(*shell, Cli::command()))
            .unwrap_or_else(|error| panic!("the {shell} script is not UTF-8: {error}"));

        assert!(script.contains("ganja"), "the {shell} script names the binary:\n{script}");
        let offered = offered_after_ganja(*shell, &script);
        for name in &subcommands {
            assert!(
                offered.contains(&name.as_str()),
                "the {shell} script offers `{name}` after `ganja`, among {offered:?}:\n{script}"
            );
        }
        // The control: a reading that took a flag for a bare word would let
        // the loop above pass for a subcommand the script no longer offers.
        for flag in &flags {
            assert!(
                !offered.contains(&flag.as_str()),
                "`--{flag}` is no subcommand, and the {shell} reading has it bare: {offered:?}"
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
