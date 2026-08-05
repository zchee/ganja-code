//! What every handler can reach: the engine, the read-only context the
//! informational routes answer from, and the pending-permission map the
//! tracker keeps current.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use ganja_core::{Config, Engine, Storage};
use ganja_protocol::{PermissionId, SessionId};
use serde::Serialize;

use crate::{Credentials, ServeConfig};

/// One permission request the engine is waiting on, as `GET /permission`
/// lists it — the fields of `Event::PermissionRequested`, held until
/// something answers.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct PendingPermission {
    /// Session whose turn is waiting.
    pub(crate) session_id: SessionId,
    /// What a reply names.
    pub(crate) id: PermissionId,
    /// The tool call waiting on the decision.
    pub(crate) call_id: String,
    /// Tool asking to run.
    pub(crate) tool: String,
    /// One line saying what would run.
    pub(crate) title: String,
    /// The arguments it would run with.
    pub(crate) args: serde_json::Value,
    /// Directories outside the project the call would touch.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) directories: Vec<String>,
}

/// The map behind `GET /permission`, shared between the tracker that writes
/// it and the handlers that read it. A `BTreeMap` so the listing is in
/// request order — permission ids sort in creation order.
#[derive(Clone, Default)]
pub(crate) struct Pending(Arc<Mutex<BTreeMap<String, PendingPermission>>>);

impl Pending {
    pub(crate) fn insert(&self, request: PendingPermission) {
        self.0
            .lock()
            .expect("the pending map is never poisoned")
            .insert(request.id.as_str().to_owned(), request);
    }

    pub(crate) fn remove(&self, id: &PermissionId) {
        self.0
            .lock()
            .expect("the pending map is never poisoned")
            .remove(id.as_str());
    }

    /// Every request still waiting, oldest first.
    pub(crate) fn list(&self) -> Vec<PendingPermission> {
        self.0
            .lock()
            .expect("the pending map is never poisoned")
            .values()
            .cloned()
            .collect()
    }
}

/// The directory this server serves, held beside its canonical form so the
/// guard compares paths rather than spellings.
pub(crate) struct ServedDirectory {
    given: PathBuf,
    canonical: PathBuf,
}

impl ServedDirectory {
    fn new(given: PathBuf) -> Self {
        // A directory that cannot canonicalize still serves: the guard then
        // matches on the spelling alone, which is all there is to match.
        let canonical = given.canonicalize().unwrap_or_else(|_| given.clone());

        Self { given, canonical }
    }

    /// Whether `asked` names this directory — by spelling, or by resolving to
    /// the same place. A path that does not exist resolves to nothing and
    /// matches nothing.
    pub(crate) fn matches(&self, asked: &str) -> bool {
        let asked = PathBuf::from(asked);
        if asked == self.given || asked == self.canonical {
            return true;
        }

        asked
            .canonicalize()
            .is_ok_and(|resolved| resolved == self.canonical)
    }

    pub(crate) fn given(&self) -> &PathBuf {
        &self.given
    }
}

/// Everything the router closes over. Cloned per request by axum, so the
/// contents are shared handles rather than values.
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) engine: Arc<Engine>,
    pub(crate) credentials: Option<Arc<Credentials>>,
    pub(crate) directory: Arc<ServedDirectory>,
    pub(crate) root: Arc<PathBuf>,
    pub(crate) data: Option<Arc<PathBuf>>,
    pub(crate) storage: Option<Storage>,
    pub(crate) config: Option<Arc<Config>>,
    pub(crate) pending: Pending,
    pub(crate) heartbeat: Duration,
    /// Flipped once when the server is asked to stop. An SSE stream is
    /// otherwise endless, and a graceful shutdown that waited for one would
    /// wait forever — upstream force-closes its open connections for exactly
    /// this reason (`server/server.ts:195-211`); here every pump watches
    /// this and ends its own stream instead.
    pub(crate) shutdown: tokio::sync::watch::Receiver<bool>,
}

impl AppState {
    pub(crate) fn new(
        engine: Arc<Engine>,
        config: ServeConfig,
        pending: Pending,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Self {
        Self {
            engine,
            credentials: config.credentials.map(Arc::new),
            directory: Arc::new(ServedDirectory::new(config.directory)),
            root: Arc::new(config.root),
            data: config.data.map(Arc::new),
            storage: config.storage,
            config: config.config.map(Arc::new),
            pending,
            heartbeat: config.heartbeat,
            shutdown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ServedDirectory;

    #[test]
    fn a_directory_matches_its_own_spellings_and_nothing_else() {
        let temp = std::env::temp_dir();
        let served = ServedDirectory::new(temp.clone());

        assert!(served.matches(&temp.display().to_string()));
        // The canonical spelling matches too — on macOS `/tmp` and
        // `/private/tmp` are the same place, and the guard must know it.
        assert!(
            served.matches(
                &temp
                    .canonicalize()
                    .expect("temp resolves")
                    .display()
                    .to_string()
            )
        );

        assert!(!served.matches("/nonexistent/elsewhere"));
        assert!(!served.matches(""));
    }
}
