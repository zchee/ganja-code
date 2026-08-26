//! Skills end to end: what a session is told it can load, and what loading one
//! actually hands the model.
//!
//! The frontmatter parser, the discovery walk and the `<available_skills>`
//! block are unit-tested where each is written (`ganja-tool`'s `skill` module
//! and `src/instruction.rs`). What is proved here is the join: that the list a
//! model is offered in its system prompt and the list a `skill` call can load
//! from are **the same list**, because both are built from one
//! `instruction::skill_roots` value — and which directories that list is drawn
//! from, with every directory upstream helps itself to sitting on the disk
//! beside ganja's own while it is.
//!
//! Four phases, in the order a reader needs them: ganja's own two homes are
//! read and nothing foreign is; a directory a config names is read too, and
//! ranks above them; the directory *upstream* would have taken for itself is
//! read the moment somebody names it there; and then where ganja's own global
//! home actually **is** — `$GANJA_CONFIG_HOME`, else `<XDG config>/ganja`, else
//! `~/.ganja` — proved per tier and proved to be one seam, by moving the home
//! and watching the global `AGENTS.md` move with the skills. The standing user
//! ruling behind the first and third is recorded at `tool::skill`'s module
//! docs.
//!
//! # Process-wide state
//!
//! One test, one binary. `HOME` **and** `XDG_CONFIG_HOME` are redirected
//! because both default tiers are resolved against them — one of the two hangs
//! off the config home, and phase 3 plants under `~/.claude/skills` and asks
//! for it by that spelling. `GANJA_CONFIG_HOME` is **removed** at the top for
//! the same reason and set deliberately in phase 4. A suite that read the
//! developer's own directories would assert something different on every
//! machine, and the golden differential would be comparing this port against
//! whatever that developer happened to have installed. `XDG_DATA_HOME` is
//! redirected for the usual reason — nothing here may read or write the real
//! user's stored permissions or spilled output.

use std::sync::Arc;

use ganja_core::{
    Config,
    Engine,
    // Spelled through the module rather than the crate root: `WebfetchConfig`
    // is re-exported there and this is not, and adding it is a one-line change
    // to a file this lane does not own.
    config::SkillsConfig,
    instruction,
    permission::Permissions,
    protocol::{Command, PartBody, ToolState},
    provider::ChatRequest,
    tool::{
        Registry,
        skill::{Roots, SkillTool},
    },
};
use ganja_testkit::{ScriptedProvider, drain, says, tool_call};

/// Model both engines ask for; nothing depends on its family.
const MODEL: &str = "skills-model";

/// Writes a skill at `<root>/<name>/SKILL.md` and returns the directory it
/// went into.
fn plant(root: &std::path::Path, name: &str, frontmatter: &str, body: &str) -> std::path::PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).expect("the fixture's directories are creatable");
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\n{frontmatter}\n---\n{body}"),
    )
    .expect("the fixture is writable");

    dir
}

/// The system prompt of the first request a scripted provider was sent.
fn system(seen: &Arc<std::sync::Mutex<Vec<ChatRequest>>>) -> String {
    seen.lock()
        .expect("the request log is never poisoned")
        .first()
        .expect("the engine reached the provider")
        .system
        .clone()
        .unwrap_or_default()
}

/// What every tool call in `events` handed back, whether it finished or
/// failed — the two are separate lists because which one a phase expects is
/// the assertion.
fn results(events: &[ganja_core::protocol::Event]) -> (Vec<String>, Vec<String>) {
    let mut completed = Vec::new();
    let mut errors = Vec::new();
    for event in events {
        let ganja_core::protocol::Event::PartUpdated { part, .. } = event else {
            continue;
        };
        let PartBody::Tool { state, .. } = &part.body else {
            continue;
        };
        match state {
            ToolState::Completed { output, .. } => completed.push(output.clone()),
            ToolState::Error { error, .. } => errors.push(error.clone()),
            ToolState::Pending { .. } | ToolState::Running { .. } => {}
        }
    }

    (completed, errors)
}

#[tokio::test]
async fn a_session_reads_ganjas_own_two_homes_and_whatever_its_config_named() {
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently. All three are set before
    // anything composes a prompt, resolves a store or spills tool output.
    let home = ganja_testkit::temp_dir();
    unsafe {
        std::env::set_var("HOME", home.path());
        // Windows asks USERPROFILE the question HOME answers everywhere else,
        // and phase 3's tilde resolves against whichever this platform reads —
        // so both must spell the same directory, or the planted tier sits
        // where no expansion will look.
        std::env::set_var("USERPROFILE", home.path());
        std::env::set_var("XDG_DATA_HOME", home.path().join("xdg"));
        std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
        // Phase 4 sets this deliberately; a developer who exports it would
        // otherwise move the global home out from under phases 1-3.
        std::env::remove_var(ganja_core::config::CONFIG_HOME_ENV);
    }

    let directory = ganja_testkit::temp_dir();
    let cwd = directory.path().to_path_buf();
    std::fs::create_dir_all(cwd.join(".git")).expect("the fixture checkout is creatable");
    let elsewhere = directory.path().join("shared-skills");
    plant(
        &elsewhere,
        "porting",
        "name: porting\ndescription: How to port a module.",
        "# Porting\n\nRead the upstream file first.\n",
    );

    // Ganja's own two homes, and every directory upstream walks unasked, each
    // holding a distinctly named skill so that anything reading one can be told
    // from anything reading another.
    let claude_home = home.path().join(".claude").join("skills");
    let ganja_global = home.path().join("config").join("ganja").join("skills");
    let ganja_project = cwd.join(".ganja").join("skills");
    for (root, name) in [
        // Ganja's own — read by default.
        (ganja_global.clone(), "from-ganja-global"),
        (ganja_project.clone(), "from-ganja-project"),
        // Foreign, and the generic project-root names — never read.
        (claude_home.clone(), "from-home-claude"),
        (
            home.path().join(".agents").join("skills"),
            "from-home-agents",
        ),
        (cwd.join(".claude").join("skills"), "from-project-claude"),
        (cwd.join(".agents").join("skills"), "from-project-agents"),
        // The singular spelling upstream also accepts, under ganja's own global
        // home: one name, not two, so this one stays unread.
        (
            home.path().join("config").join("ganja").join("skill"),
            "from-ganja-global-singular",
        ),
        (cwd.join("skills"), "from-project-root"),
    ] {
        plant(
            &root,
            name,
            &format!("name: {name}\ndescription: Found by convention."),
            &format!("# {name}\n\nThe body of {name}.\n"),
        );
    }

    const FOREIGN: [&str; 5] = [
        "from-home-claude",
        "from-home-agents",
        "from-project-claude",
        "from-project-agents",
        "from-project-root",
    ];

    // --- Phase 1. No config at all: ganja's own two homes are read, in
    // precedence order, and nothing foreign is.
    let bare_roots = instruction::skill_roots(&Config::default(), &cwd);
    let canonical_cwd = std::fs::canonicalize(&cwd).expect("the fixture checkout canonicalises");
    assert_eq!(
        bare_roots.dirs(),
        [
            ganja_global.clone(),
            canonical_cwd.join(".ganja").join("skills")
        ],
        "the global home first, the checkout's second"
    );

    let discovered: Vec<String> = ganja_core::tool::skill::discover(&bare_roots)
        .into_iter()
        .map(|skill| skill.name)
        .collect();
    assert_eq!(
        discovered,
        vec![
            "from-ganja-global".to_owned(),
            "from-ganja-project".to_owned()
        ],
        "both of ganja's own and neither the foreign ones nor the singular spelling"
    );

    // The session a frontend assembles out of exactly those defaults: both are
    // offered, either is loadable, and no foreign name appears anywhere.
    let tools = Registry::with_builtins().with(Arc::new(SkillTool::over(bare_roots.clone())));
    let (provider, default_seen) = ScriptedProvider::new(vec![
        tool_call("skill", serde_json::json!({ "name": "from-ganja-project" })),
        says("read it"),
    ]);
    let engine = Engine::new(provider, MODEL, Arc::new(tools), Permissions::default())
        .with_environment({
            let cwd = cwd.clone();
            move |model| instruction::suffix(&Config::default(), &cwd, model)
        });
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(Command::SendPrompt {
            text: "load ganja's own".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let default_events = drain(&mut events).await;

    let default_prompt = system(&default_seen);
    assert!(
        default_prompt.contains("<name>from-ganja-global</name>")
            && default_prompt.contains("<name>from-ganja-project</name>"),
        "a session that configured nothing is still offered ganja's own: {default_prompt}"
    );
    for foreign in FOREIGN {
        assert!(
            !default_prompt.contains(foreign),
            "and never {foreign}: {default_prompt}"
        );
    }
    assert!(
        !default_prompt.contains("from-ganja-global-singular"),
        "nor the `skill/` spelling beside the one that is read: {default_prompt}"
    );

    let (default_loaded, default_failed) = results(&default_events);
    assert!(default_failed.is_empty(), "{default_failed:?}");
    assert_eq!(default_loaded.len(), 1, "{default_loaded:?}");
    assert!(
        default_loaded[0].contains("The body of from-ganja-project."),
        "and a default tier's skill loads, not merely advertises: {}",
        default_loaded[0]
    );

    // --- Phase 1b. The fixture-built engine, which is how `tests/golden.rs`
    // assembles its leg: no environment half and the registry's own skill tool,
    // which holds no roots because a tool may not resolve them. Nothing about
    // the machine can reach either half. This is what keeps a differential
    // comparing two agents rather than two laptops.
    let (provider, bare_seen) = ScriptedProvider::new(vec![
        tool_call("skill", serde_json::json!({ "name": "from-ganja-project" })),
        says("nothing to load"),
    ]);
    let engine = Engine::new(
        provider,
        MODEL,
        Arc::new(Registry::with_builtins()),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(Command::SendPrompt {
            text: "hello".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let bare_events = drain(&mut events).await;

    assert_eq!(
        system(&bare_seen),
        String::new(),
        "a fixture-built engine carries no system prompt at all, so no skill can reach one"
    );
    let (bare_loaded, bare_errors) = results(&bare_events);
    assert!(bare_loaded.is_empty(), "nothing loaded: {bare_loaded:?}");
    assert_eq!(
        bare_errors,
        vec!["Skill \"from-ganja-project\" not found. Available skills: none".to_owned()],
        "the shipped tool was handed no directory, so it finds none — ganja's own included"
    );

    // --- Phase 2. A config naming a directory of its own: it is read on top of
    // the two defaults, and it ranks above them.
    let config = Config {
        skills: SkillsConfig {
            paths: vec![elsewhere.display().to_string()],
            urls: Vec::new(),
        },
        ..Config::default()
    };
    let roots: Roots = instruction::skill_roots(&config, &cwd);
    let tools = Registry::with_builtins().with(Arc::new(SkillTool::over(roots.clone())));

    let (provider, seen) = ScriptedProvider::new(vec![
        tool_call("skill", serde_json::json!({ "name": "porting" })),
        says("read it"),
    ]);
    let engine = Engine::new(provider, MODEL, Arc::new(tools), Permissions::default())
        .with_environment({
            let config = config.clone();
            let cwd = cwd.clone();
            move |model| instruction::suffix(&config, &cwd, model)
        });
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(Command::SendPrompt {
            text: "port the module".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen_events = drain(&mut events).await;

    // The prompt offers it, in upstream's shape.
    let prompt = system(&seen);
    assert!(
        prompt.contains("<available_skills>")
            && prompt.contains("<name>porting</name>")
            && prompt.contains("<description>How to port a module.</description>"),
        "the skill the config pointed at is offered: {prompt}"
    );
    assert!(
        prompt
            .find("Working directory")
            .expect("the environment block")
            < prompt.find("<available_skills>").expect("the skills block"),
        "and last, where upstream puts it: {prompt}"
    );

    assert_eq!(
        roots.dirs().last(),
        Some(&elsewhere),
        "the configured path ranks above both homes: {:?}",
        roots.dirs()
    );
    assert!(
        prompt.contains("<name>from-ganja-global</name>")
            && prompt.contains("<name>from-ganja-project</name>"),
        "which are still read — a config adds, it does not replace: {prompt}"
    );

    // Every foreign skill is still on the disk and still unmentioned: naming a
    // directory does not re-open the ones this build declines to read.
    for foreign in FOREIGN {
        assert!(
            !prompt.contains(foreign),
            "a configured path does not re-open a foreign tier ({foreign}): {prompt}"
        );
    }

    // And the call loads it — the same skill, out of the same roots, without
    // the registry having been told anything the prompt was not.
    let (loaded, failed) = results(&seen_events);
    assert!(failed.is_empty(), "nothing was refused: {failed:?}");
    assert_eq!(loaded.len(), 1, "one call, one result: {loaded:?}");
    assert!(
        loaded[0].contains("<skill_content name=\"porting\">")
            && loaded[0].contains("Read the upstream file first."),
        "the model is handed the skill's own instructions: {}",
        loaded[0]
    );
    assert!(
        loaded[0].contains(&format!(
            "Base directory for this skill: {}",
            elsewhere.join("porting").display()
        )),
        "and where its relative paths start from: {}",
        loaded[0]
    );

    // --- Phase 3. The tier upstream helps itself to, asked for by name. This
    // is the honest replacement for the scan that is not made: `~/.claude/skills`
    // is one line of config away, and the tilde resolves against the home
    // directory the way upstream resolves it.
    let claude_config = Config {
        skills: SkillsConfig {
            paths: vec!["~/.claude/skills".to_owned()],
            urls: Vec::new(),
        },
        ..Config::default()
    };
    let claude_roots: Roots = instruction::skill_roots(&claude_config, &cwd);
    assert_eq!(
        claude_roots.dirs().last(),
        Some(&claude_home),
        "the tilde expands to the tier upstream would have walked unasked: {:?}",
        claude_roots.dirs()
    );

    let tools = Registry::with_builtins().with(Arc::new(SkillTool::over(claude_roots)));
    let (provider, claude_seen) = ScriptedProvider::new(vec![
        tool_call("skill", serde_json::json!({ "name": "from-home-claude" })),
        says("read it"),
    ]);
    let engine = Engine::new(provider, MODEL, Arc::new(tools), Permissions::default())
        .with_environment({
            let cwd = cwd.clone();
            move |model| instruction::suffix(&claude_config, &cwd, model)
        });
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(Command::SendPrompt {
            text: "load the one from claude".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let claude_events = drain(&mut events).await;

    let claude_prompt = system(&claude_seen);
    assert!(
        claude_prompt.contains("<name>from-home-claude</name>"),
        "the named tier is offered: {claude_prompt}"
    );
    assert!(
        !claude_prompt.contains("from-home-agents"),
        "and only the one that was named — the sibling tier is still unscanned: {claude_prompt}"
    );
    assert!(
        !claude_prompt.contains("from-project-claude"),
        "naming the one under the home does not name the one beside the checkout: {claude_prompt}"
    );

    let (claude_loaded, claude_failed) = results(&claude_events);
    assert!(claude_failed.is_empty(), "{claude_failed:?}");
    assert_eq!(claude_loaded.len(), 1, "{claude_loaded:?}");
    assert!(
        claude_loaded[0].contains("<skill_content name=\"from-home-claude\">"),
        "and loadable, not merely advertised: {}",
        claude_loaded[0]
    );

    // --- Phase 4. Where ganja's own global home is. Three places in order, and
    // one seam: what moves the skills has to move the global `AGENTS.md` with
    // them, or a build reads its instructions out of a directory its config is
    // not in.
    //
    // Every phase above ran on tier 2, because `$XDG_CONFIG_HOME/ganja` exists
    // there — it is the directory `from-ganja-global` was planted in.

    // 4a. The environment variable, taken as written and outranking both
    // discovered places.
    let named = directory.path().join("named-home");
    plant(
        &named.join("skills"),
        "from-env-home",
        "name: from-env-home\ndescription: Found where the variable pointed.",
        "# from-env-home\n",
    );
    std::fs::write(named.join("AGENTS.md"), "the named home's instructions")
        .expect("the fixture is writable");
    // SAFETY: still the only test in this binary; nothing else reads the
    // environment concurrently.
    unsafe {
        std::env::set_var(ganja_core::config::CONFIG_HOME_ENV, &named);
    }

    assert_eq!(
        ganja_core::config::config_home(),
        Some(named.clone()),
        "the variable wins outright"
    );
    let env_roots = instruction::skill_roots(&Config::default(), &cwd);
    assert_eq!(
        env_roots.dirs().first(),
        Some(&named.join("skills")),
        "so the global skills tier hangs off it: {:?}",
        env_roots.dirs()
    );

    // The suffix rather than the whole prompt: the base half is chosen by the
    // model's family and says nothing about where a home is, so everything
    // this case is about lands here.
    let env_prompt =
        instruction::suffix(&Config::default(), &cwd, "fake-1").expect("a prompt is composed");
    assert!(
        env_prompt.contains("the named home's instructions")
            && env_prompt.contains("<name>from-env-home</name>"),
        "and both the global instructions and the global skills come out of it: {env_prompt}"
    );
    assert!(
        !env_prompt.contains("from-ganja-global"),
        "while the home it outranked is no longer read at all: {env_prompt}"
    );

    // 4b. No variable, and no `<XDG config>/ganja`: the dotted home answers.
    let dot_ganja = home.path().join(".ganja");
    plant(
        &dot_ganja.join("skills"),
        "from-dot-ganja",
        "name: from-dot-ganja\ndescription: Found in the dotted home.",
        "# from-dot-ganja\n",
    );
    // SAFETY: as above.
    unsafe {
        std::env::remove_var(ganja_core::config::CONFIG_HOME_ENV);
        std::env::set_var("XDG_CONFIG_HOME", home.path().join("no-config-here"));
    }

    assert_eq!(
        ganja_core::config::config_home(),
        Some(dot_ganja.clone()),
        "the dotted home is reached when the XDG one is not there"
    );
    let dotted_roots = instruction::skill_roots(&Config::default(), &cwd);
    assert_eq!(
        dotted_roots.dirs().first(),
        Some(&dot_ganja.join("skills")),
        "{:?}",
        dotted_roots.dirs()
    );
    assert!(
        ganja_core::tool::skill::discover(&dotted_roots)
            .iter()
            .any(|skill| skill.name == "from-dot-ganja"),
        "and a skill in it is found"
    );

    // 4c. Neither exists. There is nothing to read either way, so what is
    // returned is the one a writer should create — the XDG path, not the
    // dotted one.
    std::fs::remove_dir_all(&dot_ganja).expect("the fixture's dotted home is removable");

    assert_eq!(
        ganja_core::config::config_home(),
        Some(home.path().join("no-config-here").join("ganja")),
        "with nothing on disk, the answer is the directory a writer should make"
    );
    let empty_roots = instruction::skill_roots(&Config::default(), &cwd);
    assert_eq!(
        empty_roots.dirs().len(),
        2,
        "a home that is not there is still a tier, and contributes nothing: {:?}",
        empty_roots.dirs()
    );
    assert!(
        ganja_core::tool::skill::discover(&empty_roots)
            .iter()
            .all(|skill| skill.name != "from-dot-ganja"),
        "the home that was removed stops answering"
    );
}
