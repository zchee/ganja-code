//! Who the model is being asked to be: its prompt, and what it may do.
//!
//! Spec: upstream `packages/opencode/src/agent/agent.ts` (the v1 stack; the
//! `packages/core` mirror is not the one the session pipeline runs).
//!
//! An agent is four things a turn reads: a **prompt** that replaces the base
//! one for the model's family, a **model** it prefers, a **mode** saying who
//! may run it, and a **ruleset** that is the entire tool-enable mechanism —
//! upstream has no `tools` field at runtime, and neither does this.
//!
//! The rules of every agent are assembled in one order, and the order *is* the
//! precedence, because evaluation is last-match-wins ([`crate::permission`]):
//!
//! ```text
//! shared defaults  <  the agent's own delta  <  config `permission`  <  the
//! agent's own config `permission`   …and, at the session, the stored rules
//! ```
//!
//! The last tier lives in [`crate::permission::Permissions`], layered over
//! whatever [`Agent::rules`] an engine installs as its baseline.
//!
//! # What is adapted, and why
//!
//! Upstream's defaults open with `"*": "allow"`, which is the whole reason its
//! `build` agent runs a shell command without asking. Ganja's baseline is the
//! other way round — [`crate::permission::ASK_BY_DEFAULT`] names the tools that
//! change the world, and everything else already runs unasked — so porting that
//! rule literally would erase P3's hardening in one line. It is therefore *not*
//! emitted: ganja's ask-by-default table is the `"*": "allow"`-equivalent, and
//! the two agree everywhere except on the tools ganja deliberately gates.
//!
//! Upstream rules naming permissions this build has no tool for — `list`,
//! `doom_loop`, `lsp` — are not ported.
//! A rule about a tool that cannot be called decides nothing, and carrying it
//! would suggest the tool exists. `task` is on [`PLAN`] for the opposite
//! reason: the agent whose point is "do not act" denies the subagent that
//! would act for it, and `subagent.rs`'s `denies_task` reads exactly that
//! rule. `websearch` came off
//! that list with the tool: [`EXPLORE`] allows it, exactly as upstream's does
//! — and both plan doors came off it with the tools behind them: the shared
//! defaults deny `plan_exit` and `plan_enter`, [`PLAN`] alone allows the exit
//! and [`BUILD`] alone the enter, which is exactly upstream's own pair of
//! deltas. Denied tools are still not hidden, so build's model sees
//! `plan_exit` in its schema — and plan's sees `plan_enter` — and a call comes
//! back as refusal text it reads.
//!
//! Upstream also *hides* a tool from the model's schema when the last rule
//! matching it is `"*": "deny"` (`permission/index.ts`, `disabled`). That is
//! not ported: a denied call still reaches the gate and comes back as a
//! refusal the model reads, which is the same outcome by the route this port
//! already has for every other refusal.
//!
//! # Agent definition files (**D482**)
//!
//! Beside the config's `agent` map there is a **file tier**: a markdown file
//! per agent, under ganja's own two homes and no others —
//! `<config home>/agents/*.md` and `<project root>/.ganja/agents/*.md`, the
//! standing skills precedent ([`crate::config::default_skill_dirs`]). Nothing
//! foreign is discovered: no `~/.claude/agents`, no walk-up. Its shape is
//! Claude Code's — frontmatter naming the agent, its `description`, its
//! `model` and the `tools` it may use, with the body as the prompt — and no
//! upstream opencode counterpart exists at all.
//!
//! Three things are decided here rather than borrowed:
//!
//! - **A `tools:` list compiles to permission rules**, which is the only
//!   tool-enable mechanism this build has (`tool_rules`). Claude *hides* the
//!   tools an agent may not use; ganja **refuses** them — the schema still
//!   carries them and the call comes back as refusal text the model reads,
//!   which is the same route every other refusal here takes.
//! - **A `model:` must be a full `provider/model` id.** Claude's `opus` /
//!   `sonnet` aliases name nothing this build can resolve, so a file carrying
//!   one is skipped with a warning naming the file and the form expected,
//!   rather than starting a session against a model nobody chose.
//! - **The file tier sits below the config tier**: builtin < global file <
//!   project file < `agent.<name>` in `ganja.toml`, and the collision is
//!   logged by name. A config `disable: true` removes a file agent outright,
//!   which is the escape hatch that keeps the config the last word.

use std::fs;
use std::path::{Path, PathBuf};

use ganja_tool::frontmatter::{fields, split};

use crate::config::{AgentConfig, AgentMode, Config};
use crate::permission::{Action, EXTERNAL_DIRECTORY, MCP_PREFIX, Rule};

/// The pattern that covers every call to a permission.
const ANY: &str = "*";

/// Name of the agent that may act. Upstream keys the plan/build reminders off
/// these two names rather than off a mode, and so does this.
pub const BUILD: &str = "build";

/// Name of the read-only agent.
pub const PLAN: &str = "plan";

/// Name of the general-purpose subagent.
pub const GENERAL: &str = "general";

/// Name of the search subagent.
pub const EXPLORE: &str = "explore";

/// Name of the requirements subagent, the first of the five roles the `/team`
/// pipeline routes its stages to.
pub const ANALYST: &str = "analyst";

/// Name of the implementation subagent.
pub const EXECUTOR: &str = "executor";

/// Name of the evidence-checking subagent.
pub const VERIFIER: &str = "verifier";

/// Name of the plan-challenging subagent.
pub const CRITIC: &str = "critic";

/// Name of the root-cause subagent.
pub const DEBUGGER: &str = "debugger";

/// The subdirectory of each home an agent definition file lives in (**D482**).
const AGENTS_SUBDIR: &str = "agents";

/// Every tool this build registers, which is the roster a `tools:` list is
/// judged against.
///
/// A name, not a shape: MCP tools are named only once a server has been
/// dialled, so [`tool_rules`] closes that whole namespace with
/// [`MCP_PREFIX`]'s glob instead. `task` is here although the engine — not
/// [`crate::tool::Registry::with_builtins`] — registers it, because an agent
/// restricted to reading must not be able to delegate its way out of the
/// restriction. Upstream's permission aliases (`apply_patch`, `shell`) are
/// not: no tool answers to either name here.
const TOOL_NAMES: &[&str] = &[
    "read",
    "edit",
    "write",
    "glob",
    "grep",
    "bash",
    "bash_output",
    "kill_shell",
    "todowrite",
    "webfetch",
    "websearch",
    "skill",
    "task",
    "question",
    "plan_exit",
    "plan_enter",
];

/// The tools a `tools:` list may never close, and may never open either.
///
/// `question` is how a turn asks the person watching it something, and the two
/// plan doors are how a session changes what it is doing. None of the three is
/// work the model does *to* the project, and an agent file that lists five
/// tools is describing that work — so a wall built from such a list leaves all
/// three exactly as it found them: `question` keeps its un-ruled allow, and
/// the doors keep whichever answer [`defaults`] and the agent's own delta gave
/// them, which is what stops a `tools:` line silently sealing the plan agent's
/// exit (or handing an unrelated agent a door into it).
const CONVERSATION_TOOLS: &[&str] = &["question", "plan_exit", "plan_enter"];

/// The search subagent's prompt, ported verbatim (MIT; see
/// `THIRD_PARTY_NOTICES.md`).
const EXPLORE_PROMPT: &str = include_str!("prompt/explore.txt");

/// The five team-role prompts. Ganja's own prose, written from this port's
/// team-orchestration behavior specification and modelled on no other
/// project's text, which is why none of them earns a
/// `THIRD_PARTY_NOTICES.md` entry the way [`EXPLORE_PROMPT`] does.
///
/// Each says what the role *is* and nothing about how a team coordinates: the
/// pipeline's protocol — the stages, the shared list, the claim discipline,
/// the shutdown — is `/team`'s template's to state, and a copy of it here
/// would be a second place for it to drift. An agent spawned outside a team
/// is then still a coherent agent.
const ANALYST_PROMPT: &str = include_str!("prompt/analyst.txt");
const EXECUTOR_PROMPT: &str = include_str!("prompt/executor.txt");
const VERIFIER_PROMPT: &str = include_str!("prompt/verifier.txt");
const CRITIC_PROMPT: &str = include_str!("prompt/critic.txt");
const DEBUGGER_PROMPT: &str = include_str!("prompt/debugger.txt");

/// Injected as a synthetic user part on every turn the [`PLAN`] agent runs,
/// ported verbatim from upstream `session/prompt/plan.txt`.
pub const PLAN_REMINDER: &str = include_str!("prompt/plan.txt");

/// Injected once when a session that was planning starts building, ported
/// verbatim from upstream `session/prompt/build-switch.txt`.
pub const BUILD_SWITCH_REMINDER: &str = include_str!("prompt/build-switch.txt");

/// An agent could not be resolved.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum AgentError {
    /// A name was configured and no agent answers to it.
    #[error("agent \"{name}\" does not exist")]
    Unknown {
        /// What was asked for.
        name: String,
    },
    /// A name was configured and names a subagent, which only the task tool
    /// may run.
    #[error("agent \"{name}\" is a subagent")]
    Subagent {
        /// What was asked for.
        name: String,
    },
    /// A name was configured and names a hidden agent.
    #[error("agent \"{name}\" is hidden")]
    Hidden {
        /// What was asked for.
        name: String,
    },
    /// Every agent left is a subagent or hidden, so a session has nothing to
    /// start on. Upstream's `no primary visible agent found`; reachable only
    /// by disabling or hiding the builtins.
    #[error("no visible primary agent is configured")]
    NoneVisible,
}

/// One agent, resolved: everything a turn running as it needs to know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agent {
    /// What it is called, in a picker and in `default_agent`.
    pub name: String,
    /// One line the task tool shows the model when it lists the agents it may
    /// spawn.
    pub description: Option<String>,
    /// Who may run it.
    pub mode: AgentMode,
    /// Hidden agents exist and run; they are only left out of the pickers, of
    /// `default_agent`, and of whatever cycles through agents. Upstream's
    /// `hidden` is exactly this narrow.
    pub hidden: bool,
    /// System prompt, which **replaces** the base prompt for the model's
    /// family rather than adding to it.
    pub prompt: Option<String>,
    /// Model it prefers. Switching to an agent that names one switches the
    /// model with it, which is upstream's behaviour.
    pub model: Option<String>,
    /// Rules this agent's calls are judged by, beneath whatever the session
    /// has stored. Assembled in precedence order; see the module docs.
    pub rules: Vec<Rule>,
}

impl Agent {
    /// Whether the task tool may spawn this one — upstream's
    /// `mode !== "primary"` (`tool/registry.ts`), so `all` qualifies too.
    #[must_use]
    pub fn spawnable(&self) -> bool {
        self.mode != AgentMode::Primary
    }

    /// Whether a user may switch to it: not a subagent, and not hidden.
    #[must_use]
    pub fn selectable(&self) -> bool {
        self.mode != AgentMode::Subagent && !self.hidden
    }
}

/// Every agent a session may run as, in the order they were defined.
///
/// Order is load-bearing twice: it decides which agent a config with no
/// `default_agent` starts on, and it is the order a picker lists. The builtins
/// come first, `build` at the front, and config-defined agents follow.
#[derive(Clone, Debug)]
pub struct Registry {
    agents: Vec<Agent>,
    default: String,
}

impl Registry {
    /// The agents this session may run as: the builtins, then the definition
    /// files under `root`'s two homes, then the config's `agent` map over both.
    ///
    /// `root` is the **project root**, not the directory the process happens to
    /// have been started in — the same value the command roster, the MCP
    /// servers and the LSP workspaces are resolved against, so that opening a
    /// subdirectory and opening the checkout offer the same agents.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when `default_agent` — or the `--agent` flag
    /// above it — names an agent that does not exist, is a subagent, or is
    /// hidden, and when nothing visible is left to start on. Upstream throws in
    /// exactly these four places (`agent.ts`, `defaultInfo`).
    ///
    /// A definition file that cannot be read is **not** one of those: it is
    /// skipped with a warning naming it, and the session starts without it.
    pub fn build(config: &Config, root: &Path) -> Result<Self, AgentError> {
        Self::from_dirs(config, &definition_dirs(root))
    }

    /// The agents `config` alone describes: the builtins and its `agent` map,
    /// with **no** file tier read at all.
    ///
    /// For a caller that holds no project root, and for a fixture that must
    /// resolve the same agents on every machine — a definition file in the
    /// config home of whoever is running a suite must not be able to change
    /// what that suite sees.
    ///
    /// # Errors
    ///
    /// The four [`Registry::build`] returns, for the same reasons.
    pub fn from_config(config: &Config) -> Result<Self, AgentError> {
        Self::from_dirs(config, &[])
    }

    /// The same, over the directories handed in rather than the two resolved
    /// from a root.
    ///
    /// Split out for the reason [`memory_rules`] is: what the file tier *does*
    /// can then be asserted against two temporary directories, instead of a
    /// test having to own the process's `GANJA_CONFIG_HOME` to say it.
    fn from_dirs(config: &Config, directories: &[PathBuf]) -> Result<Self, AgentError> {
        let mut agents = builtins(config);
        file_tier(&mut agents, config, directories);
        overlay(&mut agents, config);

        let default = resolve_default(&agents, config)?;

        Ok(Self { agents, default })
    }

    /// The agent named `name`, or nothing.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Agent> {
        self.agents.iter().find(|agent| agent.name == name)
    }

    /// Every agent, in definition order.
    #[must_use]
    pub fn agents(&self) -> &[Agent] {
        &self.agents
    }

    /// The agent a session starts on.
    #[must_use]
    pub fn default_agent(&self) -> &str {
        &self.default
    }

    /// What a `/team` spec's roster predicate answers for `name` (**D549**,
    /// **R8**).
    ///
    /// Here rather than as a closure at its one production call site because
    /// two spellings of it can come to disagree, and one of them already did
    /// once: **Dv-1** widened which answers the grammar's bare-name arm accepts,
    /// and a test carrying its own copy of the mapping would have gone on
    /// passing. `crate::command::parse_team` still takes a closure — the roster
    /// varies per session, which is the whole reason it is injected — and this
    /// is the closure every caller holding a registry should hand it.
    ///
    /// A registry that does not hold `name` answers
    /// [`RosterAnswer::Unknown`](crate::command::RosterAnswer::Unknown); a
    /// session holding no registry at all answers that for *every* name, which
    /// is the caller's arm rather than this one's.
    #[must_use]
    pub fn roster_answer(&self, name: &str) -> crate::command::RosterAnswer {
        match self.get(name) {
            None => crate::command::RosterAnswer::Unknown,
            Some(agent) if agent.spawnable() => crate::command::RosterAnswer::Spawnable,
            Some(_) => crate::command::RosterAnswer::Primary,
        }
    }
}

/// The nine agents this build ships: upstream's roster, and ganja's own five.
///
/// Four of them are upstream's seven minus the three ganja does not have.
/// `compaction`, `title` and `summary` are hidden agents upstream because its
/// pipeline runs *every* request through an agent. Ganja's title and
/// compaction requests are direct — they carry their own prompt and no tools —
/// so there is nothing for an agent to add (**D7**).
///
/// The other five — [`ANALYST`], [`EXECUTOR`], [`VERIFIER`], [`CRITIC`],
/// [`DEBUGGER`] — are the roles `/team`'s stage routing names, and have no
/// upstream counterpart at all: opencode has no teams. Their prompts are
/// ganja's own.
fn builtins(config: &Config) -> Vec<Agent> {
    let user = config.permission.rules();
    // `attended` is what decides whether the memory door is opened for an
    // agent: a person is watching a primary agent's turn and nobody is
    // watching a subagent's, and memory is written by whoever is being
    // watched. See [`memory_door`].
    let assemble = |attended: bool, own: Vec<Rule>| {
        let mut rules = defaults();
        if attended {
            rules.extend(memory_door(config));
        }
        rules.extend(own);
        rules.extend(user.iter().cloned());
        rules
    };

    let mut agents = vec![
        Agent {
            name: BUILD.to_owned(),
            description: Some(
                "The default agent. Executes tools based on configured permissions.".to_owned(),
            ),
            mode: AgentMode::Primary,
            hidden: false,
            prompt: None,
            model: None,
            // Upstream's delta is `question: allow` and `plan_enter: allow`.
            // The second is adopted now that the door exists (**D477**): the
            // one agent that could usefully stop and plan is the one that
            // would otherwise start implementing, and the shared default
            // denies it to everybody else. Subagents inherit refusals and
            // never allows, so no child ever carries this one down.
            //
            // `question: allow` is still not adopted, and would change
            // nothing: its un-ruled baseline is already *allow* — `decide()`
            // falls through to allow for a tool no rule and no
            // ask-by-default entry names, and `question` appears in neither.
            // Whether it should instead gain an explicit rule is a decision
            // deliberately not taken here.
            rules: assemble(true, vec![rule("plan_enter", ANY, Action::Allow)]),
        },
        Agent {
            name: PLAN.to_owned(),
            description: Some("Plan mode. Disallows all edit tools.".to_owned()),
            mode: AgentMode::Primary,
            hidden: false,
            // Upstream's plan agent has no prompt of its own: what makes it
            // plan is the reminder injected on every one of its turns.
            prompt: None,
            model: None,
            rules: assemble(
                true,
                vec![
                    // The rule that stops a planning session delegating its way
                    // into an edit. `subagent.rs`'s `denies_task` is what reads
                    // it, before a child is ever spawned.
                    rule("task", GENERAL, Action::Deny),
                    // Upstream denies `edit` and then carves two exceptions for
                    // the file a plan is written to. Ganja has no plans directory
                    // — a plan is prose in the transcript — so the deny stands
                    // alone. `write` is denied beside it because upstream's rule
                    // reaches it through an alias table (`edit|write|apply_patch`)
                    // that this port does not have (deviation: plan-denies-write).
                    rule("edit", ANY, Action::Deny),
                    rule("write", ANY, Action::Deny),
                    // The one agent whose finished plan has somewhere to go:
                    // upstream's `plan_exit: allow`, over the shared default deny.
                    // Subagents inherit refusals and never allows, so no child
                    // ever carries this one down.
                    rule("plan_exit", ANY, Action::Allow),
                ],
            ),
        },
        Agent {
            name: GENERAL.to_owned(),
            description: Some(
                "General-purpose agent for researching complex questions and executing \
                 multi-step tasks. Use this agent to execute multiple units of work in parallel."
                    .to_owned(),
            ),
            mode: AgentMode::Subagent,
            hidden: false,
            prompt: None,
            model: None,
            rules: assemble(false, vec![rule("todowrite", ANY, Action::Deny)]),
        },
        Agent {
            name: EXPLORE.to_owned(),
            description: Some(
                "Fast agent specialized for exploring codebases. Use this when you need to \
                 quickly find files by patterns (eg. \"src/components/**/*.tsx\"), search code \
                 for keywords (eg. \"API endpoints\"), or answer questions about the codebase \
                 (eg. \"how do API endpoints work?\"). When calling this agent, specify the \
                 desired thoroughness level: \"quick\" for basic searches, \"medium\" for \
                 moderate exploration, or \"very thorough\" for comprehensive analysis across \
                 multiple locations and naming conventions."
                    .to_owned(),
            ),
            mode: AgentMode::Subagent,
            hidden: false,
            prompt: Some(EXPLORE_PROMPT.to_owned()),
            model: None,
            rules: assemble(
                false,
                vec![
                    // An allow-list, spelled the way upstream spells it: deny
                    // everything, then name what is left. `list` is in upstream's
                    // list and has no tool here.
                    //
                    // **`skill` is not in that list**, and its absence is the
                    // whole point of the deny above: upstream allows this agent
                    // seven permissions and `skill` is not one of them
                    // (`agent/agent.ts:200-211`), so a search agent cannot load a
                    // skill and act on instructions nobody in this session read.
                    // The blanket deny is what enforces that, which is why no
                    // `skill` rule appears below — a rule saying `deny` here would
                    // read as the *only* thing stopping it.
                    rule(ANY, ANY, Action::Deny),
                    rule("grep", ANY, Action::Allow),
                    rule("glob", ANY, Action::Allow),
                    rule("read", ANY, Action::Allow),
                    rule("webfetch", ANY, Action::Allow),
                    rule("websearch", ANY, Action::Allow),
                    // Upstream writes `bash: "allow"`, which on top of its
                    // `"*": "allow"` default changes nothing there and would
                    // change a great deal here: it would hand a subagent
                    // unattended shell access that no other ganja agent has.
                    // What the rule has to do is undo the blanket deny above, and
                    // `ask` does that while leaving the gate exactly where every
                    // other agent has it (deviation: explore-bash-asks).
                    rule("bash", ANY, Action::Ask),
                    // Upstream's `readonlyExternalDirectory`, which is the shared
                    // default put back after the blanket deny; without it a
                    // command naming somewhere outside the project would be
                    // refused outright rather than asked about.
                    rule(EXTERNAL_DIRECTORY, ANY, Action::Ask),
                ],
            ),
        },
    ];

    // The five roles `/team` routes its stages to. Nothing upstream has a
    // counterpart to any of them.
    //
    // Each is `Subagent` mode and not hidden, which is exactly what
    // `general` and `explore` above are: spawnable by a `task` call —
    // either as a disposable subagent or, with a `name`, as a teammate —
    // and never offered as a primary a person switches the session to.
    // A `/team` run is driven by whoever the session already is; these are
    // who it delegates to.
    //
    // They take the shared defaults and **no rule delta at all**, and that
    // is a decision rather than an omission: a role here is a prompt, not
    // a permission posture. `explore` earns its allow-list by being a
    // search agent that must not act, and `general` denies `todowrite`
    // because upstream does; an executor or a debugger that could not run
    // what every other agent can run would be a different agent from the
    // one its prompt describes. A project that wants a narrower one says
    // so in `.ganja/agents/`, whose same-named definition outranks the
    // builtin.
    //
    // Differing in nothing but those three strings, they are a table rather
    // than five literals: what a reader has to check about a new role is its
    // name, its offer line and its prompt, and a shape spelled once cannot
    // drift between them. The order is the roster's own and is asserted.
    agents.extend(
        [
            (
                ANALYST,
                "Turns a request into acceptance criteria somebody else can build against and \
                 check. Use before implementation when the scope is ambiguous, or to write down \
                 what \"done\" means.",
                ANALYST_PROMPT,
            ),
            (
                EXECUTOR,
                "Implements one scoped task and stops there. Use for a change whose shape is \
                 already decided, when the work should not grow past what was asked.",
                EXECUTOR_PROMPT,
            ),
            (
                VERIFIER,
                "Checks finished work against its acceptance criteria and reports a verdict per \
                 criterion with the evidence for it. Use to decide whether something is really \
                 done rather than to build it.",
                VERIFIER_PROMPT,
            ),
            (
                CRITIC,
                "Attacks a plan or a scope before it is built: unstated assumptions, unhandled \
                 cases, work that is bigger or smaller than the problem. Use to stress a plan, \
                 not to write one.",
                CRITIC_PROMPT,
            ),
            (
                DEBUGGER,
                "Isolates the root cause of a failing build, test or behavior and reports the \
                 evidence for it. Use when something is broken and why is not yet known.",
                DEBUGGER_PROMPT,
            ),
        ]
        .map(|(name, description, prompt)| Agent {
            name: name.to_owned(),
            description: Some(description.to_owned()),
            mode: AgentMode::Subagent,
            hidden: false,
            prompt: Some(prompt.to_owned()),
            model: None,
            rules: assemble(false, Vec::new()),
        }),
    );

    agents
}

/// The ruleset every agent starts from, ported from upstream's `defaults`.
///
/// Upstream's `"*": "allow"` is deliberately absent — see the module docs. So
/// are its whitelisted directories: they name upstream's temporary, skill and
/// reference directories, none of which this build has. The one directory
/// this build ever whitelists is a session's own memory root, and it is not
/// here but beside the agents that may reach it — [`memory_door`] is where
/// that is argued.
fn defaults() -> Vec<Rule> {
    vec![
        // Ganja already asks about this one by default; written out because
        // the agents below turn it off and on again, and a rule that is only
        // implied cannot be put back.
        rule(EXTERNAL_DIRECTORY, ANY, Action::Ask),
        // Upstream defaults `plan_exit` to deny and allows it on plan alone.
        // The deny must be written here, not merely implied: `plan_exit` is
        // not in the ask-by-default table, so an un-ruled call would fall
        // through `decide()` to *allow* and build could leave a mode it was
        // never in, unasked.
        rule("plan_exit", ANY, Action::Deny),
        // And the mirror, upstream's `plan_enter: "deny"` (agent.ts:127),
        // allowed on build alone (**D477**). Written out for exactly the
        // reason above: not hidden, just refused — and the fallthrough is
        // allow, so an implied deny would be no deny at all.
        rule("plan_enter", ANY, Action::Deny),
        // Upstream's comment: mirrors github/gitignore's Node.gitignore
        // patterns for .env files. Reading is otherwise free, so this is the
        // one place a read stops to ask.
        rule("read", "*.env", Action::Ask),
        rule("read", "*.env.*", Action::Ask),
        rule("read", "*.env.example", Action::Allow),
    ]
}

/// The rules that let a session keep its own memory, or none at all when the
/// config never asked for memory (**D478**, declared at
/// [`crate::instruction::memory_dir`]).
///
/// # Why any rule is needed
///
/// The memory root is outside the worktree, and this build asks about
/// everything out there — twice over: the tool's own permission (`write` and
/// `edit` are both in [`crate::permission::ASK_BY_DEFAULT`]) and the location
/// gate raised beside it. That posture is right for the general case and
/// exactly wrong for this one: the model is being *told*, in its own system
/// prompt, to maintain files in a directory it would then have to interrupt
/// the user about on every single fact it records.
///
/// # Why it is this narrow
///
/// Three rules, all patterned on the memory directory alone:
///
/// - the location gate for that directory, which is what `read` needs too —
///   reading is otherwise free, but the gate is raised for a read outside the
///   project like everything else;
/// - `write` and `edit` under it, which is what recording a fact is.
///
/// Nothing else outside the worktree moves: a `write` one directory up from
/// the memory root still asks exactly as it did before this feature existed.
/// The alternative shapes were a dedicated memory tool (a second way to write
/// a file, so that a permission rule could name it) and a blessed root on
/// `ToolCtx` (a path the tools trust, which is a gate outside the gate).
/// Both add a mechanism; this adds three rules to a table that already
/// expresses exactly this.
///
/// # Who gets it: the attended agents, and only them
///
/// The door is added per agent rather than to [`defaults`], and only for the
/// **primary** ones — the agents a person runs a session as. A subagent's
/// ruleset is its *own* agent's rules, not the parent's, so a door written
/// into the shared defaults would arrive at `general` and `explore` complete,
/// and inheritance would never get a chance to withhold it. Two things then
/// hold together: the location rule the parent carries still travels
/// ([`crate::permission::Permissions::inherited_by_subagent`] passes every
/// `external_directory` rule down), so a child may *read* the memory its
/// prompt showed it, and the two tool rules reach no child at all, so its
/// `write` falls back to asking. That is "a subagent inherits refusals and
/// never allows", made true of this feature by construction.
///
/// A config-defined agent in mode `all` gets no door either, for the same
/// reason: an agent that may be spawned may be spawned unwatched. Declaring
/// one `primary` is what asks for the door, and writing the rule out by hand
/// in a `permission` block is what asks for anything else.
///
/// # Where the directory comes from
///
/// The process's own working directory, resolved through the same
/// [`crate::instruction::memory_dir`] the prompt is composed from, so the door
/// and the block cannot name two different places. [`Registry::build`] has since
/// grown a `root` of its own (**D482**), and this deliberately still does not
/// read it: `memory_dir` resolves the project from whatever it is handed, so
/// the two agree — and the prompt half composes from the working directory, so
/// switching only this half would be the one way to make them disagree.
fn memory_door(config: &Config) -> Vec<Rule> {
    if !config.memory_enabled() {
        return Vec::new();
    }

    let Ok(cwd) = std::env::current_dir() else {
        tracing::warn!("the working directory is unreadable, so project memory has no door");
        return Vec::new();
    };
    let Some(directory) = crate::instruction::memory_dir(&cwd) else {
        return Vec::new();
    };

    memory_rules(&directory)
}

/// The three rules themselves, over a directory handed in.
///
/// Split from [`memory_door`] so that what they say can be asserted without a
/// test having to own the process's working directory to say it.
fn memory_rules(directory: &Path) -> Vec<Rule> {
    // Resolved and suffixed the way the gate spells a directory it is asked
    // about (`permission`'s own `covering`), because a rule written in another
    // spelling of the same place answers nothing. `*` spans separators here,
    // so one pattern covers the topic files as well as the index.
    let pattern = crate::permission::resolve(directory).join(ANY).to_string_lossy().into_owned();

    vec![
        rule(EXTERNAL_DIRECTORY, &pattern, Action::Allow),
        rule("write", &pattern, Action::Allow),
        rule("edit", &pattern, Action::Allow),
    ]
}

/// One rule, spelled out.
fn rule(permission: &str, pattern: &str, action: Action) -> Rule {
    Rule { permission: permission.to_owned(), pattern: pattern.to_owned(), action }
}

/// Applies `config.agent` to `agents`, upstream's overlay loop
/// (`agent.ts:267-294`).
///
/// A disabled agent is removed; a name nothing answers to becomes a new agent
/// in mode `all`; every field the config states replaces the one below it, and
/// its rules are **appended**, so a config can only ever have the last word.
///
/// Upstream iterates its config object in insertion order. Ganja's `agent` map
/// is sorted by name, so two config-defined agents are defined alphabetically
/// (deviation: config-agent-order) — which decides only their order in a
/// picker, and which of them a config with no `default_agent` would start on
/// if every builtin were disabled.
fn overlay(agents: &mut Vec<Agent>, config: &Config) {
    for (name, definition) in &config.agent {
        if definition.disable == Some(true) {
            agents.retain(|agent| agent.name != *name);
            continue;
        }

        let position = match agents.iter().position(|agent| agent.name == *name) {
            Some(position) => position,
            None => {
                agents.push(fresh(name, config));
                agents.len() - 1
            }
        };
        apply(&mut agents[position], definition);
    }
}

/// An agent a config named and no builtin answers to, in upstream's shape: mode
/// `all`, and nothing but the shared defaults and the user's own rules under it.
fn fresh(name: &str, config: &Config) -> Agent {
    let mut rules = defaults();
    rules.extend(config.permission.rules());

    Agent {
        name: name.to_owned(),
        description: None,
        mode: AgentMode::All,
        hidden: false,
        prompt: None,
        model: None,
        rules,
    }
}

/// Overlays one config definition onto a resolved agent.
fn apply(agent: &mut Agent, definition: &AgentConfig) {
    if let Some(model) = &definition.model {
        agent.model = Some(model.clone());
    }
    if let Some(prompt) = &definition.prompt {
        agent.prompt = Some(prompt.clone());
    }
    if let Some(description) = &definition.description {
        agent.description = Some(description.clone());
    }
    if let Some(mode) = definition.mode {
        agent.mode = mode;
    }
    if let Some(hidden) = definition.hidden {
        agent.hidden = hidden;
    }

    // Appended rather than merged: upstream's `Permission.merge` is
    // concatenation and its order is its precedence, so a config's rule about
    // a call the agent already had a rule about simply comes later.
    agent.rules.extend(definition.permission.rules());
}

/// The file tier: every `agents/*.md` under ganja's own two homes, applied in
/// discovery order (**D482**).
///
/// Global first and project second, so a definition in the checkout wins the
/// name — the order every layered thing here resolves in. Applying a file is
/// the same act as applying a config definition ([`apply`]) plus whatever its
/// `tools:` line compiles to, which is what makes "a file agent is an agent"
/// true rather than approximately true: it joins the same vector, so the Tab
/// cycle, `/agents` and the task roster all see it by the rules they already
/// have.
fn file_tier(agents: &mut Vec<Agent>, config: &Config, directories: &[PathBuf]) {
    for directory in directories {
        for definition in definitions_in(directory) {
            if config.agent.contains_key(&definition.name) {
                // Principle: the curated config is the last word, and a
                // collision it wins is worth saying out loud — the file is
                // still applied, and `overlay` then runs over the top of it.
                tracing::info!(
                    agent = definition.name.as_str(),
                    file = %definition.source.display(),
                    "an `agent` entry in the configuration overrides this definition file"
                );
            }
            apply_definition(agents, &definition, config);
        }
    }
}

/// Ganja's own two homes, in precedence order: `agents/` under
/// [`crate::config::config_home`], then `<project root>/.ganja/agents`.
///
/// [`crate::config::home_dirs`] is the walk, shared with the skills and
/// commands rosters — including what it does when the two homes turn out to be
/// one directory. The global half being spelled through that function is
/// that function's own reason: `GANJA_CONFIG_HOME` — or a `~/.ganja` — moves
/// this build's config, its global `AGENTS.md`, its skills and its agents
/// together, and a session cannot end up reading one of them out of a
/// directory the others are not in.
fn definition_dirs(root: &Path) -> Vec<PathBuf> {
    crate::config::home_dirs(root, AGENTS_SUBDIR)
}

/// The definitions one directory holds, in file-name order, with the
/// unreadable and the malformed skipped by name.
///
/// Flat: a subdirectory is not descended into, so Claude's namespaced
/// `agents/team/reviewer.md` is a recorded follow-up rather than a silent
/// half-feature. Order is the file name's, so what a directory offers does not
/// depend on the order the filesystem happens to list it in.
fn definitions_in(directory: &Path) -> Vec<Definition> {
    let Ok(entries) = fs::read_dir(directory) else {
        // A home that holds no `agents/` is the normal case, not a failure.
        return Vec::new();
    };

    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
                && path.is_file()
        })
        .collect();
    files.sort();

    let mut found: Vec<Definition> = Vec::new();
    for file in files {
        let text = match fs::read_to_string(&file) {
            Ok(text) => text,
            Err(error) => {
                tracing::warn!(file = %file.display(), %error, "an agent definition file could not be read; skipping it");
                continue;
            }
        };
        match Definition::parse(&file, &text) {
            Ok(definition) => {
                if found.iter().any(|other| other.name == definition.name) {
                    tracing::warn!(
                        agent = definition.name.as_str(),
                        file = %file.display(),
                        "another file in this directory already defines that agent; skipping it"
                    );
                    continue;
                }
                found.push(definition);
            }
            Err(reason) => tracing::warn!(
                file = %file.display(),
                reason = reason.as_str(),
                "skipping an agent definition file"
            ),
        }
    }

    found
}

/// Applies one definition to the roster, over whatever already answers to its
/// name.
fn apply_definition(agents: &mut Vec<Agent>, definition: &Definition, config: &Config) {
    let position = match agents.iter().position(|agent| agent.name == definition.name) {
        Some(position) => position,
        None => {
            agents.push(fresh(&definition.name, config));
            agents.len() - 1
        }
    };
    apply(&mut agents[position], &definition.config);

    // After the rules `fresh` (or a builtin) already carried, which puts an
    // agent's own tool roster over the config's *global* `permission` block
    // and under its `agent.<name>` one — the specific answer beating the
    // general one, in both directions.
    if let Some(tools) = &definition.tools {
        agents[position].rules.extend(tool_rules(tools, &definition.name));
    }
}

/// What one `agents/*.md` file says.
///
/// The fields land in an [`AgentConfig`] rather than in a shape of their own,
/// so that a file and a config entry are merged by the same [`apply`] — there
/// is one field-by-field overlay in this module, and both tiers go through it.
#[derive(Clone, Debug, PartialEq)]
struct Definition {
    /// What the agent is called: the frontmatter's `name`, or the file stem.
    name: String,
    /// Where it was read from, for the warnings and the collision log.
    source: PathBuf,
    /// The fields a config entry could have stated.
    config: AgentConfig,
    /// The `tools:` line, split into names — `None` when the file states none,
    /// which is what leaves an agent with today's whole roster.
    tools: Option<Vec<String>>,
}

impl Definition {
    /// The definition `text` describes, or the reason it was skipped.
    ///
    /// Every refusal is a skip-with-warning rather than an error that stops a
    /// session: a file somebody is halfway through writing must not be able to
    /// keep ganja from starting.
    fn parse(file: &Path, text: &str) -> Result<Self, String> {
        // The same reader a `SKILL.md` is read with
        // ([`ganja_tool::frontmatter`]): an agent file and a skill file open
        // with the same fence, and a person who wrote one for another agent
        // wrote it once.
        let (frontmatter, body) = split(text).unwrap_or(("", text));
        let fields = fields(frontmatter);

        let stem =
            file.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
        let name = fields
            .get("name")
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or(stem);
        if name.is_empty() {
            return Err("it names no agent and its file name is empty".to_owned());
        }
        if name.contains('/') || name.contains('\\') {
            return Err(format!(
                "`name: {name}` holds a path separator, and an agent is named, not located"
            ));
        }

        let model = fields
            .get("model")
            .map(|model| model.trim().to_owned())
            .filter(|model| !model.is_empty());
        if let Some(model) = &model
            && !model.contains('/')
        {
            return Err(format!(
                "`model: {model}` is not a full `provider/model` id (for example \
                 `anthropic/claude-sonnet-4-5`); this build resolves no model aliases"
            ));
        }

        let mode = match fields.get("mode").map(|mode| mode.trim()) {
            None | Some("") => None,
            Some(mode) => Some(
                serde_json::from_value::<AgentMode>(serde_json::Value::String(mode.to_owned()))
                    .map_err(|_| {
                        format!("`mode: {mode}` is not one of `primary`, `subagent`, `all`")
                    })?,
            ),
        };
        let hidden = match fields.get("hidden").map(|hidden| hidden.trim()) {
            None | Some("") => None,
            Some(hidden) => Some(
                hidden
                    .parse::<bool>()
                    .map_err(|_| format!("`hidden: {hidden}` is not `true` or `false`"))?,
            ),
        };

        Ok(Self {
            name,
            source: file.to_owned(),
            config: AgentConfig {
                model,
                prompt: Some(body.trim().to_owned()).filter(|body| !body.is_empty()),
                description: fields
                    .get("description")
                    .map(|description| description.trim().to_owned())
                    .filter(|description| !description.is_empty()),
                mode,
                hidden,
                // A file cannot disable itself — deleting it is that — and it
                // carries no `permission` block: `tools:` is the vocabulary
                // this shape offers, and a second, richer one in the same file
                // would be two ways to say the same thing.
                disable: None,
                permission: crate::permission::PermissionConfig::default(),
            },
            tools: fields.get("tools").map(|tools| tool_names(tools)),
        })
    }
}

/// The tool names a `tools:` value lists.
///
/// Comma- and/or whitespace-separated, which is how Claude's own files are
/// written, and tolerant of the inline-list spelling (`[read, grep]`) an
/// editor's YAML autocompletion produces.
fn tool_names(value: &str) -> Vec<String> {
    value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split([',', ' ', '\t', '\n'])
        .map(|name| name.trim().trim_matches(['"', '\'']).to_owned())
        .filter(|name| !name.is_empty())
        .collect()
}

/// A `tools:` list, compiled to rules (**the D482 heart**).
///
/// # The divergence
///
/// Claude restricts an agent by handing the model a **smaller schema**: a tool
/// left off the list is not offered at all. This build has one mechanism for
/// what an agent may do — the permission engine — and one posture about
/// refusals: *refused, never hidden*. So the list becomes rules. An unlisted
/// tool stays in the schema, the model may call it, and the call comes back as
/// the same refusal text a denied `edit` comes back as, which the model reads
/// and acts on. The information the model loses under Claude's shape (that the
/// tool exists at all) it keeps here, and the outcome — the call does not run
/// — is identical.
///
/// # The spelling
///
/// A deny per *other* name, not one blanket `"*": "deny"` with exceptions
/// carved back out of it. The blanket form is what [`EXPLORE`] uses and it is
/// wrong here, twice over: it would also close [`EXTERNAL_DIRECTORY`] (turning
/// a question about working outside the project into a refusal) and it would
/// close whatever door the agent underneath already had — a `tools:` line on a
/// file named `build.md` would seal `plan_enter`, which is the one thing that
/// agent has. Naming the others leaves both of those exactly where they were.
///
/// [`CONVERSATION_TOOLS`] are in neither half: never denied, and never allowed
/// either. The MCP namespace *is* denied, by glob, because its names are not
/// known until a server has been dialled — and the allows are emitted last, so
/// a list naming one MCP tool by hand still outranks that glob.
fn tool_rules(listed: &[String], agent: &str) -> Vec<Rule> {
    let mut rules = Vec::new();

    for name in TOOL_NAMES {
        if CONVERSATION_TOOLS.contains(name) || listed.iter().any(|tool| tool == name) {
            continue;
        }
        rules.push(rule(name, ANY, Action::Deny));
    }
    rules.push(rule(&format!("{MCP_PREFIX}{ANY}"), ANY, Action::Deny));

    for name in listed {
        if CONVERSATION_TOOLS.contains(&name.as_str()) {
            tracing::info!(
                agent,
                tool = name.as_str(),
                "a `tools:` list neither opens nor closes this one; it is left as it was"
            );
            continue;
        }
        if !TOOL_NAMES.contains(&name.as_str()) && !name.starts_with(MCP_PREFIX) {
            // Emitted anyway: a rule about a tool that does not exist decides
            // nothing. The warning is because the likeliest cause is a typo,
            // and a typo silently *removes* a tool the file meant to keep.
            tracing::warn!(
                agent,
                tool = name.as_str(),
                "a `tools:` list names no tool this build registers"
            );
        }
        rules.push(rule(name, ANY, Action::Allow));
    }

    rules
}

/// Which agent a session starts on.
///
/// The `--agent` flag outranks `default_agent`, which outranks the first
/// selectable agent there is. A name that was asked for and cannot be honoured
/// is an error rather than a fallback: silently starting on a different agent
/// than the one named would be a different session than the one asked for.
fn resolve_default(agents: &[Agent], config: &Config) -> Result<String, AgentError> {
    let named = config.overrides.agent.as_deref().or(config.default_agent.as_deref());

    let Some(name) = named else {
        return agents
            .iter()
            .find(|agent| agent.selectable())
            .map(|agent| agent.name.clone())
            .ok_or(AgentError::NoneVisible);
    };

    let Some(agent) = agents.iter().find(|agent| agent.name == name) else {
        return Err(AgentError::Unknown { name: name.to_owned() });
    };
    if agent.mode == AgentMode::Subagent {
        return Err(AgentError::Subagent { name: name.to_owned() });
    }
    if agent.hidden {
        return Err(AgentError::Hidden { name: name.to_owned() });
    }

    Ok(agent.name.clone())
}

#[cfg(test)]
#[path = "agent_tests.rs"]
mod tests;
