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
//!    `Instructions from: {path}` followed by its contents.
//!
//! Which files apply is [`paths`], and the order is upstream's: the one global
//! file, then the project's own, then whatever `instructions` in the config
//! named. A file appears once however many routes reach it, at the position the
//! first route put it.
//!
//! # Divergences
//!
//! - **D18** — the prompt is composed once, at startup, where upstream rebuilds
//!   it for every request. The two things that can go stale in a long session
//!   are the date and the working directory; neither moves during a session
//!   ganja can have, since the working directory is captured once at engine
//!   construction, and a session that outlives midnight tells the model
//!   yesterday's date.
//! - **D19** — the environment block's model line names the model the way the
//!   provider is asked for it. Upstream additionally spells the `provider/model`
//!   pair, which is not available where this is composed.
//! - **D20** — the date is UTC. Upstream renders the machine's local date;
//!   there is no date library in this workspace, and reaching `localtime_r`
//!   through `libc` for one line — unsafe, and unix-only — buys less than it
//!   costs.
//! - **D2** — `http(s)` entries in `instructions` are skipped with a warning
//!   rather than fetched.

use std::{
    collections::BTreeSet,
    fmt::Write as _,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use etcetera::base_strategy::{BaseStrategy as _, Xdg};

use crate::{config::Config, project::Project};

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

/// Directory the global instruction file lives in, under the XDG config home.
const DIRECTORY: &str = "ganja";

/// Project instruction file names, most preferred first. The **first name with
/// any match at all** wins outright, so a checkout carrying both `AGENTS.md`
/// and `CLAUDE.md` sends the first and never mixes the two.
const PROJECT: [&str; 3] = ["AGENTS.md", "CLAUDE.md", "CONTEXT.md"];

/// What every instruction file is introduced with.
const HEADER: &str = "Instructions from: ";

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
    compose(&global_files(), config, cwd, model_id)
}

/// [`system_prompt`], with the global instruction candidates handed in.
///
/// The split is what lets the tests below prove the composition without the
/// machine running them contributing an `AGENTS.md` of its own — and the global
/// candidates really are an input to discovery rather than something it knows.
fn compose(global: &[PathBuf], config: &Config, cwd: &Path, model_id: &str) -> Option<String> {
    let mut prompt = String::with_capacity(base_prompt(model_id).len() * 2);
    prompt.push_str(base_prompt(model_id));
    prompt.push('\n');
    prompt.push_str(&environment(cwd, model_id));

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

    (!prompt.is_empty()).then_some(prompt)
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

/// Every instruction file that applies, in the order it should be sent.
///
/// Deduplicated by resolved path — the same file reached twice appears once, at
/// the position the first route put it.
#[must_use]
pub fn paths(config: &Config, cwd: &Path) -> Vec<PathBuf> {
    discover(&global_files(), config, cwd)
}

/// [`paths`], with the global instruction candidates handed in.
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
fn global_files() -> Vec<PathBuf> {
    let Ok(base) = Xdg::new() else {
        return Vec::new();
    };

    vec![
        base.config_dir().join(DIRECTORY).join(GLOBAL),
        CLAUDE_GLOBAL
            .iter()
            .fold(base.home_dir().to_owned(), |path, part| path.join(part)),
    ]
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
/// Ignore files are deliberately not consulted: `.gitignore` says what a
/// *search* should show, and this is not a search — it is a path the user wrote
/// down, and git's opinion of a file says nothing about whether they meant it.
/// The hidden-file rule is kept, so an unanchored pattern does not descend into
/// `.git`; a pattern naming a dotfile directly still matches it, because an
/// override match is checked before the hidden rule.
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
    use std::{fs, path::Path};

    use tempfile::TempDir;

    use super::{
        ANTHROPIC, DEFAULT, GPT, base_prompt, civil_date, compose, discover, environment, find_up,
        glob, resolve_entry, today,
    };
    use crate::config::Config;

    fn temporary() -> TempDir {
        TempDir::new().expect("a temporary directory is creatable")
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
        let owners: Vec<String> = found
            .iter()
            .map(|path| {
                path.strip_prefix(fs::canonicalize(&root).expect("the fixture exists"))
                    .unwrap_or(path)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();

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
        let names: Vec<String> = found
            .iter()
            .map(|path| {
                path.strip_prefix(&root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();

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
        let prompt = compose(&[], &config, &root, "claude-sonnet-5").expect("a prompt is composed");

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
        let prompt = compose(&[], &config, &root, "fake-1").expect("a prompt is composed");

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
}
