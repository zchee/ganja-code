use std::path::PathBuf;
use std::sync::Arc;

use super::skill::{Roots, SkillTool};
use super::{Credentials, FileTimes, Registry, Tool, ToolCtx, ToolError, is_same_file};

/// A call whose credential store is `store`, which is the only thing the
/// guard tests below vary.
fn ctx(store: Option<PathBuf>) -> ToolCtx {
    let mut ctx = ToolCtx::fixture(std::env::temp_dir());
    ctx.credentials = store.map_or(Credentials::Unguarded, Credentials::Guarded);
    ctx
}

/// Three tools refuse for one cause — a context with no jobs handle — and
/// a person meeting the refusal through any of them is meeting the same
/// fact, so they say the same sentence.
///
/// Each tool's own test asserts only that its message contains "not
/// available", which three drifting sentences would all still pass. This
/// is the assertion that keeps them one sentence.
#[tokio::test]
async fn the_three_background_tools_refuse_a_jobless_context_in_the_same_words() {
    let registry = Registry::with_builtins();
    let dir = tempfile::tempdir().expect("a scratch directory");
    let mut jobless = ctx(None);
    jobless.cwd = dir.path().to_owned();

    let mut refusals = Vec::new();
    for (id, arguments) in [
        ("bash", serde_json::json!({ "command": "true", "run_in_background": true })),
        ("bash_output", serde_json::json!({ "bash_id": "bash_1" })),
        ("kill_shell", serde_json::json!({ "bash_id": "bash_1" })),
    ] {
        let refused = registry
            .get(id)
            .expect("every one of the three ships")
            .run(arguments, &jobless)
            .await
            .expect_err("a context with no jobs handle can serve none of them");
        let ToolError::Failed(message) = refused else {
            panic!("{id} refused with something other than a failure: {refused:?}");
        };
        refusals.push((id, message));
    }

    let (first, said) = &refusals[0];
    for (id, message) in &refusals[1..] {
        assert_eq!(
            message, said,
            "{id} and {first} refuse the same cause and must say so identically"
        );
    }
}

/// `with_all`'s stated contract is that each tool *replaces* whatever was
/// registered under its id, and its two production callers cannot show
/// that: both pass MCP tools, whose `mcp__server__tool` ids can collide
/// with nothing. Rewritten as a plain append it would still serve them —
/// and would silently offer the model two tools of one name the first time
/// anything else used it.
#[test]
fn with_all_replaces_a_tool_of_the_same_id_rather_than_offering_it_twice() {
    let registry = Registry::with_builtins();
    let rooted: Arc<dyn Tool> = Arc::new(SkillTool::over(Roots::none()));

    let after = registry.with_all([Arc::clone(&rooted)]);

    let skills =
        after.definitions().into_iter().filter(|definition| definition.name == "skill").count();
    assert_eq!(skills, 1, "one `skill` is offered, not two");
    assert!(
        Arc::ptr_eq(after.get("skill").expect("the roster still holds one"), &rooted),
        "and it is the one that was handed in"
    );
}

#[test]
fn the_registry_finds_a_tool_by_id_and_misses_unknown_names() {
    let registry = Registry::with_builtins();

    let read = registry.get("read").expect("read ships in every build");
    assert_eq!(read.id(), "read");
    assert!(registry.get("no-such-tool").is_none());
}

#[test]
fn a_file_must_be_read_before_it_may_be_touched() {
    let times = FileTimes::default();
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "one").expect("the fixture writes");

    let refused = times.check_fresh(&path).expect_err("an unread file is refused");
    assert!(
        matches!(&refused, ToolError::Failed(message) if message.contains("read it first")),
        "got {refused:?}"
    );

    times.record(&path);
    times.check_fresh(&path).expect("a freshly read file is fresh");
}

#[test]
fn a_file_changed_after_its_read_goes_stale() {
    let times = FileTimes::default();
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "one").expect("the fixture writes");

    times.record(&path);
    // Filesystem stamps can be coarse; force one that differs. Opened for
    // writing throughout this module because a stamp is metadata a handle
    // must be allowed to write: unix grants that with the file's own
    // permissions, Windows only through a handle that asked for write
    // access.
    let stale = std::time::SystemTime::UNIX_EPOCH;
    std::fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|file| file.set_modified(stale))
        .expect("the fixture can move the stamp");

    let refused = times.check_fresh(&path).expect_err("a changed file is refused");
    assert!(
        matches!(&refused, ToolError::Failed(message) if message.contains("read it again")),
        "got {refused:?}"
    );

    times.record(&path);
    times.check_fresh(&path).expect("re-reading repairs it");
}

/// **D78.** The stamp forms answer for the *descriptor* they were given a
/// stamp of, never for whatever the name resolves to when they are asked.
///
/// The fixture is the race in slow motion: the name is repointed at a
/// different file while the first one stays open, which is exactly what an
/// attacker gets to do between a permission dialog and the write that
/// follows it. The held file has not changed, so the write may proceed; a
/// second look at the name would have said otherwise, and the path form is
/// asserted alongside to show it really does say otherwise.
///
/// What this cannot reach is the call sites: whether `write.rs` and
/// `edit.rs` hand over `anchor::stamp(&file)` or a fresh path stat is
/// visible only in the source, because outside the race window the two
/// agree and the window is not one a test can step into.
#[test]
fn a_held_files_own_stamp_is_what_freshness_is_judged_on() {
    let times = FileTimes::default();
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "one").expect("the fixture writes");

    let held = std::fs::File::open(&path).expect("the file opens");
    times.record_stat(&path, crate::anchor::stamp(&held));

    // The name is repointed at a different file with a stamp of its own.
    // Renaming rather than rewriting is the point: the descriptor above
    // still refers to the original, which is now reachable no other way.
    let replacement = dir.path().join("b.txt");
    std::fs::write(&replacement, "something else entirely").expect("the fixture writes");
    std::fs::File::options()
        .write(true)
        .open(&replacement)
        .and_then(|file| file.set_modified(std::time::SystemTime::UNIX_EPOCH))
        .expect("the fixture can move the stamp");
    std::fs::rename(&replacement, &path).expect("the fixture can repoint the name");

    times
        .check_fresh_stat(&path, crate::anchor::stamp(&held))
        .expect("the file this call holds open has not changed");

    let refused = times.check_fresh(&path).expect_err("the name now leads somewhere else");
    assert!(
        matches!(&refused, ToolError::Failed(message) if message.contains("read it again")),
        "the path form must really disagree, or the assertion above proves nothing: {refused:?}"
    );
}

/// The stamp comparison the watcher makes, and what it costs the file that
/// loses it: the refusal is the one `write` and `edit` already print, and
/// it comes from the recorded state rather than from a fresh look.
#[test]
fn a_file_the_watcher_condemned_is_refused_until_it_is_read_again() {
    let times = FileTimes::default();
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "one").expect("the fixture writes");

    times.record(&path);
    let as_read = super::modification_stamp(&path);
    times.note_change(&path);
    times.check_fresh(&path).expect("nothing moved, so nothing is stale");

    std::fs::write(&path, "somebody else's edit").expect("the fixture writes");
    age(&path, 0);
    times.note_change(&path);

    let refused = times.check_fresh(&path).expect_err("the file moved under the session");
    assert!(
        matches!(&refused, ToolError::Failed(message) if message.contains("read it again")),
        "got {refused:?}"
    );

    // Put the stamp back where the read found it. A stamp comparison would
    // now say the file never moved, which is exactly why the condemnation
    // is a state and not a comparison: the contents are somebody else's
    // and the model has not seen them.
    std::fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|file| file.set_modified(as_read.expect("the fixture's filesystem stamps")))
        .expect("the fixture can move the stamp");
    assert!(
        times.check_fresh(&path).is_err(),
        "a file changed and changed back is still not the one that was read"
    );

    times.record(&path);
    times.check_fresh(&path).expect("reading it again is what repairs it");
}

#[test]
fn a_file_names_itself_to_the_model_once_per_time_it_goes_stale() {
    let times = FileTimes::default();
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "one").expect("the fixture writes");

    times.record(&path);
    age(&path, 0);
    times.note_change(&path);
    times.note_change(&path);

    assert_eq!(
        times.take_stale(),
        vec![path.clone()],
        "one episode is one mention, however many events reported it"
    );
    assert!(
        times.take_stale().is_empty(),
        "the queue is drained by being read; a later turn is not told again"
    );

    // Read again, changed again: a second episode, told a second time.
    times.record(&path);
    age(&path, 60);
    times.note_change(&path);
    assert_eq!(times.take_stale(), vec![path]);
}

#[test]
fn a_file_read_again_before_the_notice_goes_out_is_not_mentioned() {
    let times = FileTimes::default();
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "one").expect("the fixture writes");

    times.record(&path);
    age(&path, 0);
    times.note_change(&path);
    // The model asked for the file itself before the turn that would have
    // told it to. There is nothing left to advise.
    times.record(&path);

    assert!(times.take_stale().is_empty());
    times.check_fresh(&path).expect("the read that beat the notice also cleared the staleness");
}

#[test]
fn a_file_nobody_read_is_not_the_watchers_business() {
    let times = FileTimes::default();
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "one").expect("the fixture writes");

    times.note_change(&path);

    assert!(times.take_stale().is_empty());
    let refused = times.check_fresh(&path).expect_err("an unread file is unread, not stale");
    assert!(
        matches!(&refused, ToolError::Failed(message) if message.contains("read it first")),
        "got {refused:?}"
    );
}

#[test]
fn a_new_conversation_inherits_neither_the_reads_nor_what_became_of_them() {
    let times = FileTimes::default();
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "one").expect("the fixture writes");

    times.record(&path);
    age(&path, 0);
    times.note_change(&path);

    times.clear();

    assert!(
        times.take_stale().is_empty(),
        "a queued notice belongs to the session that read the file"
    );
}

/// Puts `path`'s modification stamp at a named second, so that no
/// assertion below rides on a filesystem's stamp resolution — and so that
/// two changes in a row are provably two, which "set it to the epoch"
/// twice would not be.
fn age(path: &std::path::Path, second: u64) {
    let stamp = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(second);
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(stamp))
        .expect("the fixture can move the stamp");
}

#[test]
fn the_file_log_is_shared_by_clone_not_copied() {
    let times = Arc::new(FileTimes::default());
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "one").expect("the fixture writes");

    Arc::clone(&times).record(&path);
    times.check_fresh(&path).expect("both handles see the same log");
}

#[test]
fn the_credential_store_guard_answers_for_the_store_and_not_for_a_namesake() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let store = dir.path().join("ganja").join("auth.json");
    std::fs::create_dir_all(store.parent().expect("the store sits in a directory"))
        .expect("the fixture nests");
    std::fs::write(&store, "{}").expect("the fixture writes");
    let ctx = ctx(Some(store.clone()));

    assert!(
        ctx.is_credential_store(&store),
        "the guard has to recognize the store it exists to protect"
    );

    let namesake = dir.path().join("auth.json");
    std::fs::write(&namesake, "{}").expect("the fixture writes");

    assert!(
        !ctx.is_credential_store(&namesake),
        "the guard is about which file this is, not what it is called"
    );
}

/// A call nobody named a store to — a frontend's own context, or a test's —
/// reads exactly like one whose store is not on this disk. There is then
/// nothing here to protect, and no file is special.
#[test]
fn a_call_that_was_handed_no_store_has_nothing_to_refuse() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let looks_the_part = dir.path().join("auth.json");
    std::fs::write(&looks_the_part, "{}").expect("the fixture writes");

    assert!(!ctx(None).is_credential_store(&looks_the_part));
}

#[cfg(unix)]
#[test]
fn a_link_planted_at_an_innocent_name_is_still_the_file_it_points_at() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let target = dir.path().join("auth.json");
    std::fs::write(&target, "{}").expect("the fixture writes");
    let planted = dir.path().join("notes.json");
    std::os::unix::fs::symlink(&target, &planted).expect("the link plants");

    assert!(
        is_same_file(&planted, &target),
        "a link is the file it points at, whatever it is called"
    );
}

#[test]
fn a_route_that_climbs_out_and_back_down_lands_on_the_same_file() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let target = dir.path().join("auth.json");
    std::fs::write(&target, "{}").expect("the fixture writes");
    let nested = dir.path().join("one").join("two");
    std::fs::create_dir_all(&nested).expect("the fixture nests");

    let climbed = nested.join("..").join("..").join("auth.json");

    assert!(
        is_same_file(&climbed, &target),
        "{} should resolve onto {}",
        climbed.display(),
        target.display()
    );
}

#[test]
fn paths_that_cannot_be_canonicalized_are_compared_as_written() {
    // Canonicalizing needs the file to be there, and the store is not until
    // the first login: what is left to compare is the paths themselves.
    let dir = tempfile::tempdir().expect("a scratch directory");
    let absent = dir.path().join("ganja").join("auth.json");
    let present = dir.path().join("auth.json");
    std::fs::write(&present, "{}").expect("the fixture writes");

    assert!(is_same_file(&absent, &absent));
    assert!(is_same_file(&dir.path().join("./ganja/auth.json"), &absent));
    assert!(!is_same_file(&dir.path().join("ganja").join("other.json"), &absent));
    assert!(!is_same_file(&present, &absent), "a file that exists is not one that does not");
}
