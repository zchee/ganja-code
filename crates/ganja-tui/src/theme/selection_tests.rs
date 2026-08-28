use std::fs;

use tempfile::TempDir;

use super::{VERSION, read, write};

fn temporary() -> TempDir {
    TempDir::new().expect("a temporary directory is creatable")
}

#[test]
fn a_written_pick_reads_back() {
    let directory = temporary();
    let path = directory.path().join("tui.json");

    write(&path, "gruvbox").expect("the pick writes");

    assert_eq!(read(&path).as_deref(), Some("gruvbox"));
}

/// The dialog is used more than once per session, so the second write has
/// to land on top of the first rather than trip over its own temporary.
#[test]
fn writing_twice_keeps_the_second_pick() {
    let directory = temporary();
    let path = directory.path().join("tui.json");

    write(&path, "aura").expect("the first pick writes");
    write(&path, "tokyonight").expect("the second pick writes");

    assert_eq!(read(&path).as_deref(), Some("tokyonight"));
    assert_eq!(
        fs::read_dir(directory.path()).expect("the directory lists").count(),
        1,
        "no temporary file should be left behind"
    );
}

#[test]
fn the_stored_shape_is_the_one_that_was_specified() {
    let directory = temporary();
    let path = directory.path().join("tui.json");

    write(&path, "opencode").expect("the pick writes");

    assert_eq!(
        fs::read_to_string(&path).expect("the file reads"),
        "{\"version\":1,\"theme\":\"opencode\"}\n"
    );
}

/// The directory is the app's own and may not exist on a first run.
#[test]
fn a_missing_directory_is_created_on_the_way() {
    let directory = temporary();
    let path = directory.path().join("nested").join("deeper").join("tui.json");

    write(&path, "aura").expect("the pick writes");

    assert_eq!(read(&path).as_deref(), Some("aura"));
}

#[test]
fn nothing_stored_reads_as_no_pick() {
    let directory = temporary();

    assert_eq!(read(&directory.path().join("tui.json")), None);
}

#[test]
fn a_file_that_is_not_a_pick_reads_as_no_pick() {
    let directory = temporary();
    let path = directory.path().join("tui.json");
    fs::write(&path, "half a file").expect("the fixture writes");

    assert_eq!(read(&path), None);
}

/// Same rule the session store follows: a newer version is left alone, not
/// guessed at and not overwritten on read.
#[test]
fn a_pick_from_a_newer_build_is_left_alone() {
    let directory = temporary();
    let path = directory.path().join("tui.json");
    let body = format!("{{\"version\":{},\"theme\":\"gruvbox\"}}", VERSION + 1);
    fs::write(&path, &body).expect("the fixture writes");

    assert_eq!(read(&path), None);
    assert_eq!(
        fs::read_to_string(&path).expect("the file reads"),
        body,
        "reading must not rewrite it"
    );
}
