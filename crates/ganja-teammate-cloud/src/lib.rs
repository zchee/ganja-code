//! Hosted teammates sit above the engine so a served worker never includes its own
//! provisioner. The transport is ordinary REST and resumable SSE; resource creation
//! and suspension are delegated through one vendor-neutral lifecycle contract.
//!
//! Reported reverts and contradictory lifecycle observations invalidate the held
//! cursor before reconnecting. An unannounced revert with no such observation is
//! unsupported: snapshot metadata and cursor ordering do not identify it.

mod backend;
mod delivery;
pub mod gate;
mod health;
mod member;
pub mod provisioner;
pub mod state;
pub mod trace;
mod transport;

pub use backend::{CloudBackend, PASSWORD_ENV};
pub use member::{
    CloudControl, CloudMember, ControlAction, ControlAdmissionError, ControlOperation,
    ControlOutcome, control,
};
