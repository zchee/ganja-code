//! The system prompt: what the model is told before it is told anything else.
//!
//! Spec: upstream `packages/opencode/src/session/instruction.ts` and
//! `packages/opencode/src/session/system.ts`.
//!
//! Four things are concatenated, joined by a bare newline, in this order —
//! five in a project that opted into memory (**D478**), whose section
//! `suffix_from` appends last of all:
//!
//! 1. the base prompt for the model's family, derived from upstream's
//!    `session/prompt/*.txt` with ganja's identity substituted (**D522**);
//! 2. an environment block — the model's own name, where it is working, and
//!    what day it is;
//! 3. every instruction file that applies, each rendered as
//!    `Instructions from: {path}` followed by its contents;
//! 4. the skills this session can load, when there are any — last, where
//!    upstream puts them (`session/prompt.ts:1257-1268`).
//!
//! Which files apply is `discover`, and the order is upstream's: the one global
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
//!   keeping UTC makes the prompt stable across machines regardless of where
//!   ganja happens to run. That product property is independent of which date
//!   library is available.
//! - **D25** — instruction globs do not consult ignore files, but do keep the
//!   hidden-file rule; see `glob`.
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
//!   and the honest alternatives are all declared at `nested_files`.
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
    time::SystemTime,
};

use etcetera::base_strategy::{BaseStrategy as _, Xdg};
use jiff::Timestamp;

use crate::{config::Config, project::Project, tool::skill};

/// Base prompt for Anthropic's models, derived from upstream with ganja's
/// identity substituted (**D522**; MIT, see `THIRD_PARTY_NOTICES.md`).
const ANTHROPIC: &str = include_str!("prompt/anthropic.txt");

/// Base prompt for OpenAI's models, derived the same way (**D522**).
const GPT: &str = include_str!("prompt/gpt.txt");

/// Base prompt for everything else, derived the same way (**D522**).
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

/// [`suffix`], with the global instruction candidates and the skill roots
/// handed in.
///
/// The split is what lets the tests below prove the composition without the
/// machine running them contributing an `AGENTS.md` of its own — and the
/// global candidates really are an input to discovery rather than something it
/// knows. The roots are an input for a second reason the candidates share:
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
/// read into every request's system prompt by `memory_section`, and the
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
/// upkeep instructions in `memory_section` are **synthesized, not ported** —
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
    let timestamp = Timestamp::try_from(SystemTime::now())
        .unwrap_or(Timestamp::UNIX_EPOCH)
        .max(Timestamp::UNIX_EPOCH);

    date_at(timestamp)
}

/// Kept separate so the byte format is pinned at fixed instants without
/// controlling the process clock.
fn date_at(timestamp: Timestamp) -> String {
    timestamp.strftime("%a %b %d %Y").to_string()
}

#[cfg(test)]
#[path = "instruction_tests.rs"]
mod tests;
