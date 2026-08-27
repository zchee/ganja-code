use std::sync::Arc;

use async_trait::async_trait;

use super::{
    DIRECTORY_UNREADABLE, LEAD_MARK, LIVE_SESSIONS_HEADER, ListSessionsTool, LiveSession,
    MOST_SHOWN_POINTS, NO_LIVE_SESSIONS, NO_TEAMMATE_DESCRIPTION, TEAMMATES_HEADER,
    UNVERIFIED_LABEL, neutralize, render,
};
use crate::{
    Tool as _, ToolCtx, ToolError, registry, socket,
    team::{Address, Body, Peer, Postbox, Reserved, Sent, Undelivered},
};

/// A postbox whose only job is to hand back a fixed roster — everything
/// `list_sessions` ever calls on a [`Postbox`].
#[derive(Debug)]
struct Fake {
    roster: Vec<Peer>,
}

impl Fake {
    fn new() -> Self {
        Self {
            roster: vec![
                Peer {
                    name: "team-lead".to_owned(),
                    description: Some("runs the team".to_owned()),
                    lead: true,
                },
                Peer {
                    name: "worker-1".to_owned(),
                    description: None,
                    lead: false,
                },
            ],
        }
    }
}

#[async_trait]
impl Postbox for Fake {
    fn classify(&self, _text: &str) -> Reserved {
        Reserved::No
    }

    async fn deliver(&self, _to: Address, _body: Body) -> Result<Sent, Undelivered> {
        unreachable!("list_sessions never calls Postbox::deliver")
    }

    fn roster(&self) -> Vec<Peer> {
        self.roster.clone()
    }
}

/// A record for `stem`, in `registry_tests.rs`'s own shape — this module
/// cannot import that one's private helper, so it is spelled again.
fn record(name: &str, session_id: &str, cwd: &str) -> registry::Record {
    registry::Record {
        format: registry::FORMAT,
        session_id: session_id.to_owned(),
        name: name.to_owned(),
        name_source: registry::NameSource::User,
        cwd: cwd.into(),
        root: cwd.into(),
        pid: 4242,
        started_at: 1_756_150_000_000,
    }
}

/// Writes `record` at `stem` and holds its lock, as a binder would — the
/// fixture every end-to-end test here builds a live session out of.
fn write_live(dir: &std::path::Path, stem: &str, record: &registry::Record) -> std::fs::File {
    registry::write(dir, stem, record).expect("a record writes");
    let lock = socket::open_lock(&dir.join(format!("{stem}.{}", socket::EXTENSION)))
        .expect("the lock file opens");
    lock.try_lock().expect("nothing else holds a fresh lock");

    lock
}

fn ctx(dir: &std::path::Path, postbox: Option<Arc<dyn Postbox>>) -> ToolCtx {
    let mut ctx = ToolCtx::fixture(dir.to_owned());
    ctx.postbox = postbox;

    ctx
}

/// AC-34's teammate half: the roster renders with the lead marked, and a
/// member with nothing said about it falls back to the stock line.
#[test]
fn render_lists_teammates_with_the_lead_marked() {
    let teammates = vec![
        Peer {
            name: "team-lead".to_owned(),
            description: Some("runs the team".to_owned()),
            lead: true,
        },
        Peer {
            name: "worker-1".to_owned(),
            description: None,
            lead: false,
        },
    ];

    let output = render(&teammates, &[]);

    assert!(output.contains(TEAMMATES_HEADER), "{output}");
    assert!(
        output.contains(&format!("team-lead: runs the team ({LEAD_MARK})")),
        "{output}"
    );
    assert!(
        output.contains(&format!("worker-1: {NO_TEAMMATE_DESCRIPTION}")),
        "{output}"
    );
}

/// AC-35's negative half: the honesty label a live session carries never
/// appears on a teammate's own row — that name is lead-assigned, not
/// self-asserted.
#[test]
fn render_never_puts_the_unverified_label_on_a_teammate_row() {
    let teammates = vec![Peer {
        name: "worker-1".to_owned(),
        description: None,
        lead: false,
    }];

    let output = render(&teammates, &[]);

    assert!(
        !output.contains(UNVERIFIED_LABEL),
        "a teammate row is never labeled unverified: {output}"
    );
}

/// A roster with nobody on it — this session leads no team, or leads a team
/// of nobody else, and `Postbox::roster` cannot itself tell the two apart —
/// omits the section rather than heading an empty one.
#[test]
fn render_omits_the_teammates_section_when_the_roster_is_empty() {
    let output = render(&[], &[]);

    assert!(!output.contains(TEAMMATES_HEADER), "{output}");
    assert!(output.contains(LIVE_SESSIONS_HEADER), "{output}");
    assert!(output.contains(NO_LIVE_SESSIONS), "{output}");
}

/// AC-34's live-session half: every field lands, and AC-35's positive half —
/// the unverified label is on the row.
#[test]
fn render_live_session_rows_carry_every_field_and_the_unverified_label() {
    let sessions = vec![LiveSession {
        name: "backend".to_owned(),
        stem: "0198c1a2".to_owned(),
        cwd: "/work/backend".to_owned(),
        address: "uds:/tmp/ganja-501/0198c1a2.sock".to_owned(),
    }];

    let output = render(&[], &sessions);

    assert!(output.contains("backend"), "{output}");
    assert!(output.contains(UNVERIFIED_LABEL), "{output}");
    assert!(output.contains("0198c1a2"), "{output}");
    assert!(output.contains("/work/backend"), "{output}");
    assert!(
        output.contains("uds:/tmp/ganja-501/0198c1a2.sock"),
        "{output}"
    );
}

/// AC-35's neutralization half, restated at this second model-facing surface:
/// a name or a cwd carrying control characters or angle brackets renders
/// scrubbed rather than verbatim.
#[test]
fn render_neutralizes_a_live_sessions_self_written_name_and_cwd() {
    let sessions = vec![LiveSession {
        name: "evil\u{7}<script>".to_owned(),
        stem: "0198c1a2".to_owned(),
        cwd: "/work/<injected>".to_owned(),
        address: "uds:/tmp/ganja-501/0198c1a2.sock".to_owned(),
    }];

    let output = render(&[], &sessions);

    assert!(
        !output.contains("evil\u{7}<script>"),
        "the raw bytes never reach the model: {output}"
    );
    assert!(
        !output.contains("/work/<injected>"),
        "the raw cwd never reaches the model: {output}"
    );
    assert!(output.contains("evilscript"), "{output}");
    assert!(output.contains("/work/injected"), "{output}");
}

/// `neutralize`'s own unit coverage: control characters and both brackets
/// are dropped, and an over-long value is cut with the cut admitted.
#[test]
fn neutralize_drops_control_characters_and_brackets_and_caps_length() {
    assert_eq!(neutralize("plain"), "plain");
    assert_eq!(neutralize("has\u{7}control"), "hascontrol");
    assert_eq!(neutralize("<script>"), "script");

    let over_long = "x".repeat(MOST_SHOWN_POINTS + 5);
    let cut = neutralize(&over_long);
    assert_eq!(cut.chars().count(), MOST_SHOWN_POINTS + 1, "{cut}");
    assert!(cut.ends_with('…'), "the cut is admitted: {cut}");
}

/// AC-34's exclusion and staleness rules, and the disambiguation AC-34 makes
/// of duplicate names, all through the tool's own `run` — not just `render`.
#[tokio::test]
async fn the_tool_excludes_itself_and_staleness_and_disambiguates_duplicates_by_stem() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let own_session = "0198c1a2-0000-7000-8000-000000000001";

    // This session's own record: live, and must never appear anyway.
    let _own_lock = write_live(
        dir.path(),
        "0198c1a2",
        &record("self", own_session, "/work/self"),
    );

    // Two other live sessions sharing one name — told apart by stem alone.
    let _a_lock = write_live(
        dir.path(),
        "0299d2b3",
        &record("backend", "0299d2b3-0000-7000-8000-000000000002", "/work/a"),
    );
    let _b_lock = write_live(
        dir.path(),
        "0399e3c4",
        &record("backend", "0399e3c4-0000-7000-8000-000000000003", "/work/b"),
    );

    // A registered but stale session: nobody holds its lock.
    registry::write(
        dir.path(),
        "0499f4d5",
        &record(
            "ghost",
            "0499f4d5-0000-7000-8000-000000000004",
            "/work/ghost",
        ),
    )
    .expect("a record writes");

    let tool = ListSessionsTool::new(dir.path().to_owned(), own_session.to_owned());
    let ctx = ctx(dir.path(), Some(Arc::new(Fake::new()) as Arc<dyn Postbox>));

    let result = tool
        .run(serde_json::json!({}), &ctx)
        .await
        .expect("the tool answers");

    assert!(
        !result.output.contains("/work/self"),
        "the caller's own session never appears: {}",
        result.output
    );
    assert!(
        !result.output.contains("ghost"),
        "a stale record never appears: {}",
        result.output
    );
    assert!(result.output.contains("0299d2b3"), "{}", result.output);
    assert!(result.output.contains("0399e3c4"), "{}", result.output);
    assert_eq!(
        result.output.matches("backend").count(),
        2,
        "both rows for the shared name print: {}",
        result.output
    );
    assert!(
        result.output.contains(TEAMMATES_HEADER),
        "{}",
        result.output
    );
    assert_eq!(result.metadata["live_sessions"], 2);
    assert_eq!(result.metadata["teammates"], 2);
}

/// The tool with no postbox at all (a fixture, or a surface running tools
/// outside a turn) still answers the live-session half.
#[tokio::test]
async fn the_tool_answers_with_no_postbox_at_all() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let tool = ListSessionsTool::new(dir.path().to_owned(), "self".to_owned());
    let ctx = ctx(dir.path(), None);

    let result = tool
        .run(serde_json::json!({}), &ctx)
        .await
        .expect("the tool answers");

    assert!(
        !result.output.contains(TEAMMATES_HEADER),
        "{}",
        result.output
    );
    assert!(
        result.output.contains(NO_LIVE_SESSIONS),
        "{}",
        result.output
    );
}

/// AC-37: an unreadable socket directory is a typed refusal, never an empty
/// list presented as "no sessions" — the refuse-don't-guess rule
/// `registry::list` already enforces, re-asserted at this caller.
#[tokio::test]
async fn an_unreadable_socket_directory_is_a_typed_refusal_not_an_empty_list() {
    let missing = std::path::Path::new("/nonexistent-ganja-registry-for-list-sessions");
    let tool = ListSessionsTool::new(missing.to_owned(), "self".to_owned());
    let ctx = ctx(std::path::Path::new("/tmp"), None);

    match tool.run(serde_json::json!({}), &ctx).await {
        Err(ToolError::Failed(message)) => {
            assert!(message.starts_with(DIRECTORY_UNREADABLE), "{message}");
        }
        other => panic!("expected a typed refusal, got {other:?}"),
    }
}

/// AC-36's description pin: the same-uid, self-asserted posture is stated in
/// the model-facing text itself, not only argued in the module doc.
#[test]
fn the_description_states_the_same_uid_self_asserted_posture() {
    let tool = ListSessionsTool::new(std::env::temp_dir(), "self".to_owned());
    let described = tool.description();

    assert!(described.contains("same-uid"), "{described}");
    assert!(
        described.contains("nothing here verifies it"),
        "{described}"
    );
}
