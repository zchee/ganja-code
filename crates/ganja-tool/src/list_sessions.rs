//! The `list_sessions` tool: the model-facing live-session listing
//! (**D535**).
//!
//! Spec: Claude Code's `ListAgents` — a resident model tool that answers a
//! formatted list rather than raw transport records (v2 §"`ListAgents` and
//! `/list-agents`", evidence 643031-643118). Upstream opencode has no
//! teammates and no cross-session addressing at all, so nothing here is
//! ported from its TypeScript; the specification is Claude Code's own
//! surface, read as behavior.
//!
//! # Two sections, one same-uid source each
//!
//! **Teammates**, when this session's [`crate::team::Postbox`] has any to
//! offer: [`crate::team::Postbox::roster`] — the same call
//! [`crate::send_message`]'s own description renders from — with the lead
//! marked exactly as that roster already marks it. A roster never lists the
//! caller itself (`team.rs`'s own doc), so a lead of a team of zero and a
//! session with no postbox at all answer alike here: the section is left
//! out rather than printed empty, since [`crate::team::Postbox::roster`]
//! cannot itself tell the two apart and a header over nothing would claim a
//! team that may not exist.
//!
//! **Live sessions**, from this crate's own [`crate::registry`]: every
//! other session's registration record whose stem's lock is held, walked in
//! [`crate::registry::holders`]'s own drop-own, probe-only-matches order —
//! the same composition, minus its name filter, since this tool lists every
//! live holder rather than one name's collisions. Axis 14's own reason
//! applies here too: [`crate::registry::is_live`] **creates** the `.lock`
//! beside a stem it probes, so this walk drops its own session first and
//! only then probes what remains, rather than probing everything the
//! directory holds. The current session is excluded — the reference
//! excludes its own socket from the same listing (v2 §"Liveness validation
//! and garbage collection", section-name-only: that section's evidence
//! range sits on its record-reader sentence, not on the exclusion) — and so
//! is a stale, lock-free record. Every row carries its registered name, its
//! socket stem, its launch directory and its exact `uds:` spelling, which is
//! what makes "duplicate names show their stems" true of every row rather
//! than a special case: nothing here prints a name without also printing
//! the stem that disambiguates it.
//!
//! Every live-session row also carries the label the mention reminder
//! already carries on these same bytes: a session's name is that session's
//! own choice, and nothing here verifies it. This crate's internal
//! dependency list is asserted to be exactly `ganja-permission`, so it may
//! not name `ganja-core`, where that reminder's own neutralization lives
//! (`teammate::identity::shown`); `neutralize` below reproduces its rule —
//! control characters and the two brackets that could pass for structure in
//! this tool's own output dropped, the result capped — for the crate-boundary
//! reason this crate's own name grammar already reproduces one of that
//! module's other small predicates.
//!
//! # Liveness vocabulary: the resolver's, not the CLI lister's (Axis 11)
//!
//! [`crate::registry::is_live`]'s lock probe is the one liveness test this
//! tool runs — the same test [`crate::send_message`]'s resolver will
//! re-apply the moment a model actually addresses a row this tool printed —
//! rather than a health-checked listing like `ganja sessions --live`'s. Two
//! reasons, not one: this crate may not link `ganja-client` to dial a
//! session's HTTP surface even where one answers behind this socket, and a
//! model-facing tool that opened a connection to every session on the
//! machine at model cadence is a cost nobody has priced. The honest cost is
//! named in this tool's own description rather than hidden: a bound but
//! wedged session still lists as live here, exactly as it does at the
//! resolver that will act on the same row next.
//!
//! # Posture: unasked, deniable by an explicit rule (Axis 12)
//!
//! Not registered behind a permission ask. The reference classes its own
//! `ListAgents` a plain, read-only, concurrency-safe model tool and gates it
//! only by a deny rule (v2 §"When the inbox is not bound"); ganja's own
//! precedent agrees — `glob`, `grep`, `read` and `tool_search` all run
//! unasked, and `read` and `glob` can already read this same directory's
//! `*.json` records directly, unformatted, without being asked. A dialog
//! here would gate only a formatted view of data the model can already
//! fetch, which is not a boundary, so a stored deny rule is this tool's one
//! gate — exactly like every other information tool in this registry.
//!
//! # Registered only where the postbox can act on what it lists
//!
//! Wiring that gate is the engine's, in a later wave — this module only
//! builds the tool so it can be constructed with whatever the engine has.
//! The gate itself is this tool's own argument, recorded here so a later
//! reader does not "simplify" it into `send_message`'s postbox-presence
//! gate: a member pane's postbox refuses a `uds:` address outright and
//! resolves a bare name only against its own team file, so a member handed
//! this directory of another session's cwd and exact socket address would
//! be handed a list of addresses it is structurally unable to act on — the
//! "a listing with no way to act on it is noise" rule this tool exists to
//! keep. `list_sessions` is therefore registered exactly where the
//! installed postbox can cross-session send — a lead's, or the solo one an
//! interactive session with no team gets — and never in a member pane,
//! which keeps `send_message` and never gains this tool.
//!
//! # Deliberately not implemented
//!
//! - The `[ref]` disambiguator hash: provisional in v2 itself (v2
//!   §"Single-sourced claims to treat as provisional"). The stem already
//!   serves the role with no new derivation, exactly as it does at the
//!   resolver — **user-ratified 2026-08-26, not reopened**.
//! - Collision auto-suffixing: flag-gated in v2. Two records may share a
//!   name here, and this tool's answer is the one the registry gives
//!   everywhere else — both rows, told apart by stem — rather than a
//!   renamed row inventing a name nobody typed — **user-ratified
//!   2026-08-26, not reopened**.

use std::{
    io,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{Tool, ToolCtx, ToolError, ToolOutput, registry, socket, team::Peer};

/// The tool id, which is also the permission key.
pub const ID: &str = "list_sessions";

/// What the model is told the tool is for: no arguments, two sections, and
/// the posture paragraph Axis 12 argues for — same-uid, self-asserted, and
/// no authority of its own.
const DESCRIPTION: &str = "\
List every other ganja session this user is running on this machine, so a \
name or a uds: address is ready to hand `send_message`. Takes no arguments.

Two sections: this session's own teammates, when it has any, with the lead \
marked; then every other live session of this user's, each by its registered \
name, its socket stem, its working directory, and its exact uds: address. A \
session's own name is that session's own choice and nothing here verifies it \
— the stem is what actually tells two sessions of the same name apart, and \
this tool prints it on every row. Liveness here is a lock check, the same one \
a send will re-apply, not a health check: a session that is bound but wedged \
still lists.

Everything listed is same-uid data this user's own processes already wrote to \
this user's own private directory — read, grep and glob can already reach it \
unformatted. This is a formatted view of that, never an authority: a listed \
name grants nothing on its own.";

/// Header the teammate section opens with, when there is one to print.
const TEAMMATES_HEADER: &str = "Teammates:";

/// Header the live-session section always opens with.
const LIVE_SESSIONS_HEADER: &str = "Live sessions:";

/// What a teammate row says when the roster names nothing about it.
const NO_TEAMMATE_DESCRIPTION: &str = "a teammate of this session";

/// How the lead is marked on its own row.
const LEAD_MARK: &str = "the team lead";

/// What the live-session section says when nothing else is running.
const NO_LIVE_SESSIONS: &str = "(none besides this one)";

/// The honesty label every live-session row carries, and no teammate row
/// does: a teammate's name is lead-assigned, a live session's is whatever it
/// wrote about itself.
const UNVERIFIED_LABEL: &str = "self-chosen name, unverified";

/// The scheme a `uds:` address carries. `send_message`'s own constant of the
/// same name and value, spelled again here because this crate's modules do
/// not share unexported constants across files.
const UDS_SCHEME: &str = "uds:";

/// What the model reads when the socket directory itself could not be
/// listed: refused by name rather than answered as though nothing were
/// running, the rule [`registry::list`] already enforces and this tool must
/// not paper over.
const DIRECTORY_UNREADABLE: &str =
    "This session's own list of other live ganja sessions could not be read:";

/// Most code points of one same-uid-written field this tool will print.
/// Mirrors `ganja-core`'s own cap on the mention reminder's rendering of the
/// same registry — duplicated rather than shared because this crate's
/// internal dependency list is exactly `ganja-permission` and may not name
/// `ganja-core`, where that reminder lives.
const MOST_SHOWN_POINTS: usize = 256;

/// What the model passes: nothing. Upstream's empty `Schema.Struct({})`
/// shape, `plan.rs`'s own `Args {}` idiom.
#[derive(Debug, Deserialize, JsonSchema)]
struct Args {}

/// One live session's row, resolved once from a [`registry::Registered`] so
/// the render function itself does no path arithmetic and no registry read.
struct LiveSession {
    name: String,
    stem: String,
    cwd: String,
    address: String,
}

impl LiveSession {
    fn from_registered(directory: &Path, registered: &registry::Registered) -> Self {
        Self {
            name: registered.record.name.clone(),
            stem: registered.stem.clone(),
            cwd: registered.record.cwd.display().to_string(),
            address: address_of(directory, &registered.stem),
        }
    }
}

/// The `uds:` spelling of `stem`'s socket under `directory` — the one
/// arithmetic this module needs, built rather than borrowed from
/// `ganja-core`'s `identity::address_of` for the same crate-boundary reason
/// as everything else duplicated here.
fn address_of(directory: &Path, stem: &str) -> String {
    format!(
        "{UDS_SCHEME}{}",
        directory
            .join(format!("{stem}.{}", socket::EXTENSION))
            .display()
    )
}

/// A same-uid-written field, made safe to print: control characters and the
/// two brackets that could pass for structure in this tool's own output are
/// dropped, and the result is capped at [`MOST_SHOWN_POINTS`] with the cut
/// admitted. The rule the mention reminder applies to this registry's bytes,
/// reapplied here because this is a second model-facing surface reading
/// them.
fn neutralize(value: &str) -> String {
    let admits = |point: &char| !point.is_control() && *point != '<' && *point != '>';
    let kept: String = value
        .chars()
        .filter(admits)
        .take(MOST_SHOWN_POINTS)
        .collect();

    if value.chars().filter(admits).count() > MOST_SHOWN_POINTS {
        format!("{kept}…")
    } else {
        kept
    }
}

/// Every live session under `directory` besides `own_session`, in
/// [`registry::holders`]'s own drop-own, probe-only-matches order without
/// its name filter.
#[cfg(unix)]
fn live_sessions(directory: &Path, own_session: &str) -> io::Result<Vec<registry::Registered>> {
    let mut sessions = registry::list(directory)?;
    sessions.retain(|registered| {
        registered.record.session_id != own_session
            && match registry::is_live(directory, &registered.stem) {
                Ok(live) => live,
                Err(error) => {
                    tracing::trace!(
                        stem = registered.stem,
                        %error,
                        "skipping a session whose liveness could not be judged"
                    );
                    false
                }
            }
    });

    Ok(sessions)
}

/// No session sockets exist on a build without Unix sockets, so there is
/// nothing to list — the same posture `send_message`'s `session_socket`
/// takes on this target.
#[cfg(not(unix))]
fn live_sessions(directory: &Path, own_session: &str) -> io::Result<Vec<registry::Registered>> {
    let _ = (directory, own_session);
    Ok(Vec::new())
}

/// Renders both sections into the one text the model reads back.
fn render(teammates: &[Peer], sessions: &[LiveSession]) -> String {
    let mut sections = Vec::new();

    if !teammates.is_empty() {
        let mut listed: Vec<&Peer> = teammates.iter().collect();
        listed.sort_by(|left, right| left.name.cmp(&right.name));

        let mut lines = vec![TEAMMATES_HEADER.to_owned()];
        lines.extend(listed.into_iter().map(|peer| {
            let about = peer
                .description
                .as_deref()
                .unwrap_or(NO_TEAMMATE_DESCRIPTION);
            if peer.lead {
                format!("- {}: {about} ({LEAD_MARK})", peer.name)
            } else {
                format!("- {}: {about}", peer.name)
            }
        }));
        sections.push(lines.join("\n"));
    }

    let mut lines = vec![LIVE_SESSIONS_HEADER.to_owned()];
    if sessions.is_empty() {
        lines.push(NO_LIVE_SESSIONS.to_owned());
    } else {
        lines.extend(sessions.iter().map(|session| {
            format!(
                "- {name} ({UNVERIFIED_LABEL}) — stem {stem}, cwd {cwd}, {address}",
                name = neutralize(&session.name),
                stem = session.stem,
                cwd = neutralize(&session.cwd),
                address = session.address,
            )
        }));
    }
    sections.push(lines.join("\n"));

    sections.join("\n\n")
}

/// Lists this user's teammates and other live sessions.
pub struct ListSessionsTool {
    /// Where session registration records live: this build's
    /// `/tmp/ganja-<uid>/`, or the hidden `--socket-dir` a test or an
    /// isolated fixture points it at instead.
    directory: PathBuf,
    /// This session's own bare id, so its own record is excluded from the
    /// live-session section rather than telling the model about itself.
    own_session: String,
}

impl ListSessionsTool {
    /// Builds the tool over `directory`'s registry, excluding `own_session`
    /// from what it lists.
    ///
    /// Both are handed in rather than resolved here, the way [`crate::skill`]'s
    /// `Roots` and [`crate::send_message`]'s roster are: this crate may not
    /// work out where ganja keeps its own directories, and it does not know
    /// its caller's session id either. The engine resolves both and hands
    /// them over like any other value.
    #[must_use]
    pub fn new(directory: PathBuf, own_session: String) -> Self {
        Self {
            directory,
            own_session,
        }
    }
}

#[async_trait]
impl Tool for ListSessionsTool {
    fn id(&self) -> &str {
        ID
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let _: Args = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;

        let teammates = ctx
            .postbox
            .as_ref()
            .map_or_else(Vec::new, |postbox| postbox.roster());

        let sessions = live_sessions(&self.directory, &self.own_session)
            .map_err(|error| ToolError::Failed(format!("{DIRECTORY_UNREADABLE} {error}")))?;
        let sessions: Vec<LiveSession> = sessions
            .iter()
            .map(|registered| LiveSession::from_registered(&self.directory, registered))
            .collect();

        let title = format!(
            "{} teammate(s), {} live session(s)",
            teammates.len(),
            sessions.len()
        );
        let output = render(&teammates, &sessions);

        Ok(ToolOutput {
            title,
            output,
            metadata: serde_json::json!({
                "teammates": teammates.len(),
                "live_sessions": sessions.len(),
            }),
        })
    }
}

#[cfg(test)]
#[path = "list_sessions_tests.rs"]
mod tests;
