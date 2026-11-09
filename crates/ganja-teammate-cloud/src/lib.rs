//! Hosted teammates sit above the engine so a served worker never includes its own
//! provisioner. The transport is ordinary REST and resumable SSE; resource creation
//! and suspension are delegated through one vendor-neutral lifecycle contract.

pub mod gate;
pub mod provisioner;
pub mod state;
pub mod trace;
