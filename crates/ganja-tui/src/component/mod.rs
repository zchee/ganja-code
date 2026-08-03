//! The three panes P1 draws: transcript, prompt editor, status bar, plus the
//! modals that overlay them — one for a tool call waiting on a decision, one
//! for choosing a stored session to resume.

pub mod chat;
pub mod editor;
pub mod permission;
pub mod sessions;
pub mod status;
