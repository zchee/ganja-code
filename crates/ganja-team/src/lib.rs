//! Claude Code's teams directory: member records, and the file-backed
//! mailboxes teammates are addressed through.
//!
//! **Upstream opencode has no counterpart.** It has no teams, no mailbox and
//! no second agent to address, so unlike almost everything else in this
//! workspace there is no TypeScript to port behavior from. The specification is
//! Claude Code's, read out of `docs/references/claude-teammates.en.md`: §2 for
//! the on-disk data model and §3 for the mailbox surface. The divergences and
//! the reasons for them are **D497**.
//!
//! # Why this is a crate
//!
//! Because the documents are **somebody else's format**. A real `claude`
//! process can be sharing the very directory this crate writes into (D-1), so
//! two things are interop contracts rather than implementation details: the
//! bytes of a document, and the protocol by which a writer holds an inbox.
//! Both are served by shapes that keep what they do not understand — a
//! `#[serde(flatten)]` passthrough over an [`indexmap::IndexMap`], so an
//! unknown key survives a rewrite in the position it arrived in.
//!
//! That is the exact opposite of `ganja-protocol`'s posture, which declares an
//! exhaustive vocabulary and refuses a peer that grew a field rather than
//! guessing at it. Putting a passthrough shape there would contradict the
//! doctrine at the point it is stated, so the split is: Claude's documents
//! here, beside the file I/O, and ganja's own `TeamView`/`MemberView`
//! projection in `ganja-protocol` for anything that merely *renders* a team.
//! A frontend therefore needs no dependency on this crate at all.
//!
//! # What it does not know
//!
//! Where ganja keeps its homes, what a session is, and what a permission
//! decides — every one of those would put an engine's answers underneath the
//! engine. The teams directory arrives as a [`TeamsRoot`] value, the way
//! `skill::Roots` arrives in `ganja-tool`; a message timestamp arrives as a
//! string. CI asserts the whole of it: this crate's internal dependency list is
//! exactly `ganja-protocol`.
//!
//! The crate is **synchronous**. A mailbox write is a sub-second
//! read-modify-write on a small file, and the lock schedule it may sleep
//! through is measured in milliseconds; making that async would put a runtime
//! under the file I/O to buy nothing. Whoever calls it from inside a turn
//! wraps it in `spawn_blocking`.
//!
//! ```
//! use ganja_team::{MailboxMessage, MemberName, TeamName, TeamsRoot, mailbox, record};
//!
//! let home = tempfile::tempdir()?;
//! let root = TeamsRoot::new(home.path().join("teams"));
//! let team = TeamName::parse("session-224cbeab")?;
//! let worker = MemberName::parse("demo-worker-1")?;
//!
//! let inbox = root.inbox_path(&team, &worker);
//! mailbox::write(
//!     &inbox,
//!     MailboxMessage::new("team-lead", "start on the parser", record::now_iso8601()),
//! )?;
//!
//! let held = mailbox::read(&inbox)?;
//! assert_eq!(held.valid[0].text, "start on the parser");
//!
//! // Delivered means gone: an inbox's depth is backlog, not history.
//! let delivered: Vec<_> = held.valid.iter().map(mailbox::identity).collect();
//! mailbox::prune_delivered(&inbox, &delivered)?;
//! assert!(mailbox::read(&inbox)?.valid.is_empty());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod lock;
pub mod mailbox;
pub mod record;
pub mod team;

pub use mailbox::{Contents, Identity, MailboxError, Pruned};
pub use record::{MailboxMessage, MemberRecord, Spawn, Surface, TeamFile};
pub use team::{
    COLLISION_SEPARATOR, DEFAULT_TEAM, LEAD, MAIN, MemberName, NAME_MAX, NameError, TeamName,
    TeamsRoot,
};
