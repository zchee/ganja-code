//! Where sessions live between runs, and the working-tree snapshots `/undo`
//! and `/rewind` walk.
//!
//! Two modules, born together as [`storage`] and [`snapshot`] inside
//! `ganja-core` and split into a leaf of their own in **D540**
//! (`.omc/plans/2026-08-28-teammate-seam-crate-split.md`, W3): the
//! `634d8dd` provider-split procedure, applied a second time to the crate on
//! the other side of the engine. This crate's internal dependency list is
//! closed at exactly `ganja-permission` and `ganja-protocol` — CI asserts it
//! — because a session store and a snapshot repository need to know which
//! worktree they are anchored on and what a stored record decodes to, and
//! nothing else; the engine is not in this crate's dependency graph, and
//! never will be.
//!
//! `ganja-core` re-exports both modules under the names they always had —
//! `ganja_core::storage` and `ganja_core::snapshot`, [`storage::SessionId`],
//! [`storage::SessionInfo`], [`storage::Storage`], [`storage::StorageError`],
//! [`snapshot::RevertState`] and [`snapshot::Snapshots`] all keep resolving —
//! so no caller outside this crate had to change a single path. A caller that
//! wants only one of the two depends on this crate directly, the way
//! `ganja-cli` reaches `auth login` through `ganja-provider` rather than
//! through the engine's facade.

pub mod snapshot;
pub mod storage;
