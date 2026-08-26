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
