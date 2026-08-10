//! The three panes P1 draws: transcript, prompt editor, status bar, plus the
//! modals that overlay them — one for a tool call waiting on a decision, one
//! for choosing a stored session to resume, one for choosing a theme, one for
//! choosing a model or an agent, the command palette, the reference card, and
//! the two inline menus the editor raises — one on a leading slash, one on an
//! `@`.

pub mod chat;
pub mod dropdown;
pub mod editor;
pub mod files;
pub mod help;
pub mod list;
pub mod palette;
pub mod permission;
pub mod question;
pub mod sessions;
pub mod status;
pub mod themes;
pub mod variants;
