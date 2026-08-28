use ganja_team::ShimCli;

use super::{
    Identity, Recorded, Records, Started, Unreadable, VERSION, own_identity, own_pgid, own_pid,
    parse, path_for, render, started_at, stem_of, temp_path_for,
};

fn identity(pid: i32, started: &str) -> Identity {
    Identity { pid, started: started.to_owned() }
}

#[test]
fn a_records_file_round_trips_through_its_own_renderer() {
    let records = Records {
        owner: identity(4711, "Wed Aug 19 14:54:57 2026"),
        children: vec![
            Recorded {
                cli: ShimCli::Codex,
                process: identity(4823, "Wed Aug 19 14:55:02 2026"),
                pgid: 4823,
            },
            Recorded {
                cli: ShimCli::Grok,
                process: identity(4824, "Wed Aug 19 14:55:03 2026"),
                pgid: 4824,
            },
        ],
    };

    let text = render(&records);
    assert!(text.starts_with(&format!("{VERSION}\n")), "{text}");
    assert_eq!(parse(&text), Ok(records));
}

#[test]
fn a_file_with_no_children_still_names_its_owner() {
    let records = Records { owner: identity(1, "Wed Aug 19 14:54:57 2026"), children: Vec::new() };

    assert_eq!(parse(&render(&records)), Ok(records));
}

/// The four unreadable shapes are kept apart because each one is a
/// different retention answer, and collapsing any two of them would either
/// delete a newer lead's file or accumulate corruption forever.
#[test]
fn every_unreadable_shape_is_told_from_every_other() {
    assert_eq!(parse(""), Err(Unreadable::Headerless));
    assert_eq!(parse("\n"), Err(Unreadable::Headerless));
    assert_eq!(parse("   \n1\tx\n"), Err(Unreadable::Headerless));

    let Err(Unreadable::Version { token }) = parse("ganja-shims-2\n1\tx\n") else {
        panic!("a token this build does not know is a newer lead's file");
    };
    assert_eq!(token, "ganja-shims-2");

    assert!(matches!(parse(VERSION), Err(Unreadable::Malformed { .. })));
    assert!(matches!(parse(&format!("{VERSION}\n4711\n")), Err(Unreadable::Malformed { .. })));
    assert!(matches!(
        parse(&format!("{VERSION}\nnotapid\tWed Aug 19 14:54:57 2026\n")),
        Err(Unreadable::Malformed { .. })
    ));
    assert!(matches!(
        parse(&format!("{VERSION}\n4711\tWed\ncodex\t1\t1\n")),
        Err(Unreadable::Malformed { .. })
    ));
    assert!(
        matches!(
            parse(&format!("{VERSION}\n4711\tWed\nclaude\t1\t1\tWed\n")),
            Err(Unreadable::Malformed { .. })
        ),
        "`claude` is a backend but not a shim, so it names no CLI here"
    );
}

/// Only the two arms with no future reader may be removed, and the test
/// says which is which rather than trusting the caller to remember.
#[test]
fn a_newer_leads_file_is_never_this_builds_to_remove() {
    assert!(Unreadable::Headerless.removable());
    assert!(Unreadable::Malformed { reason: "x" }.removable());
    assert!(!Unreadable::Version { token: "ganja-shims-2".to_owned() }.removable());
}

/// A pid alone is recycled and a start time alone identifies nothing, so
/// the pair is the identity — and neither "gone" nor "could not tell" is
/// ever a match, because the caller of this is asking whether it may
/// signal.
#[test]
fn an_identity_matches_only_a_live_process_born_when_it_says() {
    let recorded = identity(4711, "Wed Aug 19 14:54:57 2026");

    assert!(recorded.matches(&Started::At("Wed Aug 19 14:54:57 2026".to_owned())));
    assert!(!recorded.matches(&Started::At("Wed Aug 19 14:54:58 2026".to_owned())));
    assert!(!recorded.matches(&Started::Gone));
    assert!(!recorded.matches(&Started::Unknown));
}

#[test]
fn a_records_name_carries_the_stem_and_the_leads_own_pid() {
    let directory = std::path::Path::new("/tmp/ganja-501");

    assert_eq!(path_for(directory, "0198c1a2", 4711), directory.join("0198c1a2-4711.shims"));
    // The staging name sits inside the sweep's own glob, so the sweep's
    // header-less arm removes one a crash left behind.
    assert_eq!(
        temp_path_for(directory, "0198c1a2", 4711),
        directory.join("0198c1a2-4711.tmp.shims")
    );
    assert!(temp_path_for(directory, "0198c1a2", 4711).to_string_lossy().ends_with(".shims"));
}

#[test]
fn a_stem_is_the_sessions_own_hex_where_it_has_one() {
    assert_eq!(stem_of("0198c1a2-7c3d-7000-8000-0123456789ab"), "0198c1a2");
    // A pre-UUIDv7 id has no eight hex digits to take, so the fallback
    // keeps it a filename rather than refusing to record anything.
    assert_eq!(stem_of("lead"), "lead");
    assert_eq!(stem_of("../../etc/passwd"), "etcpasswd");
    assert_eq!(stem_of("///"), "session");
}

/// The primitive is asked about this very process, which is the one pid a
/// test can be certain about — and about one that cannot exist.
#[test]
fn the_start_time_primitive_answers_for_this_process_and_says_gone_for_nobody() {
    let own = own_identity().expect("this process can read its own start time");
    assert_eq!(own.pid, own_pid());
    assert!(!own.started.is_empty());

    // Asked twice, the same process must render identically — which is
    // what makes the rendering an identity at all.
    assert_eq!(started_at(own.pid), Started::At(own.started));

    // A non-positive pid is answered by the guard before any probe runs,
    // on every platform — so both the boundary and a negative are pinned.
    assert_eq!(started_at(0), Started::Gone);
    assert_eq!(started_at(-1), Started::Gone);
}

/// The guard the sweep checks before any `kill(-pgid, …)` needs this to be
/// a real answer rather than zero.
#[test]
fn this_process_knows_its_own_process_group() {
    assert!(own_pgid() > 0);
}
