//! Admission for one local database generation. The stable lock inode is never moved.

use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{fs, thread};

use rusqlite::Connection;

use super::maintenance::{Pending, exists};
use super::{
    Inner, PREUUID, QUARANTINE, QUARANTINE_LOCK, StorageError, connect, convert, damaged,
    integrity, migrate, preuuid_id, with_suffix,
};
#[cfg(test)]
use super::{Observation, Phase};

const ADMISSION_BUDGET: Duration = Duration::from_secs(5);
const ADMISSION_POLL: Duration = Duration::from_millis(10);

/// One independently opened lock descriptor, shared only through its Arc.
pub(super) struct Lease(fs::File);

impl Lease {
    fn try_take(database: &Path, exclusive: bool) -> Result<Option<Arc<Self>>, StorageError> {
        let path = with_suffix(database, &format!(".{QUARANTINE_LOCK}"));
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;

            options.custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
            );
        }
        let file = loop {
            match options.open(&path) {
                Ok(file) => break file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    match options.create_new(true).open(&path) {
                        Ok(file) => break file,
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                            // Another opener won creation; open its stable inode without truncating.
                            options.create_new(false);
                        }
                        Err(source) => return Err(StorageError::Io { path, source }),
                    }
                }
                Err(source) => {
                    // Special-file open errors differ between macOS and Linux;
                    // classify the object without following its final component.
                    if fs::symlink_metadata(&path).is_ok_and(|metadata| !metadata.is_file()) {
                        return Err(StorageError::Maintenance {
                            path,
                            reason: "the admission lock must be a regular file".to_owned(),
                        });
                    }
                    return Err(StorageError::Io { path, source });
                }
            }
        };
        if !file
            .metadata()
            .map_err(|source| StorageError::Io { path: path.clone(), source })?
            .is_file()
        {
            return Err(StorageError::Maintenance {
                path,
                reason: "the admission lock must be a regular file".to_owned(),
            });
        }
        let outcome = if exclusive { file.try_lock() } else { file.try_lock_shared() };
        match outcome {
            Ok(()) => Ok(Some(Arc::new(Self(file)))),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(source)) => Err(StorageError::Io { path, source }),
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Connection first, lease last, including thread-spawn and startup failures.
pub(super) struct LeasedConnection {
    connection: Option<Connection>,
    pub(super) lease: Arc<Lease>,
    #[cfg(test)]
    observation: Observation,
    #[cfg(test)]
    closed: Phase,
}

impl LeasedConnection {
    pub(super) fn new(
        connection: Connection,
        lease: Arc<Lease>,
        #[cfg(test)] observation: Observation,
        #[cfg(test)] closed: Phase,
    ) -> Self {
        Self {
            connection: Some(connection),
            lease,
            #[cfg(test)]
            observation,
            #[cfg(test)]
            closed,
        }
    }
}

impl Deref for LeasedConnection {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.connection.as_ref().expect("a live connection is only taken during drop")
    }
}

impl DerefMut for LeasedConnection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.connection.as_mut().expect("a live connection is only taken during drop")
    }
}

impl Drop for LeasedConnection {
    fn drop(&mut self) {
        #[cfg(test)]
        if self.closed == Phase::WriterClosed {
            self.observation.hit(Phase::WriterClosing);
        }
        drop(self.connection.take());
        #[cfg(test)]
        self.observation.hit(self.closed);
    }
}

enum Probe {
    Ready(LeasedConnection),
    Old(LeasedConnection, String),
    Corrupt(String),
    Absent,
    Pending,
}

impl Inner {
    pub(super) fn admit(&self) -> Result<LeasedConnection, StorageError> {
        let budget = ADMISSION_BUDGET;
        #[cfg(test)]
        let budget = self.admission_budget.unwrap_or(budget);
        let deadline = Instant::now() + budget;
        if let Some(parent) = self.database.parent() {
            fs::create_dir_all(parent)
                .map_err(|source| StorageError::Io { path: parent.to_path_buf(), source })?;
        }
        loop {
            #[cfg(test)]
            self.observation.hit(Phase::AdmissionDeadline(deadline));
            #[cfg(test)]
            self.observation.hit(Phase::Admission);
            let Some(shared) = Lease::try_take(&self.database, false)? else {
                #[cfg(test)]
                self.observation.hit(Phase::SharedBlocked);
                if Instant::now() >= deadline {
                    return Err(StorageError::Maintaining { path: self.database.clone() });
                }
                thread::sleep(
                    ADMISSION_POLL.min(deadline.saturating_duration_since(Instant::now())),
                );
                continue;
            };
            #[cfg(test)]
            self.observation.hit(Phase::SharedAcquired);
            match self.probe(&shared)? {
                Probe::Ready(connection) => return Ok(connection),
                Probe::Old(connection, _) => drop(connection),
                Probe::Corrupt(_) | Probe::Absent | Probe::Pending => {}
            }
            drop(shared);
            #[cfg(test)]
            self.observation.hit(Phase::ExclusiveWanted);
            if let Some(exclusive) = Lease::try_take(&self.database, true)? {
                #[cfg(test)]
                self.observation.hit(Phase::ExclusiveAcquired);
                self.maintain(&exclusive)?;
                drop(exclusive);
                continue;
            }
            #[cfg(test)]
            self.observation.hit(Phase::ExclusiveBlocked);
            if Instant::now() >= deadline {
                // Re-enter shared admission once more to distinguish a readable
                // old store from a pending/corrupt generation before deferring.
                let Some(shared) = Lease::try_take(&self.database, false)? else {
                    return Err(StorageError::Maintaining { path: self.database.clone() });
                };
                match self.probe(&shared)? {
                    Probe::Ready(connection) => return Ok(connection),
                    Probe::Old(connection, _) => {
                        tracing::warn!(path = %self.database.display(),
                            "setting the old session store aside was deferred: another process still owns a shared lease; the existing readable store remains live with its old ids");
                        return Ok(connection);
                    }
                    Probe::Corrupt(_) | Probe::Absent | Probe::Pending => {
                        return Err(StorageError::Maintaining { path: self.database.clone() });
                    }
                }
            }
            thread::sleep(ADMISSION_POLL.min(deadline.saturating_duration_since(Instant::now())));
        }
    }

    fn connection(&self, lease: &Arc<Lease>) -> Result<LeasedConnection, StorageError> {
        #[cfg(test)]
        self.observation.hit(Phase::ReaderOpening);
        let connection = connect(&self.database)?;
        #[cfg(test)]
        if self.no_checkpoint {
            connection
                .set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
                .expect("the real SQLite fixture supports no-checkpoint-on-close");
        }
        #[cfg(test)]
        self.observation.hit(Phase::ReaderOpened);
        Ok(LeasedConnection::new(
            connection,
            lease.clone(),
            #[cfg(test)]
            self.observation.clone(),
            #[cfg(test)]
            Phase::ReaderClosed,
        ))
    }

    fn probe(&self, lease: &Arc<Lease>) -> Result<Probe, StorageError> {
        if Pending::read(&self.database)?.is_some() {
            return Ok(Probe::Pending);
        }
        if !exists(&self.database)? {
            return Ok(Probe::Absent);
        }
        let mut connection = match self.connection(lease) {
            Ok(connection) => connection,
            Err(error) if damaged(&error) => return Ok(Probe::Corrupt(error.to_string())),
            Err(error) => return Err(error),
        };
        if let Err(reason) = integrity(&connection) {
            return Ok(Probe::Corrupt(reason));
        }
        migrate(&mut connection, &self.database)?;
        match preuuid_id(&connection, &self.database)? {
            Some(old) => {
                #[cfg(test)]
                self.observation.hit(Phase::OldIds);
                Ok(Probe::Old(connection, old))
            }
            None => Ok(Probe::Ready(connection)),
        }
    }

    fn maintain(&self, lease: &Arc<Lease>) -> Result<(), StorageError> {
        if let Some(pending) = Pending::read(&self.database)? {
            return self.recover(lease, pending);
        }
        // The earlier shared probe is not authority: a competing starter may
        // have completed the replacement before this exclusive acquisition.
        let (reason, kind) = match self.probe(lease)? {
            Probe::Ready(connection) => {
                drop(connection);
                return Ok(());
            }
            Probe::Absent => return self.initialize(lease, true),
            Probe::Old(connection, old) => {
                drop(connection);
                (format!("session {old} predates UUIDv7 ids"), PREUUID)
            }
            Probe::Corrupt(reason) => (reason, QUARANTINE),
            Probe::Pending => {
                return Err(StorageError::Maintenance {
                    path: self.database.clone(),
                    reason: "the pending record changed during maintenance".to_owned(),
                });
            }
        };
        let pending = Pending::begin(
            &self.database,
            kind,
            #[cfg(test)]
            &self.observation,
        )?;
        tracing::warn!(path = %self.database.display(), reason, "setting the session database aside");
        self.recover(lease, pending)
    }

    pub(super) fn initialize(&self, lease: &Arc<Lease>, import: bool) -> Result<(), StorageError> {
        let mut connection = self.connection(lease)?;
        #[cfg(test)]
        self.observation.hit(Phase::ReplacementCreated);
        migrate(&mut connection, &self.database)?;
        #[cfg(test)]
        self.observation.hit(Phase::Migrated);
        if import {
            convert(&mut connection, &self.root, &self.database)?;
        }
        Ok(())
    }
}
