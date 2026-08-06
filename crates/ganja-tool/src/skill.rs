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
//! # Divergences
//!
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

use crate::{Tool, ToolCtx, ToolError, ToolOutput};

/// The file that makes a directory a skill.
const MANIFEST: &str = "SKILL.md";

/// Directory Claude Code keeps its skills under, scanned as upstream scans it
/// (`skill/index.ts:21-23`).
const CLAUDE_DIR: &str = ".claude";

/// The vendor-neutral spelling of the same thing (`skill/index.ts:22`).
const AGENTS_DIR: &str = ".agents";

/// The subdirectory both of the above hold their skills in.
const EXTERNAL_SUBDIR: &str = "skills";

/// The two names a project's or the global config directory may hold skills
/// under (`skill/index.ts:24`).
const CONFIG_SUBDIRS: [&str; 2] = ["skill", "skills"];

/// Turns off the `.claude` and `.agents` tiers entirely. Upstream's is
/// `OPENCODE_DISABLE_EXTERNAL_SKILLS`.
pub const DISABLE_EXTERNAL_ENV: &str = "GANJA_DISABLE_EXTERNAL_SKILLS";

/// Turns off the `.claude` tier alone, leaving `.agents`. Upstream's is
/// `OPENCODE_DISABLE_CLAUDE_CODE_SKILLS`.
pub const DISABLE_CLAUDE_ENV: &str = "GANJA_DISABLE_CLAUDE_CODE_SKILLS";

/// Most files one loaded skill lists beside its manifest
/// (`tool/skill.ts:42`).
const SAMPLED_FILES: usize = 10;

/// How deep a scan walks below a root before it stops.
///
/// A skill lives at `<root>/<name>/SKILL.md`, and upstream's glob is
/// unbounded. Bounded here because a root a config named could be a home
/// directory by accident, and a prompt composition is not a good place to walk
/// one.
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

/// Where skills are looked for.
///
/// A value rather than a scan of "wherever skills live", because the one thing
/// this must be able to say is *nothing*: a fixture run, a golden differential
/// or any test composing a prompt has to be able to hold a set of roots that
/// cannot reach the machine it is running on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Roots {
    /// The directories to scan, in the order upstream scans them — later
    /// entries win a name collision.
    dirs: Vec<PathBuf>,
}

impl Roots {
    /// No roots at all: discovery finds nothing.
    ///
    /// What a test and a fixture-built engine hold, and the reason this type
    /// exists.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// The roots a session working in `cwd` scans, upstream's `discoverSkills`
    /// (`skill/index.ts:173-233`) minus the two tiers this port answers
    /// differently:
    ///
    /// 1. `~/.claude/skills` and `~/.agents/skills`, unless
    ///    [`DISABLE_EXTERNAL_ENV`] or — for the first alone —
    ///    [`DISABLE_CLAUDE_ENV`] says otherwise;
    /// 2. the same two directory names walking up from `cwd` to the project
    ///    root;
    /// 3. `skill/` and `skills/` under the project root and under the global
    ///    config directory, which is where upstream's `config.directories()`
    ///    points.
    ///
    /// `config_dirs` is that third tier's roots, handed in rather than
    /// resolved: which directory holds this build's config is the engine's
    /// answer, and a tool that went and worked it out would be a tool that
    /// knows where config lives.
    #[must_use]
    pub fn standard(cwd: &Path, config_dirs: &[PathBuf]) -> Self {
        let mut dirs = Vec::new();
        // `.claude` alone can be switched off, which is why the set is built
        // rather than filtered at each use.
        let external: Vec<&str> = if disabled(DISABLE_CLAUDE_ENV) {
            vec![AGENTS_DIR]
        } else {
            vec![CLAUDE_DIR, AGENTS_DIR]
        };

        if !disabled(DISABLE_EXTERNAL_ENV) {
            if let Some(home) = home() {
                for name in &external {
                    dirs.push(home.join(name).join(EXTERNAL_SUBDIR));
                }
            }
            // Outermost first, so a skill nearer the file being worked on wins
            // the name — the order every other layered thing in this workspace
            // resolves in.
            let root = ganja_permission::project::Project::resolve(cwd)
                .root()
                .to_path_buf();
            let start = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
            let mut walked: Vec<PathBuf> = Vec::new();
            for directory in start.ancestors() {
                for name in &external {
                    walked.push(directory.join(name).join(EXTERNAL_SUBDIR));
                }
                if directory == root {
                    break;
                }
            }
            walked.reverse();
            dirs.extend(walked);
        }

        for directory in config_dirs {
            for name in CONFIG_SUBDIRS {
                dirs.push(directory.join(name));
            }
        }

        Self { dirs }
    }

    /// The same roots with `paths` on the end: the directories a config's
    /// `skills.paths` named, already expanded and resolved by whoever read it.
    ///
    /// Last, so a path somebody wrote down by hand outranks the tiers found by
    /// convention.
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
/// A directory that is not there contributes nothing — most of the tiers above
/// are conventions rather than promises. A `SKILL.md` that will not parse is
/// warned about and skipped, as upstream skips it: one malformed file may not
/// take the rest of a session's skills with it.
///
/// Two skills claiming one name is upstream's warning too, and upstream's
/// answer — the later scan wins (`skill/index.ts:125-138`), which is what makes
/// the ordering in [`Roots::standard`] mean anything.
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
/// Hidden directories are walked — `.claude` is one — and symbolic links are
/// not followed: a link out of a skills directory is a way to have a prompt
/// composition walk somewhere nobody meant it to.
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

/// The frontmatter and the body of a markdown file that opens with one.
///
/// A file that does not open with `---` has no frontmatter, and a skill
/// without frontmatter has no name — upstream reaches the same answer through
/// `gray-matter`, which returns empty data for such a file.
fn split(text: &str) -> Option<(&str, &str)> {
    // A byte-order mark ahead of the fence is what an editor on another
    // platform leaves behind, and it must not cost somebody their skill.
    let text = text.trim_start_matches('\u{feff}');
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;

    for (index, _) in rest.match_indices("\n---") {
        let after = &rest[index + 4..];
        // The closing fence owns its whole line: `---` inside a value is not
        // the end of the block.
        if after.is_empty() || after.starts_with('\n') || after.starts_with("\r\n") {
            let frontmatter = &rest[..index];
            let body = after
                .strip_prefix("\r\n")
                .or_else(|| after.strip_prefix('\n'));

            return Some((frontmatter, body.unwrap_or(after)));
        }
    }

    None
}

/// The scalar fields a frontmatter block names.
///
/// Everything this port asks of YAML: `key: value` at the top level, with
/// quotes stripped, plus the block scalars (`|`, `|-`, `>`, `>-`) a long
/// description is usually written as. A value that runs on into an unquoted
/// colon — which other agents accept and upstream rescues with a permissive
/// re-parse — is kept whole, because everything after the first colon is the
/// value. Nested maps and lists are skipped: no field read here is one.
fn fields(frontmatter: &str) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    let mut lines = frontmatter.lines().peekable();

    while let Some(line) = lines.next() {
        let trimmed = line.trim_end_matches('\r');
        if trimmed.trim().is_empty() || trimmed.trim_start().starts_with('#') {
            continue;
        }
        // An indented line belongs to whatever came before it, and what came
        // before it was either a block scalar this already consumed or a
        // structure this does not read.
        if trimmed.starts_with([' ', '\t']) {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim().to_owned(), value.trim());

        if matches!(value, "|" | "|-" | "|+" | ">" | ">-" | ">+") {
            let folded = value.starts_with('>');
            let mut block: Vec<String> = Vec::new();
            while let Some(next) = lines.peek() {
                let next = next.trim_end_matches('\r');
                if !next.trim().is_empty() && !next.starts_with([' ', '\t']) {
                    break;
                }
                block.push(next.trim().to_owned());
                lines.next();
            }
            while block.last().is_some_and(String::is_empty) {
                block.pop();
            }
            fields.insert(key, block.join(if folded { " " } else { "\n" }));
            continue;
        }

        fields.insert(key, unquote(value).to_owned());
    }

    fields
}

/// `value` without the quotes it may be wrapped in.
fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }

    value
}

/// Whether `variable` is set to something this build reads as true, by the
/// same rule the model catalog's own switch uses.
fn disabled(variable: &str) -> bool {
    std::env::var(variable).is_ok_and(|value| {
        let value = value.trim().to_ascii_lowercase();
        value == "1" || value == "true"
    })
}

/// This machine's home directory, or nothing when it has none to speak of.
fn home() -> Option<PathBuf> {
    etcetera::home_dir().ok()
}

/// Where a tool's roots come from.
enum Lookup {
    /// The conventional tiers, worked out against the working directory of
    /// whichever call runs — which is the only directory a registry built
    /// before a session started can be sure of.
    Conventional,
    /// Exactly these, wherever the call is working.
    Fixed(Roots),
}

/// Loads a skill.
pub struct SkillTool {
    /// How this tool answers "where are the skills".
    lookup: Lookup,
}

impl SkillTool {
    /// The tool as it ships in [`crate::Registry::with_builtins`]: the
    /// conventional roots, resolved against the working directory of whichever
    /// call runs it.
    ///
    /// What it cannot know is what a config said, because a tool may not read
    /// one — see [`SkillTool::over`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            lookup: Lookup::Conventional,
        }
    }

    /// The tool over exactly `roots`.
    ///
    /// What an engine installs over the default once it has read a config, the
    /// way it installs `task` once it knows which agents a session may spawn:
    /// the caller resolves the roots — [`Roots::standard`] plus whatever
    /// `skills.paths` named — composes the system prompt's
    /// `<available_skills>` from [`discover`] over *those* roots, and hands the
    /// same value here. That shared value is what makes the prompt's list and
    /// this tool's answers the same list.
    ///
    /// [`Roots::none`] is the other reason it exists: a fixture, a golden
    /// differential or any test composing a prompt holds roots that cannot
    /// reach the machine running it.
    #[must_use]
    pub fn over(roots: Roots) -> Self {
        Self {
            lookup: Lookup::Fixed(roots),
        }
    }

    /// Where this tool looks, for a call working in `cwd`.
    fn roots(&self, cwd: &Path) -> Roots {
        match &self.lookup {
            Lookup::Conventional => Roots::standard(cwd, &[]),
            Lookup::Fixed(roots) => roots.clone(),
        }
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

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;
        let skills = discover(&self.roots(&ctx.cwd));

        let Some(skill) = skills.iter().find(|skill| skill.name == args.name) else {
            let available: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();

            // Upstream's own sentence (`skill/index.ts:77-79`), and the reason
            // it is a failure rather than a panic: a tool result is
            // information the model reads and acts on, so the list of what it
            // *could* have asked for is the useful half.
            return Err(ToolError::Failed(format!(
                "Skill \"{}\" not found. Available skills: {}",
                args.name,
                if available.is_empty() {
                    "none".to_owned()
                } else {
                    available.join(", ")
                }
            )));
        };

        let dir = skill
            .location
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);

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

/// What a loaded skill hands the model (`tool/skill.ts:45-61`).
fn rendered(skill: &Skill, dir: &Path) -> String {
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
    use std::{path::PathBuf, sync::Arc};

    use tokio_util::sync::CancellationToken;

    use super::{Roots, Skill, SkillTool};
    use crate::{FileTimes, Tool as _, ToolCtx, ToolError};

    /// A skill directory tree: `<root>/<name>/SKILL.md` holding `text`.
    fn write(root: &std::path::Path, name: &str, text: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("the fixture's directories are creatable");
        let manifest = dir.join("SKILL.md");
        std::fs::write(&manifest, text).expect("the fixture is writable");

        manifest
    }

    fn ctx(cwd: &std::path::Path) -> ToolCtx {
        ToolCtx {
            cwd: cwd.to_path_buf(),
            cancel: CancellationToken::new(),
            call_id: "call-1".to_owned(),
            files: Arc::new(FileTimes::default()),
            credentials: crate::Credentials::Unguarded,
            spawn: None,
            ask: None,
        }
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
    /// the golden differential depend on.
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

    /// The one directory a config named is scanned; the conventional tiers are
    /// unreachable from a temporary directory that holds none of them.
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

    /// The conventional tiers, as directories rather than as a scan: what is
    /// asserted is the list `standard` would look in, which is the half of
    /// discovery that cannot be tested against a temporary directory without
    /// moving this machine's home.
    #[test]
    fn the_conventional_roots_are_the_ones_upstream_scans() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let config = dir.path().join("config");
        let roots = Roots::standard(dir.path(), std::slice::from_ref(&config));
        let dirs = roots.dirs();

        assert!(
            dirs.contains(&dir.path().join(".claude").join("skills"))
                || dirs.iter().any(|root| root.ends_with(".claude/skills")),
            "the project's own claude-code skills are a tier: {dirs:?}"
        );
        assert!(
            dirs.iter().any(|root| root.ends_with(".agents/skills")),
            "and the vendor-neutral spelling of it: {dirs:?}"
        );
        assert_eq!(
            dirs.last(),
            Some(&config.join("skills")),
            "the config directory's tiers come last: {dirs:?}"
        );
        assert!(
            dirs.contains(&config.join("skill")),
            "both spellings upstream accepts: {dirs:?}"
        );
    }
}
