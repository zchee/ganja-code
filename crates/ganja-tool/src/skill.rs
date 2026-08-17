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

    let mut found: Vec<PathBuf> = ignore::WalkBuilder::new(dir)
        .hidden(false)
        .follow_links(false)
        // Ignore files are for source trees; a skills directory that happens
        // to sit inside one would otherwise be invisible.
        .standard_filters(false)
        .max_depth(Some(MAX_DEPTH))
        .build()
        .filter_map(Result::ok)
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
    let mut found: Vec<PathBuf> = ignore::WalkBuilder::new(dir)
        .hidden(false)
        .follow_links(false)
        .standard_filters(false)
        .max_depth(Some(MAX_DEPTH))
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file() && entry.file_name() != MANIFEST)
        .map(|entry| entry.into_path())
        .collect();
    found.sort();
    found.truncate(SAMPLED_FILES);

    found
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Roots, Skill, SkillTool};
    use crate::{Tool as _, ToolCtx, ToolError};

    /// A skill directory tree: `<root>/<name>/SKILL.md` holding `text`.
    fn write(root: &std::path::Path, name: &str, text: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("the fixture's directories are creatable");
        let manifest = dir.join("SKILL.md");
        std::fs::write(&manifest, text).expect("the fixture is writable");

        manifest
    }

    fn ctx(cwd: &std::path::Path) -> ToolCtx {
        ToolCtx::fixture(cwd.to_path_buf())
    }

    /// A skill whose frontmatter is the two fields upstream reads, and a body
    /// that is everything after the fence.
    #[test]
    fn a_manifest_is_its_frontmatter_and_the_markdown_below_it() {
        let skill = super::parse(
            std::path::Path::new("/skills/porting/SKILL.md"),
            "---\nname: porting\ndescription: How to port a module.\n---\n# Porting\n\nStep one.\n",
        )
        .expect("the fixture names a skill");

        assert_eq!(
            skill,
            Skill {
                name: "porting".to_owned(),
                description: Some("How to port a module.".to_owned()),
                location: PathBuf::from("/skills/porting/SKILL.md"),
                content: "# Porting\n\nStep one.\n".to_owned(),
            }
        );
    }

    /// The frontmatter shapes real skills are written in, all of which have to
    /// survive a parser that is not a YAML library.
    #[test]
    fn the_frontmatter_shapes_other_agents_write_are_read_as_written() {
        let cases = [
            (
                "quoted values",
                "---\nname: \"a\"\ndescription: 'b: with a colon'\n---\nbody",
                Some(("a", Some("b: with a colon"))),
            ),
            (
                "an unquoted colon, which upstream rescues with a second parse",
                "---\nname: a\ndescription: Use when: the task matches\n---\nbody",
                Some(("a", Some("Use when: the task matches"))),
            ),
            (
                "a literal block scalar",
                "---\nname: a\ndescription: |\n  first\n  second\n---\nbody",
                Some(("a", Some("first\nsecond"))),
            ),
            (
                "a folded block scalar",
                "---\nname: a\ndescription: >-\n  first\n  second\n---\nbody",
                Some(("a", Some("first second"))),
            ),
            (
                "comments and blank lines",
                "---\n# a comment\n\nname: a\n---\nbody",
                Some(("a", None)),
            ),
            (
                "keys this port does not read",
                "---\nname: a\nallowed-tools:\n  - read\n  - grep\nlicense: MIT\n---\nbody",
                Some(("a", None)),
            ),
            (
                "carriage returns",
                "---\r\nname: a\r\n---\r\nbody",
                Some(("a", None)),
            ),
            ("no frontmatter at all", "# just markdown\n", None),
            (
                "frontmatter naming no name",
                "---\ndescription: b\n---\nbody",
                None,
            ),
            ("an empty name", "---\nname:   \n---\nbody", None),
            ("an unterminated fence", "---\nname: a\nbody", None),
        ];

        for (what, text, expected) in cases {
            let parsed = super::parse(std::path::Path::new("SKILL.md"), text);
            let actual = parsed
                .as_ref()
                .map(|skill| (skill.name.as_str(), skill.description.as_deref()));

            assert_eq!(actual, expected, "{what}: {text:?}");
        }
    }

    /// A `---` inside a value does not end the block, and the body is what
    /// follows the fence that does.
    #[test]
    fn a_fence_ends_the_frontmatter_only_on_a_line_of_its_own() {
        let skill = super::parse(
            std::path::Path::new("SKILL.md"),
            "---\nname: a\ndescription: a --- b\n---\nbody --- still body\n",
        )
        .expect("the fixture names a skill");

        assert_eq!(skill.description.as_deref(), Some("a --- b"));
        assert_eq!(skill.content, "body --- still body\n");
    }

    /// Roots that name nothing find nothing — the property a fixture run and
    /// the golden differential depend on, and the floor every other set is
    /// built up from.
    #[test]
    fn roots_that_name_nowhere_discover_nothing() {
        assert!(super::discover(&Roots::none()).is_empty());
        assert!(Roots::none().dirs().is_empty());
    }

    #[test]
    fn every_skill_under_a_root_is_found_and_sorted_by_name() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        write(
            dir.path(),
            "beta",
            "---\nname: beta\ndescription: second\n---\nb",
        );
        write(
            dir.path(),
            "alpha",
            "---\nname: alpha\ndescription: first\n---\na",
        );
        // Nested one deeper, which upstream's `**/SKILL.md` also reaches.
        write(
            &dir.path().join("nested"),
            "gamma",
            "---\nname: gamma\n---\ng",
        );
        // Not a skill, and not a reason to fail the rest.
        std::fs::write(dir.path().join("beta").join("notes.md"), "x").expect("writable");
        write(dir.path(), "broken", "no frontmatter here");

        let found = super::discover(&Roots::none().with_paths([dir.path().to_path_buf()]));
        let names: Vec<&str> = found.iter().map(|skill| skill.name.as_str()).collect();

        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
        assert_eq!(found[2].description, None, "gamma names no description");
    }

    /// Two skills, one name: the later root wins, which is what makes the
    /// order of the tiers mean something.
    #[test]
    fn the_last_root_to_claim_a_name_is_the_one_that_answers_to_it() {
        let first = tempfile::tempdir().expect("a scratch directory");
        let second = tempfile::tempdir().expect("a scratch directory");
        write(
            first.path(),
            "porting",
            "---\nname: porting\n---\nthe first",
        );
        write(
            second.path(),
            "porting",
            "---\nname: porting\n---\nthe second",
        );

        let found = super::discover(
            &Roots::none().with_paths([first.path().to_path_buf(), second.path().to_path_buf()]),
        );

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].content, "the second");
    }

    /// A skill that exists only by name, for scanning text against.
    fn named(name: &str) -> Skill {
        Skill {
            name: name.to_owned(),
            description: None,
            location: PathBuf::from("SKILL.md"),
            content: String::new(),
        }
    }

    /// A `$` token invokes a skill only when a discovered name answers to it,
    /// which is what keeps scanning free text safe: pasted shell prompts and
    /// environment variables match nothing and stay literal.
    #[test]
    fn a_dollar_token_invokes_only_a_name_something_answers_to() {
        let skills = [named("porting"), named("my-skill"), named("v1.2")];
        let cases: [(&str, &[&str]); 13] = [
            ("use $porting now", &["porting"]),
            ("$porting", &["porting"]),
            ("ends with $porting", &["porting"]),
            ("echo $PATH; then $porting", &["porting"]),
            ("$ cargo build", &[]),
            ("$", &[]),
            ("no tokens at all", &[]),
            ("$portingfoo is a different word, never a prefix match", &[]),
            (
                "use $porting. then $my-skill: done",
                &["porting", "my-skill"],
            ),
            ("a dot the name owns survives the trim: $v1.2.", &["v1.2"]),
            ("$my-skill-", &["my-skill"]),
            (
                "$my-skill then $porting then $my-skill again",
                &["my-skill", "porting"],
            ),
            ("日本語の中の$porting。も見つかる", &["porting"]),
        ];

        for (text, expected) in cases {
            assert_eq!(super::requested_in(text, &skills), expected, "{text:?}");
        }
    }

    /// A listing names the tier that actually serves each name: the later
    /// root when two prefix one location, and nothing for a location outside
    /// every root.
    #[test]
    fn a_skills_origin_is_the_root_that_serves_it() {
        let outer = PathBuf::from("/skills");
        let inner = PathBuf::from("/skills/nested");
        let roots = super::Roots::none().with_paths([outer.clone(), inner.clone()]);

        let nested = Skill {
            location: inner.join("porting").join("SKILL.md"),
            ..named("porting")
        };
        assert_eq!(super::origin(&roots, &nested), Some(inner.as_path()));

        let outer_only = Skill {
            location: outer.join("tdd").join("SKILL.md"),
            ..named("tdd")
        };
        assert_eq!(super::origin(&roots, &outer_only), Some(outer.as_path()));

        assert_eq!(super::origin(&roots, &named("floating")), None);
    }

    /// The tool's refusal and the engine's expansion miss read the same
    /// sentence, because both call this.
    #[test]
    fn a_missing_name_is_reported_with_the_names_there_are() {
        assert_eq!(
            super::not_found("missing", &[named("porting"), named("tdd")]),
            "Skill \"missing\" not found. Available skills: porting, tdd"
        );
        assert_eq!(
            super::not_found("missing", &[]),
            "Skill \"missing\" not found. Available skills: none"
        );
    }

    /// The one directory a config named is scanned, and what a call gets back
    /// out of it.
    #[tokio::test]
    async fn a_loaded_skill_hands_over_its_body_its_base_directory_and_its_files() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let manifest = write(
            dir.path(),
            "porting",
            "---\nname: porting\ndescription: How to port.\n---\n# Porting\n\nStep one.\n",
        );
        let base = manifest.parent().expect("a manifest has a directory");
        std::fs::create_dir_all(base.join("scripts")).expect("creatable");
        std::fs::write(base.join("scripts").join("run.sh"), "#!/bin/sh\n").expect("writable");
        std::fs::write(base.join("reference.md"), "notes").expect("writable");

        let out = SkillTool::over(Roots::none().with_paths([dir.path().to_path_buf()]))
            .run(serde_json::json!({ "name": "porting" }), &ctx(dir.path()))
            .await
            .expect("the skill is there to load");

        assert_eq!(out.title, "Loaded skill: porting");
        assert!(
            out.output
                .starts_with("<skill_content name=\"porting\">\n# Skill: porting\n"),
            "the output opens the way upstream opens it: {}",
            out.output
        );
        assert!(
            out.output.contains("\n# Porting\n\nStep one.\n"),
            "the body is the markdown below the frontmatter: {}",
            out.output
        );
        assert!(
            out.output.contains(&format!(
                "Base directory for this skill: {}",
                base.display()
            )),
            "a relative path in a skill needs the directory it is relative to: {}",
            out.output
        );
        assert!(
            out.output.contains(&format!(
                "<file>{}</file>",
                base.join("reference.md").display()
            )) && out.output.contains(&format!(
                "<file>{}</file>",
                base.join("scripts").join("run.sh").display()
            )),
            "the files beside it are listed absolute: {}",
            out.output
        );
        assert!(
            !out.output.contains("SKILL.md"),
            "the manifest is the content, not one of the files beside it: {}",
            out.output
        );
        assert_eq!(
            out.metadata,
            serde_json::json!({ "name": "porting", "dir": base.display().to_string() })
        );
    }

    /// A name nothing answers to is information the model can act on: the
    /// message lists what it could have asked for.
    #[tokio::test]
    async fn a_skill_nobody_has_is_a_failure_that_names_the_ones_there_are() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        write(dir.path(), "porting", "---\nname: porting\n---\nbody");

        let refused = SkillTool::over(Roots::none().with_paths([dir.path().to_path_buf()]))
            .run(serde_json::json!({ "name": "missing" }), &ctx(dir.path()))
            .await
            .expect_err("nothing answers to that name");

        assert!(
            matches!(&refused, ToolError::Failed(message)
                if message == "Skill \"missing\" not found. Available skills: porting"),
            "got {refused:?}"
        );

        let empty = tempfile::tempdir().expect("a scratch directory");
        let refused = SkillTool::over(Roots::none().with_paths([empty.path().to_path_buf()]))
            .run(serde_json::json!({ "name": "missing" }), &ctx(empty.path()))
            .await
            .expect_err("there are no skills at all");

        assert!(
            matches!(&refused, ToolError::Failed(message) if message.ends_with("Available skills: none")),
            "got {refused:?}"
        );
    }

    #[tokio::test]
    async fn a_call_without_a_name_is_refused() {
        let dir = tempfile::tempdir().expect("a scratch directory");

        let refused = SkillTool::new()
            .run(serde_json::json!({}), &ctx(dir.path()))
            .await
            .expect_err("there is nothing to load");

        assert!(
            matches!(refused, ToolError::InvalidArgs(_)),
            "got {refused:?}"
        );
    }

    /// Loading a skill runs unasked, which is upstream's answer too: its
    /// defaults open with `"*": "allow"` and name `skill` nowhere
    /// (`agent/agent.ts:174-193`), so nothing turns it into a question. The
    /// content it loads is a file already on this machine, and the tool that
    /// would act on it is gated on its own account.
    #[test]
    fn loading_a_skill_runs_unasked_the_way_upstream_leaves_it() {
        let permissions = ganja_permission::permission::Permissions::default();

        assert_eq!(
            permissions
                .gate(
                    SkillTool::new().id(),
                    &serde_json::json!({ "name": "porting" })
                )
                .action,
            ganja_permission::permission::Decision::Allow
        );
        assert!(
            !ganja_permission::permission::ASK_BY_DEFAULT.contains(&"skill"),
            "and it is not in the ask-by-default table either"
        );
    }

    #[test]
    fn the_prompt_and_schema_are_what_the_model_is_given() {
        let tool = SkillTool::new();
        let schema = serde_json::to_value(tool.schema()).expect("a schema is JSON");

        assert_eq!(tool.id(), "skill");
        assert_eq!(
            tool.describe(&serde_json::json!({ "name": "porting" })),
            "skill porting"
        );
        assert!(
            tool.description()
                .starts_with("Load a specialized skill when the task at hand matches")
        );
        assert_eq!(schema["required"], serde_json::json!(["name"]));
    }

    /// Where the layering is: this crate scans what it was handed and works
    /// nothing out for itself. Every directory name in the argument is planted
    /// here — the two foreign ones this build never reads, the two generic
    /// project-root names it also never reads, **and ganja's own
    /// `.ganja/skills`, which a session does read** — and the tool as it ships
    /// finds none of them, because which directories are default is a question
    /// about where ganja keeps its things and that question is answered a crate
    /// up (`ganja-core`'s `config::default_skill_dirs`, composed into roots by
    /// `instruction::skill_roots`).
    ///
    /// The consequence worth having: the machine running this decides nothing.
    /// A set of roots that is empty cannot reach a home directory, so there is
    /// no `HOME` here to redirect and no laptop whose contents could change the
    /// answer.
    #[tokio::test]
    async fn the_shipped_tool_scans_only_the_directories_it_was_handed() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let cwd = dir.path();
        for tier in [
            cwd.join(".claude").join("skills"),
            cwd.join(".agents").join("skills"),
            cwd.join("skill"),
            cwd.join("skills"),
            cwd.join(".ganja").join("skills"),
        ] {
            write(
                &tier,
                "ambient",
                "---\nname: ambient\ndescription: found by convention.\n---\nb",
            );
        }

        assert!(
            Roots::none().dirs().is_empty(),
            "the floor every set is built from names nowhere"
        );
        assert!(
            super::discover(&Roots::none()).is_empty(),
            "so a scan of it finds nothing, with five candidate directories on the disk"
        );

        let refused = SkillTool::new()
            .run(serde_json::json!({ "name": "ambient" }), &ctx(cwd))
            .await
            .expect_err("this tool was handed no directory, so it found none");

        assert!(
            matches!(&refused, ToolError::Failed(message)
                if message == "Skill \"ambient\" not found. Available skills: none"),
            "including ganja's own, which only a caller that resolved it can supply: got {refused:?}"
        );
    }

    /// A foreign tier is *unasked*, not unreachable. Naming upstream's own
    /// directory is all it takes to get upstream's own behaviour back, which is
    /// what makes its removal from the defaults a change of default rather than
    /// a loss of the feature.
    #[tokio::test]
    async fn upstreams_own_tier_is_reachable_by_naming_it() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let claude = dir.path().join(".claude").join("skills");
        write(
            &claude,
            "porting",
            "---\nname: porting\ndescription: How to port.\n---\nRead the upstream file first.",
        );

        let roots = Roots::none().with_paths([claude.clone()]);
        assert_eq!(
            roots.dirs(),
            [claude],
            "the named directory is the whole of the set"
        );

        let found = super::discover(&roots);
        let names: Vec<&str> = found.iter().map(|skill| skill.name.as_str()).collect();
        assert_eq!(names, vec!["porting"]);

        let out = SkillTool::over(roots)
            .run(serde_json::json!({ "name": "porting" }), &ctx(dir.path()))
            .await
            .expect("a named directory's skill is loadable");

        assert!(
            out.output.contains("Read the upstream file first."),
            "and it is the body that comes back: {}",
            out.output
        );
    }
}
