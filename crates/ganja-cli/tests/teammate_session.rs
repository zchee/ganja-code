//! A teammate's conversation is a conversation, not a delegated turn (**D-8**,
//! **D500**, **AC-25**).
//!
//! What that means in the surface a person actually uses: `ganja sessions`
//! lists it, and `ganja run --session <id>` opens it and carries on. Both
//! follow from one fact about the row a teammate's engine writes — its `parent`
//! is [`None`] — and `sessions_command` filters on exactly that, so a teammate
//! whose row carried a parent would be invisible to the listing and to anybody
//! looking for the id to resume.
//!
//! # Why the teammate is built here rather than spawned through a door
//!
//! On this branch there is no door. The `task` tool's `name`/`backend`
//! arguments are W5a/L3's and the `/team spawn` dialog is W6/L3's, so the
//! honest test of *the claim this lane owns* — that the row a teammate's second
//! engine writes is a root row the binary can find and resume — builds the
//! teammate with the same constructor those doors will call, over the same
//! store the binary opens. What is exercised end to end is everything from the
//! row outward: the listing, the resume, and the transcript the resumed run
//! appended to.
//!
//! # The environment stays in the children
//!
//! Nothing here calls `std::env::set_var`. The children are given their own
//! data home and the store is *found* under it rather than computed from a
//! layout this file would otherwise have to be taught again on every change —
//! `id_collision.rs`'s rule, for its reason. So this binary may hold more than
//! one test.
//!
//! # What W5b/L2 adds
//!
//! The pane half of AC-25. It is **not** written here as an ignored test,
//! because it cannot yet be written honestly: no door in this build spawns a
//! pane teammate, and this crate cannot even name `ganja-team`'s
//! `TeamsRoot`/`TeamName` to drive the registry directly (it depends on
//! `ganja-core`, which does not re-export them). W5b/L2 therefore adds a
//! `ganja-team` dev-dependency, a private tmux server in AC-11's own shape
//! (`tmux -L ganja-test-$$`), and the same two assertions this file makes about
//! the in-process row — listed, and resumable — about the row a pane's own
//! process writes.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};

use futures::StreamExt as _;
use ganja_core::{
    Storage,
    permission::Permissions,
    protocol::{Command as EngineCommand, Role},
    provider::FakeProvider,
    teammate::{SETTLE, Teammate},
    tool::Registry,
};
use serde_json::json;
use tempfile::TempDir;

/// The store's directory under a project's data directory, as `main.rs` names
/// it. `ganja-core` names the database file itself, which is why the path
/// stops here.
const STORAGE: &str = "storage";

/// The script the child runs play. One turn, one word.
const SCRIPT: &str = "script.json";

/// What the fake provider says, appearing nowhere else.
const REPLY: &str = "child-turn-zarquon";

/// What the teammate is asked, and what the resumed run adds after it. Both
/// are read back out of one transcript, which is what makes "it opened *that*
/// session" a fact rather than an exit code.
const TEAMMATE_PROMPT: &str = "the teammate's own first turn";
const RESUMED_PROMPT: &str = "the same conversation, opened again";

/// A project and a data home that both vanish with the test.
struct Fixture {
    project: TempDir,
    data: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let project = TempDir::new().expect("a temporary directory is creatable");
        // The checkout marker pins the project — and so the one store every
        // process opens — to this directory rather than to whatever the
        // temporary directory happens to sit inside.
        fs::create_dir(project.path().join(".git")).expect("the checkout marker is creatable");
        fs::write(
            project.path().join(SCRIPT),
            json!({"cadence_ms": 1, "turns": [{"text": REPLY}]}).to_string(),
        )
        .expect("the script is writable");

        Self {
            project,
            data: TempDir::new().expect("a temporary directory is creatable"),
        }
    }

    fn path(&self) -> &Path {
        self.project.path()
    }

    /// Pins everything that could decide what a run does onto this fixture's
    /// own directories, exactly as `run.rs` pins it: a developer's global
    /// config can choose a provider, and their cached catalog can decide what
    /// a model is sized at, so all of it moves or none of it has moved.
    fn ganja(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
        command
            .current_dir(self.path())
            .env("GANJA_PROVIDER", "fake")
            .env("GANJA_FAKE_SCRIPT", self.path().join(SCRIPT))
            .env("XDG_DATA_HOME", self.data.path())
            .env("HOME", self.data.path())
            .env("XDG_CONFIG_HOME", self.data.path().join("config"))
            .env("XDG_CACHE_HOME", self.data.path().join("cache"))
            .env("GANJA_DISABLE_MODELS_FETCH", "1")
            .env_remove("GANJA_CONFIG_HOME")
            .env_remove("GANJA_CONFIG")
            .env_remove("GANJA_MODEL")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENROUTER_API_KEY");

        command
    }

    /// Runs the binary and hands back its standard output, failing here rather
    /// than downstream when it did not exit 0.
    fn run(&self, arguments: &[&str]) -> String {
        let output = self
            .ganja()
            .args(arguments)
            .output()
            .expect("the binary is runnable");

        assert!(
            output.status.success(),
            "`ganja {}` exited {}\n--- stderr ---\n{}",
            arguments.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );

        String::from_utf8(output.stdout).expect("the binary writes UTF-8")
    }

    /// The store the runs wrote into.
    ///
    /// Found rather than computed: the project directory's name is
    /// `ganja-permission`'s to decide, and that there is exactly one of them
    /// is itself worth asserting — a teammate that stored under a second
    /// project stored somewhere the binary will never look.
    fn store(&self) -> Storage {
        let mut roots: Vec<PathBuf> = fs::read_dir(self.data.path().join("ganja").join("project"))
            .expect("the run created a project directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        roots.sort();

        assert_eq!(
            roots.len(),
            1,
            "one working directory is one project, got {roots:?}"
        );

        Storage::open(roots.remove(0).join(STORAGE))
    }
}

/// D-8's second half, in the surface it is claimed about.
///
/// The lead's own run is here to give the store a second root row: a listing
/// that showed the teammate because it shows everything would prove nothing
/// about `parent`, and two rows is what makes the filter's answer visible.
#[tokio::test]
async fn a_teammate_session_is_listed_and_resumable_on_both_backends() {
    let fixture = Fixture::new();

    // A first ordinary run, which is also what creates the store.
    fixture.run(&["run", "the lead's own turn"]);
    let storage = fixture.store();

    // The teammate: a second engine over a clone of that same handle, which is
    // the D500 shape. Its row is written by the engine's own lazy create, and
    // that is the whole of why it is a root.
    let teammate = Teammate::new(
        "worker",
        Arc::new(FakeProvider::new("on it", Duration::ZERO)),
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage.clone(),
    );
    // The birth queue is a lossless lane, and one nobody drains fills and then
    // makes the teammate's own turn wait.
    let mut events = teammate
        .engine()
        .subscribe()
        .await
        .expect("the first subscriber wins");
    tokio::spawn(async move { while events.next().await.is_some() {} });

    let session = teammate.engine().session_id();
    teammate
        .engine()
        .send(EngineCommand::SendPrompt {
            text: TEAMMATE_PROMPT.to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    assert!(
        teammate.shutdown(SETTLE).await,
        "the teammate's turn should have settled well inside the limit"
    );

    // Listed: `ganja sessions` shows roots only, so the id being here *is* the
    // claim that the row carries no parent.
    let listed = fixture.run(&["sessions"]);
    assert!(
        listed.contains(session.as_str()),
        "the teammate's session should be listed: {listed}"
    );

    // Resumable: the binary opens that id and adds to it. Nothing about the
    // exit code says *which* session was opened, so the transcript does.
    fixture.run(&["run", "--session", session.as_str(), RESUMED_PROMPT]);

    let transcript = storage
        .load_transcript(&session)
        .expect("the teammate's transcript reads back");
    let said: Vec<&str> = transcript
        .iter()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| message.parts.iter())
        .filter_map(ganja_core::protocol::Part::as_text)
        .collect();

    assert!(
        said.iter().any(|text| text.contains(TEAMMATE_PROMPT)),
        "the teammate's own turn is in this transcript: {said:?}"
    );
    assert!(
        said.iter().any(|text| text.contains(RESUMED_PROMPT)),
        "and the resumed run continued the same conversation: {said:?}"
    );
}
