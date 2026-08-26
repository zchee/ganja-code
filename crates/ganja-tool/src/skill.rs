//! The `skill` tool: loads a named skill's instructions into the conversation.
//!
//! Spec: upstream `packages/opencode/src/tool/skill.ts` and `skill.txt` for the
//! tool, `packages/opencode/src/skill/index.ts` for what a skill is and where
//! they are found, and `packages/core/src/v1/config/skills.ts` for the two
//! config keys.
//!
//! A skill is a directory holding a `SKILL.md`: YAML frontmatter naming it and
//! describing it, then markdown the model reads when it loads one. The
//! description is what reaches the system prompt — `<available_skills>`, which
//! `ganja-core`'s `instruction` module composes — and the body is what this
//! tool hands back. Both sides read the same directories through [`discover`],
//! so what the prompt advertises is what a call can load.
//!
//! Which directories those are is the whole of the next paragraph, and it is
//! the one place this module does not port upstream.
//!
//! # Divergences
//!
//! - **`nothing-foreign-is-discovered`** — upstream scans four tiers nobody
//!   asked it to (`skill/index.ts:173-233`): `~/.claude/skills` and
//!   `~/.agents/skills`, those same two names walking up from the working
//!   directory to the project root, and `skill/`+`skills/` under every
//!   directory its config lives in. **None of those four is scanned here.**
//!   What is scanned unasked is ganja's own two homes and nothing else —
//!   `<XDG config>/ganja/skills` and `<project root>/.ganja/skills`, composed
//!   by `ganja-core`'s `config::default_skill_dirs` — plus whatever a config
//!   named in `skills.paths`.
//!
//!   This is a **standing ruling by the user of this build**, not a porting
//!   judgment and not an oversight. It reads: *ganja's own homes are default
//!   tiers; nothing foreign is discovered.* The two halves are one idea —
//!   placing a `SKILL.md` in a directory that exists because of this tool **is**
//!   the opt-in act, and it is exactly what a config key would otherwise stand
//!   in for, while another tool's directory is another tool's however
//!   conveniently it sits in the same home. On a real machine `~/.claude/skills`
//!   holds a hundred of them, every description landing in every system prompt
//!   of a session that never asked for one. That is what the ruling is about,
//!   and it binds `agent`, `mcp`, and whoever ports `plugin` and `hooks`. Do
//!   not "restore" a foreign tier here to close the gap with upstream — the gap
//!   is the feature.
//!
//!   Note which project-root name is scanned: `.ganja/skills`, namespaced, and
//!   **not** the bare `skill/` or `skills/` upstream reads. A directory called
//!   `skills` at the top of somebody's repository is a name they may already
//!   mean something else by; one called `.ganja` is not.
//!
//!   Nothing foreign is lost that naming a directory does not buy back.
//!   Upstream's own tier is one line — `"skills": { "paths":
//!   ["~/.claude/skills"] }` — and then it is a thing somebody did rather than
//!   a thing that happened.
//!
//!   [`Roots`] itself still has no constructor that goes looking: the two
//!   defaults are resolved by the engine, which is the layer that knows where
//!   ganja's config lives, and handed here as a value like any other.
//! - **`skill-discovery-runs-per-call`** — upstream discovers once per
//!   instance and caches (`skill/index.ts:259-287`). Discovery here runs when
//!   it is asked: a walk of a handful of directories costs less than the
//!   machinery to invalidate a cache, and a skill written while a session is
//!   open is then loadable in that session rather than in the next one.
//! - **`skill-urls-are-not-fetched`** — upstream downloads `skills.urls` into a
//!   cache directory and scans it (`skill/discovery.ts`). Nothing here fetches
//!   over the network to compose a prompt, for the same reason `http(s)`
//!   entries in `instructions` are skipped with a warning (**D2**): a session
//!   that cannot start because a host is down is worse than one that starts
//!   without a remote skill. The key is accepted and each entry is warned
//!   about by name.
//! - **`skill-frontmatter-reads-scalars`** — upstream parses the frontmatter
//!   with a YAML library and asks for two string fields
//!   (`skill/index.ts:53-59`). There is no YAML crate in this workspace, and
//!   this reads exactly those fields: `key: value` lines, quoted or not, plus
//!   block scalars (`|`, `>`), which is the shape every skill in the wild
//!   writes and the shape upstream's own permissive fallback exists to rescue
//!   (`config/markdown.ts:20-33`).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{
    Tool, ToolCtx, ToolError, ToolOutput,
    frontmatter::{fields, split},
};

/// The file that makes a directory a skill.
const MANIFEST: &str = "SKILL.md";

/// Most files one loaded skill lists beside its manifest
/// (`tool/skill.ts:42`).
const SAMPLED_FILES: usize = 10;

/// How deep a scan walks below a root before it stops.
///
/// A skill lives at `<root>/<name>/SKILL.md`, and upstream's glob is
/// unbounded. Bounded here because a root a config named could be a home
/// directory by accident — every root is one somebody wrote down, and what
/// somebody writes down is sometimes `~` — and a prompt composition is not a
/// good place to walk one.
const MAX_DEPTH: usize = 6;

/// What the model passes to `skill`.
#[derive(Debug, Deserialize, JsonSchema)]
struct Args {
    /// The name of the skill from available_skills
    name: String,
}

/// One skill, as the prompt advertises it and the tool loads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    /// What the model names to load it. Its frontmatter's, not its
    /// directory's.
    pub name: String,
    /// The sentence the system prompt lists it by. A skill without one is
    /// loadable and unadvertised, exactly as upstream leaves it
    /// (`skill/index.ts:322`).
    pub description: Option<String>,
    /// The `SKILL.md` this came from.
    pub location: PathBuf,
    /// The markdown below the frontmatter.
    pub content: String,
}

/// Where skills are looked for: exactly the directories somebody handed in.
///
/// A value rather than a scan of "wherever skills live", for two reasons. The
/// first is that the thing this must be able to say is *nothing*: a fixture
/// run, a golden differential or any test composing a prompt has to hold a set
/// of roots that cannot reach the machine it is running on. The second is the
/// module's `nothing-foreign-is-discovered` — the two directories a session
/// scans unasked are ganja's own, and *which* directories those are is a
/// question about where this build keeps its config, which is the engine's
/// answer and not a tool's. So there is deliberately no constructor here that
/// goes looking: `ganja-core`'s `config::default_skill_dirs` works the two out
/// and they arrive through [`Roots::with_paths`] like anything else.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Roots {
    /// The directories to scan, in the order they were handed in — later
    /// entries win a name collision.
    dirs: Vec<PathBuf>,
}

impl Roots {
    /// No roots at all: discovery finds nothing.
    ///
    /// Where every set of roots starts, a real session's included — the engine
    /// builds its two defaults onto this — and the value a fixture keeps,
    /// because a set that names nowhere cannot reach the machine a test is
    /// running on.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// The same roots with `paths` on the end, already expanded and resolved by
    /// whoever worked them out.
    ///
    /// The one way a directory gets in front of [`discover`], for ganja's own
    /// two homes as much as for a `skills.paths` entry — which is why the
    /// engine appends the defaults first and the configured paths after, so a
    /// directory somebody wrote down outranks one that was there by
    /// convention. A caller that wants upstream's `~/.claude/skills` writes it
    /// here too, which is the difference between a directory being read and a
    /// directory being read *on purpose*.
    #[must_use]
    pub fn with_paths(mut self, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        self.dirs.extend(paths);

        self
    }

    /// The directories that will be scanned, in scan order.
    #[must_use]
    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }
}

/// Every skill under `roots`, by name, sorted.
///
/// A directory that is not there contributes nothing — a path in a config
/// outlives the directory it named. A `SKILL.md` that will not parse is warned
/// about and skipped, as upstream skips it: one malformed file may not take the
/// rest of a session's skills with it.
///
/// Two skills claiming one name is upstream's warning too, and upstream's
/// answer — the later scan wins (`skill/index.ts:125-138`), which is what makes
/// the order [`Roots::with_paths`] preserves mean anything.
#[must_use]
pub fn discover(roots: &Roots) -> Vec<Skill> {
    let mut found: BTreeMap<String, Skill> = BTreeMap::new();

    for dir in &roots.dirs {
        for manifest in manifests(dir) {
            let Ok(text) = std::fs::read_to_string(&manifest) else {
                tracing::warn!(path = %manifest.display(), "a skill could not be read");
                continue;
            };
            let Some(skill) = parse(&manifest, &text) else {
                tracing::warn!(
                    path = %manifest.display(),
                    "a skill's frontmatter names no `name`; skipping it"
                );
                continue;
            };

            if let Some(existing) = found.get(&skill.name) {
                tracing::warn!(
                    name = skill.name.as_str(),
                    existing = %existing.location.display(),
                    duplicate = %manifest.display(),
                    "two skills claim one name; the later one wins"
                );
            }
            found.insert(skill.name.clone(), skill);
        }
    }

    found.into_values().collect()
}

/// Every `SKILL.md` under `dir`, in a stable order.
///
/// Hidden directories are walked — a config is free to name `~/.claude/skills`,
/// whose every component after the home directory is hidden — and symbolic
/// links are not followed: a link out of a skills directory is a way to have a
/// prompt composition walk somewhere nobody meant it to.
fn manifests(dir: &Path) -> Vec<PathBuf> {
    if !dir.is_dir() {
        return Vec::new();
    }

    let mut found: Vec<PathBuf> = walk(dir)
        .filter(|entry| entry.file_name() == MANIFEST && entry.path().is_file())
        .map(|entry| entry.into_path())
        .collect();
    // The walk's order is the filesystem's; the prompt's is not allowed to be.
    found.sort();

    found
}

/// The skill `text` describes, or nothing when its frontmatter names none.
///
/// `location` is only recorded, never read from — which is what lets this be
/// tested without a directory tree.
#[must_use]
pub fn parse(location: &Path, text: &str) -> Option<Skill> {
    let (frontmatter, body) = split(text)?;
    let fields = fields(frontmatter);
    let name = fields.get("name")?.trim().to_owned();
    if name.is_empty() {
        return None;
    }

    Some(Skill {
        name,
        description: fields
            .get("description")
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
        location: location.to_owned(),
        content: body.to_owned(),
    })
}

/// Loads a skill.
pub struct SkillTool {
    /// The directories this tool loads from.
    roots: Roots,
}

impl SkillTool {
    /// The tool as it ships in [`crate::Registry::with_builtins`]: **no roots
    /// at all**, so it loads nothing.
    ///
    /// Not a placeholder so much as the only honest default: this crate may
    /// not read a config, and it may not work out where ganja keeps its own
    /// directories either — both answers belong to the engine (the module's
    /// `nothing-foreign-is-discovered`). A frontend that has read a config
    /// installs [`SkillTool::over`] on top of this with the roots
    /// `instruction::skill_roots` composed — ganja's own two homes plus
    /// whatever `skills.paths` named — the way it installs `task` once it knows
    /// which agents a session may spawn.
    #[must_use]
    pub fn new() -> Self {
        Self {
            roots: Roots::none(),
        }
    }

    /// The tool over exactly `roots`.
    ///
    /// The caller resolves the roots — ganja's own two homes, then whatever
    /// `skills.paths` named — composes the system prompt's `<available_skills>`
    /// from [`discover`] over *those* roots, and hands the same value here.
    /// That shared value is what makes the prompt's list and this tool's
    /// answers the same list.
    #[must_use]
    pub fn over(roots: Roots) -> Self {
        Self { roots }
    }
}

impl Default for SkillTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn id(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        include_str!("skill.txt")
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    fn describe(&self, args: &serde_json::Value) -> String {
        let name = args
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();

        format!("skill {name}")
    }

    /// Takes no `cwd`: every root was resolved by whoever named it, so where
    /// the call happens to be working decides nothing here.
    async fn run(&self, args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;
        let skills = discover(&self.roots);

        let Some(skill) = skills.iter().find(|skill| skill.name == args.name) else {
            return Err(ToolError::Failed(not_found(&args.name, &skills)));
        };

        let dir = base_dir(skill);

        Ok(ToolOutput {
            title: format!("Loaded skill: {}", skill.name),
            output: rendered(skill, &dir),
            metadata: serde_json::json!({
                "name": skill.name,
                "dir": dir.display().to_string(),
            }),
        })
    }
}

/// The sentence a name nothing answers to gets back, listing what it could
/// have asked for.
///
/// Upstream's own (`skill/index.ts:77-79`), and a failure rather than a panic
/// because a tool result is information the model reads and acts on — the
/// list of what it *could* have asked for is the useful half. Public for the
/// same reason [`rendered`] is: the engine's `$name` expansion reports a miss
/// in this exact spelling, so the model reads one sentence wherever the miss
/// happened.
#[must_use]
pub fn not_found(name: &str, skills: &[Skill]) -> String {
    let available: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();

    format!(
        "Skill \"{name}\" not found. Available skills: {}",
        if available.is_empty() {
            "none".to_owned()
        } else {
            available.join(", ")
        }
    )
}

/// The skills `text` explicitly invokes: every `$name` token naming one of
/// `skills`, in first-appearance order, deduped.
///
/// The grammar is the OpenAI Codex CLI's `$skill-name` mention — an inline
/// token like a composer's `@file`, not a line-prefix classifier like `!` —
/// and validation against the discovered set is what keeps it safe to scan
/// free text: `$PATH` in a pasted shell snippet stays literal because nothing
/// answers to it. A token is the longest run of `[A-Za-z0-9._:-]` after a
/// `$`; when that run as a whole names no skill, trailing punctuation from
/// that same set (`.`, `:`, `_`, `-`) is trimmed one character at a time and
/// retried, so `use $porting.` invokes `porting` while `$portingfoo` invokes
/// nothing — a longer word is a different word, never a prefix match.
#[must_use]
pub fn requested_in(text: &str, skills: &[Skill]) -> Vec<String> {
    let is_token_byte =
        |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-');
    let bytes = text.as_bytes();
    let mut found: Vec<String> = Vec::new();
    let mut at = 0;

    while at < bytes.len() {
        if bytes[at] != b'$' {
            at += 1;
            continue;
        }
        let start = at + 1;
        let mut end = start;
        while end < bytes.len() && is_token_byte(bytes[end]) {
            end += 1;
        }
        // The run is ASCII throughout, so these byte offsets are char
        // boundaries.
        let mut run = &text[start..end];
        while !run.is_empty() {
            if skills.iter().any(|skill| skill.name == run) {
                if !found.iter().any(|name| name == run) {
                    found.push(run.to_owned());
                }
                break;
            }
            match run.as_bytes().last() {
                Some(b'.' | b'_' | b':' | b'-') => run = &run[..run.len() - 1],
                _ => break,
            }
        }
        at = end.max(at + 1);
    }

    found
}

/// The directory a skill's relative paths resolve against: its manifest's
/// own, with `.` for a hand-built skill whose location has none. The one
/// spelling of that rule, fed to [`rendered`] by the tool and the engine's
/// `$` expansion alike.
#[must_use]
pub fn base_dir(skill: &Skill) -> PathBuf {
    skill
        .location
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// The root `skill` was found under, for a listing that names where each
/// skill came from.
///
/// Later roots are consulted first because a later root wins a name
/// collision, so when two nested roots both prefix one location the answer
/// is the tier that actually served it. [`None`] is a skill whose location
/// no root prefixes, which only a hand-built [`Skill`] can be.
#[must_use]
pub fn origin<'a>(roots: &'a Roots, skill: &Skill) -> Option<&'a Path> {
    roots
        .dirs()
        .iter()
        .rev()
        .find(|dir| skill.location.starts_with(dir))
        .map(PathBuf::as_path)
}

/// What a loaded skill hands the model (`tool/skill.ts:45-61`).
///
/// Public because the tool is no longer the only door: the engine expands a
/// composer's `$name` invocation through this same function, and the two must
/// stay byte-identical — a model that loads a skill both ways may never see
/// two spellings of one body. Share it; never copy it.
#[must_use]
pub fn rendered(skill: &Skill, dir: &Path) -> String {
    let files: Vec<String> = beside(dir)
        .iter()
        .map(|path| format!("<file>{}</file>", path.display()))
        .collect();

    [
        format!("<skill_content name=\"{}\">", skill.name),
        format!("# Skill: {}", skill.name),
        String::new(),
        skill.content.trim().to_owned(),
        String::new(),
        format!("Base directory for this skill: {}", dir.display()),
        "Relative paths in this skill (e.g., scripts/, reference/) are relative to this base \
         directory."
            .to_owned(),
        "Note: file list is sampled.".to_owned(),
        String::new(),
        "<skill_files>".to_owned(),
        files.join("\n"),
        "</skill_files>".to_owned(),
        "</skill_content>".to_owned(),
    ]
    .join("\n")
}

/// The files a skill ships beside its manifest, sampled.
///
/// Upstream asks ripgrep for everything under the directory but the manifest,
/// capped at ten (`tool/skill.ts:36-43`); this walks the same tree with the
/// same crates `glob` uses. Sorted, where upstream takes whatever order the
/// walk produced: "sampled" should still mean the same sample twice.
fn beside(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = walk(dir)
        .filter(|entry| entry.path().is_file() && entry.file_name() != MANIFEST)
        .map(|entry| entry.into_path())
        .collect();
    found.sort();
    found.truncate(SAMPLED_FILES);

    found
}

/// The walk both readers of a skills tree share: hidden files included,
/// ignore files not consulted — those are for source trees, and a skills
/// directory that happens to sit inside one would otherwise be invisible —
/// and symbolic links not followed, capped at [`MAX_DEPTH`].
fn walk(dir: &Path) -> impl Iterator<Item = ignore::DirEntry> {
    ignore::WalkBuilder::new(dir)
        .hidden(false)
        .follow_links(false)
        .standard_filters(false)
        .max_depth(Some(MAX_DEPTH))
        .build()
        .filter_map(Result::ok)
}

#[cfg(test)]
#[path = "skill_tests.rs"]
mod tests;
