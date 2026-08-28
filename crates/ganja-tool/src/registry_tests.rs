use serde_json::json;

use super::{
    FALLBACK_NAME, FORMAT, MOST_NAME_POINTS, NameRefusal, NameSource, Record, list, record_path,
    same_name, sanitize, vet_name, write,
};

/// A record for `stem`, distinct enough that a test mixing several can
/// tell them apart.
fn record(stem: &str, name: &str, session_id: &str) -> Record {
    Record {
        format: FORMAT,
        session_id: session_id.to_owned(),
        name: name.to_owned(),
        name_source: NameSource::User,
        cwd: format!("/work/{stem}").into(),
        root: format!("/work/{stem}").into(),
        pid: 4242,
        started_at: 1_756_150_000_000,
    }
}

/// AC-1's serde half: storage preserves the typed case byte for byte,
/// and the round trip loses nothing.
#[test]
fn a_record_round_trips_with_its_typed_case_preserved() {
    let written = record("0198c1a2", "MiXeD-Case", "0198c1a2-0000-7000-8000-000000000001");

    let json = serde_json::to_string(&written).expect("a record serializes");
    let read: Record = serde_json::from_str(&json).expect("and reads back");

    assert_eq!(read, written);
    assert_eq!(read.name, "MiXeD-Case", "the typed case is storage's");
}

/// AC-1's walk half: unknown extra fields still read (tolerant), an
/// unknown format and a torn or foreign name are skipped, and nothing
/// that is not `<hex stem>.json` is ever read — the staging dot-name
/// included.
#[test]
fn the_listing_walk_reads_tolerantly_and_skips_what_it_does_not_know() {
    let dir = tempfile::tempdir().expect("a scratch directory");

    // A good record carrying a field this build never wrote.
    let mut good =
        serde_json::to_value(record("0198c1a2", "worker", "0198c1a2-0000-7000-8000-000000000001"))
            .expect("a record is JSON");
    good["a_field_from_the_future"] = json!("still reads");
    std::fs::write(record_path(dir.path(), "0198c1a2"), serde_json::to_vec(&good).expect("json"))
        .expect("the fixture writes");

    // A record from a format this build does not know.
    let mut newer =
        serde_json::to_value(record("0299d2b3", "future", "0299d2b3-0000-7000-8000-000000000002"))
            .expect("a record is JSON");
    newer["format"] = json!(2);
    std::fs::write(record_path(dir.path(), "0299d2b3"), serde_json::to_vec(&newer).expect("json"))
        .expect("the fixture writes");

    // A half-written record at the staging spelling, a foreign name, a
    // record-shaped file that is not JSON, and one that is JSON of the
    // wrong shape.
    std::fs::write(dir.path().join(".0398e3c4.json"), b"{\"format\":1,\"trunc")
        .expect("the fixture writes");
    std::fs::write(dir.path().join("notes.json"), b"{}").expect("the fixture writes");
    std::fs::write(dir.path().join("0398e3c4.json"), b"not json at all")
        .expect("the fixture writes");
    std::fs::write(dir.path().join("0498f4d5.json"), b"{\"format\":1,\"name\":7}")
        .expect("the fixture writes");

    let listed = list(dir.path()).expect("the directory lists");

    assert_eq!(listed.len(), 1, "one readable record: {listed:?}");
    assert_eq!(listed[0].stem, "0198c1a2");
    assert_eq!(listed[0].record.name, "worker");
}

/// The size cap: a stem-named giant is skipped, not slurped.
#[test]
fn an_oversized_record_is_skipped_rather_than_read() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    std::fs::write(record_path(dir.path(), "0198c1a2"), vec![b' '; 65 * 1024])
        .expect("the fixture writes");

    assert!(
        list(dir.path()).expect("the directory lists").is_empty(),
        "over the cap is over the cap"
    );
}

/// An incomplete search refuses rather than answers short: a directory
/// that cannot be read is an error, never an empty listing.
#[test]
fn an_unreadable_directory_is_an_error_not_an_empty_listing() {
    assert!(list(std::path::Path::new("/nonexistent-ganja-registry")).is_err());
}

/// AC-2's mechanism half (F8): the write stages under a leading dot the
/// stem filter can never match, lands atomically at the stem's name, and
/// refuses a stem the walk would never list.
#[test]
fn a_write_lands_atomically_and_its_staging_name_is_never_a_session_stem() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let written = record("0198c1a2", "worker", "0198c1a2-0000-7000-8000-000000000001");

    write(dir.path(), "0198c1a2", &written).expect("a record writes");

    let names: Vec<String> = std::fs::read_dir(dir.path())
        .expect("the directory lists")
        .map(|entry| entry.expect("an entry reads").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec!["0198c1a2.json".to_owned()],
        "the staging file is gone and only the record remains"
    );
    // The listing walk's filter is what makes the staging spelling safe,
    // so the shape claim is asserted against that filter itself.
    assert!(!crate::socket::is_session_stem(".0198c1a2"), "a dot-led stem can never pass the walk");

    let listed = list(dir.path()).expect("the directory lists");
    assert_eq!(listed[0].record, written);

    assert_eq!(
        write(dir.path(), "not-a-stem", &written).expect_err("a non-stem name is refused").kind(),
        std::io::ErrorKind::InvalidInput
    );
}

/// AC-5: every clause of the grammar refuses by name, at one predicate
/// every door shares.
#[test]
fn the_name_grammar_refuses_each_clause_by_name() {
    assert_eq!(vet_name(""), Err(NameRefusal::Empty));
    assert_eq!(vet_name("two\nlines"), Err(NameRefusal::MultiLine));
    assert_eq!(vet_name("a b"), Err(NameRefusal::Whitespace));
    assert_eq!(
        vet_name(&"x".repeat(MOST_NAME_POINTS + 1)),
        Err(NameRefusal::TooLong { points: MOST_NAME_POINTS + 1 })
    );
    assert_eq!(vet_name("*"), Err(NameRefusal::Broadcast));
    assert_eq!(vet_name("name@scope"), Err(NameRefusal::Scoped));
    assert_eq!(vet_name("uds:name"), Err(NameRefusal::Scheme));
    assert_eq!(vet_name("/leading"), Err(NameRefusal::LeadingSlash));
    assert_eq!(vet_name("build#2"), Err(NameRefusal::MentionRange));
    assert_eq!(vet_name("w#5-9"), Err(NameRefusal::MentionRange));
    assert_eq!(vet_name("w#5-"), Err(NameRefusal::MentionRange));
    assert_eq!(vet_name("bell\u{7}"), Err(NameRefusal::Control));

    assert_eq!(vet_name("worker-1"), Ok(()));
    assert_eq!(
        vet_name("release-candidate"),
        Ok(()),
        "a `#`-free hyphenated name is untouched by the new clause"
    );
    assert_eq!(
        vet_name("build#one"),
        Ok(()),
        "a `#`-tail that is not digits is a name, not a range"
    );
    assert_eq!(vet_name("日本語の名前"), Ok(()), "non-ASCII names are admitted");
    assert_eq!(
        vet_name(&"あ".repeat(MOST_NAME_POINTS)),
        Ok(()),
        "the cap counts code points, not bytes"
    );

    // Every refusal is one single-spaced sentence, like the socket
    // gate's own.
    for refusal in [
        NameRefusal::Empty,
        NameRefusal::MultiLine,
        NameRefusal::Whitespace,
        NameRefusal::TooLong { points: 65 },
        NameRefusal::Broadcast,
        NameRefusal::Scoped,
        NameRefusal::Scheme,
        NameRefusal::LeadingSlash,
        NameRefusal::MentionRange,
        NameRefusal::Control,
    ] {
        let sentence = refusal.to_string();
        assert!(!sentence.contains("  "), "single-spaced: {sentence:?}");
    }
}

/// AC-4's sanitizer half: the basename through the same grammar —
/// invalid characters dropped, typed case preserved, nothing valid left
/// falling back — and whatever comes out always passes the predicate.
#[test]
fn a_derived_name_is_the_basename_run_through_the_same_grammar() {
    assert_eq!(sanitize("ganja-code"), "ganja-code");
    assert_eq!(sanitize("MyProject"), "MyProject", "typed case survives");
    assert_eq!(sanitize("my project"), "myproject");
    assert_eq!(sanitize("a@b:c"), "abc");
    assert_eq!(sanitize("/leading/kept"), "leading/kept");
    assert_eq!(sanitize(""), FALLBACK_NAME);
    assert_eq!(sanitize("///"), FALLBACK_NAME);
    assert_eq!(sanitize("*"), FALLBACK_NAME);
    assert_eq!(sanitize(" \t\n"), FALLBACK_NAME);
    assert_eq!(
        sanitize("server#2"),
        FALLBACK_NAME,
        "a basename shaped like a mention range falls back like `*` does"
    );
    assert_eq!(
        sanitize(&"x".repeat(MOST_NAME_POINTS + 20)).chars().count(),
        MOST_NAME_POINTS,
        "an over-long basename is cut at the cap rather than refused"
    );

    for hostile in ["", "*", "///", "a b@c:d\n", "\u{7}\u{8}", "server#2", &"y".repeat(200)] {
        assert_eq!(
            vet_name(&sanitize(hostile)),
            Ok(()),
            "whatever {hostile:?} became, the grammar admits it"
        );
    }
}

/// The one comparison predicate: ASCII case folds, non-ASCII case does
/// not — the rung-8 precedent extended, not a new folding regime.
#[test]
fn name_comparison_folds_ascii_case_and_only_ascii_case() {
    assert!(same_name("Worker", "wORKER"));
    assert!(!same_name("worker", "worker-1"));
    assert!(!same_name("É", "é"), "two names differing only in non-ASCII case are two names");
}

/// AC-9: a record is live exactly while its stem's lock is held, the
/// probe's only footprint is the documented one — an absent `.lock`
/// created, nothing ever unlinked.
#[cfg(unix)]
#[test]
fn a_record_is_live_exactly_while_its_lock_is_held_and_the_probe_unlinks_nothing() {
    use super::is_live;

    let dir = tempfile::tempdir().expect("a scratch directory");
    let stem = "0198c1a2";
    write(dir.path(), stem, &record(stem, "worker", "0198c1a2-0000-7000-8000-000000000001"))
        .expect("a record writes");

    // Held, as a binder holds it: a second descriptor's try-lock blocks.
    let socket = dir.path().join(format!("{stem}.sock"));
    let held = crate::socket::open_lock(&socket).expect("the lock file opens");
    held.try_lock().expect("nothing else holds a fresh lock");

    let names = |dir: &std::path::Path| -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("the directory lists")
            .map(|entry| entry.expect("an entry reads").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };

    let before = names(dir.path());
    assert!(is_live(dir.path(), stem).expect("the probe answers"), "a held lock is a live name");
    assert_eq!(names(dir.path()), before, "the probe touched nothing");

    drop(held);
    assert!(!is_live(dir.path(), stem).expect("the probe answers"), "a freed lock is a stale name");
    assert_eq!(names(dir.path()), before, "stale is a verdict, not an unlink");

    // A name never bound: the probe creates the absent `.lock` — the
    // lister's standing price, lock files being never removed — and
    // reads stale. Everything already there survives.
    assert!(
        !is_live(dir.path(), "0299d2b3").expect("the probe answers"),
        "an unbound name is stale"
    );
    let after = names(dir.path());
    assert!(after.contains(&"0299d2b3.lock".to_owned()), "{after:?}");
    for kept in before {
        assert!(after.contains(&kept), "{kept} survived the probe");
    }
}

/// The collision scan behind the notice: live same-named holders under
/// the folding predicate, the stale and this session's own excluded.
#[cfg(unix)]
#[test]
fn the_collision_scan_reports_live_same_named_holders_and_never_this_session() {
    use super::holders;

    let dir = tempfile::tempdir().expect("a scratch directory");
    let live_id = "0198c1a2-0000-7000-8000-000000000001";
    write(dir.path(), "0198c1a2", &record("0198c1a2", "Worker", live_id)).expect("a record writes");
    write(
        dir.path(),
        "0299d2b3",
        &record("0299d2b3", "worker", "0299d2b3-0000-7000-8000-000000000002"),
    )
    .expect("a record writes");

    // Only the first is live.
    let held =
        crate::socket::open_lock(&dir.path().join("0198c1a2.sock")).expect("the lock file opens");
    held.try_lock().expect("nothing else holds a fresh lock");

    let found = holders(dir.path(), "wORKER", "some-other-session").expect("the scan answers");
    assert_eq!(
        found.iter().map(|held| held.stem.as_str()).collect::<Vec<_>>(),
        vec!["0198c1a2"],
        "the live holder matches case-insensitively; the stale one is no collision"
    );
    assert_eq!(found[0].record.name, "Worker", "reported as typed");

    assert!(
        holders(dir.path(), "worker", live_id).expect("the scan answers").is_empty(),
        "a session is never its own collision"
    );
}
