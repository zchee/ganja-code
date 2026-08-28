use std::io::Write as _;

use ganja_core::teammate::preamble::{self, Names};
use ganja_protocol::team::MemberBackend;
use ganja_team::ShimCli;

use super::{
    Cursor, Road, SystemTime, answers_clause, appended, beyond, holds, matching, of, whole,
};

/// The fingerprint a reader searches for is the sentence the preamble
/// opens with — one spelling, so a reworded preamble cannot leave every
/// reader hunting for a sentence nobody sends (**D515**).
#[test]
fn the_fingerprint_is_the_preambles_own_opening_sentence() {
    let who = Names { name: "w1", team: "session-abcd1234", lead: "team-lead" };
    let mark = preamble::opening(who);

    assert!(
        preamble::frame(who, "however you answer", "hold the fort").starts_with(&mark),
        "{mark}"
    );
    assert!(mark.contains("w1"), "{mark}");
    assert!(mark.contains("session-abcd1234"), "{mark}");
}

/// A tail reader only ever moves past whole lines: a transcript is being
/// written while it is read, and half a JSON object is not a record.
#[test]
fn a_tail_read_leaves_a_half_written_line_where_it_is() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("updates.jsonl");
    std::fs::write(&path, "one\ntwo\nthr").expect("the file is written");

    let mut cursor = Cursor::default();
    assert_eq!(appended(&path, &mut cursor), ["one", "two"]);
    assert_eq!(cursor.bytes, 8, "only the two whole lines are behind us");

    // The writer finishes that line and adds another.
    let mut file = std::fs::OpenOptions::new().append(true).open(&path).expect("the file opens");
    write!(file, "ee\nfour\n").expect("the tail is written");

    assert_eq!(appended(&path, &mut cursor), ["three", "four"]);
    assert_eq!(appended(&path, &mut cursor), Vec::<String>::new());
}

/// The re-read shape carries each answer once, and a transcript that
/// shrank repeats nothing.
#[test]
fn a_reread_carries_each_answer_once_and_never_repeats_after_a_compaction() {
    let mut cursor = Cursor::default();

    assert_eq!(beyond(vec!["a".to_owned(), "b".to_owned()], &mut cursor), ["a", "b"]);
    assert_eq!(cursor.answers, 2);
    assert_eq!(beyond(vec!["a".to_owned(), "b".to_owned()], &mut cursor), Vec::<String>::new());
    assert_eq!(beyond(vec!["a".to_owned(), "b".to_owned(), "c".to_owned()], &mut cursor), ["c"]);
    // A CLI that compacted its own record: fewer answers than carried.
    assert_eq!(beyond(vec!["z".to_owned()], &mut cursor), Vec::<String>::new());
    assert_eq!(cursor.answers, 3, "the count only moves forward");
}

/// A session is found by the mark **its own user record** holds, never by
/// being the newest file — and never by a mark that merely appears
/// somewhere in it, which is how one member would come to mail another
/// session's answers to the lead under its own name.
#[test]
fn a_session_is_found_by_the_message_its_user_sent_and_not_by_being_newest() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let reader = of(ShimCli::Codex);
    let mark = "You are w1, a teammate on the team t.";
    let said = |role: &str, text: &str| {
        format!(
            "{}\n",
            serde_json::json!({
                "type": "response_item",
                "payload": {"type": "message", "role": role,
                            "content": [{"type": "text", "text": text}]},
            })
        )
    };

    let mine = directory.path().join("mine.jsonl");
    let theirs = directory.path().join("theirs.jsonl");
    let reader_of_mailboxes = directory.path().join("nosy.jsonl");
    std::fs::write(&mine, said("user", mark)).expect("written");
    std::fs::write(&theirs, said("user", "You are w2, a teammate on the team t."))
        .expect("written");
    // A session that only ever **read** this member's mailbox: the same
    // sentence, in a record its own CLI attributes to a tool's output.
    std::fs::write(
        &reader_of_mailboxes,
        format!(
            "{}\n",
            serde_json::json!({
                "type": "response_item",
                "payload": {"type": "function_call_output",
                            "output": format!("{{\"text\": \"{mark}\"}}")},
            })
        ),
    )
    .expect("written");

    let candidates = vec![mine.clone(), theirs.clone(), reader_of_mailboxes.clone()];
    assert_eq!(
        matching(candidates.clone(), mark, reader, SystemTime::UNIX_EPOCH).as_deref(),
        Some(mine.as_path()),
        "the session this member's own message opened"
    );
    assert!(
        !holds(&reader_of_mailboxes, mark, reader),
        "a session that merely read the sentence is not this member's"
    );
    assert_eq!(
        matching(
            candidates.clone(),
            "You are w9, a teammate on the team t.",
            reader,
            SystemTime::UNIX_EPOCH
        ),
        None,
        "a member with no session yet is answered with none, not with a guess"
    );
    // And a conversation older than the member cannot be its own.
    assert_eq!(
        matching(candidates, mark, reader, SystemTime::now() + std::time::Duration::from_secs(60)),
        None,
        "nothing written since the spawn is nothing this member said"
    );
}

/// **A line this cannot read is not the end of the reading.** A stray
/// non-UTF-8 byte ahead of the pasted message — in some tool's output the
/// CLI recorded — would otherwise end the scan there and leave a member's
/// own transcript unmatchable for good, with nothing said about it but one
/// ring line a minute and a half later.
#[test]
fn a_line_that_is_not_utf8_is_skipped_rather_than_ending_the_search() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("rollout.jsonl");
    let mark = "You are w1, a teammate on the team t.";
    let said = serde_json::json!({
        "type": "response_item",
        "payload": {"type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": mark}]},
    })
    .to_string();

    // A record this build reads, then one byte it cannot, then the
    // member's own message.
    let mut bytes = b"{\"type\":\"event_msg\"}\n".to_vec();
    bytes.extend_from_slice(b"{\"tool_output\":\"\xff\"}\n");
    bytes.extend_from_slice(said.as_bytes());
    bytes.push(b'\n');
    std::fs::write(&path, bytes).expect("the transcript is written");

    assert!(
        holds(&path, mark, of(ShimCli::Codex)),
        "the message after the unreadable line is still this member's"
    );
}

/// Whole-file reads answer an absent file with nothing rather than an
/// error: before a CLI's first record there is no transcript, and that is
/// the ordinary case on a two-second poll.
#[test]
fn an_absent_transcript_reads_as_nothing() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let missing = directory.path().join("nothing.jsonl");

    assert_eq!(whole(&missing), Vec::<String>::new());
    assert_eq!(appended(&missing, &mut Cursor::default()), Vec::<String>::new());
    assert!(!holds(&missing, "anything", of(ShimCli::Codex)));
}

/// Every CLI answers both questions — which is what makes a seventh
/// backend a build failure rather than a silent teammate.
#[test]
fn every_shim_cli_has_a_reader_and_a_clause() {
    for (cli, backend) in [
        (ShimCli::Codex, MemberBackend::Codex),
        (ShimCli::Grok, MemberBackend::Grok),
        (ShimCli::Agy, MemberBackend::Agy),
    ] {
        let _: &dyn super::Transcript = of(cli);
        for road in [Road::Headless, Road::Pane] {
            assert!(
                answers_clause(backend, road).is_some(),
                "{backend:?} says nothing about its answers on {road:?}"
            );
        }
    }
    for backend in [MemberBackend::InProcess, MemberBackend::Ganja, MemberBackend::Claude] {
        assert_eq!(answers_clause(backend, Road::Pane), None);
        assert_eq!(answers_clause(backend, Road::Headless), None);
    }
    // The one CLI whose two doors differ, said rather than implied.
    assert_ne!(
        answers_clause(MemberBackend::Agy, Road::Headless),
        answers_clause(MemberBackend::Agy, Road::Pane),
        "agy's headless child answers once a turn; its pane records each answer"
    );
}
