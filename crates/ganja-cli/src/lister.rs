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
#[path = "lister_tests.rs"]
mod tests;
