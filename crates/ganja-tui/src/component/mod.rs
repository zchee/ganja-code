//! The three panes P1 draws: transcript, prompt editor, status bar, plus the
//! modals that overlay them — one for a tool call waiting on a decision, one
//! for choosing a stored session to resume, one for choosing a theme, one for
//! choosing a model or an agent, the command palette, the reference card, and
//! the inline command menu the editor raises on a leading slash.

pub mod chat;
pub mod dropdown;
pub mod editor;
pub mod help;
pub mod list;
pub mod palette;
pub mod permission;
pub mod sessions;
pub mod status;
pub mod themes;
