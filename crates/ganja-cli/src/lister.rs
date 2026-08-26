//! The live-session listing `ganja-tui`'s `@` menu is handed
//! (**D529** Axis 5): `ganja_tui::lister::Lister` implemented over
//! `ganja-tool`'s registry plus a `ganja-client` health probe, sharing its
//! vocabulary with `ganja sessions --live` (`main.rs`'s own listing) so the
//! two cannot come to disagree about what "live" means.
//!
//! No upstream counterpart, for [`crate::binder`]'s own reason: the health
//! half needs `ganja-client`, which `ganja-tui`'s CI allowlist keeps out of
//! that crate, so the listing is a value this binary hands in rather than a
//! call `ganja-tui` makes itself.
//!
//! # Own-session exclusion
//!
//! This lister answers with **every** live record the registry holds,
//! including this session's own — the D528/AC-17 own-session exclusion is
//! the caller's, exactly as the module doc of `ganja_tui::lister` says
//! ("the TUI filters out its own session id"): a lister that already knew
//! its caller's session id would be a second place that rule could drift
//! from the identity resolver's own copy of it.

use std::{path::PathBuf, time::Duration};

use futures::{FutureExt as _, future::BoxFuture};
use ganja_core::tool::registry;
use ganja_tui::lister::{Health, Listing, LiveSession};

/// How long one socket is given to answer before its row falls back to
/// [`Health::Held`]/[`Health::Unreadable`] — `sessions --live`'s own
/// deadline (`main.rs`'s `HEALTH_DEADLINE`), mirrored rather than shared:
/// the two constants live in different modules of one binary, and the
/// number is small enough that a second literal costs nothing a shared one
/// would save.
const HEALTH_DEADLINE: Duration = Duration::from_secs(2);

/// The live-session listing over one socket directory — this user's own
/// unless the hidden `--socket-dir` names a private one, the same value the
/// binder binds under and the identity resolver reads (**D528**).
pub struct RegistryLister {
    directory: PathBuf,
}

impl RegistryLister {
    /// A lister over `directory`.
    #[must_use]
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }
}

impl ganja_tui::lister::Lister for RegistryLister {
    fn list(&self) -> BoxFuture<'static, Listing> {
        let directory = self.directory.clone();

        async move { list(&directory).await }.boxed()
    }
}

/// [`RegistryLister`]'s [`ganja_tui::lister::Lister::list`] body, free of
/// the trait method's `'static` bound so a test can drive it directly over
/// a private tempdir.
async fn list(directory: &std::path::Path) -> Listing {
    let registered = match registry::list(directory) {
        Ok(registered) => registered,
        Err(error) => {
            return Listing::Partial {
                rows: Vec::new(),
                error: error.to_string(),
            };
        }
    };

    let mut rows = Vec::with_capacity(registered.len());
    let mut incomplete = None;
    for registry::Registered { stem, record } in registered {
        // The registry's own liveness token first — a stale record (its
        // holder crashed or exited) is not a session to offer, whatever
        // `sessions --live`'s own GC has or has not caught up to yet.
        match registry::is_live(directory, &stem) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => {
                incomplete.get_or_insert_with(|| format!("{stem}: {error}"));
                continue;
            }
        }

        let socket = directory.join(format!("{stem}.{}", ganja_core::tool::socket::EXTENSION));
        let health = probe(&socket).await;
        rows.push(LiveSession {
            name: record.name,
            name_source: record.name_source,
            session_id: record.session_id,
            stem,
            socket,
            cwd: record.cwd,
            health,
        });
    }

    match incomplete {
        Some(error) => Listing::Partial { rows, error },
        None => Listing::Complete(rows),
    }
}

/// One socket's health, translated into [`ganja_tui::lister`]'s own
/// vocabulary — the same three-way split `ganja sessions --live` already
/// reads its own probe through (`main.rs`'s `live_sessions_command`), so a
/// session reads the same wherever it is asked about.
async fn probe(socket: &std::path::Path) -> Health {
    let Ok(client) = ganja_client::Client::on_socket(socket) else {
        return Health::Unreadable;
    };

    match tokio::time::timeout(HEALTH_DEADLINE, client.health()).await {
        Ok(Ok(_)) => Health::Answered,
        // Something answered and it was not health as this build reads it —
        // a server all the same, and the listing's own `UNREADABLE` mark.
        Ok(Err(
            ganja_client::ClientError::Refused { .. }
            | ganja_client::ClientError::Unauthorized { .. }
            | ganja_client::ClientError::Skew { .. },
        )) => Health::Unreadable,
        // Nothing answered at all, and the registry already said this name's
        // lock is held: live, whatever the silence — `sessions --live`'s
        // `HELD` mark, over here for the same reason.
        Ok(Err(_)) | Err(_) => Health::Held,
    }
}

#[cfg(test)]
mod tests {
    use ganja_tui::lister::{Health, Listing, LiveSession};

    use super::list;

    /// An empty directory lists nothing, and reads as complete: an absent
    /// registry is not a partial one.
    #[tokio::test]
    async fn an_empty_registry_lists_as_complete_and_empty() {
        let dir = tempfile::tempdir().expect("a scratch directory");

        assert_eq!(list(dir.path()).await, Listing::Complete(Vec::new()));
    }

    /// A registry this listing cannot even read at all answers `Partial`
    /// with no rows — the same refuse-don't-guess posture the resolver
    /// holds for an unreadable directory.
    #[tokio::test]
    async fn an_unreadable_directory_answers_partial_with_no_rows() {
        let listing = list(std::path::Path::new("/nonexistent-ganja-registry")).await;

        assert!(
            matches!(&listing, Listing::Partial { rows, .. } if rows.is_empty()),
            "{listing:?}"
        );
    }

    /// A stale record — nobody holds its stem's lock — is excluded, exactly
    /// as it is from resolution; a live one is listed, its health probed
    /// against a socket nothing is serving, which is neither `Answered` nor
    /// silently dropped: the registry says live (the lock is held), the
    /// socket says nothing back, and that combination is `Held`.
    #[tokio::test]
    async fn a_stale_record_is_excluded_and_a_live_one_is_listed_with_its_health_probed() {
        use ganja_core::tool::registry::{NameSource, Record, write};

        let dir = tempfile::tempdir().expect("a scratch directory");
        let record = |name: &str, id: &str| Record {
            format: ganja_core::tool::registry::FORMAT,
            session_id: id.to_owned(),
            name: name.to_owned(),
            name_source: NameSource::User,
            cwd: "/work".into(),
            root: "/work".into(),
            pid: 4242,
            started_at: 1_756_150_000_000,
        };

        write(
            dir.path(),
            "0198c1a2",
            &record("worker", "0198c1a2-0000-7000-8000-000000000001"),
        )
        .expect("a record writes");
        write(
            dir.path(),
            "0299d2b3",
            &record("stale", "0299d2b3-0000-7000-8000-000000000002"),
        )
        .expect("a record writes");

        // Only the first is live: its lock is held, unbound socket and all —
        // a socket the health probe then reaches nobody behind.
        let held = ganja_core::tool::socket::open_lock(&dir.path().join("0198c1a2.sock"))
            .expect("the lock file opens");
        held.try_lock().expect("nothing else holds a fresh lock");

        let listing = list(dir.path()).await;
        let Listing::Complete(rows) = listing else {
            panic!("a directory this test just wrote reads back complete: {listing:?}");
        };
        assert_eq!(
            rows,
            vec![LiveSession {
                name: "worker".to_owned(),
                name_source: NameSource::User,
                session_id: "0198c1a2-0000-7000-8000-000000000001".to_owned(),
                stem: "0198c1a2".to_owned(),
                socket: dir.path().join("0198c1a2.sock"),
                cwd: "/work".into(),
                health: Health::Held,
            }],
            "the stale record is excluded and the live one's health is probed: {rows:?}"
        );
    }
}
