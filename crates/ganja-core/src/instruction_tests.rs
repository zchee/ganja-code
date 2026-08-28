use std::fs;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use tempfile::TempDir;

use super::{
    ANTHROPIC, DEFAULT, GPT, HEADER, NESTED_MAX, base_prompt, date_at, discover, environment,
    find_up, glob, joined, nested_files, nested_suffix, resolve_entry, resolved, skill,
    skills_block, suffix_from, suffix_measure,
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
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
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
        assert!(base_prompt(model) == expected, "{model} picked the wrong prompt");
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
                path.parent().and_then(Path::file_name).unwrap_or_default().to_string_lossy(),
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
        .map(|path| path.file_name().unwrap_or_default().to_string_lossy().into())
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
    plant(&root.join("packages").join("core").join("AGENTS.md"), "core");
    plant(&root.join("packages").join("web").join("README.md"), "no");

    let found = glob(&root, "packages/*/AGENTS.md");
    let names: Vec<String> = found.iter().map(|path| under(&root, path)).collect();

    assert_eq!(names, vec!["packages/core/AGENTS.md", "packages/web/AGENTS.md"]);
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

    assert!(block.starts_with("You are powered by the model named claude-sonnet-5."), "{block}");
    assert!(block.contains("  Is directory a git repo: yes\n"), "{block}");
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

    let config = Config { instructions: vec!["docs/*.md".to_owned()], ..Config::default() };
    // The two halves through the joiner the engine composes with, which is
    // what makes "the base comes first" a fact about the real seam rather
    // than about this test's own concatenation.
    let prompt = joined(
        Some(base_prompt("claude-sonnet-5")),
        suffix_from(&[], &skill::Roots::none(), &config, &root, "claude-sonnet-5").as_deref(),
    )
    .expect("a prompt is composed");

    assert!(prompt.starts_with(ANTHROPIC), "the base prompt comes first");
    assert!(
        prompt.contains("\nYou are powered by the model named claude-sonnet-5."),
        "the environment block follows it"
    );
    let agents = prompt.find("Instructions from: ").expect("the project file is attached");
    let api = prompt.find("prefer explicit types").expect("a configured file is attached");
    assert!(agents < api, "the project tier precedes the configured one");
    assert!(prompt.contains("always run the tests"));
    assert!(!prompt.contains("style.md"), "an empty file contributes nothing, not even its header");
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

    let config = Config { instructions: vec!["docs/*.md".to_owned()], ..Config::default() };
    let prompt = suffix_from(&[], &skill::Roots::none(), &config, &root, "fake-1")
        .expect("a prompt suffix is composed");

    assert!(prompt.contains("kept"));
    assert!(!prompt.contains("gone.md"), "{prompt}");
}

#[test]
fn the_date_is_spelled_the_way_upstream_spells_it() {
    let cases = [
        (0_i64, "Thu Jan 01 1970"),
        (59, "Sun Mar 01 1970"),
        // 2000 is a leap year; 2100 is not, which is what makes the
        // century rule worth a case of its own.
        (11_016, "Tue Feb 29 2000"),
        (20_577, "Mon May 04 2026"),
        (47_541, "Mon Mar 01 2100"),
    ];

    for (days, expected) in cases {
        let timestamp = Timestamp::from_second(days * 86_400).expect("the day is in range");
        assert_eq!(date_at(timestamp), expected, "day {days}");
    }
}

#[test]
fn a_prompt_date_has_the_exact_shape_it_promises() {
    let timestamp = Timestamp::from_second(20_577 * 86_400).expect("the day is in range");

    assert_eq!(date_at(timestamp), "Mon May 04 2026");
}

/// Writes a skill at `<root>/<name>/SKILL.md`.
fn plant_skill(root: &Path, name: &str, frontmatter: &str) {
    plant(&root.join(name).join("SKILL.md"), &format!("---\n{frontmatter}\n---\n# {name}\n"));
}

/// The block is upstream's, field for field, and the skills in it are
/// sorted by name whatever order the disk offered them in.
#[test]
fn the_skills_block_is_the_one_upstream_composes() {
    let directory = temporary();
    let root = directory.path().join("api");
    checkout(&root);
    let skills = root.join("skills");
    plant_skill(&skills, "porting", "name: porting\ndescription: How to port.");
    plant_skill(&skills, "auditing", "name: auditing\ndescription: How to audit.");

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
    plant_skill(&skills, "porting", "name: porting\ndescription: How to port.");

    let bare = suffix_from(&[], &skill::Roots::none(), &Config::default(), &root, "fake-1")
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

    let instructions = composed.find("always run the tests").expect("the project file is attached");
    let block = composed.find("<available_skills>").expect("the skill is advertised");
    assert!(instructions < block, "upstream puts the skills after the instructions: {composed}");
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
    plant_skill(&elsewhere, "porting", "name: porting\ndescription: How to port.");

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
        skill::discover(&roots).iter().any(|found| found.name == "porting"),
        "and it is scanned"
    );
    // Nothing was fetched, and nothing failed for not having been: the URL
    // contributes no root at all.
    assert!(
        !roots.dirs().iter().any(|dir| dir.display().to_string().contains("example.invalid")),
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
        plant_skill(&tier, name, &format!("name: {name}\ndescription: Found by convention."));
    }

    let roots = super::skill_roots(&Config::default(), &cwd);
    let found: Vec<String> = skill::discover(&roots).into_iter().map(|skill| skill.name).collect();
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
        assert!(!composed.contains(foreign), "and the model is never told about it: {composed}");
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
    plant_skill(&skills, "porting", "name: porting\ndescription: How to port.");

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
    let instructions: String =
        suffix.chars().skip(measure.environment).take(measure.instructions).collect();
    assert!(
        instructions.contains("always run the tests"),
        "the middle part holds the instruction files: {instructions}"
    );
    let skills_part: String =
        suffix.chars().skip(measure.environment + measure.instructions).collect();
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

    let suffix = suffix_from(&[], &skill::Roots::none(), &Config::default(), &root, "fake-1")
        .expect("the environment block always says something");
    let measure = suffix_measure(&suffix);

    assert_eq!(measure.environment, suffix.chars().count());
    assert_eq!(measure.instructions, 0);
    assert_eq!(measure.skills, 0);
}

/// The nested walk (**D480**), named the way the assertions below read it:
/// what a session working at `root` walks in after touching `touched`.
fn walked(root: &Path, touched: &[PathBuf]) -> Vec<String> {
    nested_files(root, root, touched).iter().map(|path| under(&resolved(root), path)).collect()
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

    assert_eq!(walked(&root, &[sub.join("one.rs"), sub.join("two.rs")]), vec!["sub/AGENTS.md"],);
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

    assert_eq!(walked(&root, &[sub.join("brand-new.rs")]), vec!["sub/AGENTS.md"],);
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
    assert!(block.len() < long.len(), "a clamped file is shorter than the file");
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

    assert!(section.starts_with(&format!("\n{}", super::MEMORY_HEAD)), "{section}");
    assert!(
        section.contains(&format!("{HEADER}{index}\n- style: prefers explicit types")),
        "{section}"
    );
    let facts = section.find("prefers explicit types").expect("the index is quoted");
    let upkeep = section.find("Keeping it: record a fact").expect("the upkeep block follows it");
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
    assert!(!memory.exists(), "and composing a prompt creates nothing on disk");
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
