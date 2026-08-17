//! Where a team's files live, and what a member is allowed to be called.
//!
//! **Upstream opencode has no counterpart**: it has no teams, no mailbox and
//! no second agent to address, so there is no TypeScript here to port behavior
//! from. The specification is Claude Code's, read out of the reference
//! document — §1.1 for the name grammar, the reserved recipient and the
//! collision counter, §2.1 for the directory layout and the two sanitized path
//! components (**D497**).
//!
//! Two things are worth stating plainly, because both are load-bearing.
//!
//! The first is that **the root is a value**. [`TeamsRoot`] is handed in by
//! whoever knows where homes are; nothing here reads an environment variable
//! or asks a config where it lives. That is `skill::Roots`'s discipline
//! (`ganja-tool/src/skill.rs`) applied to a second directory, and it is what
//! makes D-1 buy anything: ganja's own teams live under its config home and a
//! real `claude` pane's live under `$CLAUDE_CONFIG_DIR/teams`, and the
//! difference between those two runs is a different value, never different
//! code. It is also what lets a test point this crate at a temporary directory
//! and know it cannot reach the machine it is running on.
//!
//! The second is that **a name is refused, never repaired**. §2.1 passes both
//! the team and the agent component through a sanitizer before joining them
//! into a path, and the agent half derives from a name a *model* chose. A
//! sanitizer that quietly rewrites `../../etc/passwd` into something joinable
//! answers a question nobody asked; [`MemberName::parse`] and
//! [`TeamName::parse`] refuse it instead, and the path builders below take
//! those types rather than `&str` so there is no door left that accepts an
//! unchecked name.

use std::{
    collections::HashSet,
    fmt,
    path::{Path, PathBuf},
};

/// The lead's canonical member name (§1.1's `LEAD`).
pub const LEAD: &str = "team-lead";

/// The reserved recipient (§1.1's `MAIN`): it addresses whoever is asking, so
/// no member may answer to it.
pub const MAIN: &str = "main";

/// The team a session falls back to when none is active (§2.1).
pub const DEFAULT_TEAM: &str = "default";

/// The longest a name may be (§1.1: 1–64 characters).
pub const NAME_MAX: usize = 64;

/// What goes between a colliding name and its counter.
///
/// §1.1 says only that registration "appends an incrementing counter starting
/// at 2" and does not say whether anything separates the two, so `worker2` and
/// `worker-2` were both readings of the same sentence, and this build shipped
/// `worker-2` unverified. **It is verified now**, statically, out of the
/// Claude Code 2.1.233 binary's own registration — the function that uniques a
/// name against the team file before the member record is pushed:
///
/// ```text
/// function tzf(e,t){ let r=$Ia(e);
///   if(r===t9) throw Error('"main" is a reserved recipient name …');
///   let n=new Set(t.members.map((i)=>i.name.toLowerCase()));
///   if(!n.has(r.toLowerCase())) return r;
///   let o=2;
///   while(n.has(`${r}-${o}`.toLowerCase())) o++;
///   return `${r}-${o}` }
/// ```
///
/// A hyphen, a counter opening at 2, and a comparison lowercased on both sides
/// — which is [`resolve_unique`], field for field. It stays one constant, but
/// no longer because it might be wrong.
///
/// The static reading is what settles it because a live one cannot: the
/// question needs a `claude` that *registers* a teammate, and a pane teammate
/// is refused that by its own tool surface (see
/// `ganja-core/tests/teammate_claude_live.rs`).
pub const COLLISION_SEPARATOR: &str = "-";

/// Why a name was refused. Ganja's own sentences — nothing here is copied from
/// Claude Code, which is proprietary (**D497**).
pub const REFUSED_SHAPE: &str = "a name is 1 to 64 characters, starts with a letter or a digit, \
     and then holds only letters, digits, hyphens and underscores";

/// Why `main` may not be a member name.
pub const REFUSED_RESERVED: &str =
    "\"main\" already addresses the conversation asking, so no member may answer to it";

/// Why a colliding name could not be made unique.
pub const REFUSED_NO_FREE_COUNTER: &str =
    "every counter that would make this name unique runs it past 64 characters";

/// A name this crate would not accept.
///
/// Carries the name it refused, which is safe to render: a name is an address,
/// not the content of a message. What a teammate *wrote* never reaches an
/// error or a log line here — see `tests/no_bodies_in_logs.rs`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    /// The name does not match §1.1's grammar.
    #[error("{}: {name:?}", REFUSED_SHAPE)]
    Shape {
        /// What was offered.
        name: String,
    },
    /// The name is §1.1's reserved recipient.
    #[error("{}", REFUSED_RESERVED)]
    Reserved,
    /// The name collides and no counter fits under [`NAME_MAX`].
    #[error("{}: {desired:?}", REFUSED_NO_FREE_COUNTER)]
    NoFreeCounter {
        /// The name that could not be made unique.
        desired: String,
    },
}

/// §1.1's grammar, `^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$`, hand-checked.
///
/// Hand-checked rather than compiled, because a five-condition ASCII grammar
/// is not a reason for this crate to grow a regex engine — and because the
/// conditions read as the sentence [`REFUSED_SHAPE`] already says.
///
/// This is also the whole of the §2.1 path sanitizer. Every character the
/// grammar admits is a character that joins into one path component: there is
/// no `/`, no `\`, no `.` and therefore no `..`, no NUL and no leading dash to
/// be read as a flag. So "cannot escape the teams root" is not a second check
/// somewhere below — it is a property of having passed this one.
fn shaped(name: &str) -> Result<(), NameError> {
    let refuse = || {
        Err(NameError::Shape {
            name: name.to_owned(),
        })
    };
    // Byte length is character length here: every character the grammar admits
    // is ASCII, so a name that is not fails the character test below anyway.
    if name.is_empty() || name.len() > NAME_MAX {
        return refuse();
    }

    let mut characters = name.chars();
    match characters.next() {
        Some(first) if first.is_ascii_alphanumeric() => {}
        _ => return refuse(),
    }
    if characters
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        Ok(())
    } else {
        refuse()
    }
}

/// A team name that has passed §1.1's grammar.
///
/// Distinct from [`MemberName`] for one reason: `main` is reserved as a
/// *recipient*, and a team is not a recipient. Everything else about the two is
/// the same check.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TeamName(String);

impl TeamName {
    /// Accepts a team name, or refuses it.
    ///
    /// # Errors
    ///
    /// [`NameError::Shape`] when the name is not §1.1's grammar.
    pub fn parse(name: &str) -> Result<Self, NameError> {
        shaped(name)?;

        Ok(Self(name.to_owned()))
    }

    /// The team §2.1 falls back to when no team is active.
    #[must_use]
    pub fn default_team() -> Self {
        Self(DEFAULT_TEAM.to_owned())
    }

    /// The name as it is spelled on disk.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Gives up the check and returns the name.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Display for TeamName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A member name that has passed §1.1's grammar and is not the reserved
/// recipient.
///
/// A *created* member's name goes through here. A member name *read back* out
/// of a team file does not — [`crate::MemberRecord::name`] is a `String`,
/// because refusing to decode a document a real `claude` wrote is not this
/// crate's call to make. The type marks the door, not the storage.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MemberName(String);

impl MemberName {
    /// Accepts a member name, or refuses it.
    ///
    /// The reserved comparison is **exact**, matching §1.1's constant
    /// comparison rather than being case-insensitive about it. Being stricter
    /// than the peer sharing the directory would refuse a name a real `claude`
    /// registers happily, and then the two builds would disagree about who is
    /// in the team.
    ///
    /// # Errors
    ///
    /// [`NameError::Shape`] when the name is not §1.1's grammar, and
    /// [`NameError::Reserved`] when it is [`MAIN`].
    pub fn parse(name: &str) -> Result<Self, NameError> {
        shaped(name)?;
        if name == MAIN {
            return Err(NameError::Reserved);
        }

        Ok(Self(name.to_owned()))
    }

    /// The team's lead, whose name is a constant rather than a choice.
    #[must_use]
    pub fn lead() -> Self {
        Self(LEAD.to_owned())
    }

    /// The name as it is spelled on disk, and as it is addressed.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Gives up the check and returns the name.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }

    /// §2.2's derived identity, `<name>@<team>`.
    #[must_use]
    pub fn agent_id(&self, team: &TeamName) -> String {
        format!("{}@{}", self.0, team.0)
    }
}

impl fmt::Display for MemberName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// §1.1's registration step: the name somebody asked for, made unique.
///
/// Existing names are lowercased into a set and the comparison runs against
/// that, so `Worker` collides with `worker` — the reference's own rule, and the
/// one that matters, since a case-insensitive filesystem would otherwise give
/// two members one inbox file.
///
/// # Errors
///
/// Whatever [`MemberName::parse`] refuses, and [`NameError::NoFreeCounter`]
/// when every counter would run the name past [`NAME_MAX`].
pub fn resolve_unique<'a>(
    desired: &str,
    taken: impl IntoIterator<Item = &'a str>,
) -> Result<MemberName, NameError> {
    let desired = MemberName::parse(desired)?;
    let taken: HashSet<String> = taken.into_iter().map(str::to_lowercase).collect();
    if !taken.contains(&desired.0.to_lowercase()) {
        return Ok(desired);
    }

    // Bounded rather than open-ended, and provably enough: `taken` holds at
    // most `n` names, so `n + 1` candidates cannot all be taken. Falling out of
    // the loop therefore means every candidate was too long, never that the
    // search gave up.
    let ceiling = u32::try_from(taken.len())
        .unwrap_or(u32::MAX)
        .saturating_add(2);
    for counter in 2..=ceiling {
        let candidate = format!("{}{COLLISION_SEPARATOR}{counter}", desired.0);
        if candidate.len() > NAME_MAX {
            break;
        }
        if !taken.contains(&candidate.to_lowercase()) {
            return Ok(MemberName(candidate));
        }
    }

    Err(NameError::NoFreeCounter {
        desired: desired.into_inner(),
    })
}

/// The directory a build keeps its teams in — a value somebody handed over,
/// never a directory this crate went looking for.
///
/// Ganja's own is `<config home>/teams`; a real `claude` pane's is
/// `$CLAUDE_CONFIG_DIR/teams`. Both are this type holding a different path,
/// which is the entire mechanism behind D-1: one implementation of Claude's
/// format, pointed at whichever directory the run is sharing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamsRoot(PathBuf);

impl TeamsRoot {
    /// The teams directory, as worked out by whoever knows where homes are.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self(dir.into())
    }

    /// The directory itself.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.0
    }

    /// `<root>/<team>` (§2.1).
    #[must_use]
    pub fn team_dir(&self, team: &TeamName) -> PathBuf {
        self.0.join(&team.0)
    }

    /// `<root>/<team>/config.json` (§2.2).
    #[must_use]
    pub fn config_path(&self, team: &TeamName) -> PathBuf {
        self.team_dir(team).join("config.json")
    }

    /// `<root>/<team>/inboxes` (§2.1).
    #[must_use]
    pub fn inbox_dir(&self, team: &TeamName) -> PathBuf {
        self.team_dir(team).join("inboxes")
    }

    /// `<root>/<team>/inboxes/<agent>.json` — §2.1's `getInboxPath`, with the
    /// sanitizer already spent on the two arguments' types.
    #[must_use]
    pub fn inbox_path(&self, team: &TeamName, agent: &MemberName) -> PathBuf {
        self.inbox_dir(team).join(format!("{}.json", agent.0))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{MemberName, NameError, TeamName, TeamsRoot, resolve_unique};

    #[test]
    fn main_is_refused_as_a_member_name() {
        assert_eq!(MemberName::parse("main"), Err(NameError::Reserved));
        // A team is not a recipient, so the reservation does not reach it.
        assert!(TeamName::parse("main").is_ok());
        // And the reservation is exact: refusing a spelling a real `claude`
        // accepts would put the two builds' member lists out of step.
        assert!(MemberName::parse("Main").is_ok());
    }

    #[test]
    fn a_colliding_name_gets_a_counter_suffix() {
        assert_eq!(
            resolve_unique("worker", ["team-lead"]).expect("no collision"),
            MemberName::parse("worker").expect("a valid name")
        );
        assert_eq!(
            resolve_unique("worker", ["worker"])
                .expect("one counter is enough")
                .as_str(),
            "worker-2"
        );
        assert_eq!(
            resolve_unique("worker", ["worker", "worker-2", "worker-3"])
                .expect("three counters are enough")
                .as_str(),
            "worker-4"
        );
        // §1.1 lowercases what is taken before comparing, so a differently
        // cased sibling still collides — which is what keeps two members off
        // one inbox file on a case-insensitive filesystem.
        assert_eq!(
            resolve_unique("Worker", ["worker"])
                .expect("the collision is case-insensitive")
                .as_str(),
            "Worker-2"
        );
        // A name with no room left for a counter is refused rather than
        // truncated into a different member's address.
        let longest = "w".repeat(super::NAME_MAX);
        assert_eq!(
            resolve_unique(&longest, [longest.as_str()]),
            Err(NameError::NoFreeCounter { desired: longest })
        );
    }

    #[test]
    fn a_model_supplied_name_cannot_escape_the_teams_root() {
        let root = TeamsRoot::new("/tmp/teams");
        let team = TeamName::parse("session-224cbeab").expect("a valid team name");

        for hostile in [
            "..",
            "../../etc/passwd",
            "worker/../../..",
            "/etc/passwd",
            "worker/sub",
            "worker\\sub",
            ".hidden",
            "-flag",
            "worker\0",
            "worker\n",
            "wörker",
            "",
            &"w".repeat(super::NAME_MAX + 1),
        ] {
            assert!(
                matches!(MemberName::parse(hostile), Err(NameError::Shape { .. })),
                "{hostile:?} should not be a member name"
            );
            assert!(
                matches!(TeamName::parse(hostile), Err(NameError::Shape { .. })),
                "{hostile:?} should not be a team name"
            );
        }

        // And what does pass stays one component under the root, which is the
        // property the refusals above are protecting.
        let agent = MemberName::parse("demo-worker-1").expect("a valid member name");
        let inbox = root.inbox_path(&team, &agent);
        assert_eq!(
            inbox,
            Path::new("/tmp/teams/session-224cbeab/inboxes/demo-worker-1.json")
        );
        assert!(inbox.starts_with(root.dir()));
        assert!(
            !inbox
                .components()
                .any(|component| { matches!(component, std::path::Component::ParentDir) }),
            "a built path never walks upward: {inbox:?}"
        );
    }

    #[test]
    fn an_agent_id_is_the_name_and_the_team() {
        let team = TeamName::parse("session-224cbeab").expect("a valid team name");
        assert_eq!(
            MemberName::lead().agent_id(&team),
            "team-lead@session-224cbeab"
        );
    }
}
