use tempfile::TempDir;

use super::{Direction, History, MAX_HISTORY_ENTRIES, PromptInfo, parse};

fn temporary() -> TempDir {
    TempDir::new().expect("a temporary directory is creatable")
}

fn history_in(directory: &TempDir) -> History {
    History::load_from(directory.path().join("prompt-history.jsonl"))
}

#[test]
fn a_submission_reads_back_on_the_next_load() {
    let directory = temporary();
    let mut history = history_in(&directory);
    history.append(PromptInfo::text("what does this crate do"));

    let mut reloaded = history_in(&directory);
    assert_eq!(
        reloaded.step(Direction::Older, ""),
        Some(PromptInfo::text("what does this crate do"))
    );
}

#[test]
fn fifty_one_submissions_keep_exactly_fifty() {
    let directory = temporary();
    let path = directory.path().join("prompt-history.jsonl");
    let mut history = History::load_from(path.clone());

    for n in 0..=MAX_HISTORY_ENTRIES {
        history.append(PromptInfo::text(format!("prompt {n}")));
    }

    let on_disk = std::fs::read_to_string(&path).expect("the file reads");
    assert_eq!(
        on_disk.lines().filter(|line| !line.is_empty()).count(),
        MAX_HISTORY_ENTRIES,
        "the cap holds on disk"
    );
    // The oldest fell off the front; the newest is still there.
    assert!(!on_disk.contains("\"prompt 0\""), "the oldest was dropped");
    assert!(
        on_disk.contains(&format!("\"prompt {MAX_HISTORY_ENTRIES}\"")),
        "the newest was kept"
    );
}

#[test]
fn a_corrupt_line_is_dropped_and_the_file_self_heals() {
    let directory = temporary();
    let path = directory.path().join("prompt-history.jsonl");
    std::fs::write(
        &path,
        "{\"input\":\"good one\"}\nnot json at all\n{\"input\":\"good two\"}\n",
    )
    .expect("the fixture writes");

    let mut history = History::load_from(path.clone());

    // The load kept the two that parsed and rewrote the file without the
    // corrupt line.
    let healed = std::fs::read_to_string(&path).expect("the file reads");
    assert!(!healed.contains("not json at all"), "got:\n{healed}");
    assert_eq!(
        healed.lines().filter(|line| !line.is_empty()).count(),
        2,
        "only the two good lines remain:\n{healed}"
    );
    assert_eq!(
        history.step(Direction::Older, ""),
        Some(PromptInfo::text("good two")),
        "the newest good line is reachable"
    );
}

#[test]
fn two_identical_submissions_store_once() {
    let directory = temporary();
    let path = directory.path().join("prompt-history.jsonl");
    let mut history = History::load_from(path.clone());

    history.append(PromptInfo::text("say it again"));
    history.append(PromptInfo::text("say it again"));

    let on_disk = std::fs::read_to_string(&path).expect("the file reads");
    assert_eq!(
        on_disk.lines().filter(|line| !line.is_empty()).count(),
        1,
        "the consecutive duplicate was suppressed:\n{on_disk}"
    );
}

#[test]
fn a_non_consecutive_repeat_is_stored_again() {
    let directory = temporary();
    let mut history = history_in(&directory);

    history.append(PromptInfo::text("first"));
    history.append(PromptInfo::text("second"));
    history.append(PromptInfo::text("first"));

    // Only a run of the *same* prompt is collapsed; the same words after
    // something else is a real second visit.
    assert_eq!(history.entries.len(), 3);
}

#[test]
fn up_on_a_dirty_buffer_moves_nothing() {
    let directory = temporary();
    let mut history = history_in(&directory);
    history.append(PromptInfo::text("the stored one"));

    // The buffer holds something the user typed that is not the entry a
    // walk would show, so the walk is refused and the buffer is left alone.
    assert_eq!(history.step(Direction::Older, "a half-typed thought"), None);
}

#[test]
fn up_restores_the_last_prompt_and_down_returns_to_empty() {
    let directory = temporary();
    let mut history = history_in(&directory);
    history.append(PromptInfo::text("older"));
    history.append(PromptInfo::text("newer"));

    assert_eq!(
        history.step(Direction::Older, ""),
        Some(PromptInfo::text("newer")),
        "the first Up is the newest entry"
    );
    assert_eq!(
        history.step(Direction::Older, "newer"),
        Some(PromptInfo::text("older")),
        "the second Up is the one before it"
    );
    assert_eq!(
        history.step(Direction::Older, "older"),
        None,
        "there is nothing older to reach"
    );
    assert_eq!(
        history.step(Direction::Newer, "older"),
        Some(PromptInfo::text("newer")),
        "Down comes back toward the buffer"
    );
    assert_eq!(
        history.step(Direction::Newer, "newer"),
        Some(PromptInfo::text("")),
        "and lands on the empty live buffer"
    );
}

/// The shell mode and the empty parts vector are on the wire the way
/// upstream's `PromptInfo` is, and a file this build writes reads back
/// unchanged.
#[test]
fn the_mode_survives_the_round_trip_and_the_empty_parts_stay_off_the_file() {
    let directory = temporary();
    let path = directory.path().join("prompt-history.jsonl");
    let mut history = History::load_from(path.clone());
    history.append(PromptInfo {
        input: "ls -la".to_owned(),
        mode: Some(super::Mode::Shell),
        parts: Vec::new(),
    });

    let on_disk = std::fs::read_to_string(&path).expect("the file reads");
    assert!(on_disk.contains("\"mode\":\"shell\""), "got:\n{on_disk}");
    assert!(
        !on_disk.contains("\"parts\""),
        "an empty parts vector is not written:\n{on_disk}"
    );

    let recalled = parse(&on_disk);
    assert_eq!(
        recalled.first().and_then(|e| e.mode),
        Some(super::Mode::Shell)
    );
}

/// The search modal reads the whole store newest first — the opposite
/// order [`History`] keeps internally for the walk, so `entries` is
/// where that reversal happens rather than in every caller.
#[test]
fn entries_reads_back_newest_first() {
    let directory = temporary();
    let mut history = history_in(&directory);
    history.append(PromptInfo::text("first"));
    history.append(PromptInfo::text("second"));
    history.append(PromptInfo::text("third"));

    let inputs: Vec<String> = history
        .entries()
        .into_iter()
        .map(|recalled| recalled.prompt.input)
        .collect();
    assert_eq!(inputs, ["third", "second", "first"]);
}

/// Every line a load finds on disk is dated to the same instant — the
/// file's own last-write time, since the JSONL format never recorded a
/// per-line one (**D448**).
#[test]
fn entries_loaded_from_disk_share_one_dated_instant() {
    let directory = temporary();
    let path = directory.path().join("prompt-history.jsonl");
    std::fs::write(&path, "{\"input\":\"older\"}\n{\"input\":\"newer\"}\n")
        .expect("the fixture writes");

    let history = History::load_from(path);
    let ats: Vec<u64> = history
        .entries()
        .into_iter()
        .map(|recalled| recalled.at)
        .collect();

    assert_eq!(ats.len(), 2);
    assert_eq!(ats[0], ats[1], "a load has one instant, not a spread");
    assert!(ats[0] > 0, "the file's last-write time reads as nonzero");
}

/// An entry appended this run is dated no earlier than what was already
/// on disk when the run started, unlike a loaded entry which is dated to
/// the file's last write rather than to when it was actually typed.
#[test]
fn an_appended_entry_is_never_dated_before_what_was_loaded() {
    let directory = temporary();
    let path = directory.path().join("prompt-history.jsonl");
    std::fs::write(&path, "{\"input\":\"loaded\"}\n").expect("the fixture writes");

    let mut history = History::load_from(path);
    let loaded_at = history.entries()[0].at;

    history.append(PromptInfo::text("appended"));
    let entries = history.entries();

    assert_eq!(entries[0].prompt.input, "appended", "newest first");
    assert!(entries[0].at >= loaded_at);
}

/// An empty store has nothing to date and nothing to crash over.
#[test]
fn an_empty_store_has_no_entries() {
    let directory = temporary();
    let history = history_in(&directory);

    assert!(history.entries().is_empty());
}
