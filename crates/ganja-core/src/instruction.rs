//! The system prompt: what the model is told before it is told anything else.
//!
//! Spec: upstream `packages/opencode/src/session/instruction.ts` and
//! `packages/opencode/src/session/system.ts`.
//!
//! Three things are concatenated, joined by a bare newline, in this order:
//!
//! 1. the base prompt for the model's family, ported verbatim from upstream's
//!    `session/prompt/*.txt`;
//! 2. an environment block — the model's own name, where it is working, and
//!    what day it is;
//! 3. every instruction file that applies, each rendered as
//!    `Instructions from: {path}` followed by its contents;
//! 4. the skills this session can load, when there are any — last, where
//!    upstream puts them (`session/prompt.ts:1257-1268`).
//!
//! Which files apply is [`paths`], and the order is upstream's: the one global
//! file, then the project's own, then whatever `instructions` in the config
//! named. A file appears once however many routes reach it, at the position the
//! first route put it.
//!
//! # Divergences
//!
//! - **D22** — the prompt is composed when the model moves, where upstream
//!   rebuilds it for every request. Both halves are recomposed by the engine
//!   after anything that changes the active model — `Engine::with_base_for_model`
//!   for the base, `Engine::with_environment` for this one — because both are
//!   written against a model: the base is its family's, and the environment
//!   block states its name as fact. What is left composed-once is what does not
//!   depend on the model: the working directory, captured at engine
//!   construction, and the date, so a session that outlives midnight tells the
//!   model yesterday's.
//! - **D23** — the environment block's model line names the model the way the
//!   provider is asked for it. Upstream additionally spells the `provider/model`
//!   pair, which is not available where this is composed.
//! - **D24** — the date is UTC. Upstream renders the machine's local date;
//!   there is no date library in this workspace, and reaching `localtime_r`
//!   through `libc` for one line — unsafe, and unix-only — buys less than it
//!   costs.
//! - **D25** — instruction globs do not consult ignore files, but do keep the
//!   hidden-file rule; see [`glob`].
//! - **D2** — `http(s)` entries in `instructions` are skipped with a warning
//!   rather than fetched. `skills.urls` takes the same posture, in the same
//!   words, at [`skill_roots`].
//! - **`nothing-foreign-is-discovered`** — the skills half of this prompt lists
//!   what is under ganja's own two homes plus whatever `skills.paths` named,
//!   where upstream also walks two of another tool's directories and their
//!   walk-ups. A **standing user ruling**, recorded in full where the scanning
//!   happens (`tool::skill`'s module docs); named here because this is the file
//!   that decides what a model is *told* it can load, and a reader who only
//!   ever opens this one would otherwise find no trace of the reason.
//! - **D478** — a session whose config asks for it carries its project's own
//!   memory: an index and the topic files beside it, kept outside the
//!   repository and maintained by the model through the ordinary file tools.
//!   Where it lives, what the block says and why the text is ganja's own
//!   rather than ported are all declared at [`memory_dir`].
//! - **D480** — instruction files **below** the project root are walked in
//!   lazily as the session touches files under them; the walker, the carrier
//!   and the honest alternatives are all declared at [`nested_files`].
//! - **`skills-block-omitted-when-there-are-none`** — upstream emits the
//!   heading and the sentence "No skills are currently available." for a
//!   session that has none (`skill/index.ts:321-323`). Here the whole block is
//!   left out: every token of a system prompt is a token the model reads on
//!   every request, and a paragraph announcing an empty list is a paragraph
//!   about nothing. A session **with** skills gets upstream's block verbatim.

use std::{
    collections::BTreeSet,
    fmt::Write as _,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use etcetera::base_strategy::{BaseStrategy as _, Xdg};

use crate::{config::Config, project::Project, tool::skill};

/// Base prompt for Anthropic's models, ported verbatim (MIT; see
/// `THIRD_PARTY_NOTICES.md`).
const ANTHROPIC: &str = include_str!("prompt/anthropic.txt");

/// Base prompt for OpenAI's models, ported verbatim.
const GPT: &str = include_str!("prompt/gpt.txt");

/// Base prompt for everything else, ported verbatim.
const DEFAULT: &str = include_str!("prompt/default.txt");

/// The global instruction file, under the same directory the global config
/// lives in.
const GLOBAL: &str = "AGENTS.md";

/// Where Claude Code keeps its own global instructions, which upstream reads as
/// a fallback when there is no `AGENTS.md`.
const CLAUDE_GLOBAL: [&str; 2] = [".claude", "CLAUDE.md"];

/// Project instruction file names, most preferred first. The **first name with
/// any match at all** wins outright, so a checkout carrying both `AGENTS.md`
/// and `CLAUDE.md` sends the first and never mixes the two.
const PROJECT: [&str; 3] = ["AGENTS.md", "CLAUDE.md", "CONTEXT.md"];

/// What every instruction file is introduced with.
const HEADER: &str = "Instructions from: ";

/// The first line of the skills block, shared with [`suffix_measure`] so the
/// splitter and the composer can never disagree about where the block starts.
const SKILLS_HEAD: &str =
    "Skills provide specialized instructions and workflows for specific tasks.";

/// Directory a project's memory lives in, under its own data directory.
const MEMORY_DIR: &str = "memory";

/// The index inside it: one line per topic file beside it.
const MEMORY_INDEX: &str = "MEMORY.md";

/// The first line of the memory section, shared with [`suffix_measure`] for
/// [`SKILLS_HEAD`]'s reason: the splitter and the composer must not be able to
/// disagree about where a block starts.
const MEMORY_HEAD: &str =
    "Project memory: durable facts about this project, kept outside the repository.";

/// Days in each month of a non-leap year, for [`civil_date`].
const MONTH_LENGTHS: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/// Month names as `Date.prototype.toDateString` spells them.
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Weekday names as `Date.prototype.toDateString` spells them, starting at
/// Thursday because that is what 1970-01-01 was.
const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];

/// The base prompt for `model_id`.
///
/// Upstream selects by substring on the model's own identifier, first match
/// wins, and this ports the three arms whose providers ganja has. The
/// comparison is case-sensitive, as upstream's is.
///
/// This is the half an agent replaces, and it is not composed once for the
/// session: which arm answers depends on the model's family, so
/// `Engine::with_base_for_model` calls this again whenever the active model
/// moves, and a session that switches across families stops running the new
/// model under the old one's instructions.
#[must_use]
pub fn base_prompt(model_id: &str) -> &'static str {
    if model_id.contains("gpt") {
        GPT
    } else if model_id.contains("claude") {
        ANTHROPIC
    } else {
        DEFAULT
    }
}

/// The whole system prompt for a session working in `cwd` and asking
/// `model_id`.
///
/// [`None`] only when the composition is empty, which it cannot be while a base
/// prompt is compiled in — the type matches
/// [`ChatRequest::system`](crate::provider::ChatRequest::system) so a caller
/// does not have to know that.
#[must_use]
pub fn system_prompt(config: &Config, cwd: &Path, model_id: &str) -> Option<String> {
    compose(
        &global_files(),
        &skill_roots(config, cwd),
        config,
        cwd,
        model_id,
    )
}

/// Where this session's skills are looked for: ganja's own two homes, then
/// whatever `skills.paths` named.
///
/// The one value both halves of the feature are built from. The system prompt
/// lists what [`skill::discover`] finds here, and the same roots handed to
/// [`skill::SkillTool::over`] are what a `skill` call loads from — which is how
/// the list a model is offered and the list it can actually load stay the same
/// list.
///
/// The two defaults are [`crate::config::default_skill_dirs`] and they are the
/// **whole** of what is scanned unasked: a config that says nothing about
/// skills reaches `<XDG config>/ganja/skills` and `<project root>/.ganja/skills`
/// and no third place. Nothing foreign is read — no `~/.claude`, no
/// `~/.agents`, neither name walked up to, and no bare `skill/` or `skills/` at
/// a project root. That is the module's `nothing-foreign-is-discovered`, whose
/// full text and provenance live where the scanning happens
/// (`tool::skill`'s module docs).
///
/// Configured paths go **after** the defaults, so a directory somebody wrote
/// down outranks one that was there by convention.
///
/// `skills.urls` is named in a warning and not fetched, for **D2**'s reason.
#[must_use]
pub fn skill_roots(config: &Config, cwd: &Path) -> skill::Roots {
    for url in config.skill_urls() {
        tracing::warn!(
            url = url.as_str(),
            "remote skills are not fetched; point skills.paths at a directory instead"
        );
    }

    skill::Roots::none()
        .with_paths(crate::config::default_skill_dirs(cwd))
        .with_paths(config.skill_paths(cwd))
}

/// The half of the system prompt no agent replaces: the environment block and
/// the instruction files, true of every agent working in `cwd`.
///
/// This is what `Engine::with_system_parts` takes as `suffix` — an agent
/// switch swaps the base-or-agent half and keeps this one. It is not composed
/// once for the session: the environment block states the model as fact, so
/// `Engine::with_environment` calls this again whenever the active model
/// moves, and a session that switches model mid-conversation stops telling the
/// new model it is the old one.
///
/// Never [`None`] in practice — the environment block always says something —
/// but typed to match its consumer.
#[must_use]
pub fn suffix(config: &Config, cwd: &Path, model_id: &str) -> Option<String> {
    suffix_from(
        &global_files(),
        &skill_roots(config, cwd),
        config,
        cwd,
        model_id,
    )
}

/// Puts the two halves of a system prompt together: the half an agent replaces,
/// then the half none of them do.
///
/// Joined by a bare newline, as upstream's `session/llm/request.ts` joins them,
/// and [`None`] only when neither half says anything — which is the engine
/// nobody configured, and which every scripted and golden run depends on.
#[must_use]
pub(crate) fn joined(head: Option<&str>, suffix: Option<&str>) -> Option<String> {
    match (head, suffix) {
        (None, None) => None,
        (Some(only), None) | (None, Some(only)) => Some(only.to_owned()),
        (Some(head), Some(suffix)) => Some(format!("{head}\n{suffix}")),
    }
}

/// [`system_prompt`], with the global instruction candidates handed in.
///
/// The split is what lets the tests below prove the composition without the
/// machine running them contributing an `AGENTS.md` of its own — and the global
/// candidates really are an input to discovery rather than something it knows.
fn compose(
    global: &[PathBuf],
    roots: &skill::Roots,
    config: &Config,
    cwd: &Path,
    model_id: &str,
) -> Option<String> {
    let base = base_prompt(model_id);
    let tail = suffix_from(global, roots, config, cwd, model_id).unwrap_or_default();

    let mut prompt = String::with_capacity(base.len() + tail.len() + 1);
    prompt.push_str(base);
    prompt.push('\n');
    prompt.push_str(&tail);

    (!prompt.is_empty()).then_some(prompt)
}

/// [`suffix`], with the global instruction candidates and the skill roots
/// handed in — the same test seam [`compose`] has, for the same reason.
///
/// The roots are an input for a second reason the instruction candidates share:
/// they name directories on the machine running this, so a test that composed a
/// prompt without being able to say *which* directories would be a test whose
/// answer depended on whose laptop it ran on.
fn suffix_from(
    global: &[PathBuf],
    roots: &skill::Roots,
    config: &Config,
    cwd: &Path,
    model_id: &str,
) -> Option<String> {
    let mut prompt = environment(cwd, model_id);

    for path in discover(global, config, cwd) {
        // A file that cannot be read contributes nothing, exactly as an empty
        // one does: upstream reads with a catch-all that yields "" and then
        // drops empty entries. Naming an unreadable file in the prompt would
        // tell the model about instructions it was never given.
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        if content.is_empty() {
            continue;
        }

        prompt.push('\n');
        prompt.push_str(HEADER);
        prompt.push_str(&path.display().to_string());
        prompt.push('\n');
        prompt.push_str(&content);
    }

    // After the instruction files and before the skills, which is where it
    // belongs on both sides: memory is instruction text — facts the model is
    // told, in the same voice as an `AGENTS.md` — and upstream's rule that the
    // skills go last is not this feature's to bend (**D478**, at
    // [`memory_dir`]).
    if config.memory_enabled()
        && let Some(directory) = memory_dir(cwd)
    {
        prompt.push_str(&memory_section(&directory));
    }

    // Last, as upstream orders it, and only when there is something to list;
    // see the module's `skills-block-omitted-when-there-are-none`.
    if let Some(block) = skills_block(&skill::discover(roots)) {
        prompt.push('\n');
        prompt.push_str(&block);
    }

    (!prompt.is_empty()).then_some(prompt)
}

/// A composed suffix taken back apart at the seams [`suffix_from`] joined it
/// at, as character counts per category — what `Engine::context_breakdown`
/// prices the `/context` categories from (P14 **D470**).
///
/// The three parts partition the string: their sum is always the whole
/// suffix's character count, which is what lets the breakdown's grid total
/// equal its accessor total by construction rather than by luck.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SuffixMeasure {
    /// The environment block: everything before the first instruction file.
    pub environment: usize,
    /// The instruction files, headers included — and the memory section with
    /// them, which is what makes `/context` price memory as instructions
    /// rather than as weight nobody can see (**D478**).
    pub instructions: usize,
    /// The skills block, when one was composed.
    pub skills: usize,
}

/// Measures the composed `suffix` by the markers [`suffix_from`] wrote into
/// it: the first [`HEADER`] line opens the instruction files, [`MEMORY_HEAD`]
/// opens the memory section beside them, and [`SKILLS_HEAD`] opens the skills
/// block, always last.
///
/// The **earliest** of the three openers ends the environment block, rather
/// than the first one that exists: the memory section carries a [`HEADER`]
/// line of its own for the index file it quotes, so a session with memory on
/// and no instruction files of its own would otherwise have its memory
/// heading counted as environment.
///
/// A measurement of the string the request path already holds — never a
/// second composition, which could disagree with what the wire carries — so
/// this is an estimate's split, not a parse: an instruction file that itself
/// contains one of the markers shifts characters between neighbouring
/// categories and changes no total.
pub(crate) fn suffix_measure(suffix: &str) -> SuffixMeasure {
    let files = suffix.find(&format!("\n{HEADER}"));
    let memory = suffix.find(&format!("\n{MEMORY_HEAD}"));
    let skills = suffix.find(&format!("\n{SKILLS_HEAD}"));

    let environment_end = [files, memory, skills]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(suffix.len());
    let instructions_end = skills.unwrap_or(suffix.len());

    SuffixMeasure {
        environment: suffix[..environment_end].chars().count(),
        instructions: suffix[environment_end..instructions_end].chars().count(),
        skills: suffix[instructions_end..].chars().count(),
    }
}

/// Upper bound on the bytes one nested instruction file may put in a prompt.
///
/// [`ganja_tool::truncate::MAX_CHARS`], the budget every one-shot tool result
/// in this build is already held to, rather than a number invented here: the
/// question "how much text may one file drop into the context window" is the
/// same question, and answering it twice with two constants is how the two
/// answers start to disagree.
const NESTED_MAX: usize = crate::tool::truncate::MAX_CHARS;

/// Every instruction file **below** `root` that this session's touched paths
/// have walked past, ordered shallowest-first — closest-last, the same way the
/// up-walk tier stacks at [`discover`] (**D480**).
///
/// # What a touch is, and why the walk is lazy
///
/// `touched` is the file paths the session has actually **opened or written**:
/// `read`, `edit` and `write` calls that completed. A `glob` or `grep` listing
/// is deliberately not one — it names files nobody asked to work in, and a
/// single unanchored glob over a vendored tree would otherwise walk that whole
/// tree's `AGENTS.md` files into the prompt. Where the signal is read from is
/// [`crate::session::touched_files`]'s to state; this function only takes the
/// paths.
///
/// Laziness is the whole design. The rejected alternative — glob `**/AGENTS.md`
/// at startup and concatenate — costs unbounded prompt weight in a monorepo and
/// reads a tree the user never asked about, which is the plan's third principle
/// stated as a defect.
///
/// # The rules the walk keeps, and the one it bends
///
/// - **Below the root only.** The walk climbs from a touched file's own
///   directory and stops *before* `root`: the root's own file, and every file
///   between `cwd` and the root, is the up-walk tier's already
///   ([`find_up`]), and a directory on `cwd`'s own ancestor chain is skipped
///   for the same reason. A touched path outside the project contributes
///   nothing at all.
/// - **The project vocabulary.** [`PROJECT`] names, first existing name wins —
///   so a subdirectory carrying only `CLAUDE.md` is honoured.
/// - **The bent rule**: upstream's tier rule is that the first *name* with any
///   match takes the whole tier, so a checkout holding both never sends both.
///   Here the choice is made **per directory** instead. A directory below the
///   root is its own scope, reached because work happened inside it; a
///   tier-wide rule would let one subtree's `AGENTS.md` silently mute another
///   subtree that spells its file `CLAUDE.md`, which is a rule about a
///   checkout's style applied to a question about a subtree's contents.
///
/// Deduplicated by directory, so however many files under `sub/` are touched,
/// `sub/AGENTS.md` is named once.
pub(crate) fn nested_files(root: &Path, cwd: &Path, touched: &[PathBuf]) -> Vec<PathBuf> {
    let root = resolved(root);
    // The directories the up-walk tier already reached. Canonical, because
    // that is what the walk below compares against.
    let covered: BTreeSet<PathBuf> = resolved(cwd)
        .ancestors()
        .map(Path::to_path_buf)
        .collect::<BTreeSet<_>>();

    let mut directories = BTreeSet::new();
    for path in touched {
        // The *parent* is canonicalized rather than the file: a `write` names
        // a path that may not exist yet, and a canonicalization that fails
        // would leave an uncanonical path that never recognises the root.
        let Some(parent) = path.parent() else {
            continue;
        };
        let parent = resolved(parent);
        if !parent.starts_with(&root) {
            continue;
        }

        for directory in parent.ancestors() {
            if directory == root {
                break;
            }
            if !covered.contains(directory) {
                directories.insert(directory.to_path_buf());
            }
        }
    }

    let mut found: Vec<PathBuf> = directories
        .iter()
        .filter_map(|directory| {
            PROJECT
                .iter()
                .map(|name| directory.join(name))
                .find(|candidate| candidate.is_file())
        })
        .collect();

    // Depth first, path second: a deeper file is more specific and is read
    // last, and the tie-break keeps two unrelated subtrees in one stable order
    // rather than in whatever order they were touched.
    found.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });

    found
}

/// What [`nested_files`] adds to the next request's system prompt: each file
/// under the [`HEADER`] the up-walk tier already uses, clamped, closest-last.
///
/// Empty when nothing was found, which is every session that never touched a
/// file below a directory carrying instructions of its own.
///
/// # The carrier, and why it is this one
///
/// This is appended to the **request's** system prompt at assembly time, and
/// is not a part of the transcript. Two pins decide that:
///
/// - The loaded set must be **transcript-derived**, never a side map that can
///   drift. It is derived here in the strongest possible sense: there is no
///   set. Every request recomputes the walk from the tool calls the transcript
///   carries, so a resumed session rebuilds it exactly, a revert that drops
///   the touch drops the injection with it, and a re-touch brings it back.
/// - "The event stream is complete" — a frontend applying every event holds
///   what the next request will carry. It holds the **reference** here, which
///   is the shape this build already uses for content resolved at send time:
///   an `@` mention's stored part carries a path and the request carries the
///   bytes (`PartBody::File`), and the whole `AGENTS.md` family has never been
///   a transcript part at all — instruction files live in the system prompt,
///   and this is an instruction file.
///
/// A synthetic transcript part was the alternative. It would have put
/// instruction text into the conversation, where `/context` prices it as
/// conversation rather than as instructions — the honesty AC the feature is
/// held to — and where a frontend would render the file's contents as
/// something somebody said.
pub(crate) fn nested_suffix(root: &Path, cwd: &Path, touched: &[PathBuf]) -> String {
    let root = resolved(root);

    let mut block = String::new();
    for path in nested_files(&root, cwd, touched) {
        // Unreadable and empty both contribute nothing, for `suffix_from`'s
        // reason: naming a file the model was never given is worse than
        // silence.
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        if content.is_empty() {
            continue;
        }

        let named = relative(&root, &path);
        let (body, clamped) = clamped(&content, &named);
        // File names only, never content: this line exists so a prompt-weight
        // surprise is diagnosable from a `-v` log, not so a log becomes a
        // second copy of the repository.
        tracing::debug!(
            file = named.as_str(),
            clamped,
            "a nested instruction file joined the prompt"
        );

        block.push('\n');
        block.push_str(HEADER);
        block.push_str(&named);
        block.push('\n');
        block.push_str(&body);
    }

    block
}

/// `path` named the way a nested [`HEADER`] spells it: relative to the project
/// root, always with `/`.
///
/// The up-walk tier spells its own files absolutely, because those paths are
/// whatever route reached them. These are generated from the root every
/// request, so naming them relatively keeps one repository's prompt the same
/// text on every machine it is checked out on — and keeps the header short
/// enough to read.
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// `content` held to [`NESTED_MAX`], and whether anything was cut.
///
/// The marker is `truncate::clamp`'s own wording — `...N bytes truncated...` —
/// and deliberately **not** that function: a clamp there spills the full text
/// to a file so the model can read it back, and a system prompt is recomposed
/// on every single request, so reusing it would write one identical overflow
/// file per request forever. The rest is already on disk at a path the header
/// just named, so this points at that instead, which is the more honest
/// answer anyway.
fn clamped(content: &str, named: &str) -> (String, bool) {
    if content.len() <= NESTED_MAX {
        return (content.to_owned(), false);
    }

    // Back off to a character boundary rather than slicing blind: a budget in
    // bytes must not be allowed to split a code point.
    let mut end = NESTED_MAX;
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    let removed = content.len() - end;

    (
        format!(
            "{}\n\n...{removed} bytes truncated...\nRead {named} for the rest.",
            &content[..end]
        ),
        true,
    )
}

/// Where this project's memory lives — `<data dir>/memory` — or nothing when
/// this machine has no data directory to hang it off (**D478**).
///
/// # What the feature is
///
/// A session whose config says `memory: true` carries a small store of
/// durable facts about the project it is working in: `MEMORY.md` is the
/// index, and each topic is a file of its own beside it. The whole of it is
/// read into every request's system prompt by [`memory_section`], and the
/// whole of it is maintained by the **model** through the ordinary `read`,
/// `write` and `edit` tools — there is no memory tool, and deliberately not:
/// a file the model already knows how to write is one fewer thing to teach
/// it, and a topic file is nothing but a file.
///
/// # Where it lives, and why not in the repository
///
/// [`Project::data_dir`], the same per-project home the permission answers
/// and the session store already use — ganja's analogue of Claude Code's
/// `~/.claude/projects/<hash>/memory/`. Inside the worktree was the obvious
/// alternative and is wrong: memory is one person's accumulated notes about a
/// checkout, and a checkout is shared. Nothing is created here; the first
/// write creates what it needs, so a session that records nothing leaves no
/// empty directory behind.
///
/// # The block is ganja's own text
///
/// Claude Code's real memory prompt is not public documentation, so the
/// upkeep instructions in [`memory_section`] are **synthesized, not ported** —
/// the same posture D477 took for the plan door upstream describes and does
/// not implement. What is ported is the *shape* the feature has from the
/// outside: an index, topic files beside it, and a model that keeps them.
///
/// # Off unless asked
///
/// [`Config::memory_enabled`] is false unless a config says otherwise, where
/// Claude's is on. Two reasons, both recorded at the config key: a session
/// with memory on writes files **outside** the worktree, and it carries a
/// standing block of prompt weight for as long as it runs. Neither should
/// arrive because somebody opened a checkout.
///
/// # The door
///
/// A write under this directory is a write outside the project, which the
/// permission engine asks about by default — twice, once for the tool and
/// once for the location gate. `crate::agent`'s shared defaults open exactly
/// this directory and nothing else when memory is on; the reasoning, and what
/// a subagent does and does not inherit of it, is written there.
#[must_use]
pub fn memory_dir(cwd: &Path) -> Option<PathBuf> {
    match Project::resolve(cwd).data_dir() {
        Ok(directory) => Some(directory.join(MEMORY_DIR)),
        Err(error) => {
            tracing::warn!(
                %error,
                "project memory has nowhere to live on this machine and is off for this session"
            );
            None
        }
    }
}

/// How the model is told to keep the memory it was just shown.
///
/// Four questions, in the order somebody would ask them: how to write one
/// down, what is worth writing down, what is not, and the one thing that
/// never is. The last is the guard for this feature's own worst outcome — a
/// credential recorded into a file that outlives the session that saw it —
/// and it is stated as a prohibition rather than as advice because a model
/// reading advice weighs it against whatever else it was told.
const MEMORY_UPKEEP: &str = "\
Keeping it: record a fact by writing a file named for its one topic in that \
directory, then adding a one-line entry naming the file to MEMORY.md. Correct \
a fact in place; delete a file that has stopped being true, and drop its line \
from the index with it.
Worth recording: a correction the user made, a preference they stated as \
standing, a fact about this project that its own files do not say.
Not worth recording: anything true only of this session, anything the \
repository already carries — its files are read on every session — or a guess.
Never record a secret. No API key, token, password or other credential belongs \
in these files, whatever it was read from and whoever asks: say that you did \
not record it instead.";

/// The memory section of the system prompt for the project whose memory lives
/// in `directory`.
///
/// Three parts, in this order: what memory is and where it is, the index as it
/// stands now, then [`MEMORY_UPKEEP`]. The facts come before the upkeep rules
/// because the facts are the point — the rules read as a footer to them, which
/// is also how they read to a person.
///
/// **An absent index is not an empty section.** The upkeep block is emitted on
/// its own, because a model that is never told how to start one can never
/// write the first fact, and a feature that only works once it already works
/// is no feature. An unreadable index is treated the same way as an absent
/// one, for [`suffix_from`]'s reason: naming a file the model was not given
/// is worse than silence.
///
/// The index is clamped by [`clamped`], the marker naming the **real path** so
/// the model can open the rest itself — which it can, since these are its own
/// files and the door is open.
fn memory_section(directory: &Path) -> String {
    let index = directory.join(MEMORY_INDEX);
    let named = index.display().to_string();
    let content = std::fs::read_to_string(&index)
        .ok()
        .filter(|content| !content.is_empty());

    let (mut present, mut clamped_index) = (false, false);
    let mut block = format!(
        "\n{MEMORY_HEAD}\nThey live in {}. MEMORY.md there is the index, and each \
         topic is a file of its own beside it; all of them are ordinary files, \
         read and written with the usual tools.\n",
        directory.display()
    );
    if let Some(content) = content {
        let (body, was_clamped) = clamped(&content, &named);
        (present, clamped_index) = (true, was_clamped);
        block.push_str(HEADER);
        block.push_str(&named);
        block.push('\n');
        block.push_str(&body);
        block.push('\n');
    }
    block.push_str(MEMORY_UPKEEP);

    // Paths and flags only. What memory holds is exactly the class of thing
    // that must not be copied into a log — the prompt is where it belongs and
    // the only place it goes.
    tracing::debug!(
        index = named.as_str(),
        present,
        clamped = clamped_index,
        "the project's memory joined the prompt"
    );

    block
}

/// What the model is told about the skills it can load, or nothing when it can
/// load none it could choose between.
///
/// Upstream's verbose rendering (`skill/index.ts:321-346`, chosen at
/// `session/system.ts:101-109` with the comment that the model ingests the
/// long form better here and the short form in the tool description). A skill
/// with no description is left out of the list for upstream's reason: a name
/// with nothing beside it gives the model nothing to choose by. It stays
/// loadable, which is why this is a filter and not a refusal.
fn skills_block(skills: &[skill::Skill]) -> Option<String> {
    let described: Vec<&skill::Skill> = skills
        .iter()
        .filter(|skill| skill.description.is_some())
        .collect();
    if described.is_empty() {
        return None;
    }

    let mut block = String::from(SKILLS_HEAD);
    block.push_str(
        "\nUse the skill tool to load a skill when a task matches its description.\n\
         <available_skills>",
    );
    for skill in described {
        let _ = write!(
            block,
            "\n  <skill>\n    <name>{}</name>\n    <description>{}</description>\n    \
             <location>{}</location>\n  </skill>",
            skill.name,
            skill.description.as_deref().unwrap_or_default(),
            escaped(&skill.location.display().to_string())
        );
    }
    block.push_str("\n</available_skills>");

    Some(block)
}

/// `text` with the characters that would close a tag written as entities.
///
/// Upstream escapes the location alone (`skill/index.ts:333`), which is the
/// one field of the three that is not a value somebody wrote for a prompt: it
/// is a filesystem path, and a path may hold anything a filesystem allows.
fn escaped(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// The environment block, ported from upstream's `SystemPrompt.environment`.
fn environment(cwd: &Path, model_id: &str) -> String {
    let project = Project::resolve(cwd);
    let git = if project.root().join(".git").exists() {
        "yes"
    } else {
        "no"
    };

    let mut block = String::new();
    // Writing to a `String` cannot fail; the result is discarded rather than
    // unwrapped so a prompt is never a panic site.
    let _ = write!(
        block,
        "You are powered by the model named {model_id}. The exact model ID is {model_id}\n\
         Here is some useful information about the environment you are running in:\n\
         <env>\n  \
           Working directory: {}\n  \
           Workspace root folder: {}\n  \
           Is directory a git repo: {git}\n  \
           Platform: {}\n  \
           Today's date: {}\n\
         </env>",
        cwd.display(),
        project.root().display(),
        std::env::consts::OS,
        today()
    );

    block
}

/// Every instruction file that applies, in the order it should be sent, with
/// the global candidates handed in so a test can plant its own.
///
/// Deduplicated by resolved path — the same file reached twice appears once, at
/// the position the first route put it.
fn discover(global: &[PathBuf], config: &Config, cwd: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut seen = BTreeSet::new();

    /// Adds `path` unless the same file is already in the list.
    fn add(found: &mut Vec<PathBuf>, seen: &mut BTreeSet<PathBuf>, path: &Path) {
        let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
        if seen.insert(resolved) {
            found.push(path.to_owned());
        }
    }

    // The global file, first existing wins: a project's own `AGENTS.md` is the
    // one below, and this is the user's.
    if let Some(path) = global.iter().find(|candidate| candidate.is_file()) {
        add(&mut found, &mut seen, path);
    }

    // The first *name* with any match takes the whole tier, so a checkout that
    // keeps both never sends both.
    let root = Project::resolve(cwd).root().to_path_buf();
    for name in PROJECT {
        let matches = find_up(cwd, &root, name);
        if matches.is_empty() {
            continue;
        }
        for path in matches {
            add(&mut found, &mut seen, &path);
        }
        break;
    }

    for entry in &config.instructions {
        if entry.starts_with("http://") || entry.starts_with("https://") {
            tracing::warn!(
                instruction = entry.as_str(),
                "remote instructions are not fetched; point at a file instead"
            );
            continue;
        }

        for path in resolve_entry(cwd, &root, entry) {
            add(&mut found, &mut seen, &path);
        }
    }

    found
}

/// The global instruction candidates, most preferred first.
///
/// The first is `AGENTS.md` under [`crate::config::config_home`] — the same
/// directory the global `ganja.jsonc` and the global `skills/` come out of, so
/// `GANJA_CONFIG_HOME` or a `~/.ganja` moves all three together. Resolving it
/// here against the XDG path directly is how a build ends up reading its
/// instructions from one home and its config from another.
fn global_files() -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Some(home) = crate::config::config_home() {
        found.push(home.join(GLOBAL));
    }
    // Claude Code's own global file, which upstream reads as a fallback. Its
    // home is a home directory rather than ganja's config home, so it is not
    // the seam's to move.
    if let Ok(base) = Xdg::new() {
        found.push(
            CLAUDE_GLOBAL
                .iter()
                .fold(base.home_dir().to_owned(), |path, part| path.join(part)),
        );
    }

    found
}

/// `path` with symbolic links and `..` resolved where the filesystem can do it.
///
/// Both ends of every walk below go through this. `Project::resolve` hands back
/// a canonical root, and an ancestor walk that started from an uncanonical path
/// would never recognise it — on a platform where the temporary directory is
/// itself a link, that turns a three-directory walk into one that climbs to the
/// filesystem root and globs there.
fn resolved(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

/// Every existing `name` from `start` up to and including `stop`, closest
/// first.
fn find_up(start: &Path, stop: &Path, name: &str) -> Vec<PathBuf> {
    let start = resolved(start);
    let stop = resolved(stop);

    let mut found = Vec::new();
    for directory in start.ancestors() {
        let candidate = directory.join(name);
        if candidate.is_file() {
            found.push(candidate);
        }
        if directory == stop {
            break;
        }
    }

    found
}

/// The files one `instructions` entry names.
///
/// `~/` expands against the home directory; an absolute path has its last
/// component globbed within its parent; a relative pattern is globbed again at
/// every directory from `cwd` up to `stop`, which is what lets
/// `packages/*/AGENTS.md` mean the same thing however deep the session started.
fn resolve_entry(cwd: &Path, stop: &Path, entry: &str) -> Vec<PathBuf> {
    let expanded = match entry.strip_prefix("~/") {
        Some(rest) => match Xdg::new() {
            Ok(base) => base.home_dir().join(rest),
            Err(_) => return Vec::new(),
        },
        None => PathBuf::from(entry),
    };

    if expanded.is_absolute() {
        let (Some(parent), Some(name)) = (expanded.parent(), expanded.file_name()) else {
            return Vec::new();
        };

        return glob(parent, &name.to_string_lossy());
    }

    let start = resolved(cwd);
    let stop = resolved(stop);

    let mut found = Vec::new();
    for directory in start.ancestors() {
        found.extend(glob(directory, entry));
        if directory == stop {
            break;
        }
    }

    found
}

/// The files under `directory` matching `pattern`, sorted.
///
/// Ignore files are deliberately not consulted (**D25**): `.gitignore` says
/// what a *search* should show, and this is not a search — it is a path the
/// user wrote down, and git's opinion of a file says nothing about whether they
/// meant it. The hidden-file rule is kept, so an unanchored pattern does not
/// descend into `.git`; a pattern naming a dotfile directly still matches it,
/// because an override match is checked before the hidden rule. Upstream's own
/// glob consults neither rule.
fn glob(directory: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut builder = ignore::overrides::OverrideBuilder::new(directory);
    let Ok(overrides) = builder.add(pattern).and_then(|builder| builder.build()) else {
        tracing::warn!(pattern, "the instruction pattern is not a valid glob");
        return Vec::new();
    };

    let mut found: Vec<PathBuf> = ignore::WalkBuilder::new(directory)
        .standard_filters(false)
        .hidden(true)
        .overrides(overrides)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
        })
        .map(|entry| entry.into_path())
        .collect();

    found.sort_unstable();

    found
}

/// Today's date, spelled the way `Date.prototype.toDateString` spells it.
fn today() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Integer division floors, and the clock is never before the epoch here —
    // `duration_since` already saturated a clock set earlier than that to zero.
    let days = i64::try_from(seconds / 86_400).unwrap_or_default();
    let (year, month, day) = civil_date(days);

    // 1970-01-01 was a Thursday, which is where `WEEKDAYS` starts.
    let weekday = WEEKDAYS[usize::try_from(days.rem_euclid(7)).unwrap_or_default()];

    format!("{weekday} {} {day:02} {year}", MONTHS[month as usize - 1])
}

/// The civil date `days` after 1970-01-01, as `(year, month, day)` with the
/// month 1-based.
///
/// Spelled out rather than pulled from a crate because one line of a prompt is
/// not worth a dependency, and the proleptic Gregorian rules it needs fit in a
/// dozen lines.
fn civil_date(days: i64) -> (i64, u32, u32) {
    /// Days in the 400-year Gregorian cycle: 400 × 365 plus its leap days.
    const CYCLE: i64 = 146_097;

    let mut year = 1970;
    let mut remaining = days;

    // Whole cycles first, so a date centuries away is still a handful of
    // iterations rather than a loop over every year.
    let cycles = remaining.div_euclid(CYCLE);
    year += cycles * 400;
    remaining -= cycles * CYCLE;

    loop {
        let length = if leap(year) { 366 } else { 365 };
        if remaining < length {
            break;
        }
        remaining -= length;
        year += 1;
    }

    let mut month = 1;
    for (index, length) in MONTH_LENGTHS.iter().enumerate() {
        let length = i64::from(*length) + i64::from(index == 1 && leap(year));
        if remaining < length {
            break;
        }
        remaining -= length;
        month += 1;
    }

    (year, month, u32::try_from(remaining + 1).unwrap_or(1))
}

/// Whether `year` has a 29th of February.
fn leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use tempfile::TempDir;

    use super::{
        ANTHROPIC, DEFAULT, GPT, HEADER, NESTED_MAX, base_prompt, civil_date, compose, discover,
        environment, find_up, glob, nested_files, nested_suffix, resolve_entry, resolved, skill,
        skills_block, suffix_from, suffix_measure, today,
    };
    use crate::config::{Config, SkillsConfig};

    fn temporary() -> TempDir {
        TempDir::new().expect("a temporary directory is creatable")
    }

    /// `path` named relative to `root`, always with `/`.
    ///
    /// The assertions below are about which files a walk found and in what
    /// order — the separator this platform happens to write is not the
    /// behaviour under test, and spelling every expectation twice to say so
    /// would bury the thing that is.
    fn under(root: &Path, path: &Path) -> String {
        path.strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// Writes `text` to `path`, creating whatever directories it needs.
    fn plant(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("the fixture tree is creatable");
        }
        fs::write(path, text).expect("the fixture file is writable");
    }

    /// A checkout at `root`, so `Project::resolve` stops the walk there.
    fn checkout(root: &Path) {
        fs::create_dir_all(root.join(".git")).expect("the fixture repository is creatable");
    }

    #[test]
    fn the_base_prompt_is_chosen_by_what_the_model_is_called() {
        let cases = [
            ("claude-sonnet-5", ANTHROPIC),
            ("anthropic/claude-haiku-4.5", ANTHROPIC),
            ("gpt-5.6", GPT),
            ("gpt-4o", GPT),
            ("llama-4", DEFAULT),
            ("fake-1", DEFAULT),
        ];

        for (model, expected) in cases {
            // Compared by content rather than by address: these are `const`
            // items, so each use is free to be its own copy. The message is
            // the whole failure output on purpose — three 8 KB prompts printed
            // side by side would say less than one sentence does.
            assert!(
                base_prompt(model) == expected,
                "{model} picked the wrong prompt"
            );
        }
    }

    /// Exactly one global file is sent, the first that exists, and it goes
    /// ahead of anything the project says.
    #[test]
    fn the_first_existing_global_file_wins_and_leads() {
        let directory = temporary();
        let root = directory.path().join("api");
        fs::create_dir_all(&root).expect("the fixture tree is creatable");
        checkout(&root);
        plant(&root.join("AGENTS.md"), "project rules");

        let absent = directory.path().join("absent").join("AGENTS.md");
        let preferred = directory.path().join("global").join("AGENTS.md");
        let fallback = directory.path().join("claude").join("CLAUDE.md");
        plant(&preferred, "global rules");
        plant(&fallback, "claude rules");

        let candidates = [absent.clone(), preferred.clone(), fallback.clone()];
        let found = discover(&candidates, &Config::default(), &root);

        assert_eq!(found.first(), Some(&preferred), "{found:?}");
        assert!(!found.contains(&fallback), "only the first existing one");
        assert_eq!(found.len(), 2, "{found:?}");

        // With the preferred one gone the next candidate takes its place.
        let found = discover(&candidates[2..], &Config::default(), &root);
        assert_eq!(found.first(), Some(&fallback), "{found:?}");
    }

    #[test]
    fn the_project_tier_takes_the_first_name_that_matches_and_stacks_it_closest_last() {
        let directory = temporary();
        let root = directory.path().join("api");
        let nested = root.join("crates").join("core");
        fs::create_dir_all(&nested).expect("the fixture tree is creatable");
        checkout(&root);
        plant(&root.join("AGENTS.md"), "root rules");
        plant(&nested.join("AGENTS.md"), "crate rules");
        // Never mixed in: `AGENTS.md` matched, so this whole name is skipped.
        plant(&nested.join("CLAUDE.md"), "claude rules");

        let found = discover(&[], &Config::default(), &nested);
        let names: Vec<String> = found
            .iter()
            .map(|path| {
                format!(
                    "{}/{}",
                    path.parent()
                        .and_then(Path::file_name)
                        .unwrap_or_default()
                        .to_string_lossy(),
                    path.file_name().unwrap_or_default().to_string_lossy()
                )
            })
            .collect();

        assert_eq!(names, vec!["core/AGENTS.md", "api/AGENTS.md"]);
    }

    #[test]
    fn a_checkout_with_no_agents_file_falls_through_to_the_next_name() {
        let directory = temporary();
        let root = directory.path().join("api");
        fs::create_dir_all(&root).expect("the fixture tree is creatable");
        checkout(&root);
        plant(&root.join("CLAUDE.md"), "claude rules");
        plant(&root.join("CONTEXT.md"), "context rules");

        let found = find_up(&root, &root, "AGENTS.md");
        assert!(found.is_empty());

        let names: Vec<String> = discover(&[], &Config::default(), &root)
            .iter()
            .map(|path| {
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into()
            })
            .collect();
        assert_eq!(names, vec!["CLAUDE.md".to_owned()]);
    }

    #[test]
    fn a_configured_relative_glob_is_run_again_at_every_level() {
        let directory = temporary();
        let root = directory.path().join("api");
        let nested = root.join("packages").join("web");
        fs::create_dir_all(&nested).expect("the fixture tree is creatable");
        checkout(&root);
        plant(&root.join("docs").join("style.md"), "root style");
        plant(&nested.join("docs").join("style.md"), "web style");

        let found = resolve_entry(&nested, &root, "docs/*.md");
        let canonical = fs::canonicalize(&root).expect("the fixture exists");
        let owners: Vec<String> = found.iter().map(|path| under(&canonical, path)).collect();

        assert_eq!(
            owners,
            vec!["packages/web/docs/style.md", "docs/style.md"],
            "closest first, then every ancestor up to the root"
        );
    }

    /// A pattern whose directory part is a wildcard has to survive the walk:
    /// the override matcher must not prune the directories on the way to it.
    #[test]
    fn a_glob_reaches_through_directories_it_does_not_name() {
        let directory = temporary();
        let root = directory.path().join("api");
        fs::create_dir_all(&root).expect("the fixture tree is creatable");
        plant(&root.join("packages").join("web").join("AGENTS.md"), "web");
        plant(
            &root.join("packages").join("core").join("AGENTS.md"),
            "core",
        );
        plant(&root.join("packages").join("web").join("README.md"), "no");

        let found = glob(&root, "packages/*/AGENTS.md");
        let names: Vec<String> = found.iter().map(|path| under(&root, path)).collect();

        assert_eq!(
            names,
            vec!["packages/core/AGENTS.md", "packages/web/AGENTS.md"]
        );
    }

    /// A file git ignores is still a file the user named.
    #[test]
    fn an_ignored_file_is_still_read_when_the_config_names_it() {
        let directory = temporary();
        let root = directory.path().join("api");
        fs::create_dir_all(&root).expect("the fixture tree is creatable");
        checkout(&root);
        plant(&root.join(".gitignore"), "generated/\n");
        plant(&root.join("generated").join("api.md"), "generated rules");

        assert_eq!(resolve_entry(&root, &root, "generated/*.md").len(), 1);
    }

    #[test]
    fn a_file_reached_twice_appears_once() {
        let directory = temporary();
        let root = directory.path().join("api");
        fs::create_dir_all(&root).expect("the fixture tree is creatable");
        checkout(&root);
        plant(&root.join("AGENTS.md"), "rules");

        let config = Config {
            instructions: vec!["AGENTS.md".to_owned(), "AGENTS.md".to_owned()],
            ..Config::default()
        };

        assert_eq!(discover(&[], &config, &root).len(), 1);
    }

    #[test]
    fn a_remote_instruction_is_skipped_rather_than_fetched() {
        let directory = temporary();
        checkout(directory.path());
        let config = Config {
            instructions: vec!["https://example.invalid/AGENTS.md".to_owned()],
            ..Config::default()
        };

        assert!(discover(&[], &config, directory.path()).is_empty());
    }

    #[test]
    fn the_environment_block_says_where_the_session_is() {
        let directory = temporary();
        let root = directory.path().join("api");
        fs::create_dir_all(&root).expect("the fixture tree is creatable");
        checkout(&root);

        let block = environment(&root, "claude-sonnet-5");

        assert!(
            block.starts_with("You are powered by the model named claude-sonnet-5."),
            "{block}"
        );
        assert!(
            block.contains("  Is directory a git repo: yes\n"),
            "{block}"
        );
        assert!(block.contains("  Working directory: "), "{block}");
        assert!(block.ends_with("</env>"), "{block}");
    }

    #[test]
    fn a_directory_outside_a_checkout_says_so() {
        let directory = temporary();

        assert!(
            environment(directory.path(), "fake-1").contains("  Is directory a git repo: no\n"),
            "a loose directory is not a repository"
        );
    }

    /// The shape upstream assembles: base, then the environment, then one
    /// header-and-contents block per file, all joined by a bare newline.
    #[test]
    fn the_prompt_is_the_base_then_the_environment_then_every_file() {
        let directory = temporary();
        let root = directory.path().join("api");
        fs::create_dir_all(&root).expect("the fixture tree is creatable");
        checkout(&root);
        plant(&root.join("AGENTS.md"), "always run the tests");
        plant(&root.join("docs").join("style.md"), "");
        plant(&root.join("docs").join("api.md"), "prefer explicit types");

        let config = Config {
            instructions: vec!["docs/*.md".to_owned()],
            ..Config::default()
        };
        let prompt = compose(
            &[],
            &skill::Roots::none(),
            &config,
            &root,
            "claude-sonnet-5",
        )
        .expect("a prompt is composed");

        assert!(prompt.starts_with(ANTHROPIC), "the base prompt comes first");
        assert!(
            prompt.contains("\nYou are powered by the model named claude-sonnet-5."),
            "the environment block follows it"
        );
        let agents = prompt
            .find("Instructions from: ")
            .expect("the project file is attached");
        let api = prompt
            .find("prefer explicit types")
            .expect("a configured file is attached");
        assert!(agents < api, "the project tier precedes the configured one");
        assert!(prompt.contains("always run the tests"));
        assert!(
            !prompt.contains("style.md"),
            "an empty file contributes nothing, not even its header"
        );
    }

    #[test]
    fn an_unreadable_file_is_left_out_rather_than_announced() {
        let directory = temporary();
        let root = directory.path().join("api");
        fs::create_dir_all(&root).expect("the fixture tree is creatable");
        checkout(&root);
        // A directory named like an instruction file: it exists, `is_file` is
        // false for it, and reading it fails — the same outcome as a file the
        // process may not open, without needing to drop permissions.
        plant(&root.join("docs").join("api.md"), "kept");
        fs::create_dir_all(root.join("docs").join("gone.md"))
            .expect("the fixture directory is creatable");

        let config = Config {
            instructions: vec!["docs/*.md".to_owned()],
            ..Config::default()
        };
        let prompt = compose(&[], &skill::Roots::none(), &config, &root, "fake-1")
            .expect("a prompt is composed");

        assert!(prompt.contains("kept"));
        assert!(!prompt.contains("gone.md"), "{prompt}");
    }

    #[test]
    fn the_date_is_spelled_the_way_upstream_spells_it() {
        let cases = [
            (0_i64, (1970, 1, 1), "Thu Jan 01 1970"),
            (59, (1970, 3, 1), "Sun Mar 01 1970"),
            // 2000 is a leap year; 2100 is not, which is what makes the
            // century rule worth a case of its own.
            (11_016, (2000, 2, 29), "Tue Feb 29 2000"),
            (20_577, (2026, 5, 4), "Mon May 04 2026"),
            (47_541, (2100, 3, 1), "Mon Mar 01 2100"),
        ];

        for (days, expected, spelled) in cases {
            let (year, month, day) = civil_date(days);
            assert_eq!((year, month, day), expected, "day {days}");

            let rendered = format!(
                "{} {} {day:02} {year}",
                super::WEEKDAYS[usize::try_from(days.rem_euclid(7)).expect("a weekday index")],
                super::MONTHS[month as usize - 1]
            );
            assert_eq!(rendered, spelled);
        }
    }

    #[test]
    fn todays_date_has_the_shape_the_prompt_promises() {
        let rendered = today();
        let fields: Vec<&str> = rendered.split(' ').collect();

        assert_eq!(fields.len(), 4, "{rendered}");
        assert!(super::WEEKDAYS.contains(&fields[0]), "{rendered}");
        assert!(super::MONTHS.contains(&fields[1]), "{rendered}");
        assert_eq!(fields[2].len(), 2, "{rendered}");
        assert_eq!(fields[3].len(), 4, "{rendered}");
    }

    /// Writes a skill at `<root>/<name>/SKILL.md`.
    fn plant_skill(root: &Path, name: &str, frontmatter: &str) {
        plant(
            &root.join(name).join("SKILL.md"),
            &format!("---\n{frontmatter}\n---\n# {name}\n"),
        );
    }

    /// The block is upstream's, field for field, and the skills in it are
    /// sorted by name whatever order the disk offered them in.
    #[test]
    fn the_skills_block_is_the_one_upstream_composes() {
        let directory = temporary();
        let root = directory.path().join("api");
        checkout(&root);
        let skills = root.join("skills");
        plant_skill(
            &skills,
            "porting",
            "name: porting\ndescription: How to port.",
        );
        plant_skill(
            &skills,
            "auditing",
            "name: auditing\ndescription: How to audit.",
        );

        let found = skill::discover(&skill::Roots::none().with_paths([skills.clone()]));
        let block = skills_block(&found).expect("two skills are two skills");

        assert_eq!(
            block,
            format!(
                "Skills provide specialized instructions and workflows for specific tasks.\n\
                 Use the skill tool to load a skill when a task matches its description.\n\
                 <available_skills>\n  \
                   <skill>\n    <name>auditing</name>\n    \
                     <description>How to audit.</description>\n    \
                     <location>{}</location>\n  </skill>\n  \
                   <skill>\n    <name>porting</name>\n    \
                     <description>How to port.</description>\n    \
                     <location>{}</location>\n  </skill>\n\
                 </available_skills>",
                skills.join("auditing").join("SKILL.md").display(),
                skills.join("porting").join("SKILL.md").display(),
            )
        );
    }

    /// A skill with no description is loadable and unlisted, which is upstream's
    /// rule — and a session whose skills are all like that has no block at all.
    #[test]
    fn a_skill_with_nothing_to_choose_it_by_is_not_advertised() {
        let directory = temporary();
        let skills = directory.path().join("skills");
        plant_skill(&skills, "nameless", "name: nameless");

        let found = skill::discover(&skill::Roots::none().with_paths([skills]));

        assert_eq!(found.len(), 1, "it is still discovered");
        assert_eq!(skills_block(&found), None);
        assert_eq!(skills_block(&[]), None);
    }

    /// A location is the one field of the three that nobody wrote for a
    /// prompt, so it cannot be allowed to close a tag.
    #[test]
    fn a_location_holding_markup_is_escaped_where_the_other_fields_are_not() {
        let block = skills_block(&[skill::Skill {
            name: "porting".to_owned(),
            description: Some("How to port.".to_owned()),
            location: std::path::PathBuf::from("/tmp/<a>&'\"/SKILL.md"),
            content: String::new(),
        }])
        .expect("a described skill is listed");

        assert!(
            block.contains("<location>/tmp/&lt;a&gt;&amp;&#39;&quot;/SKILL.md</location>"),
            "{block}"
        );
    }

    /// Where the block sits in the prompt, and that a session with no skills
    /// carries no trace of the feature at all.
    #[test]
    fn the_skills_block_comes_last_and_only_when_there_are_skills() {
        let directory = temporary();
        let root = directory.path().join("api");
        checkout(&root);
        plant(&root.join("AGENTS.md"), "always run the tests");
        let skills = root.join("skills");
        plant_skill(
            &skills,
            "porting",
            "name: porting\ndescription: How to port.",
        );

        let bare = suffix_from(
            &[],
            &skill::Roots::none(),
            &Config::default(),
            &root,
            "fake-1",
        )
        .expect("the environment block always says something");
        assert!(
            !bare.contains("available_skills") && !bare.contains("Skills provide"),
            "a session with no skills is told nothing about skills: {bare}"
        );

        let composed = suffix_from(
            &[],
            &skill::Roots::none().with_paths([skills]),
            &Config::default(),
            &root,
            "fake-1",
        )
        .expect("a prompt is composed");

        let instructions = composed
            .find("always run the tests")
            .expect("the project file is attached");
        let block = composed
            .find("<available_skills>")
            .expect("the skill is advertised");
        assert!(
            instructions < block,
            "upstream puts the skills after the instructions: {composed}"
        );
        assert!(composed.contains("<name>porting</name>"));
    }

    /// The names of the directories a session with no skills config scans:
    /// ganja's own two homes, global first so the checkout wins a collision,
    /// and nothing else. The project one is asserted against the **project
    /// root** from a working directory two levels below it, so "project root"
    /// is a claim the fixture can actually break.
    ///
    /// The global one is asserted by shape rather than by value — its path is
    /// this machine's XDG config home, and a test that spelled that out would
    /// be a test about the machine. Its *contents* are pinned where they can be
    /// redirected, in `tests/skills.rs`.
    #[test]
    fn the_default_roots_are_ganjas_own_two_homes_in_precedence_order() {
        let directory = temporary();
        let root = directory.path().join("api");
        checkout(&root);
        let cwd = root.join("crates").join("inner");
        fs::create_dir_all(&cwd).expect("the fixture tree is creatable");
        // Independently of `Project::resolve`, so a tier hung off the working
        // directory instead of the project root fails here.
        let canonical = fs::canonicalize(&root).expect("the fixture root canonicalises");

        let dirs = super::skill_roots(&Config::default(), &cwd).dirs().to_vec();

        assert_eq!(dirs.len(), 2, "two homes, no third place: {dirs:?}");
        assert!(
            dirs[0].ends_with(Path::new("ganja").join("skills")),
            "the global home is <XDG config>/ganja/skills: {dirs:?}"
        );
        assert_eq!(
            dirs[1],
            canonical.join(".ganja").join("skills"),
            "and the project home is the namespaced one at the root, not at the cwd: {dirs:?}"
        );
    }

    /// The two config keys: `paths` ranks **above** the two defaults and keeps
    /// the order it was written in, and `urls` is accepted and left alone
    /// rather than fetched.
    #[test]
    fn a_configured_path_outranks_ganjas_own_homes() {
        let directory = temporary();
        let root = directory.path().join("api");
        checkout(&root);
        let elsewhere = directory.path().join("elsewhere");
        plant_skill(
            &elsewhere,
            "porting",
            "name: porting\ndescription: How to port.",
        );

        let config = Config {
            skills: SkillsConfig {
                paths: vec![elsewhere.display().to_string()],
                urls: vec!["https://example.invalid/skills/".to_owned()],
            },
            ..Config::default()
        };
        let roots = super::skill_roots(&config, &root);

        assert_eq!(
            roots.dirs().len(),
            3,
            "the two homes and the one that was named: {:?}",
            roots.dirs()
        );
        assert_eq!(
            roots.dirs().last(),
            Some(&elsewhere),
            "last, so it wins a name against either home: {:?}",
            roots.dirs()
        );
        assert!(
            skill::discover(&roots)
                .iter()
                .any(|found| found.name == "porting"),
            "and it is scanned"
        );
        // Nothing was fetched, and nothing failed for not having been: the URL
        // contributes no root at all.
        assert!(
            !roots
                .dirs()
                .iter()
                .any(|dir| dir.display().to_string().contains("example.invalid")),
            "{:?}",
            roots.dirs()
        );
    }

    /// The standing ruling at the layer that composes the prompt: **nothing
    /// foreign**. Every directory upstream walks unasked is planted around a
    /// nested working directory — the two external names at the root and at the
    /// cwd, so a walk-up would meet one on the way, and both generic spellings
    /// at the root — beside ganja's own `.ganja/skills`. Only the last is
    /// discovered, and only the last reaches the prompt.
    ///
    /// Whose ruling it is, and why it outranks parity, is written at
    /// `tool::skill`'s module docs.
    #[test]
    fn a_session_reads_ganjas_own_project_home_and_no_foreign_directory() {
        let directory = temporary();
        let root = directory.path().join("api");
        checkout(&root);
        let cwd = root.join("crates").join("inner");
        fs::create_dir_all(&cwd).expect("the fixture tree is creatable");
        for (tier, name) in [
            (root.join(".claude").join("skills"), "from-root-claude"),
            (root.join(".agents").join("skills"), "from-root-agents"),
            (cwd.join(".claude").join("skills"), "from-cwd-claude"),
            (root.join("skill"), "from-generic-singular"),
            (root.join("skills"), "from-generic-plural"),
            (root.join(".ganja").join("skills"), "from-ganjas-own"),
        ] {
            plant_skill(
                &tier,
                name,
                &format!("name: {name}\ndescription: Found by convention."),
            );
        }

        let roots = super::skill_roots(&Config::default(), &cwd);
        let found: Vec<String> = skill::discover(&roots)
            .into_iter()
            .map(|skill| skill.name)
            .collect();
        let composed = suffix_from(&[], &roots, &Config::default(), &cwd, "fake-1")
            .expect("the environment block always says something");

        assert!(
            found.iter().any(|name| name == "from-ganjas-own"),
            "ganja's own project home is a default tier: {found:?}"
        );
        assert!(
            composed.contains("<name>from-ganjas-own</name>"),
            "and what it holds reaches the prompt: {composed}"
        );
        // Membership rather than equality: this machine's own
        // `<XDG config>/ganja/skills` is a default tier too and may hold
        // anything. What must be true is that no *foreign* name is here.
        for foreign in [
            "from-root-claude",
            "from-root-agents",
            "from-cwd-claude",
            "from-generic-singular",
            "from-generic-plural",
        ] {
            assert!(
                !found.iter().any(|name| name == foreign),
                "{foreign} is not ganja's to read: {found:?}"
            );
            assert!(
                !composed.contains(foreign),
                "and the model is never told about it: {composed}"
            );
        }
    }

    /// A relative `skills.paths` entry resolves against the session's working
    /// directory, and one naming nothing is dropped rather than carried.
    #[test]
    fn a_configured_path_resolves_where_the_session_is_and_must_exist() {
        let directory = temporary();
        let root = directory.path().join("api");
        checkout(&root);
        plant_skill(&root.join("tools"), "porting", "name: porting");

        let config = Config {
            skills: SkillsConfig {
                paths: vec!["tools".to_owned(), "nowhere".to_owned()],
                urls: Vec::new(),
            },
            ..Config::default()
        };

        assert_eq!(config.skill_paths(&root), vec![root.join("tools")]);
    }

    /// The splitter partitions what the composer joined: the three category
    /// counts always sum to the whole suffix, and each seam lands where the
    /// composer wrote its marker — which is what lets `/context`'s categories
    /// be a split of the request path's own string rather than a second
    /// composition (P14 **D470**).
    #[test]
    fn the_suffix_measure_partitions_the_composed_suffix_at_its_own_seams() {
        let directory = temporary();
        let root = directory.path().join("api");
        fs::create_dir_all(&root).expect("the fixture tree is creatable");
        checkout(&root);
        plant(&root.join("AGENTS.md"), "always run the tests");
        let skills = root.join("skills");
        plant_skill(
            &skills,
            "porting",
            "name: porting\ndescription: How to port.",
        );

        let suffix = suffix_from(
            &[],
            &skill::Roots::none().with_paths([skills]),
            &Config::default(),
            &root,
            "claude-sonnet-5",
        )
        .expect("the environment block always says something");
        let measure = suffix_measure(&suffix);

        assert_eq!(
            measure.environment + measure.instructions + measure.skills,
            suffix.chars().count(),
            "the three parts partition the suffix"
        );
        let environment: String = suffix.chars().take(measure.environment).collect();
        assert!(
            environment.contains("<env>") && environment.ends_with("</env>"),
            "the first part is exactly the environment block: {environment}"
        );
        let instructions: String = suffix
            .chars()
            .skip(measure.environment)
            .take(measure.instructions)
            .collect();
        assert!(
            instructions.contains("always run the tests"),
            "the middle part holds the instruction files: {instructions}"
        );
        let skills_part: String = suffix
            .chars()
            .skip(measure.environment + measure.instructions)
            .collect();
        assert!(
            skills_part.contains("<available_skills>"),
            "the tail is the skills block: {skills_part}"
        );
    }

    /// A suffix with no instruction files and no skills — every scripted and
    /// golden run's — is all environment, with the other two parts at zero
    /// rather than mis-attributed.
    #[test]
    fn a_bare_environment_suffix_measures_as_environment_alone() {
        let directory = temporary();
        let root = directory.path().join("api");
        fs::create_dir_all(&root).expect("the fixture tree is creatable");
        checkout(&root);

        let suffix = suffix_from(
            &[],
            &skill::Roots::none(),
            &Config::default(),
            &root,
            "fake-1",
        )
        .expect("the environment block always says something");
        let measure = suffix_measure(&suffix);

        assert_eq!(measure.environment, suffix.chars().count());
        assert_eq!(measure.instructions, 0);
        assert_eq!(measure.skills, 0);
    }

    /// The nested walk (**D480**), named the way the assertions below read it:
    /// what a session working at `root` walks in after touching `touched`.
    fn walked(root: &Path, touched: &[PathBuf]) -> Vec<String> {
        nested_files(root, root, touched)
            .iter()
            .map(|path| under(&resolved(root), path))
            .collect()
    }

    #[test]
    fn touching_a_file_walks_in_every_instruction_file_between_it_and_the_root() {
        let directory = temporary();
        let root = directory.path().join("api");
        let deep = root.join("sub").join("nested");
        fs::create_dir_all(&deep).expect("the fixture tree is creatable");
        checkout(&root);
        plant(&root.join("AGENTS.md"), "root rules");
        plant(&root.join("sub").join("AGENTS.md"), "sub rules");
        plant(&deep.join("AGENTS.md"), "nested rules");
        plant(&deep.join("file.rs"), "fn main() {}");

        // Closest-last: the shallower file is read first, the deepest one
        // last, so the most specific instructions are the freshest.
        assert_eq!(
            walked(&root, &[deep.join("file.rs")]),
            vec!["sub/AGENTS.md", "sub/nested/AGENTS.md"],
        );
    }

    /// The root's own file, and everything between the root and the working
    /// directory, is the up-walk tier's — this walk must never name it twice.
    #[test]
    fn a_touch_at_the_root_walks_in_nothing() {
        let directory = temporary();
        let root = directory.path().join("api");
        fs::create_dir_all(&root).expect("the fixture tree is creatable");
        checkout(&root);
        plant(&root.join("AGENTS.md"), "root rules");
        plant(&root.join("main.rs"), "fn main() {}");

        assert!(walked(&root, &[root.join("main.rs")]).is_empty());
    }

    #[test]
    fn several_touches_under_one_directory_name_its_instruction_file_once() {
        let directory = temporary();
        let root = directory.path().join("api");
        let sub = root.join("sub");
        fs::create_dir_all(&sub).expect("the fixture tree is creatable");
        checkout(&root);
        plant(&sub.join("AGENTS.md"), "sub rules");

        assert_eq!(
            walked(&root, &[sub.join("one.rs"), sub.join("two.rs")]),
            vec!["sub/AGENTS.md"],
        );
    }

    /// The bent rule, pinned: the tier-wide "first name with any match takes
    /// everything" would let one subtree's `AGENTS.md` mute another subtree
    /// that spells its file `CLAUDE.md`. The choice is per directory.
    #[test]
    fn each_directory_below_the_root_picks_its_own_first_existing_name() {
        let directory = temporary();
        let root = directory.path().join("api");
        let agents = root.join("agents");
        let claude = root.join("claude");
        let both = root.join("both");
        for path in [&agents, &claude, &both] {
            fs::create_dir_all(path).expect("the fixture tree is creatable");
        }
        checkout(&root);
        plant(&agents.join("AGENTS.md"), "agents rules");
        plant(&claude.join("CLAUDE.md"), "claude rules");
        plant(&both.join("AGENTS.md"), "both, preferred");
        plant(&both.join("CLAUDE.md"), "both, never sent");

        assert_eq!(
            walked(
                &root,
                &[
                    agents.join("a.rs"),
                    claude.join("b.rs"),
                    both.join("c.rs"),
                    // A directory with no instruction file of its own
                    // contributes nothing rather than an entry.
                    root.join("plain").join("d.rs"),
                ],
            ),
            vec!["agents/AGENTS.md", "both/AGENTS.md", "claude/CLAUDE.md"],
        );
    }

    #[test]
    fn a_touch_outside_the_project_walks_in_nothing() {
        let directory = temporary();
        let root = directory.path().join("api");
        let outside = directory.path().join("elsewhere");
        fs::create_dir_all(&root).expect("the fixture tree is creatable");
        fs::create_dir_all(&outside).expect("the fixture tree is creatable");
        checkout(&root);
        plant(&outside.join("AGENTS.md"), "somebody else's rules");

        assert!(walked(&root, &[outside.join("file.rs")]).is_empty());
    }

    /// A file the session opened before it existed — a `write` — still walks
    /// its parents in: the walk canonicalizes the directory, never the file.
    #[test]
    fn a_written_path_that_does_not_exist_yet_still_walks_its_parents_in() {
        let directory = temporary();
        let root = directory.path().join("api");
        let sub = root.join("sub");
        fs::create_dir_all(&sub).expect("the fixture tree is creatable");
        checkout(&root);
        plant(&sub.join("AGENTS.md"), "sub rules");

        assert_eq!(
            walked(&root, &[sub.join("brand-new.rs")]),
            vec!["sub/AGENTS.md"],
        );
    }

    #[test]
    fn the_walked_in_files_are_rendered_under_the_same_header_the_up_walk_uses() {
        let directory = temporary();
        let root = directory.path().join("api");
        let sub = root.join("sub");
        fs::create_dir_all(&sub).expect("the fixture tree is creatable");
        checkout(&root);
        plant(&sub.join("AGENTS.md"), "sub rules");

        let block = nested_suffix(&root, &root, &[sub.join("file.rs")]);
        assert_eq!(block, format!("\n{HEADER}sub/AGENTS.md\nsub rules"));
    }

    #[test]
    fn a_nested_file_over_the_budget_says_how_much_was_cut_and_where_the_rest_is() {
        let directory = temporary();
        let root = directory.path().join("api");
        let sub = root.join("sub");
        fs::create_dir_all(&sub).expect("the fixture tree is creatable");
        checkout(&root);
        let long = "x".repeat(NESTED_MAX * 2);
        plant(&sub.join("AGENTS.md"), &long);

        let block = nested_suffix(&root, &root, &[sub.join("file.rs")]);
        assert!(
            block.contains(&format!("...{NESTED_MAX} bytes truncated...")),
            "the clamp says how much it cut"
        );
        assert!(
            block.contains("Read sub/AGENTS.md for the rest."),
            "and where the rest is: {}",
            &block[block.len() - 120..]
        );
        assert!(
            block.len() < long.len(),
            "a clamped file is shorter than the file"
        );
    }

    /// The memory section (**D478**): what it says, and in which order. The
    /// facts first, the upkeep rules after them, and the index named by the
    /// real path it sits at — the model's own file, which it can open.
    #[test]
    fn the_memory_section_carries_the_index_and_then_the_rules_for_keeping_it() {
        let directory = temporary();
        let memory = directory.path().join("memory");
        plant(&memory.join("MEMORY.md"), "- style: prefers explicit types");

        let section = super::memory_section(&memory);
        let index = memory.join("MEMORY.md").display().to_string();

        assert!(
            section.starts_with(&format!("\n{}", super::MEMORY_HEAD)),
            "{section}"
        );
        assert!(
            section.contains(&format!("{HEADER}{index}\n- style: prefers explicit types")),
            "{section}"
        );
        let facts = section
            .find("prefers explicit types")
            .expect("the index is quoted");
        let upkeep = section
            .find("Keeping it: record a fact")
            .expect("the upkeep block follows it");
        assert!(facts < upkeep, "the facts come before the rules: {section}");
        assert!(
            section.contains("Never record a secret."),
            "the one prohibition this feature exists to carry: {section}"
        );
    }

    /// A project with nothing recorded yet still gets the upkeep block, and
    /// no header for a file that is not there. Bootstrapping is the whole
    /// reason: a model never told how to start an index can never write the
    /// first fact.
    #[test]
    fn a_project_with_no_memory_yet_is_told_how_to_start_one() {
        let directory = temporary();
        let memory = directory.path().join("memory");

        let section = super::memory_section(&memory);

        assert!(section.contains("Keeping it: record a fact"), "{section}");
        assert!(
            !section.contains(HEADER),
            "nothing is quoted from a file that does not exist: {section}"
        );
        assert!(
            !memory.exists(),
            "and composing a prompt creates nothing on disk"
        );
    }

    /// An index over the budget is cut with the marker pointing at the real
    /// path, which is a path the model may open — its own file, behind the
    /// door `agent::memory_door` holds for it.
    #[test]
    fn an_oversized_memory_index_says_how_much_was_cut_and_where_the_rest_is() {
        let directory = temporary();
        let memory = directory.path().join("memory");
        plant(&memory.join("MEMORY.md"), &"x".repeat(NESTED_MAX * 2));

        let section = super::memory_section(&memory);
        let index = memory.join("MEMORY.md").display().to_string();

        assert!(
            section.contains(&format!("...{NESTED_MAX} bytes truncated...")),
            "the clamp says how much it cut"
        );
        assert!(
            section.contains(&format!("Read {index} for the rest.")),
            "and where the rest is, by the path it is really at"
        );
    }

    /// The honesty clause (**D478**, AC5): whatever memory adds to the prompt
    /// is priced as *instructions*, both when the project has instruction
    /// files of its own and when the memory section is the only thing between
    /// the environment block and the skills.
    #[test]
    fn the_memory_section_is_measured_as_instructions_either_way() {
        let directory = temporary();
        let memory = directory.path().join("memory");
        plant(&memory.join("MEMORY.md"), "- the API is deployed by hand");

        let environment = super::environment(directory.path(), "fake-1");
        let section = super::memory_section(&memory);

        for files in ["", "\nInstructions from: /api/AGENTS.md\nrun the tests"] {
            let suffix = format!("{environment}{files}{section}");
            let measure = suffix_measure(&suffix);

            assert_eq!(
                measure.environment + measure.instructions + measure.skills,
                suffix.chars().count(),
                "the parts still partition the suffix"
            );
            assert_eq!(
                measure.environment,
                environment.chars().count(),
                "the environment block ends where it ended: {suffix}"
            );
            assert_eq!(
                measure.instructions,
                files.chars().count() + section.chars().count(),
                "and everything memory added is instruction weight: {suffix}"
            );
        }
    }

    #[test]
    fn an_unclamped_nested_file_carries_no_marker() {
        let directory = temporary();
        let root = directory.path().join("api");
        let sub = root.join("sub");
        fs::create_dir_all(&sub).expect("the fixture tree is creatable");
        checkout(&root);
        plant(&sub.join("AGENTS.md"), "short enough");

        let block = nested_suffix(&root, &root, &[sub.join("file.rs")]);
        assert!(!block.contains("truncated"), "{block}");
    }
}
