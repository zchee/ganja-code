//! Slash commands that expand into a prompt and run as an ordinary turn.
//!
//! Spec: upstream `packages/opencode/src/command/index.ts` for the builtin set
//! and `packages/opencode/src/session/prompt.ts` (`SessionPrompt.command`) for
//! the expansion. A command is a **template plus a name**: selecting it types
//! nothing into the model, it fills its placeholders from whatever the user
//! typed after the name and sends the result the way a typed message is sent.
//!
//! `/init` and `/team` are the two builtins this build ships. `/init`'s
//! template is upstream's `command/template/initialize.txt` with ganja's
//! identity substituted (**D522**), and everything it does about `AGENTS.md` —
//! create it if it is absent, improve it in place if it is there — is *prompt*
//! semantics. There is no file handling here and none upstream: the model
//! reaches for `write` and `edit` like it would for any other file.
//!
//! `/team` is the same shape and none of the same lineage: upstream opencode
//! has no teams and no second agent to address, so its template is ganja's own
//! prose written from this port's behavior specification, and every stage,
//! file and rule it describes is carried out by the model through tools this
//! build already lends it — the four task tools, `task`, `send_message`,
//! `write` and `read`. Nothing about the pipeline is machinery here; what *is*
//! machinery is the two engine-native guards beside it (the continuation
//! blocker and the name nag, both in [`crate::session`]), because neither can
//! be a sentence in a prompt and still be true — and one refusal, for the same
//! reason in miniature: [`/teammate`'s three subcommands](Misdirected) typed at
//! `/team` are answered here rather than by the model, because a template that
//! asks the model to redirect them spends a round trip to say what three fixed
//! words already say (**bead 2m46**).
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

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::{fs, io};

use ganja_tool::frontmatter::{fields, split};

use crate::config::{CommandConfig, Config};

/// Name of the builtin that writes a repository's `AGENTS.md`.
pub const INIT: &str = "init";

/// What `/init` sends, derived from upstream
/// `packages/opencode/src/command/template/initialize.txt` with ganja's
/// identity substituted (**D522**; MIT, see `THIRD_PARTY_NOTICES.md`).
const INIT_TEMPLATE: &str = include_str!("prompt/initialize.txt");

/// `/init`'s one-line description, upstream's own string
/// (`command/index.ts`).
const INIT_DESCRIPTION: &str = "guided AGENTS.md setup";

/// Name of the builtin that runs a staged team pipeline.
pub const TEAM: &str = "team";

/// What `/team` sends. Ganja's own prose throughout — behavior modelled on
/// oh-my-claudecode's team skill, no sentence taken from it — written from the
/// behavior specification in `.omc/plans/2026-09-02-team-orchestration.md`,
/// which is why it earns no `THIRD_PARTY_NOTICES.md` entry the way
/// [`INIT_TEMPLATE`] does.
const TEAM_TEMPLATE: &str = include_str!("prompt/team.txt");

/// `/team`'s one-line description.
const TEAM_DESCRIPTION: &str = "run a staged team pipeline over a shared task list";

/// What the composer draws dim after a typed `/team` (**D518**). The grammar
/// itself, because the model — not this crate — is what parses it.
const TEAM_ARGUMENT_HINT: &str = "[N[:agent]] [--backend <surface>] <task>";

/// The placeholder `/init`'s template carries for the worktree it is being run
/// in. Upstream substitutes it with a plain string replace, which in JavaScript
/// replaces the **first** occurrence only; the file holds exactly one.
const PATH_PLACEHOLDER: &str = "${path}";

/// The placeholder `/team`'s template carries for the directory this project's
/// pipeline state files live in.
const STATE_PLACEHOLDER: &str = "${state}";

/// The placeholder `/team`'s template carries for the directory this project's
/// stage handoffs live in.
const HANDOFFS_PLACEHOLDER: &str = "${handoffs}";

/// The placeholder `/team`'s template carries for the id of the session
/// running it.
///
/// Filled at **expansion**, and last of all ([`Definition::expand`]), where
/// the two directory ones are filled at roster build (**D547**). The
/// difference is what it stands for: a directory is a fact about the checkout,
/// the same for every session in it, while which session is running changes
/// with every `NewSession`, every resume, and between two ganja processes in
/// one worktree. It has to be filled by somebody because a session cannot read
/// its own id — the `<env>` block does not carry it and `list_sessions` drops
/// the caller's own row — so "use this session's id" is an instruction no
/// model can follow.
const SESSION_PLACEHOLDER: &str = "${session}";

/// Where a project's team pipeline artefacts hang off its data directory
/// (decisions 7, 8 and 19 of the team-orchestration plan): operational state
/// lives in the data home, and `.ganja/` stays a committable-config namespace.
const TEAM_DIR: &str = "team";

/// What is written where when neither directory can be resolved — a machine
/// with no home for [`crate::project::data_home`] to answer from.
///
/// The template asks for the two paths to be taken exactly as written rather
/// than worked out — an instruction true of a resolved path and of this shape
/// alike, which is why it calls them neither absolute nor already resolved:
/// this value is neither. Naming the shape is the honest fallback; filling in a
/// relative path is what the model would then create in the worktree. A
/// session in that state has no data home to write a session row into either,
/// so this is unreachable short of a machine ganja cannot store anything on.
const UNRESOLVED_HOME: &str = "<data home>/ganja/project/<slug>/team";

/// The placeholder that stands for everything the user typed, untokenized.
const ARGUMENTS: &str = "$ARGUMENTS";

/// The command that really answers to a roster line, since **D544**'s clean
/// cut moved the dialog's name.
const TEAMMATE: &str = "teammate";

/// The two of `/teammate`'s three subcommands that take arguments, restated;
/// `list`, which takes none, is matched whole where this is consulted.
///
/// The grammar is `list | spawn <name> [--backend] [--agent] [prompt] |
/// shutdown [member]`, and it is **spelled in `crates/ganja-tui/src/command.rs`**
/// — the terminal frontend sits above the engine, so this crate cannot name
/// that file's types and the words are written out here instead. Three words
/// that have not moved since D544; a fourth would be a `/teammate` this misses,
/// which costs a model round trip rather than correctness.
const ROSTER_SUBCOMMANDS: [&str; 2] = ["spawn", "shutdown"];

/// What `/team` answers with instead of expanding, when what it was given is
/// one of `/teammate`'s own subcommands (**bead 2m46**).
///
/// A value rather than a rendered sentence because the sentence a person reads
/// is the engine's — every other refusal of a command lives in `EngineError`,
/// and a second one worded here would be two places to change one wording.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Misdirected {
    /// The line the person meant: what they typed **trimmed**, with
    /// `/teammate` in place of `/team`.
    ///
    /// Internal spacing survives — `/team spawn  w1` is handed back as
    /// `/teammate spawn  w1`, because respelling somebody's arguments is not
    /// this door's business — but the ends are trimmed, since a suggested
    /// command line with a trailing space helps nobody read it.
    pub meant: String,
}

/// Whether `arguments` is a roster line that reached the wrong command.
///
/// **Conservative on purpose.** `spawn` and `shutdown` take arguments of their
/// own, so a first word of either is that subcommand however it goes on; `list`
/// takes none, so only a bare `list` is one, and `/team list the config keys`
/// is a task somebody wants done. Everything else — "start a teammate called
/// w1", "who is on the team" — is left to the template, which asks the model to
/// notice the same thing in prose. What this door buys is the exact spellings:
/// those cost no round trip and no turn.
///
/// **Case-sensitive**, and by the same argument: `/team List` is matched
/// against the words `/teammate` really parses, which it spells in lower case,
/// so respelling one here would be this door asserting a grammar the command
/// it redirects to does not have. A capitalised roster line therefore lands in
/// the template like any other phrasing and is answered there, in prose, for
/// one round trip.
fn misdirected(arguments: &str) -> Option<Misdirected> {
    let trimmed = arguments.trim();
    let first = trimmed.split_whitespace().next()?;
    if !ROSTER_SUBCOMMANDS.contains(&first) && trimmed != "list" {
        return None;
    }

    Some(Misdirected { meant: format!("/{TEAMMATE} {trimmed}") })
}

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
    /// A display-only hint about what to type after the name, when the
    /// command's file declared one (`argument-hint`, Claude's own key).
    ///
    /// The composer draws it dim after a typed name (**D518**); it is also
    /// folded into [`Definition::description`] so a palette row stays
    /// informative before the name is complete. Never parsed, never
    /// validated: what the arguments *mean* is the template's business.
    pub argument_hint: Option<String>,
    /// The Markdown file this command was read from, when it came from one.
    ///
    /// [`None`] for a builtin and for a `command` table entry — neither is a
    /// file somebody is looking at. Carried so that a refusal made after the
    /// roster is built can name the file that has to be edited, which is the
    /// only thing that distinguishes "your `plan` is misspelled" from a
    /// command that quietly is not there
    /// ([`Registry::refusing_unknown_agents`]).
    pub source: Option<PathBuf>,
    /// Whether this definition is one this build ships, rather than one a
    /// config table or a Markdown file declared.
    ///
    /// Carried for exactly one reader, [`Definition::expand`]'s misdirection
    /// gate: a `command` table entry that reuses `team` **replaces** the
    /// builtin, deliberately and by documented precedence
    /// ([`Registry::build`]), and a gate on the name alone would go on
    /// refusing three argument shapes on behalf of a command that is no longer
    /// there. [`Definition::source`] cannot answer this — it separates a file
    /// from everything else, where the tier that can take `/team` over is the
    /// one that is not a file either.
    pub builtin: bool,
}

impl Definition {
    /// What this command sends when it is run by `session` with `arguments`.
    ///
    /// `ctx.cwd` is the project root: it is both where shell substitutions run
    /// and where mentions resolve. One context keeps a template from running a
    /// command in one place while naming files in another.
    ///
    /// `session` is the id of the session about to send this, filled into the
    /// template's `${session}` **after every other step below**. It is a
    /// parameter rather than another `ctx` field because a `ToolCtx` is what a
    /// *tool call* runs under and this expansion is not one; and it is filled
    /// here rather than at roster build because one roster serves every
    /// session a process opens.
    ///
    /// Refuses before a byte of the template is filled when the **builtin**
    /// `/team` was typed with one of `/teammate`'s own subcommands (**bead
    /// 2m46**): that template's own second arm asks the model to redirect
    /// those, which is a whole round trip to be told what the three words
    /// already say.
    ///
    /// The check is on the name *and* on [`Definition::builtin`], because the
    /// name is only the difference between the two commands while this build
    /// is the one that spelled it: a config `[command.team]` replaces the
    /// builtin outright ([`Registry::build`]), and refusing three argument
    /// shapes on its behalf would make somebody else's command unreachable in
    /// favour of a sentence about a command they did not write.
    ///
    /// # Errors
    ///
    /// [`Misdirected`], carrying the line that was meant. No turn starts.
    pub async fn expand(
        &self,
        arguments: &str,
        session: &str,
        ctx: &crate::tool::ToolCtx,
    ) -> Result<Expanded, Misdirected> {
        if self.builtin
            && self.name == TEAM
            && let Some(misdirected) = misdirected(arguments)
        {
            return Err(misdirected);
        }

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
            let result =
                shell.run_reporting(serde_json::json!({ "command": command }), ctx, None).await;
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

        // The session id goes in **last**, and the ordering is the safety
        // property rather than a taste: every pass that reads this text for
        // something to *run* or to *attach* has already run, so nothing an id
        // happens to spell can become a shell command this template executes or
        // a file it reads. A plain replace, so the id arrives as the bytes it
        // is and its own text is never looked at again. The price is that a
        // person who types the placeholder after the command name gets it
        // filled too — which tells them their own session's id and nothing
        // else, where the other order risks running their id as a command.
        let prompt = prompt.replace(SESSION_PLACEHOLDER, session);

        Ok(Expanded { prompt, mentions })
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

        Self { commands: commands.into_values().map(|(_, definition)| definition).collect() }
    }

    /// The builtins alone, for an engine nobody configured.
    #[must_use]
    pub fn builtin(worktree: &Path) -> Self {
        Self { commands: builtins(worktree) }
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
        self.commands.iter().map(|command| command.name.clone()).collect()
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
            let tier =
                if Some(&dir) == global.as_ref() { Tier::GlobalFile } else { Tier::ProjectFile };

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
                && path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
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

    Some(Definition { source: Some(path.to_owned()), ..definition })
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
    let Frontmatter { description, agent, model, argument_hint } = front;

    Some(Definition {
        name: name.to_owned(),
        description: palette_line(description, argument_hint.clone()),
        template: body.to_owned(),
        agent,
        model,
        argument_hint,
        // The caller knows which file this text came from; a parser given a
        // string does not.
        source: None,
        builtin: false,
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
/// The hint also travels on its own slot for the composer's inline rendering
/// (**D518**), but a palette row is read *before* the name is complete, when
/// the inline hint cannot show yet — so the fold stays: `review the diff —
/// <path>`, or the hint alone when the file gave no description, which still
/// beats a nameless row.
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
/// The shared reader deliberately returns [`None`] both when no block opens and
/// when one opens but never closes. Command files distinguish those outcomes:
/// the former is a body-only command, while the latter is refused rather than
/// sent as a template whose first half is a header nobody meant to send.
///
/// [`parse_command`] removes a leading BOM before this adapter runs. The opener
/// check below therefore matches the shared reader's post-BOM grammar exactly;
/// the reader still strips a BOM for its other consumers, but this branch does
/// not depend on that second, independent normalization path by accident.
fn split_frontmatter(text: &str) -> Option<(Frontmatter, &str)> {
    if !opens_frontmatter(text) {
        return Some((Frontmatter::default(), text));
    }

    // A leading fence was established independently, so `None` now has only
    // one meaning: the block never closed.
    let (frontmatter, body) = split(text)?;
    let fields = fields(frontmatter);
    let field = |name: &str| {
        fields
            .iter()
            .find(|(key, value)| key.eq_ignore_ascii_case(name) && !value.is_empty())
            .map(|(_, value)| value.clone())
    };

    Some((
        Frontmatter {
            description: field("description"),
            agent: field("agent"),
            model: field("model"),
            argument_hint: field("argument-hint"),
        },
        body,
    ))
}

/// Whether BOM-free `text` opens with the exact fence grammar [`split`] uses.
/// Requiring a newline immediately after the fence, without trailing-whitespace
/// tolerance, deliberately matches that shared grammar and `agent.rs`'s use of it.
fn opens_frontmatter(text: &str) -> bool {
    let Some(rest) = text.strip_prefix(FENCE) else {
        return false;
    };

    rest.starts_with('\n') || rest.starts_with("\r\n")
}

/// The commands this build ships, with `${path}` already pointing at the
/// worktree the session is working in and `/team`'s two directory
/// placeholders already pointing into this project's data directory.
///
/// The paths are resolved **here**, at roster build, rather than left for the
/// model to assemble from a description of the layout: an instruction that
/// says "write your state under the data home" is one every session gets to
/// guess about, and two sessions that guess differently do not resume each
/// other. `/init` fills its own worktree placeholder for the same reason.
fn builtins(worktree: &Path) -> Vec<Definition> {
    let team = crate::project::Project::resolve(worktree)
        .data_dir()
        .map_or_else(|_| PathBuf::from(UNRESOLVED_HOME), |data| data.join(TEAM_DIR));
    let team_dir = |leaf: &str| team.join(leaf).to_string_lossy().into_owned();

    vec![
        Definition {
            name: INIT.to_owned(),
            description: Some(INIT_DESCRIPTION.to_owned()),
            template: INIT_TEMPLATE.replacen(PATH_PLACEHOLDER, &worktree.to_string_lossy(), 1),
            agent: None,
            model: None,
            argument_hint: None,
            source: None,
            builtin: true,
        },
        Definition {
            name: TEAM.to_owned(),
            description: Some(TEAM_DESCRIPTION.to_owned()),
            // Every occurrence, not the first: `/init`'s `replacen` mirrors
            // a JavaScript `String.replace` over a file that holds exactly
            // one placeholder, and this template names its handoffs
            // directory twice — once to say where it is and once to say what
            // to write into it. A survivor would reach the model as literal
            // `${handoffs}`, which is a path nothing on the machine answers
            // to.
            template: TEAM_TEMPLATE
                .replace(STATE_PLACEHOLDER, &team_dir("state"))
                .replace(HANDOFFS_PLACEHOLDER, &team_dir("handoffs")),
            // Deliberately not an agent of its own: the pipeline runs as
            // whoever the session already is, because it spawns the agents it
            // needs rather than becoming one.
            agent: None,
            model: None,
            argument_hint: Some(TEAM_ARGUMENT_HINT.to_owned()),
            source: None,
            // What [`Definition::expand`]'s misdirection gate is really asking
            // about: this is the `/team` whose template redirects a roster
            // line, so it is the one that may answer three of them itself.
            builtin: true,
        },
    ]
}

/// One command as a config file described it.
fn configured(name: &str, definition: &CommandConfig) -> Definition {
    Definition {
        name: name.to_owned(),
        description: definition.description.clone(),
        template: definition.template.clone(),
        agent: definition.agent.clone(),
        model: definition.model.clone(),
        // The curated `command` table has no hint key: adding one is a config
        // surface decision, not a fallout of the composer learning to draw
        // hints (**D518**).
        argument_hint: None,
        // A `command` table entry is not a file, whichever tier declared it —
        // and a plugin's contributed command arrives through this same table,
        // so it answers to the config's loud dispatch-time refusal rather than
        // to the file tier's quiet one.
        source: None,
        // The tier that is allowed to take a builtin's name over, which is why
        // this flag exists at all: a `[command.team]` entry is the project's
        // own `/team` from the moment it is loaded, and nothing here may keep
        // answering roster lines on behalf of the template it replaced.
        builtin: false,
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
            let mention = crate::protocol::Mention { path: path.to_owned(), start, end };
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

    if greedy { tokens[first..].join(" ") } else { tokens[first].clone() }
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
#[path = "command_tests.rs"]
mod tests;
