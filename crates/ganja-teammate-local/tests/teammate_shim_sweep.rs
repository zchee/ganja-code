//! The orphan net, against real processes (**D508**).
//!
//! Layer 3 is the only part of the shim design that can touch a process it does
//! not own, so its whole rule is one sentence — **no owner proof, no signal** —
//! and this suite is that sentence in its arms. Every test here spawns a real
//! `sleep`, records it the way a lead would, and then asks what a later lead's
//! sweep does about it.
//!
//! # Why the records are written by hand
//!
//! What is under test is the *reader*. Driving a real shim to write the file
//! would give one shape — a live owner and a live child — and every interesting
//! arm is a shape a live lead cannot produce: an owner that is gone, an owner
//! whose start time renders differently, a file truncated mid-rewrite. So the
//! records are rendered through the module's own renderer, which is what keeps
//! them honest, and the situations are built around them.
//!
//! # The gate this suite is about not having
//!
//! [`sweep_shims`](ganja_teammate_local::reaper::sweep_shims) is called at lead
//! start **beside** the pane sweep rather than inside it, and unconditionally:
//! the pane sweep is gated on there being a tmux server to look at, which is
//! right for panes and fatal for shims — a shim child is headless and its
//! common case has no tmux at all. That is asserted here at the **function**
//! level, which is the level available: `run` opens a real terminal, so there
//! is no headless seam in `ganja-tui/src/lib.rs` to witness the call itself
//! from. Nothing below consults `$TMUX`, because nothing in the function does.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use ganja_teammate_local::reaper::{ShimFate, sweep_shims_in, sweep_shims_in_with};
use ganja_teammate_local::shim::records::{self, Identity, Recorded, Records, Started};

/// A pid no `ps`/`kill` pair can portably answer for — the sentinel that
/// forces the "could not be established" arm. Shared by [`cannot_establish`]
/// and the record that drives the sweep so the two cannot drift apart.
const UNASKABLE: i32 = 999_999;

/// A start-time primitive that cannot establish one pid — the portable
/// stand-in for a `ps`/`kill` pair that genuinely could not answer. A real pid
/// is `Gone` on every platform (the `ESRCH` probe), so `Unknown` is untestable
/// through one; this declines [`UNASKABLE`] and defers every other pid to the
/// real primitive.
fn cannot_establish(pid: i32) -> Started {
    if pid == UNASKABLE { Started::Unknown } else { records::started_at(pid) }
}
use ganja_team::ShimCli;

/// A real process a sweep may or may not end, killed when the test is done with
/// it however that goes.
struct Decoy {
    child: std::process::Child,
    pid: i32,
    started: String,
}

impl Decoy {
    /// A `sleep` in a process group of its own, exactly as a shim child is
    /// spawned — the group is what a sweep signals, and a decoy in this
    /// process's group would be testing the guard rather than the reap.
    fn new() -> Self {
        use std::os::unix::process::CommandExt as _;

        let mut command = Command::new("sleep");
        command.arg("300");
        command.process_group(0);
        let child = command.spawn().expect("a decoy process starts");
        let pid = i32::try_from(child.id()).expect("a pid fits");
        let Started::At(started) = records::started_at(pid) else {
            panic!("a decoy that is running has a start time");
        };

        Self { child, pid, started }
    }

    /// How a lead would have recorded it.
    fn recorded(&self) -> Recorded {
        Recorded {
            cli: ShimCli::Codex,
            process: Identity { pid: self.pid, started: self.started.clone() },
            // Spawned as its own group leader, so the group is the pid — the
            // same identity a shim child's own record carries.
            pgid: self.pid,
        }
    }

    /// The same record, with a start time that is not this process's — a
    /// recycled pid, or a record written under a different `TZ`.
    fn recorded_as(&self, started: &str) -> Recorded {
        let mut recorded = self.recorded();
        recorded.process.started = started.to_owned();

        recorded
    }

    /// Whether it is still running.
    ///
    /// `try_wait` rather than `kill(pid, 0)`, and the difference is the whole
    /// reason this helper exists: a signalled child of *this* process becomes a
    /// zombie until somebody reaps it, and a zombie answers signal 0 exactly as
    /// a running process does. A real orphan has no such holder — its lead is
    /// dead, so init reaps it — which is why this is a fixture concern rather
    /// than one the sweep has.
    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Whether it is gone, waiting briefly rather than racing the signal.
    fn ended(&mut self) -> bool {
        for _ in 0..100 {
            if !self.alive() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        false
    }
}

impl Drop for Decoy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A private directory the sweep will accept, and nothing else in it.
fn records_directory() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt as _;

    tempfile::Builder::new()
        .prefix("shims-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .expect("a private records directory")
}

/// This process, as an owner line that will read as live.
fn live_owner() -> Identity {
    records::own_identity().expect("this process can read its own start time")
}

/// A pid that is certainly gone: started, waited for, and never signalled
/// again.
fn dead_pid() -> i32 {
    let mut child = Command::new("true").spawn().expect("a short-lived process");
    let pid = i32::try_from(child.id()).expect("a pid fits");
    child.wait().expect("it is reaped");

    pid
}

/// Renders a records file under `name`.
fn write_records(directory: &Path, name: &str, records: &Records) -> std::path::PathBuf {
    let path = directory.join(name);
    std::fs::write(&path, records::render(records)).expect("a records file is written");

    path
}

/// **AC-24**, and the core of the whole net: a recorded child whose lead is
/// provably gone is ended, on an exact `(pid, start-time)` match, by a sweep
/// that never asked whether there was a tmux server.
#[tokio::test]
async fn a_recorded_child_whose_lead_is_gone_is_ended_on_an_exact_identity_match() {
    let directory = records_directory();
    let mut orphan = Decoy::new();
    let path = write_records(
        directory.path(),
        "0198c1a2-4711.shims",
        &Records {
            owner: Identity { pid: dead_pid(), started: "Wed Aug 19 14:54:57 2026".to_owned() },
            children: vec![orphan.recorded()],
        },
    );

    let swept = sweep_shims_in(directory.path().to_path_buf()).await;

    assert_eq!(
        swept.fate_of(&path),
        Some(&ShimFate::Swept { signalled: 1, spared: 0, undecided: 0 }),
        "{swept:?}"
    );
    assert!(orphan.ended(), "the orphan was ended");
    // **AC-30**: owner provably gone and every child line decided, so the file
    // has no future reader and is unlinked — after the logging, never before.
    assert!(!path.exists(), "and its record went with it");
}

/// **AC-26**, the arm the whole design was restructured around: a
/// *concurrently live* lead's children are its own business. Its file is left
/// **entirely** alone — not parsed further, not rewritten, and above all not
/// signalled — while a second file whose owner is gone is reaped in the same
/// pass.
#[tokio::test]
async fn a_concurrently_live_leads_children_survive_a_sweep_that_reaps_another_leads() {
    let directory = records_directory();
    let mut mine = Decoy::new();
    let mut orphan = Decoy::new();

    let live = write_records(
        directory.path(),
        "0198c1a2-1.shims",
        &Records { owner: live_owner(), children: vec![mine.recorded()] },
    );
    let dead = write_records(
        directory.path(),
        "0198c1a2-2.shims",
        &Records {
            owner: Identity { pid: dead_pid(), started: "Wed Aug 19 14:54:57 2026".to_owned() },
            children: vec![orphan.recorded()],
        },
    );
    let before = std::fs::read_to_string(&live).expect("the live lead's file is readable");

    let swept = sweep_shims_in(directory.path().to_path_buf()).await;

    assert_eq!(swept.fate_of(&live), Some(&ShimFate::OwnerLive), "{swept:?}");
    assert!(mine.alive(), "a live lead's child is untouched");
    assert_eq!(
        std::fs::read_to_string(&live).expect("still there"),
        before,
        "and so is its file, byte for byte"
    );
    assert!(orphan.ended(), "while the other lead's orphan is ended");
    assert!(!dead.exists());
}

/// **AC-26**, the fail-closed arms. A missing owner line, an unparseable one
/// and one whose liveness cannot be established are three different facts, and
/// none of them is "gone": "cannot prove gone" and "gone" must never be the
/// same branch.
#[tokio::test]
async fn a_file_whose_owner_cannot_be_proven_gone_signals_nothing() {
    let directory = records_directory();
    let mut decoy = Decoy::new();
    let recorded = decoy.recorded();

    // No owner line at all: a version token and then straight to a child.
    let headless = directory.path().join("0198c1a2-3.shims");
    std::fs::write(
        &headless,
        format!(
            "{version}\n{cli}\t{pid}\t{pgid}\t{started}\n",
            version = records::VERSION,
            cli = recorded.cli.backend_type(),
            pid = recorded.process.pid,
            pgid = recorded.pgid,
            started = recorded.process.started,
        ),
    )
    .expect("a file with no owner line");

    // An owner line that is not one.
    let unparseable = directory.path().join("0198c1a2-4.shims");
    std::fs::write(&unparseable, format!("{}\nnot-a-pid\tnot-a-time\n", records::VERSION))
        .expect("a file with an unparseable owner line");

    let swept = sweep_shims_in(directory.path().to_path_buf()).await;

    // Both are "a known version and content that is not this format's", which
    // a same-version writer cannot produce — so they are corruption, and
    // corruption has no future reader.
    assert_eq!(swept.fate_of(&headless), Some(&ShimFate::Corrupt), "{swept:?}");
    assert_eq!(swept.fate_of(&unparseable), Some(&ShimFate::Corrupt), "{swept:?}");
    assert!(decoy.alive(), "and nothing was signalled about either");
    assert!(!headless.exists(), "**AC-30**: corruption is unlinked");
    assert!(!unparseable.exists());
}

/// **AC-26**, arm three, and the one place the owner rule is fail-**open**: a
/// pid that exists with a *different* start time is read as gone. What makes
/// that safe is not the owner rule but the child lines, which are compared with
/// the same primitive on the same rendered bytes and therefore mismatch for
/// whatever reason the owner's did.
///
/// **AC-30**'s hardening clause rides here: such a file is swept but **never**
/// unlinked, because an unlink is the one action that cannot be retried and the
/// owner might yet be alive.
#[tokio::test]
async fn an_owner_pid_that_exists_with_another_start_time_is_swept_but_never_unlinked() {
    let directory = records_directory();
    let mut decoy = Decoy::new();
    let path = write_records(
        directory.path(),
        "0198c1a2-5.shims",
        &Records {
            owner: Identity {
                pid: records::own_pid(),
                started: "Wed Aug 19 00:00:00 2026".to_owned(),
            },
            // Recorded under a start time that is not this decoy's — the same
            // mismatch the owner line has, for the same reason.
            children: vec![decoy.recorded_as("Wed Aug 19 00:00:00 2026")],
        },
    );

    let swept = sweep_shims_in(directory.path().to_path_buf()).await;

    assert_eq!(
        swept.fate_of(&path),
        Some(&ShimFate::Swept { signalled: 0, spared: 1, undecided: 0 }),
        "{swept:?}"
    );
    assert!(decoy.alive(), "a mismatched child is never signalled");
    assert!(path.exists(), "and a file whose owner pid still exists is never unlinked");
}

/// **AC-26**, arm four: the `TZ` decoy. `ps -o lstart=` renders through libc's
/// zone and locale rules, so a record written under a different `TZ` renders
/// differently for the same live process — and must therefore lead to **no**
/// child being signalled. The pinned `TZ=UTC`/`LC_ALL=C` child environment is
/// what makes that true by construction; this asserts it by writing the record
/// through a decoy renderer rather than by waiting for October.
#[tokio::test]
async fn a_record_written_under_another_timezone_signals_nothing() {
    let directory = records_directory();
    let mut decoy = Decoy::new();

    // The same live process, rendered the way a pre-pin build would have: the
    // ambient zone rather than UTC. On a machine already at UTC this is the
    // same string, so the decoy is forced by rendering under a zone that is
    // never UTC.
    let elsewhere = String::from_utf8(
        Command::new("ps")
            .args(["-o", "lstart=", "-p", &decoy.pid.to_string()])
            .env("TZ", "Pacific/Kiritimati")
            .env("LC_ALL", "C")
            .output()
            .expect("ps answers")
            .stdout,
    )
    .expect("a rendering")
    .trim()
    .to_owned();
    assert_ne!(
        elsewhere, decoy.started,
        "the decoy renderer really does differ from the pinned one"
    );

    let path = write_records(
        directory.path(),
        "0198c1a2-6.shims",
        &Records {
            owner: Identity { pid: dead_pid(), started: "Wed Aug 19 14:54:57 2026".to_owned() },
            children: vec![decoy.recorded_as(&elsewhere)],
        },
    );

    let swept = sweep_shims_in(directory.path().to_path_buf()).await;

    assert_eq!(
        swept.fate_of(&path),
        Some(&ShimFate::Swept { signalled: 0, spared: 1, undecided: 0 }),
        "{swept:?}"
    );
    assert!(decoy.alive(), "a rendering this build did not write is not an identity it may act on");
}

/// **AC-25**: belt beside braces. A record naming this lead's own process group
/// is refused before any signal, and the guard is kept precisely because the
/// owner rule should already have made it unreachable.
#[tokio::test]
async fn a_record_naming_this_leads_own_process_group_is_refused() {
    let directory = records_directory();
    let own = records::own_pgid();
    let path = write_records(
        directory.path(),
        "0198c1a2-7.shims",
        &Records {
            owner: Identity { pid: dead_pid(), started: "Wed Aug 19 14:54:57 2026".to_owned() },
            children: vec![Recorded {
                cli: ShimCli::Grok,
                process: Identity {
                    // This very process, recorded honestly — so the *only*
                    // thing standing between the sweep and a signal at our own
                    // group is the guard.
                    pid: records::own_pid(),
                    started: live_owner().started,
                },
                pgid: own,
            }],
        },
    );

    let swept = sweep_shims_in(directory.path().to_path_buf()).await;

    assert_eq!(
        swept.fate_of(&path),
        Some(&ShimFate::Swept { signalled: 0, spared: 1, undecided: 0 }),
        "the sweep refused its own group: {swept:?}"
    );
    // The proof that nothing was signalled is that this test is still running:
    // a `SIGTERM` to our own group would have ended the test binary.
}

/// **AC-30**, the version arms. A token this build does not know means a
/// **newer** lead owns the file, and a newer lead's records are not this
/// build's to delete; a file with no readable line 0 at all — a zero-byte one,
/// or the staging file a crash left between a write and its rename — has no
/// future reader and goes.
#[tokio::test]
async fn retention_turns_how_old_is_too_old_into_who_wrote_this() {
    let directory = records_directory();

    let newer = directory.path().join("0198c1a2-8.shims");
    std::fs::write(&newer, "ganja-shims-2\n4711\tWed Aug 19 14:54:57 2026\n")
        .expect("a newer lead's file");

    let empty = directory.path().join("0198c1a2-9.shims");
    std::fs::write(&empty, "").expect("a zero-byte file");

    // The staging name a crash between the write and the `rename(2)` leaves —
    // inside the sweep's own `*.shims` glob on purpose, so that one glob
    // removes it rather than a second one having to look for it.
    let staging = directory.path().join("0198c1a2-10.tmp.shims");
    std::fs::write(&staging, "ganja-shi").expect("a torn staging file");

    let swept = sweep_shims_in(directory.path().to_path_buf()).await;

    assert_eq!(swept.fate_of(&newer), Some(&ShimFate::Foreign), "{swept:?}");
    assert!(newer.exists(), "a newer lead's file is never unlinked");

    assert_eq!(swept.fate_of(&empty), Some(&ShimFate::Headerless), "{swept:?}");
    assert!(!empty.exists(), "a header-less file has no future reader");

    // A torn staging file's first line is a partial token rather than an empty
    // one, so it is classified as somebody else's version — and left. That is
    // the conservative answer and it is the one this build takes: the next
    // sweep meets it again rather than a build deleting a file it cannot
    // attribute.
    assert!(matches!(swept.fate_of(&staging), Some(ShimFate::Foreign | ShimFate::Headerless)));
}

/// **AC-30**: a child whose liveness could not be established leaves the file
/// undecided, and an undecided file is left byte-identical for the next sweep.
///
/// The unestablishable child is a pid this build cannot ask about at all — the
/// same fail-closed branch a broken `ps` would land every check on.
#[tokio::test]
async fn a_file_with_an_undecided_child_is_left_exactly_as_it_is() {
    let directory = records_directory();
    // A pid the primitive declines to establish — the "could not be
    // established" shape rather than "gone". No real pid is portably that: a
    // nonexistent one is `Gone` (the `ESRCH` probe) on every platform, so the
    // sweep is driven with `cannot_establish`, which refuses exactly this pid.
    let unaskable = UNASKABLE;
    let path = write_records(
        directory.path(),
        "0198c1a2-11.shims",
        &Records {
            owner: Identity { pid: dead_pid(), started: "Wed Aug 19 14:54:57 2026".to_owned() },
            children: vec![Recorded {
                cli: ShimCli::Agy,
                process: Identity {
                    pid: unaskable,
                    started: "Wed Aug 19 14:54:57 2026".to_owned(),
                },
                pgid: unaskable,
            }],
        },
    );
    let before = std::fs::read_to_string(&path).expect("readable");

    let swept = sweep_shims_in_with(directory.path().to_path_buf(), cannot_establish).await;

    assert_eq!(
        swept.fate_of(&path),
        Some(&ShimFate::Swept { signalled: 0, spared: 0, undecided: 1 }),
        "{swept:?}"
    );
    assert!(path.exists(), "an undecided file is kept");
    assert_eq!(
        std::fs::read_to_string(&path).expect("still there"),
        before,
        "byte-identical, for the next sweep to try again"
    );
}

/// A directory that is not there is not a directory to make: the sweep runs
/// before any shim has ever spawned, and creating a private directory in order
/// to enumerate nothing is a directory made for no reason. The first *record
/// write* is what creates it.
#[tokio::test]
async fn a_sweep_never_creates_the_directory_it_would_have_read() {
    let directory = records_directory();
    let absent = directory.path().join("not-here");

    let swept = sweep_shims_in(absent.clone()).await;

    assert!(swept.is_empty());
    assert!(!absent.exists(), "the sweep made nothing");
}

/// The writer's own round trip: what a lead records is what a sweep reads, at
/// `0600`, published by a `rename(2)` so a reader concurrent with a rewrite
/// sees either the old content or the new and never a truncated one.
#[tokio::test]
async fn what_a_lead_records_is_what_a_sweep_reads() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = records_directory();
    let mut writer =
        records::ShimRecords::new(directory.path().to_path_buf(), "0198c1a2-7c3d-7000-8000-1");
    let decoy = Decoy::new();
    writer.add(decoy.recorded());

    let path = writer.path();
    assert!(path.exists(), "the first write publishes a file");
    assert_eq!(
        std::fs::metadata(&path).expect("readable").permissions().mode() & 0o777,
        0o600,
        "nobody else may read which processes this lead owns"
    );
    let parsed = records::parse(&std::fs::read_to_string(&path).expect("readable"))
        .expect("what this build wrote, this build reads");
    assert_eq!(parsed.owner.pid, records::own_pid());
    assert_eq!(parsed.children, vec![decoy.recorded()]);

    // No staging file survives a completed write.
    let strays: Vec<_> = std::fs::read_dir(directory.path())
        .expect("readable")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp."))
        .collect();
    assert!(strays.is_empty(), "{strays:?}");

    writer.remove(decoy.recorded().process.pid);
    let parsed = records::parse(&std::fs::read_to_string(&path).expect("readable"))
        .expect("still this format");
    assert!(parsed.children.is_empty(), "a child that exited is taken back out");
}
