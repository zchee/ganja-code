//! Recoverable, flat archive publication: captured companions first, main last.
//!
//! Only cooperating processes on one host and a local filesystem participate. A
//! published main is the completion marker: ordinary archive reads may consume
//! its WAL afterwards, so recovery never replays companion moves after publication.

use std::fs;
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::admission::Lease;
use super::{Inner, PREUUID, QUARANTINE, StorageError, with_suffix};
#[cfg(test)]
use super::{Observation, Phase};

const PENDING: &str = ".quarantine.pending";
const MAX_PENDING_BYTES: u64 = 4096;

#[cfg(test)]
#[path = "storage_maintenance_protocol_tests.rs"]
mod tests;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Preuuid,
    Corrupt,
}

impl Kind {
    fn prefix(self) -> &'static str {
        match self {
            Self::Preuuid => PREUUID,
            Self::Corrupt => QUARANTINE,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Component {
    Wal,
    Shm,
    Main,
}

const COMPONENTS: [Component; 3] = [Component::Wal, Component::Shm, Component::Main];

impl Component {
    fn suffix(self) -> &'static str {
        match self {
            Self::Wal => "-wal",
            Self::Shm => "-shm",
            Self::Main => "",
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Initialization {
    NoLegacyImport,
}

/// The exact captured set and reserved name; never recomputed during recovery.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pending {
    version: u32,
    kind: Kind,
    target: String,
    components: Vec<Component>,
    initialization: Initialization,
}

fn refused(database: &Path, reason: impl Into<String>) -> StorageError {
    StorageError::Maintenance { path: database.to_path_buf(), reason: reason.into() }
}

pub(super) fn exists(path: &Path) -> Result<bool, StorageError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(refused(path, "a maintenance component must not be a link"))
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(StorageError::Io { path: path.to_path_buf(), source }),
    }
}

impl Pending {
    pub(super) fn read(database: &Path) -> Result<Option<Self>, StorageError> {
        let path = with_suffix(database, PENDING);
        if !exists(&path)? {
            return Ok(None);
        }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;

            options.custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
            );
        }
        let file = options
            .open(&path)
            .map_err(|source| StorageError::Io { path: path.clone(), source })?;
        if !file
            .metadata()
            .map_err(|source| StorageError::Io { path: path.clone(), source })?
            .is_file()
        {
            return Err(refused(database, "the pending record must be a regular file"));
        }
        // Read one extra byte to detect oversize records without trusting a size
        // observed before a concurrent change, or allocating for the whole file.
        let mut bytes = Vec::with_capacity(MAX_PENDING_BYTES as usize + 1);
        file.take(MAX_PENDING_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|source| StorageError::Io { path: path.clone(), source })?;
        if bytes.len() as u64 > MAX_PENDING_BYTES {
            return Err(refused(database, "the pending record exceeds the size limit"));
        }
        let pending: Self = serde_json::from_slice(&bytes)
            .map_err(|error| refused(database, format!("invalid pending record: {error}")))?;
        pending.validate(database)?;
        Ok(Some(pending))
    }

    fn validate(&self, database: &Path) -> Result<(), StorageError> {
        if self.version != 1 {
            return Err(refused(database, "unsupported pending record version"));
        }
        let name = database.file_name().unwrap_or_default().to_string_lossy();
        let prefix = format!("{name}.{}-", self.kind.prefix());
        if self.target.strip_prefix(&prefix).and_then(|stamp| stamp.parse::<u64>().ok()).is_none()
            || !self.components.contains(&Component::Main)
            || self.components.len() > COMPONENTS.len()
            || COMPONENTS.iter().any(|component| {
                self.components.iter().filter(|found| *found == component).count() > 1
            })
        {
            return Err(refused(database, "pending record has an invalid target or component set"));
        }
        Ok(())
    }

    pub(super) fn begin(
        database: &Path,
        kind: &str,
        #[cfg(test)] observation: &Observation,
    ) -> Result<Self, StorageError> {
        if Self::read(database)?.is_some() {
            return Err(refused(
                database,
                "a pending move must be recovered before another begins",
            ));
        }
        let kind = match kind {
            PREUUID => Kind::Preuuid,
            QUARANTINE => Kind::Corrupt,
            _ => return Err(refused(database, "unknown archive operation")),
        };
        let mut components = Vec::with_capacity(COMPONENTS.len());
        for component in COMPONENTS {
            if exists(&with_suffix(database, component.suffix()))? {
                components.push(component);
            }
        }
        if !components.contains(&Component::Main) {
            return Err(refused(database, "the captured database main is missing"));
        }
        let name = database.file_name().unwrap_or_default().to_string_lossy();
        let mut stamp = ganja_protocol::now();
        let target = loop {
            let target = format!("{name}.{}-{stamp}", kind.prefix());
            let base = database.with_file_name(&target);
            let mut taken = false;
            for component in COMPONENTS {
                taken |= exists(&with_suffix(&base, component.suffix()))?;
            }
            if !taken {
                break target;
            }
            stamp =
                stamp.checked_add(1).ok_or_else(|| refused(database, "archive names exhausted"))?;
        };
        let pending = Self {
            version: 1,
            kind,
            target,
            components,
            initialization: Initialization::NoLegacyImport,
        };
        let path = with_suffix(database, PENDING);
        let temporary = with_suffix(&path, &format!("-{}.tmp", ganja_protocol::uuidv7()));
        let bytes = serde_json::to_vec(&pending)
            .map_err(|source| StorageError::Encode { path: path.clone(), source })?;
        {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|source| StorageError::Io { path: temporary.clone(), source })?;
            file.write_all(&bytes)
                .map_err(|source| StorageError::Io { path: temporary.clone(), source })?;
            // Close the missing publication barrier, not a power-loss guarantee
            // for the later file-set moves or SQLite replacement initialization.
            file.sync_all()
                .map_err(|source| StorageError::Io { path: temporary.clone(), source })?;
        }
        #[cfg(test)]
        observation.hit(Phase::IntentTemporary);
        // Exclusive admission protects this fixed name. A temporary record is
        // never authority, even if its bytes were completely written before death.
        if exists(&path)? {
            return Err(refused(database, "the pending intent name is already occupied"));
        }
        fs::rename(&temporary, &path)
            .map_err(|source| StorageError::Io { path: path.clone(), source })?;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| StorageError::Io { path: parent.to_path_buf(), source })?;
        #[cfg(test)]
        observation.hit(Phase::IntentPublished);
        Ok(pending)
    }

    pub(super) fn move_files(
        &self,
        database: &Path,
        #[cfg(test)] observation: &Observation,
    ) -> Result<(), StorageError> {
        self.validate(database)?;
        let archived = database.with_file_name(&self.target);
        if exists(&archived)? {
            // The main rename publishes the set. Its WAL may since have been
            // checkpointed by an archive reader; live names now belong to the replacement.
            return Ok(());
        }
        for component in COMPONENTS {
            let from = with_suffix(database, component.suffix());
            let to = with_suffix(&archived, component.suffix());
            let source = exists(&from)?;
            let destination = exists(&to)?;
            if self.components.contains(&component) {
                if source == destination {
                    return Err(refused(
                        database,
                        format!("ambiguous or missing component {}", from.display()),
                    ));
                }
            } else if source || destination {
                return Err(refused(database, format!("unexpected component {}", from.display())));
            }
        }
        for component in COMPONENTS {
            if !self.components.contains(&component) {
                continue;
            }
            let from = with_suffix(database, component.suffix());
            let to = with_suffix(&archived, component.suffix());
            if exists(&to)? {
                continue;
            }
            #[cfg(test)]
            observation.hit(Phase::Renaming(component.suffix()));
            fs::rename(&from, &to).map_err(|source| StorageError::Io { path: from, source })?;
            #[cfg(test)]
            observation.hit(Phase::Renamed(component.suffix()));
        }
        Ok(())
    }
}

impl Inner {
    pub(super) fn recover(&self, lease: &Arc<Lease>, pending: Pending) -> Result<(), StorageError> {
        pending.move_files(
            &self.database,
            #[cfg(test)]
            &self.observation,
        )?;
        tracing::warn!(path = %self.database.display(),
            kept = %self.database.with_file_name(&pending.target).display(),
            "the session database was moved aside; this project starts with an empty store");
        self.initialize(lease, false)?;
        let path = with_suffix(&self.database, PENDING);
        fs::remove_file(&path).map_err(|source| StorageError::Io { path, source })?;
        #[cfg(test)]
        self.observation.hit(Phase::IntentRemoved);
        Ok(())
    }
}
