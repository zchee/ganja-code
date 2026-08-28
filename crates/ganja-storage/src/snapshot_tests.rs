use std::path::Path;
use std::time::Duration;

use ganja_permission::project::Project;
use ganja_protocol::{Message, MessageId, Part, PartBody, Role};
use tempfile::TempDir;

use super::{
    Patch, Snapshots, dedupe, patches_from, pathspecs, redo_anchor, split_nul, undo_anchor,
};

fn temporary() -> TempDir {
    TempDir::new().expect("a temporary directory is creatable")
}

/// A message with a pinned id, so the ordering the walk depends on is the
/// test's to state rather than the clock's.
fn message(id: &str, role: Role, parts: Vec<Part>) -> Message {
    let mut message = match role {
        Role::User => Message::user(String::new()),
        Role::Assistant => Message::assistant("model"),
    };
    message.id = MessageId::from(id.to_owned());
    message.parts = parts;

    message
}

fn patch(hash: &str, files: &[&str]) -> Part {
    Part {
        id: ganja_protocol::PartId::ascending(),
        body: PartBody::Patch {
            hash: hash.to_owned(),
            files: files.iter().map(|file| (*file).to_owned()).collect(),
        },
    }
}

/// The golden differential and every scripted run works in a temporary
/// directory that is not a checkout. `snapshot` defaults to *on*, so what
/// actually keeps snapshots out of those runs is this — and a build where
/// it stopped being true would have every golden run spawning git.
#[test]
fn a_directory_that_is_not_a_checkout_takes_no_snapshots() {
    let directory = temporary();
    let snapshots = Snapshots::new(&Project::resolve(directory.path()), true);

    assert!(!snapshots.enabled());
    assert!(snapshots.notice().is_some(), "a session that cannot undo has to say so");
}

#[test]
fn a_configuration_that_switched_snapshots_off_says_nothing_about_it() {
    let directory = temporary();
    std::fs::create_dir(directory.path().join(".git")).expect("the marker is creatable");
    let snapshots = Snapshots::new(&Project::resolve(directory.path()), false);

    assert!(!snapshots.enabled());
    assert_eq!(
        snapshots.notice(),
        None,
        "somebody who asked for no snapshots does not need telling they have none"
    );
}

#[test]
fn a_checkout_keeps_its_snapshots_beside_the_project_state_and_creates_nothing() {
    let directory = temporary();
    let root = directory.path().join("api");
    std::fs::create_dir_all(root.join(".git")).expect("the fixture repository is creatable");
    let project = Project::resolve(&root);
    let snapshots = Snapshots::new(&project, true);

    assert!(snapshots.enabled(), "git is a test prerequisite");
    assert!(
        snapshots
            .gitdir
            .starts_with(ganja_permission::project::data_home().expect("the data home resolves")),
        "{}",
        snapshots.gitdir.display()
    );
    assert!(
        snapshots.gitdir.to_string_lossy().contains(&format!(
            "snapshot{}{}",
            std::path::MAIN_SEPARATOR,
            project.slug()
        )),
        "{}",
        snapshots.gitdir.display()
    );
    assert!(!snapshots.gitdir.exists(), "asking where snapshots go must not create anything");
}

#[test]
fn a_walk_backwards_stops_at_each_user_message_in_turn() {
    let history = vec![
        message("msg_1", Role::User, Vec::new()),
        message("msg_2", Role::Assistant, Vec::new()),
        message("msg_3", Role::User, Vec::new()),
        message("msg_4", Role::Assistant, Vec::new()),
    ];

    let first = undo_anchor(&history, None).expect("there is a prompt to undo");
    assert_eq!(first.as_str(), "msg_3");

    let second = undo_anchor(&history, Some(&first)).expect("there is one before it");
    assert_eq!(second.as_str(), "msg_1");

    assert_eq!(
        undo_anchor(&history, Some(&second)),
        None,
        "the first prompt of a session has nothing behind it"
    );
}

#[test]
fn a_walk_forwards_steps_one_prompt_at_a_time_and_then_runs_out() {
    let history = vec![
        message("msg_1", Role::User, Vec::new()),
        message("msg_2", Role::Assistant, Vec::new()),
        message("msg_3", Role::User, Vec::new()),
    ];

    let next = redo_anchor(&history, &MessageId::from("msg_1".to_owned()))
        .expect("there is a prompt after the first");
    assert_eq!(next.as_str(), "msg_3");
    assert_eq!(redo_anchor(&history, &next), None);
}

#[test]
fn a_revert_collects_the_patches_from_the_anchor_on_and_leaves_the_earlier_ones() {
    let history = vec![
        message("msg_1", Role::User, Vec::new()),
        message("msg_2", Role::Assistant, vec![patch("older", &["kept.txt"])]),
        message("msg_3", Role::User, Vec::new()),
        message("msg_4", Role::Assistant, vec![patch("newer", &["changed.txt"])]),
    ];

    let patches = patches_from(&history, &MessageId::from("msg_3".to_owned()));

    assert_eq!(
        patches,
        vec![Patch { hash: "newer".to_owned(), files: vec!["changed.txt".to_owned()] }]
    );
}

/// Two steps that both touched a file must restore it to what it was
/// before the *first* of them, or an undo of a turn would leave the file
/// half-way through it.
#[test]
fn the_oldest_patch_naming_a_file_is_the_one_that_restores_it() {
    let patches = vec![
        Patch { hash: "before".to_owned(), files: vec!["a.txt".to_owned(), "b.txt".to_owned()] },
        Patch { hash: "midway".to_owned(), files: vec!["a.txt".to_owned(), "c.txt".to_owned()] },
    ];

    assert_eq!(
        dedupe(&patches),
        vec![
            ("before".to_owned(), "a.txt".to_owned()),
            ("before".to_owned(), "b.txt".to_owned()),
            ("midway".to_owned(), "c.txt".to_owned()),
        ]
    );
}

#[test]
fn a_pathspec_is_literal_from_the_top_and_terminated_rather_than_separated() {
    assert_eq!(
        pathspecs(&["src/[a].rs".to_owned(), "b.rs".to_owned()]),
        ":(top,literal)src/[a].rs\0:(top,literal)b.rs\0"
    );
}

#[test]
fn a_terminated_listing_does_not_end_in_an_empty_name() {
    assert_eq!(split_nul("one\0two\0"), vec!["one".to_owned(), "two".to_owned()]);
    assert!(split_nul("").is_empty());
}

/// Nothing in this module may reach the project's own git directory to
/// write: the whole point of a separate repository is that a snapshot
/// cannot cost somebody their index.
#[test]
fn the_snapshot_repository_is_never_the_project_one() {
    let directory = temporary();
    std::fs::create_dir(directory.path().join(".git")).expect("the marker is creatable");
    let snapshots = Snapshots::new(&Project::resolve(directory.path()), true);

    assert!(!snapshots.gitdir.starts_with(directory.path()), "{}", snapshots.gitdir.display());
    assert_ne!(snapshots.gitdir, Path::new(".git"));
}

/// A real, empty repository whose `.gitignore` matches everything — so
/// `check-ignore`'s reply to a stdin request is exactly as large as the
/// request, which is what this drill needs from it.
fn ignore_everything(root: &Path) {
    let status = std::process::Command::new("git")
        .arg("init")
        .arg("--quiet")
        .current_dir(root)
        .status()
        .expect("git is a test prerequisite");
    assert!(status.success(), "git init failed");
    std::fs::write(root.join(".gitignore"), "*\n").expect("the ignore file is writable");
}

/// **Regression, pipe deadlock.** `run` used to write the whole stdin
/// payload to completion before it read a single byte back — correct
/// for a reply that fits in the OS pipe buffer, and a permanent hang for
/// one that does not. `check-ignore --stdin -z` answers while it is
/// still reading, so a large enough request makes it fill its own
/// stdout pipe before this call has finished writing stdin: git blocks
/// writing a reply nobody is draining, this call blocks writing a
/// request nobody is reading, and neither is ever going to move again.
///
/// Several hundred KB of candidates, every one matched by the blanket
/// `.gitignore`, is comfortably past the ~64KB a pipe buffer usually
/// holds — enough that the old, sequential `run` deadlocked on this
/// exact call every time it was tried. **Non-vacuity, checked by hand
/// while landing the fix:** reverting `run` to write-then-drain made
/// this test hang and fail on the timeout below instead of passing.
#[tokio::test]
async fn a_huge_check_ignore_reply_does_not_deadlock_the_stdin_write_that_asked_for_it() {
    let directory = temporary();
    ignore_everything(directory.path());
    let snapshots = Snapshots::new(&Project::resolve(directory.path()), true);
    assert!(snapshots.enabled(), "git is a test prerequisite");

    let candidates: Vec<String> = (0..4000)
        .map(|i| {
            format!(
                "some/moderately/deeply/nested/directory/tree/padding/out/the/path/\
                     length/file-{i:06}.txt"
            )
        })
        .collect();

    let ignored =
        tokio::time::timeout(Duration::from_secs(30), snapshots.ignored(&candidates)).await.expect(
            "check-ignore answers well within this drill's patience; a hang here is the \
                 deadlock this test exists to catch",
        );

    assert_eq!(ignored.len(), candidates.len(), "every candidate matches the blanket .gitignore");
}
