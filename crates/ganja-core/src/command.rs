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
//! Expansion keeps upstream's order: fill the argument placeholders, run each
//! ``!`command` `` the filled template names, trim the result, then resolve the
//! `@file` tokens that survived as file parts beside the prompt. That order is
//! load-bearing: arguments may themselves name a command to run or a file to
//! attach, and both mean exactly what they would in text the person composed
//! without a slash command.
//!
//! Beside the builtins and the `command` table a config declares, a command may
//! also arrive as a **Markdown file** in one of ganja's own two homes — see
//! `command_dirs`, where that tier and its ledger number are declared.

use std::{
    collections::BTreeMap,
    fs, io,
    ops::Range,
    path::{Path, PathBuf},
};

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

/// A command template after every expansion step upstream applies.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Expanded {
    /// The final, trimmed text sent as the user's prompt.
    pub prompt: String,
    /// Existing files the final text mentions, in mention order.
    pub mentions: Vec<crate::protocol::Mention>,
}

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
    /// The Markdown file this command was read from, when it came from one.
    ///
    /// [`None`] for a builtin and for a `command` table entry — neither is a
    /// file somebody is looking at. Carried so that a refusal made after the
    /// roster is built can name the file that has to be edited, which is the
    /// only thing that distinguishes "your `plan` is misspelled" from a
    /// command that quietly is not there
    /// ([`Registry::refusing_unknown_agents`]).
    pub source: Option<PathBuf>,
}

impl Definition {
    /// What this command sends when it is run with `arguments`.
    ///
    /// `ctx.cwd` is the project root: it is both where shell substitutions run
    /// and where mentions resolve. One context keeps a template from running a
    /// command in one place while naming files in another.
    pub async fn expand(&self, arguments: &str, ctx: &crate::tool::ToolCtx) -> Expanded {
        let filled = fill_template(&self.template, arguments);
        // The scan follows filling, so a command that arrived through
        // `$ARGUMENTS` runs too. The person who typed those arguments is the
        // person the shell answers to, which is upstream's own ordering and
        // the same trust statement as a command written into the template.
        let substitutions = shell_substitutions(&filled);
        let shell = crate::tool::shell::ShellTool::new();
        let mut prompt = String::with_capacity(filled.len());
        let mut copied = 0;

        // Upstream fires these concurrently and substitutes their results in
        // match order. One at a time produces the same text and gives commands
        // that touch the same files the order their author wrote down.
        for substitution in substitutions {
            let ShellSubstitution { span, command } = substitution;
            prompt.push_str(&filled[copied..span.start]);

            // This is deviation-free: no permission question belongs here. A
            // template declared by the user's own config is the user typing,
            // exactly like the ungated `!` passthrough (D13), and upstream
            // gates neither surface.
            let result = shell
                .run_reporting(serde_json::json!({ "command": command }), ctx, None)
                .await;
            let output = match result {
                Ok(output) => {
                    // A non-zero exit still lands here with whatever it wrote,
                    // matching upstream's `nothrow: true`. Unlike upstream's
                    // stdout-only answer, the shared shell path interleaves
                    // stderr too: the transcript and a template should not give
                    // two shapes to the same command
                    // (deviation: template-shell-merges-stderr).
                    output.output
                }
                Err(error) => {
                    // Upstream throws when the command could not run at all.
                    // This engine makes tool failures information rather than
                    // control flow, so a prompt saying why is more useful than
                    // a turn that never starts
                    // (deviation: template-shell-reports-its-own-failure).
                    error.to_string()
                }
            };
            prompt.push_str(&output);
            copied = span.end;
        }
        prompt.push_str(&filled[copied..]);

        // Upstream trims after the shell has answered, not before it.
        let prompt = prompt.trim().to_owned();
        // `SessionPrompt.command` then gives that final, trimmed template to
        // `resolvePromptParts` (`packages/opencode/src/session/prompt.ts:1432`;
        // `packages/opencode/src/session/prompt.ts:157-190`), which pushes a
        // file part only for a name that stats.
        let mentions = mentions(&prompt)
            .into_iter()
            // Upstream's stat rule is the TUI's attachable filter too
            // (D113/R15(a)): `@alice` stays the person named in the sentence
            // rather than becoming an attachment-error block.
            .filter(|mention| ctx.cwd.join(&mention.path).is_file())
            .collect();

        Expanded { prompt, mentions }
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
    /// The builtins, the Markdown command files in ganja's own two homes, and
    /// whatever `config.command` describes, resolved for a session working in
    /// `worktree`.
    ///
    /// Four tiers, layered so the later one wins the name:
    ///
    /// ```text
    /// builtin  <  <config home>/commands  <  <worktree>/.ganja/commands  <  config `command`
    /// ```
    ///
    /// A config command that reuses a builtin's name replaces it: upstream's
    /// `mergeDeep` gives the user's own definition the last word, and a file
    /// tier changes nothing about that. A *file* that reuses a builtin's name
    /// is refused instead of layered — see `file_commands` — so `/init` is
    /// one thing everywhere until somebody says otherwise in the config file
    /// that this build refuses unknown keys in.
    ///
    /// Every collision involving a file is logged by name, because a command
    /// that quietly is not the file you are looking at is worse than one that
    /// says which of the two the session took.
    #[must_use]
    pub fn build(config: &Config, worktree: &Path) -> Self {
        let mut commands: BTreeMap<String, (Tier, Definition)> = BTreeMap::new();
        for command in builtins(worktree) {
            commands.insert(command.name.clone(), (Tier::Builtin, command));
        }

        for (tier, dir) in command_dirs(worktree) {
            for command in file_commands(&dir) {
                if matches!(commands.get(&command.name), Some((Tier::Builtin, _))) {
                    tracing::warn!(
                        command = %command.name,
                        directory = %dir.display(),
                        "a command file names a builtin command and was skipped"
                    );
                    continue;
                }
                insert(&mut commands, tier, command);
            }
        }

        for (name, definition) in &config.command {
            insert(&mut commands, Tier::Config, configured(name, definition));
        }

        Self {
            commands: commands
                .into_values()
                .map(|(_, definition)| definition)
                .collect(),
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

    /// Drops every command **file** whose `agent:` names nobody `agents`
    /// holds, saying which file and which agent.
    ///
    /// The dispatch-time check ([`crate::engine::EngineError::UnknownAgent`])
    /// is what a `command` table entry gets, and it stays: a config file is a
    /// curated key set whose author is told by name that a value is wrong, and
    /// silently dropping an entry out of one would be the opposite of how
    /// every other key in it behaves. A command *file* is the other posture,
    /// the one `read_command` applies to every other way a file can be
    /// wrong: absent from the roster, named in the log, nothing half-parsed
    /// reaching a session. `agent:` naming an agent that does not exist is
    /// exactly that kind of wrong — the file is unusable, and the person who
    /// can fix it is the person who wrote it.
    ///
    /// Called by [`crate::engine::Engine::with_commands`] rather than by
    /// [`Registry::build`], because the roster of agents is not something this
    /// module can resolve: `agent::Registry` is built from the config *and* a
    /// file tier of its own, and reconstructing it here would refuse command
    /// files naming a perfectly real file-declared agent. The engine is the
    /// first place both rosters exist, and it is still well before a turn.
    #[must_use]
    pub fn refusing_unknown_agents(mut self, agents: &crate::agent::Registry) -> Self {
        self.commands.retain(|command| {
            let Some(file) = &command.source else {
                return true;
            };
            let Some(agent) = &command.agent else {
                return true;
            };
            if agents.get(agent).is_some() {
                return true;
            }
            tracing::warn!(
                command = %command.name,
                file = %file.display(),
                agent = %agent,
                "a command file names an agent this session does not have and was refused"
            );

            false
        });

        self
    }
}

/// Where a command in the roster came from, in precedence order.
///
/// Kept beside each definition only so a collision can say which two things
/// collided. Nothing downstream reads it: a command is a command once it is in
/// the registry, whichever tier wrote it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    /// Shipped with this build.
    Builtin,
    /// A Markdown file under `<config home>/commands`.
    GlobalFile,
    /// A Markdown file under `<worktree>/.ganja/commands`.
    ProjectFile,
    /// An entry in a config file's `command` table.
    Config,
}

impl Tier {
    /// What a log line calls this tier.
    const fn label(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::GlobalFile => "the global commands directory",
            Self::ProjectFile => "the project commands directory",
            Self::Config => "the config file",
        }
    }
}

/// Lays `command` over whatever already holds its name, reporting the
/// collision when a command *file* is on either side of it.
///
/// A config entry replacing a builtin is the one shadowing this build has
/// always done deliberately and documented as such, so it stays silent; every
/// other pair involves a file somebody is looking at right now.
fn insert(commands: &mut BTreeMap<String, (Tier, Definition)>, tier: Tier, command: Definition) {
    if let Some((shadowed, _)) = commands.get(&command.name)
        && (*shadowed != Tier::Builtin || tier != Tier::Config)
    {
        tracing::warn!(
            command = %command.name,
            shadowed = shadowed.label(),
            winner = tier.label(),
            "two command definitions claim one name; the later tier wins"
        );
    }
    commands.insert(command.name.clone(), (tier, command));
}

/// The directory, under each of ganja's own two homes, that command files live
/// in.
const COMMANDS_SUBDIR: &str = "commands";

/// The largest command file this build will read.
///
/// A command file is a prompt somebody typed, so a quarter of a megabyte is
/// already far past generous; the cap exists so that a directory somebody
/// pointed at a video does not turn into a read of it. A file over the cap is
/// skipped by name, never truncated — half a template is a template that means
/// something else.
const MAX_COMMAND_FILE_BYTES: u64 = 256 * 1024;

/// The line that opens and closes a frontmatter block.
const FENCE: &str = "---";

/// Ganja's own two homes, in precedence order, as places a session reads
/// Markdown command files from without being told to (**D481**).
///
/// Claude Code's `.claude/commands/*.md` shape, over ganja's own pair of homes:
/// `<config home>/commands` and `<worktree>/.ganja/commands`. Upstream opencode
/// has no counterpart at all — its commands are config entries — so this is a
/// synthesis rather than a port, and the *shape* being Claude's is the whole
/// point: somebody moving a repository between the two tools moves a directory.
///
/// The global half is spelled through [`crate::config::config_home`] rather
/// than against XDG directly, which is what keeps `GANJA_CONFIG_HOME` (or a
/// `~/.ganja`) moving this build's config, its `AGENTS.md`, its skills and now
/// its commands **together**. The project half is `worktree`'s own `.ganja`,
/// the namespaced directory [`crate::config::PROJECT_DIRECTORY`] names.
///
/// The standing "nothing foreign is discovered" ruling holds: `.claude/commands`
/// is not read, here or walked up from anywhere. A file arrives in one of these
/// two because somebody put it there for *this* tool, and that placing is the
/// opt-in.
///
/// [`crate::config::home_dirs`] is the walk itself, shared with the skills and
/// agents rosters, including the case where the two homes turn out to be one
/// directory: reading it twice would find every command twice and report each
/// as shadowing itself. The tier is decided here rather than there, because a
/// tier is this module's vocabulary — and it is decided by *which* directory
/// each one is rather than by its position, since a machine with no config
/// home at all yields a single directory that is the project's.
///
/// # Not recursive, on purpose for now
///
/// Only the flat `*.md` in each directory is read. Claude namespaces a
/// subdirectory into the command name (`git/commit.md` → `/git:commit`); that
/// is a recorded follow-up rather than a decision against it, and leaving it
/// out costs nothing a later build cannot add compatibly.
fn command_dirs(worktree: &Path) -> Vec<(Tier, PathBuf)> {
    let global = crate::config::config_home().map(|home| home.join(COMMANDS_SUBDIR));

    crate::config::home_dirs(worktree, COMMANDS_SUBDIR)
        .into_iter()
        .map(|dir| {
            let tier = if Some(&dir) == global.as_ref() {
                Tier::GlobalFile
            } else {
                Tier::ProjectFile
            };

            (tier, dir)
        })
        .collect()
}

/// Every command file in one directory, by file name, skipping — with a named
/// warning — each file this build will not read.
///
/// A directory that is not there is the common case and says nothing; any other
/// failure to read it is reported, because somebody who made a `commands/` is
/// owed a reason it produced nothing.
///
/// `pub(crate)` for one other caller: an installed plugin's own `commands/`
/// directory is read by [`crate::plugin`] through exactly this function, so a
/// plugin's command file and a project's are the same file format read by the
/// same parser rather than two command systems that drift.
pub(crate) fn file_commands(dir: &Path) -> Vec<Definition> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::warn!(
                    directory = %dir.display(),
                    %error,
                    "a commands directory could not be read; no command files from it"
                );
            }
            return Vec::new();
        }
    };

    // Sorted so that the order warnings arrive in — and, on a filesystem where
    // two names differ only by case, which of the pair a session ends up with
    // — is the same on every machine rather than the directory's own order.
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| match entry {
            Ok(entry) => Some(entry.path()),
            Err(error) => {
                tracing::warn!(
                    directory = %dir.display(),
                    %error,
                    "a command directory entry could not be read and was skipped"
                );
                None
            }
        })
        .filter(|path| {
            // A subdirectory is not a command yet (see `command_dirs`), and a
            // file that is not Markdown was not meant for this directory.
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        })
        .collect();
    paths.sort();

    paths.iter().filter_map(|path| read_command(path)).collect()
}

/// One command file, or nothing and a warning saying which file and why.
///
/// Every refusal here is the same shape on purpose: the file is absent from
/// the roster, named in the log, and nothing half-parsed reaches a session. A
/// command that silently became something other than what the file says is the
/// outcome this whole function exists to prevent.
fn read_command(path: &Path) -> Option<Definition> {
    let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
        tracing::warn!(
            file = %path.display(),
            "a command file's name is not valid UTF-8 and was skipped"
        );
        return None;
    };
    // `read_dir` cannot hand back a separator in a file name, but a name is a
    // name a person types after a slash: checking here is what makes that true
    // rather than assumed.
    if name.is_empty() || name.contains(['/', '\\']) {
        tracing::warn!(
            file = %path.display(),
            "a command file's name is not a command name and was skipped"
        );
        return None;
    }

    match fs::metadata(path) {
        Ok(metadata) if metadata.len() > MAX_COMMAND_FILE_BYTES => {
            tracing::warn!(
                file = %path.display(),
                bytes = metadata.len(),
                limit = MAX_COMMAND_FILE_BYTES,
                "a command file is too large to be a prompt and was skipped"
            );
            return None;
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(
                file = %path.display(),
                %error,
                "a command file could not be measured and was skipped"
            );
            return None;
        }
    }

    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(
                file = %path.display(),
                %error,
                "a command file could not be read and was skipped"
            );
            return None;
        }
    };
    let Ok(text) = String::from_utf8(bytes) else {
        tracing::warn!(
            file = %path.display(),
            "a command file is not UTF-8 text and was skipped"
        );
        return None;
    };

    let definition = parse_command(name, &text).or_else(|| {
        tracing::warn!(
            file = %path.display(),
            "a command file opens a frontmatter block it never closes and was skipped"
        );
        None
    })?;

    Some(Definition {
        source: Some(path.to_owned()),
        ..definition
    })
}

/// The command `text` describes under `name`, or nothing when its frontmatter
/// block is never closed.
///
/// The body is the template **verbatim**, which is what makes every expansion
/// [`Definition::expand`] already does — `$ARGUMENTS`, `$1`..`$N`,
/// ``!`command` ``, `@path` — mean in a file exactly what it means in a config
/// entry. There is no second expansion path and no file-only vocabulary.
fn parse_command(name: &str, text: &str) -> Option<Definition> {
    // A file written by an editor that stamps a byte-order mark still opens
    // with `---` as far as its author is concerned.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (front, body) = split_frontmatter(text)?;
    let Frontmatter {
        description,
        agent,
        model,
        argument_hint,
    } = front;

    Some(Definition {
        name: name.to_owned(),
        description: palette_line(description, argument_hint),
        template: body.to_owned(),
        agent,
        model,
        // The caller knows which file this text came from; a parser given a
        // string does not.
        source: None,
    })
}

/// What a command file's frontmatter may say.
///
/// Claude's own four keys, minus `allowed-tools`: per-command tool scoping has
/// no seam in this build — an agent's rules are the tool-enable mechanism — and
/// accepting the key would promise something nothing enforces.
#[derive(Debug, Default, PartialEq, Eq)]
struct Frontmatter {
    /// One line for a palette.
    description: Option<String>,
    /// The agent this command runs as.
    agent: Option<String>,
    /// The model it asks for.
    model: Option<String>,
    /// A display-only hint about what to type after the name.
    argument_hint: Option<String>,
}

/// The one line a palette shows for a command file.
///
/// [`Definition`] carries no slot of its own for `argument-hint`, and adding
/// one would put the hint in a struct no frontend renders — every surface that
/// lists commands shows `description` and nothing else. Folding the hint into
/// that line is therefore what actually reaches the person the hint is for:
/// `review the diff — <path>`, or the hint alone when the file gave no
/// description, which still beats a nameless row.
fn palette_line(description: Option<String>, hint: Option<String>) -> Option<String> {
    match (description, hint) {
        (Some(description), Some(hint)) => Some(format!("{description} — {hint}")),
        (description, None) => description,
        (None, hint) => hint,
    }
}

/// Splits an optional leading frontmatter block off `text`, or reports that the
/// block is never closed.
///
/// Hand-rolled rather than reached through a YAML dependency: four string keys
/// do not justify a parser for a language with nine ways to write a string, and
/// the workspace has no YAML crate to borrow. The grammar is exactly what a
/// Claude command file uses — a `---` line, `key: value` lines, a closing `---`
/// line — and everything outside it is *tolerated*, not interpreted: unknown
/// keys, comments and blank lines are skipped, because this is somebody else's
/// file shape and a build that refused what it did not recognise would refuse
/// files that work in the tool they were written for (the D472 posture).
///
/// A missing block is not an error: the whole file is then the template.
fn split_frontmatter(text: &str) -> Option<(Frontmatter, &str)> {
    let Some(rest) = fence(text) else {
        return Some((Frontmatter::default(), text));
    };

    let mut front = Frontmatter::default();
    let mut rest = rest;
    loop {
        if let Some(body) = fence(rest) {
            return Some((front, body));
        }
        // No closing fence anywhere: the file is refused rather than read as a
        // template whose first half is a header nobody meant to send.
        let (line, tail) = rest.split_once('\n')?;
        rest = tail;

        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = unquote_scalar(value.trim());
        if value.is_empty() {
            // `description:` with nothing after it says nothing; treating it as
            // an empty description would put a blank line in a palette.
            continue;
        }
        let value = Some(value.to_owned());

        match key.trim().to_ascii_lowercase().as_str() {
            "description" => front.description = value,
            "agent" => front.agent = value,
            "model" => front.model = value,
            "argument-hint" => front.argument_hint = value,
            // Tolerated: a file may carry keys for the tool it was written for.
            _ => {}
        }
    }
}

/// What follows the fence when `text` opens with one, allowing the trailing
/// `\r` a file written on another platform carries.
fn fence(text: &str) -> Option<&str> {
    let (line, rest) = match text.split_once('\n') {
        Some((line, rest)) => (line, rest),
        // A file that is nothing but a fence has no body and no closing fence
        // either; the caller's own rules decide, and both read it as unclosed.
        None => (text, ""),
    };

    (line.trim_end() == FENCE).then_some(rest)
}

/// Strips one matched pair of surrounding quotes from a frontmatter value.
///
/// Only a *matched* pair, unlike [`unquote`], which strips one of either at
/// each end: an argument token comes from a shell-ish grammar where that is the
/// rule, and a `description: "he said 'hi'"` does not.
fn unquote_scalar(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value.strip_prefix(quote)
            && let Some(inner) = inner.strip_suffix(quote)
        {
            return inner;
        }
    }

    value
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
        source: None,
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
        // A `command` table entry is not a file, whichever tier declared it —
        // and a plugin's contributed command arrives through this same table,
        // so it answers to the config's loud dispatch-time refusal rather than
        // to the file tier's quiet one.
        source: None,
    }
}

/// One shell substitution in a filled template.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ShellSubstitution {
    /// The whole ``!`command` `` expression, including its delimiters.
    span: Range<usize>,
    /// The text between the backticks.
    command: String,
}

/// Every complete, non-empty ``!`command` `` in `text`, in match order.
///
/// Spelled out rather than reached through a regex dependency for this one
/// grammar: a literal `!` and backtick, one or more non-backticks, then the
/// closing backtick. An empty pair is not a match and remains literal text.
/// The grammar is upstream's `bashRegex`, also collected by
/// `ConfigMarkdown.shell` under the same `SHELL_REGEX`
/// (`packages/opencode/src/session/prompt.ts:1592`;
/// `packages/opencode/src/config/markdown.ts:12`); its match-order replacement
/// loop is `packages/opencode/src/session/prompt.ts:1397-1407`.
fn shell_substitutions(text: &str) -> Vec<ShellSubstitution> {
    let mut found = Vec::new();
    let mut offset = 0;

    while let Some(relative_start) = text[offset..].find("!`") {
        let start = offset + relative_start;
        let command_start = start + 2;
        let Some(relative_end) = text[command_start..].find('`') else {
            break;
        };
        let command_end = command_start + relative_end;
        let end = command_end + 1;

        if command_start != command_end {
            found.push(ShellSubstitution {
                span: start..end,
                command: text[command_start..command_end].to_owned(),
            });
        }
        offset = end;
    }

    found
}

/// Every file-like token `text` mentions, in the order it mentions them.
///
/// This is deliberately a mirror of
/// `crates/ganja-tui/src/mention.rs::scan`, not a shared frontend helper:
/// core cannot reach the TUI without reversing the dependency that keeps the
/// engine terminal-free. A test in that module pins the two scans equal over a
/// shared table of cases so their grammars cannot drift quietly.
///
/// A mention opens at an `@` that starts a line or follows whitespace and runs
/// to the next whitespace. Repeats collapse by the whole mention — path and
/// range together — because two slices of one file are distinct attachments.
#[must_use]
pub fn mentions(text: &str) -> Vec<crate::protocol::Mention> {
    let mut found: Vec<crate::protocol::Mention> = Vec::new();

    for line in text.split('\n') {
        let characters: Vec<char> = line.chars().collect();
        let mut index = 0;

        while index < characters.len() {
            let opens =
                characters[index] == '@' && (index == 0 || characters[index - 1].is_whitespace());
            if !opens {
                index += 1;
                continue;
            }

            let token: String = characters[index + 1..]
                .iter()
                .take_while(|character| !character.is_whitespace())
                .collect();
            index += token.chars().count() + 1;

            // A bare `@` names nothing.
            if token.is_empty() {
                continue;
            }
            let (path, start, end) = split_range(&token);
            // Neither does a bare range: `@#5` is lines of no file at all.
            if path.is_empty() {
                continue;
            }
            let mention = crate::protocol::Mention {
                path: path.to_owned(),
                start,
                end,
            };
            if !found.contains(&mention) {
                found.push(mention);
            }
        }
    }

    found
}

/// Splits a valid `#line-range` suffix from one mention token.
///
/// The last `#` is the separator, and the suffix must be all of `start` or
/// `start-end` in ASCII digits. Anything else leaves the whole token as a path,
/// because `#` is a character a file name may contain. An empty end is absent,
/// and an end at or before its start is discarded.
fn split_range(token: &str) -> (&str, Option<u32>, Option<u32>) {
    let Some((path, suffix)) = token.rsplit_once('#') else {
        return (token, None, None);
    };

    match parse_range(suffix) {
        Some((start, end)) => (path, Some(start), end),
        None => (token, None, None),
    }
}

/// The normalized `start` or `start-end` a suffix names.
fn parse_range(suffix: &str) -> Option<(u32, Option<u32>)> {
    // Spelled out because `u32::from_str` accepts a leading `+`, which the
    // grammar's `\d+` does not.
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());

    let (start, end) = match suffix.split_once('-') {
        None => (suffix, None),
        Some((start, end)) => (start, Some(end)),
    };
    if !digits(start) || !end.is_none_or(|end| end.is_empty() || digits(end)) {
        return None;
    }

    let start: u32 = start.parse().ok()?;
    let end = match end {
        Some(end) if !end.is_empty() => Some(end.parse::<u32>().ok()?),
        _ => None,
    };

    Some((start, end.filter(|end| start < *end)))
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
fn fill_template(template: &str, arguments: &str) -> String {
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

/// Nothing here calls [`Registry::build`]: since **D481** that function reads
/// the environment-resolved config home, and a test in this binary cannot
/// redirect it without mutating process-wide state that every other test in the
/// binary shares. The whole registry — every tier, and the precedence between
/// them — is therefore pinned by `tests/command_files.rs`, one test in one
/// binary with the home redirected. What is left here is what a directory and a
/// file alone decide.
#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use super::{
        Definition, INIT, INIT_TEMPLATE, MAX_COMMAND_FILE_BYTES, PATH_PLACEHOLDER, Registry,
        file_commands, fill_template, mentions, shell_substitutions, split_range, tokenize,
    };
    use crate::tool::{Credentials, FileTimes, ToolCtx};

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
                fill_template(template, arguments),
                expected,
                "expanding {template:?} with {arguments:?}"
            );
        }
    }

    #[test]
    fn shell_substitutions_match_complete_nonempty_commands_in_written_order() {
        let cases: &[(&str, &[&str])] = &[
            (r#"!`echo hi`"#, &["echo hi"]),
            (r#"!`first` between !`second`"#, &["first", "second"]),
            // An empty command does not satisfy the one-or-more grammar.
            (r#"!``"#, &[]),
            ("`", &[]),
            ("!", &[]),
            (r#"!`without a close"#, &[]),
            // The first backtick closes the match; it cannot be command text.
            (r#"!`echo `tail`"#, &["echo "]),
        ];

        for (text, expected) in cases {
            let matches = shell_substitutions(text);
            let commands = matches
                .iter()
                .map(|substitution| substitution.command.as_str())
                .collect::<Vec<_>>();
            assert_eq!(commands, *expected, "scanning {text:?}");
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
    fn mentions_open_only_at_a_word_boundary_and_require_a_path() {
        let cases: &[(&str, &[&str])] = &[
            ("@a.rs", &["a.rs"]),
            ("look at @a.rs\nthen @b.rs", &["a.rs", "b.rs"]),
            ("mail me@example.com", &[]),
            ("an @ on its own", &[]),
            ("@#5", &[]),
        ];

        for (text, expected) in cases {
            let found = mentions(text);
            let paths = found
                .iter()
                .map(|mention| mention.path.as_str())
                .collect::<Vec<_>>();
            assert_eq!(paths, *expected, "scanning {text:?}");
        }
    }

    #[test]
    fn a_range_suffix_is_split_only_when_it_parses() {
        let cases = [
            ("a.rs", ("a.rs", None, None)),
            ("a.rs#5", ("a.rs", Some(5), None)),
            ("a.rs#5-9", ("a.rs", Some(5), Some(9))),
            // An empty end is a start alone.
            ("a.rs#5-", ("a.rs", Some(5), None)),
            // A reversed or flat range keeps its start only.
            ("a.rs#20-10", ("a.rs", Some(20), None)),
            ("a.rs#5-5", ("a.rs", Some(5), None)),
            // Line zero is what was typed; the read clamps it, not the scan.
            ("a.rs#0", ("a.rs", Some(0), None)),
            // The split is at the *last* `#`, so a path may contain one.
            ("we#ird.rs#5-9", ("we#ird.rs", Some(5), Some(9))),
            // Tails outside the grammar stay part of the path: `#` is a
            // character a file name may contain.
            ("notes#TODO", ("notes#TODO", None, None)),
            ("a.rs#", ("a.rs#", None, None)),
            ("a.rs#5-9-12", ("a.rs#5-9-12", None, None)),
            ("a.rs#-5", ("a.rs#-5", None, None)),
            ("a.rs#+5", ("a.rs#+5", None, None)),
            // A line number past `u32` is not a line number.
            (
                "a.rs#99999999999999999999",
                ("a.rs#99999999999999999999", None, None),
            ),
        ];

        for (mentioned, (path, start, end)) in cases {
            assert_eq!(split_range(mentioned), (path, start, end), "{mentioned:?}");
        }
    }

    #[test]
    fn mentions_dedupe_by_path_and_range_together() {
        assert_eq!(mentions("@a.rs#5-9 and again @a.rs#5-9").len(), 1);

        let mentions = mentions("@a.rs#5-9 then @a.rs#30-40 then @a.rs");
        assert_eq!(mentions.len(), 3, "{mentions:?}");
    }

    #[tokio::test]
    async fn template_expansion_runs_shells_and_attaches_only_files_that_exist() {
        let root = tempfile::TempDir::new().expect("a temporary project is creatable");
        std::fs::write(root.path().join("present.md"), "present")
            .expect("the mentioned fixture is writable");
        let ctx = ToolCtx {
            cwd: root.path().to_owned(),
            cancel: tokio_util::sync::CancellationToken::new(),
            call_id: String::new(),
            files: Arc::new(FileTimes::default()),
            credentials: Credentials::Unguarded,
            spawn: None,
            ask: None,
            switch: None,
            jobs: None,
        };
        let command = |template: &str| Definition {
            name: "fixture".to_owned(),
            description: None,
            template: template.to_owned(),
            agent: None,
            model: None,
            source: None,
        };

        let echoed = command(r#"!`echo hi`"#).expand("", &ctx).await;
        assert_eq!(echoed.prompt, "hi");

        let failed = command(r#"!`printf still-here; exit 7`"#)
            .expand("", &ctx)
            .await;
        assert_eq!(
            failed.prompt, "still-here",
            "a non-zero exit still substitutes what the command wrote"
        );

        let attached = command("read @present.md and ask @alice")
            .expand("", &ctx)
            .await;
        assert_eq!(attached.prompt, "read @present.md and ask @alice");
        assert_eq!(
            attached.mentions,
            vec![crate::protocol::Mention {
                path: "present.md".to_owned(),
                start: None,
                end: None,
            }],
            "only the path that exists becomes a file part"
        );
    }

    /// A commands directory a test owns outright, so nothing here reads — or
    /// depends on the absence of — whatever the machine running the suite keeps
    /// in its own config home. The tier that *does* resolve that home is
    /// exercised in `tests/command_files.rs`, which redirects it.
    fn commands_dir(files: &[(&str, &[u8])]) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("a temporary directory is creatable");
        for (name, contents) in files {
            std::fs::write(dir.path().join(name), contents).expect("the fixture is writable");
        }

        dir
    }

    #[test]
    fn a_command_file_is_its_frontmatter_and_its_body() {
        let dir = commands_dir(&[(
            "review.md",
            b"---\n\
              description: review the diff\n\
              agent: plan\n\
              model: anthropic/claude-sonnet-4-5\n\
              argument-hint: <path>\n\
              ---\n\
              review $ARGUMENTS\n",
        )]);

        let commands = file_commands(dir.path());
        assert_eq!(commands.len(), 1, "{commands:?}");
        let review = &commands[0];
        assert_eq!(review.name, "review", "the name is the file's stem");
        assert_eq!(
            review.description.as_deref(),
            Some("review the diff — <path>"),
            "the hint rides the line a palette already shows"
        );
        assert_eq!(review.agent.as_deref(), Some("plan"));
        assert_eq!(review.model.as_deref(), Some("anthropic/claude-sonnet-4-5"));
        assert_eq!(
            review.template, "review $ARGUMENTS\n",
            "the body is the template verbatim"
        );
        assert_eq!(
            fill_template(&review.template, "src/"),
            "review src/",
            "so the expansion a config command gets is the expansion this gets"
        );
    }

    #[test]
    fn a_file_with_no_frontmatter_is_all_template() {
        let dir = commands_dir(&[("hello.md", b"say hello to $1\n")]);

        let commands = file_commands(dir.path());
        assert_eq!(commands.len(), 1, "{commands:?}");
        assert_eq!(commands[0].name, "hello");
        assert_eq!(commands[0].description, None);
        assert_eq!(commands[0].template, "say hello to $1\n");
    }

    #[test]
    fn frontmatter_tolerates_what_it_does_not_understand() {
        let dir = commands_dir(&[
            (
                "kept.md",
                b"---\n\
                  # a comment somebody left\n\
                  allowed-tools: Bash(git status:*)\n\
                  not a key-value line at all\n\
                  Description: \"quoted, and capitalised\"\n\
                  agent:\n\
                  ---\n\
                  body\n",
            ),
            // A hint with no description of its own still says something.
            ("hint.md", b"---\nargument-hint: <issue>\n---\nfix it\n"),
        ]);

        let commands = file_commands(dir.path());
        let described: Vec<(&str, Option<&str>)> = commands
            .iter()
            .map(|command| (command.name.as_str(), command.description.as_deref()))
            .collect();
        assert_eq!(
            described,
            vec![
                ("hint", Some("<issue>")),
                ("kept", Some("quoted, and capitalised")),
            ],
            "unknown keys, comments and stray lines are skipped, not fatal"
        );
        let kept = commands
            .iter()
            .find(|command| command.name == "kept")
            .expect("the tolerated file is a command");
        assert_eq!(
            kept.agent, None,
            "a key with nothing after it says nothing at all"
        );
        assert_eq!(kept.template, "body\n");
    }

    #[test]
    fn a_file_this_build_will_not_read_is_skipped_rather_than_half_parsed() {
        let oversized = vec![b'x'; usize::try_from(MAX_COMMAND_FILE_BYTES).expect("a usize") + 1];
        let dir = commands_dir(&[
            // A block that opens and never closes: the header is not a prompt.
            (
                "unterminated.md",
                b"---\ndescription: half a header\nand then a body\n",
            ),
            ("binary.md", &[0xff, 0xfe, b'n', 0x00, b'o']),
            ("huge.md", &oversized),
            // Not Markdown, so not meant for this directory.
            ("notes.txt", b"just notes"),
            ("good.md", b"this one is fine\n"),
        ]);
        std::fs::create_dir(dir.path().join("nested")).expect("a subdirectory is creatable");
        std::fs::write(dir.path().join("nested").join("deep.md"), b"not read yet")
            .expect("the nested fixture is writable");

        let commands = file_commands(dir.path());
        let names: Vec<&str> = commands
            .iter()
            .map(|command| command.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["good"],
            "every hostile file is absent from the roster: {commands:?}"
        );
    }

    #[test]
    fn a_missing_commands_directory_is_the_common_case_and_not_an_error() {
        let dir = tempfile::TempDir::new().expect("a temporary directory is creatable");

        assert!(file_commands(&dir.path().join("commands")).is_empty());
    }

    #[tokio::test]
    async fn a_file_command_expands_through_the_one_expansion_path() {
        let root = tempfile::TempDir::new().expect("a temporary project is creatable");
        std::fs::write(root.path().join("present.md"), "present")
            .expect("the mentioned fixture is writable");
        let dir = commands_dir(&[(
            "brief.md",
            b"---\ndescription: brief me\n---\n!`printf hi` about $ARGUMENTS beside @present.md\n",
        )]);
        let ctx = ToolCtx {
            cwd: root.path().to_owned(),
            cancel: tokio_util::sync::CancellationToken::new(),
            call_id: String::new(),
            files: Arc::new(FileTimes::default()),
            credentials: Credentials::Unguarded,
            spawn: None,
            ask: None,
            switch: None,
            jobs: None,
        };

        let commands = file_commands(dir.path());
        let expanded = commands[0].expand("the port", &ctx).await;

        assert_eq!(expanded.prompt, "hi about the port beside @present.md");
        assert_eq!(
            expanded.mentions,
            vec![crate::protocol::Mention {
                path: "present.md".to_owned(),
                start: None,
                end: None,
            }],
            "a file command attaches what a config command's template would"
        );
    }
}
