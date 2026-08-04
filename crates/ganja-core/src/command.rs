//! Slash commands that expand into a prompt and run as an ordinary turn.
//!
//! Spec: upstream `packages/opencode/src/command/index.ts` for the builtin set
//! and `packages/opencode/src/session/prompt.ts` (`SessionPrompt.command`) for
//! the expansion. A command is a **template plus a name**: selecting it types
//! nothing into the model, it fills its placeholders from whatever the user
//! typed after the name and sends the result the way a typed message is sent.
//!
//! `/init` is the one builtin this build ships. Its template is upstream's
//! `command/template/initialize.txt` verbatim, and everything it does about
//! `AGENTS.md` — create it if it is absent, improve it in place if it is there
//! — is *prompt* semantics. There is no file handling here and none upstream:
//! the model reaches for `write` and `edit` like it would for any other file.
//!
//! # What is not ported
//!
//! Two of upstream's expansion steps (deviation **D5**, pre-declared):
//!
//! - ``!`cmd` `` — running a shell command and substituting its output into the
//!   template. Reachable only from a template a user wrote.
//! - `@file` inside a template. Mentions come from the composer, where the user
//!   can see what they are attaching.
//!
//! Neither is reachable from `/init`, which is what made them deferrable.

use std::{collections::BTreeMap, path::Path};

use crate::config::{CommandConfig, Config};

/// Name of the builtin that writes a repository's `AGENTS.md`.
pub const INIT: &str = "init";

/// What `/init` sends, ported verbatim from upstream
/// `packages/opencode/src/command/template/initialize.txt` (MIT; see
/// `THIRD_PARTY_NOTICES.md`).
const INIT_TEMPLATE: &str = include_str!("prompt/initialize.txt");

/// `/init`'s one-line description, upstream's own string
/// (`command/index.ts`).
const INIT_DESCRIPTION: &str = "guided AGENTS.md setup";

/// The placeholder `/init`'s template carries for the worktree it is being run
/// in. Upstream substitutes it with a plain string replace, which in JavaScript
/// replaces the **first** occurrence only; the file holds exactly one.
const PATH_PLACEHOLDER: &str = "${path}";

/// The placeholder that stands for everything the user typed, untokenized.
const ARGUMENTS: &str = "$ARGUMENTS";

/// One command a session can run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Definition {
    /// What the user types after the slash.
    pub name: String,
    /// One line for a palette to show, when the command has one.
    pub description: Option<String>,
    /// The prompt it sends, before its placeholders are filled.
    pub template: String,
    /// Agent the command runs as, when it should not run as the session's
    /// current one.
    pub agent: Option<String>,
    /// Model the command asks for, when it should not ask the session's.
    pub model: Option<String>,
}

impl Definition {
    /// What this command sends when it is run with `arguments`.
    #[must_use]
    pub fn expand(&self, arguments: &str) -> String {
        expand(&self.template, arguments)
    }
}

/// Every command a session can run, sorted by name.
///
/// Sorted rather than in definition order because this is what a palette lists
/// and what an unknown-name error names, and neither has a reason to prefer the
/// order a config file happened to spell.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    commands: Vec<Definition>,
}

impl Registry {
    /// The builtins plus whatever `config.command` describes, resolved for a
    /// session working in `worktree`.
    ///
    /// A config command that reuses a builtin's name replaces it: upstream's
    /// `mergeDeep` gives the user's own definition the last word.
    #[must_use]
    pub fn build(config: &Config, worktree: &Path) -> Self {
        let mut commands: BTreeMap<String, Definition> = BTreeMap::new();
        for command in builtins(worktree) {
            commands.insert(command.name.clone(), command);
        }
        for (name, definition) in &config.command {
            commands.insert(name.clone(), configured(name, definition));
        }

        Self {
            commands: commands.into_values().collect(),
        }
    }

    /// The builtins alone, for an engine nobody configured.
    #[must_use]
    pub fn builtin(worktree: &Path) -> Self {
        Self {
            commands: builtins(worktree),
        }
    }

    /// The command named `name`, or nothing.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Definition> {
        self.commands.iter().find(|command| command.name == name)
    }

    /// Every command, sorted by name — what a palette lists.
    #[must_use]
    pub fn commands(&self) -> &[Definition] {
        &self.commands
    }

    /// The names, sorted, for an error that has to say what *would* have
    /// worked.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.commands
            .iter()
            .map(|command| command.name.clone())
            .collect()
    }
}

/// The commands this build ships, with `${path}` already pointing at the
/// worktree the session is working in.
fn builtins(worktree: &Path) -> Vec<Definition> {
    vec![Definition {
        name: INIT.to_owned(),
        description: Some(INIT_DESCRIPTION.to_owned()),
        template: INIT_TEMPLATE.replacen(PATH_PLACEHOLDER, &worktree.to_string_lossy(), 1),
        agent: None,
        model: None,
    }]
}

/// One command as a config file described it.
fn configured(name: &str, definition: &CommandConfig) -> Definition {
    Definition {
        name: name.to_owned(),
        description: definition.description.clone(),
        template: definition.template.clone(),
        agent: definition.agent.clone(),
        model: definition.model.clone(),
    }
}

/// Fills `template`'s placeholders from `arguments`, upstream's four steps in
/// upstream's order (`session/prompt.ts`, `SessionPrompt.command`).
///
/// 1. `arguments` is tokenized, keeping quoted spans whole and stripping the
///    quotes that held them together.
/// 2. `$1`..`$N` take the token at that position, and the **highest-numbered
///    placeholder present is greedy**: it takes that token and every one after
///    it, joined by spaces. A placeholder past the last token becomes empty.
/// 3. `$ARGUMENTS` takes `arguments` whole, untokenized and unstripped.
/// 4. A template mentioning neither, run with arguments, gets them appended
///    after a blank line — which is what makes `/mycommand some question` work
///    for a template that never thought about arguments.
///
/// The result is trimmed, as upstream trims it.
fn expand(template: &str, arguments: &str) -> String {
    let tokens = tokenize(arguments);
    let positions = placeholders(template);
    let greedy = positions.iter().copied().max();

    let mut expanded = String::with_capacity(template.len() + arguments.len());
    let mut rest = template;
    while let Some(start) = rest.find('$') {
        expanded.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();

        if digits.is_empty() {
            expanded.push('$');
            rest = after;
            continue;
        }

        // A number too large to be a position cannot name a token either, so it
        // expands to nothing exactly as a position past the last token does.
        let index: usize = digits.parse().unwrap_or(usize::MAX);
        expanded.push_str(&fill(&tokens, index, greedy == Some(index)));
        rest = &after[digits.len()..];
    }
    expanded.push_str(rest);

    let mentions_arguments = expanded.contains(ARGUMENTS);
    let expanded = expanded.replace(ARGUMENTS, arguments);

    if positions.is_empty() && !mentions_arguments && !arguments.trim().is_empty() {
        return format!("{}\n\n{}", expanded.trim(), arguments.trim());
    }

    expanded.trim().to_owned()
}

/// What `$index` expands to: the token at that position, or — for the
/// highest-numbered placeholder the template carries — that token and
/// everything after it.
fn fill(tokens: &[String], index: usize, greedy: bool) -> String {
    let Some(first) = index.checked_sub(1) else {
        // `$0` names no argument; upstream's regex captures it and slices from
        // -1, which yields nothing either.
        return String::new();
    };
    if first >= tokens.len() {
        return String::new();
    }

    if greedy {
        tokens[first..].join(" ")
    } else {
        tokens[first].clone()
    }
}

/// Every `$N` position the template names, in the order they appear.
fn placeholders(template: &str) -> Vec<usize> {
    let mut found = Vec::new();
    let mut rest = template;

    while let Some(start) = rest.find('$') {
        let after = &rest[start + 1..];
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty()
            && let Ok(index) = digits.parse::<usize>()
        {
            found.push(index);
        }
        rest = &after[digits.len()..];
    }

    found
}

/// Splits an argument string the way upstream's `argsRegex` does: a quoted span
/// is one token, and everything else runs to the next whitespace or quote.
///
/// The surrounding quotes are then stripped, upstream's `quoteTrimRegex`, so
/// `"two words"` reaches `$1` as `two words`.
///
/// Upstream's regex also recognizes `[Image N]` as one token. This build has no
/// image parts to recognize, so that alternative is not ported and the text
/// splits on its spaces like any other.
fn tokenize(arguments: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut rest = arguments.trim_start();

    while !rest.is_empty() {
        let quote = rest.starts_with(['"', '\'']);
        let token = if quote {
            let opening = rest.chars().next().expect("a non-empty remainder");
            match rest[1..].find(opening) {
                // The closing quote is part of the span upstream matches, and
                // stripping both is what leaves the text between them.
                Some(end) => &rest[..end + 2],
                // An unterminated quote matches nothing in upstream's regex
                // either; what is left is one token running to the end.
                None => rest,
            }
        } else {
            let end = rest
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
                .unwrap_or(rest.len());
            &rest[..end]
        };

        tokens.push(unquote(token).to_owned());
        rest = rest[token.len()..].trim_start();
    }

    tokens
}

/// Strips one leading and one trailing quote, upstream's `quoteTrimRegex`.
fn unquote(token: &str) -> &str {
    let trimmed = token.strip_prefix(['"', '\'']).unwrap_or(token);

    trimmed.strip_suffix(['"', '\'']).unwrap_or(trimmed)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{INIT, INIT_TEMPLATE, PATH_PLACEHOLDER, Registry, expand, tokenize};
    use crate::config::{CommandConfig, Config};

    #[test]
    fn the_init_template_is_upstreams_verbatim_with_the_worktree_filled_in() {
        let registry = Registry::builtin(Path::new("/repo/ganja"));
        let init = registry.get(INIT).expect("init is builtin");

        assert!(
            INIT_TEMPLATE.contains(PATH_PLACEHOLDER),
            "the ported file should still carry the placeholder"
        );
        assert!(
            !init.template.contains(PATH_PLACEHOLDER),
            "and the resolved template should not: {}",
            init.template
        );
        assert!(init.template.contains("/repo/ganja"));
        assert!(
            init.template
                .starts_with("Create or update `AGENTS.md` for this repository."),
            "the template is upstream's, unedited: {}",
            init.template
        );
        assert_eq!(init.description.as_deref(), Some("guided AGENTS.md setup"));
    }

    #[test]
    fn a_template_fills_its_placeholders_the_way_upstream_fills_them() {
        let cases = [
            // (template, arguments, expected)
            ("fix $1", "auth", "fix auth"),
            // The highest-numbered placeholder is greedy: `$2` takes the rest.
            (
                "fix $1 because $2",
                "auth it broke again",
                "fix auth because it broke again",
            ),
            // …even when it is not the last one written.
            ("$2 — fix $1", "auth it broke", "it broke — fix auth"),
            // A position past the last token is empty rather than an error.
            ("fix $1 and $2", "auth", "fix auth and"),
            ("focus: $ARGUMENTS", "the tests", "focus: the tests"),
            // Raw and untokenized: quotes survive `$ARGUMENTS`.
            (
                r#"focus: $ARGUMENTS"#,
                r#""two words""#,
                r#"focus: "two words""#,
            ),
            // Neither placeholder, so the arguments are appended.
            (
                "review the diff",
                "only src/",
                "review the diff\n\nonly src/",
            ),
            // Neither placeholder and no arguments: nothing is appended.
            ("review the diff", "", "review the diff"),
            // A quoted span is one token.
            (
                r#"say $1 to $2"#,
                r#""good morning" world"#,
                "say good morning to world",
            ),
            // A `$` that names nothing is left alone.
            ("costs $5.00 and $x", "", "costs .00 and $x"),
            // Trimmed, as upstream trims.
            ("  spaced  ", "", "spaced"),
        ];

        for (template, arguments, expected) in cases {
            assert_eq!(
                expand(template, arguments),
                expected,
                "expanding {template:?} with {arguments:?}"
            );
        }
    }

    #[test]
    fn arguments_tokenize_with_quoted_spans_kept_whole() {
        let cases = [
            ("", Vec::new()),
            ("one two", vec!["one", "two"]),
            (r#""two words" three"#, vec!["two words", "three"]),
            (r#"'single quoted' rest"#, vec!["single quoted", "rest"]),
            // An unterminated quote is one token running to the end.
            (r#""unterminated rest"#, vec!["unterminated rest"]),
            ("  padded   out  ", vec!["padded", "out"]),
        ];

        for (arguments, expected) in cases {
            assert_eq!(tokenize(arguments), expected, "tokenizing {arguments:?}");
        }
    }

    #[test]
    fn a_config_command_joins_the_roster_and_may_replace_a_builtin() {
        let mut command = std::collections::BTreeMap::new();
        command.insert(
            "review".to_owned(),
            CommandConfig {
                template: "review $ARGUMENTS".to_owned(),
                description: Some("review the diff".to_owned()),
                agent: Some("plan".to_owned()),
                model: None,
            },
        );
        command.insert(
            INIT.to_owned(),
            CommandConfig {
                template: "mine instead".to_owned(),
                description: None,
                agent: None,
                model: None,
            },
        );
        let config = Config {
            command,
            ..Config::default()
        };
        let registry = Registry::build(&config, Path::new("/repo"));

        assert_eq!(
            registry.names(),
            vec!["init".to_owned(), "review".to_owned()]
        );
        assert_eq!(
            registry.get(INIT).expect("init is still there").template,
            "mine instead",
            "a config command replaces the builtin it names"
        );
        let review = registry.get("review").expect("the config command is there");
        assert_eq!(review.agent.as_deref(), Some("plan"));
        assert_eq!(review.expand("src/"), "review src/");
        assert!(registry.get("nope").is_none());
    }
}
