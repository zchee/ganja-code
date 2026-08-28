use super::{DEFAULT, rows};

#[test]
fn the_default_row_comes_first_and_is_active_when_nothing_is_selected() {
    let rows = rows(["max", "mini"], None);

    assert_eq!(
        rows.iter().map(|row| row.value.as_str()).collect::<Vec<_>>(),
        [DEFAULT, "max", "mini"]
    );
    assert_eq!(rows[0].label, "Default");
    assert!(rows[0].active, "no selection means Default is where we are");
    assert!(rows[1..].iter().all(|row| !row.active));
}

#[test]
fn the_selected_effort_carries_the_active_mark_instead() {
    let rows = rows(["max", "mini"], Some("mini"));

    assert!(!rows[0].active);
    assert!(
        rows.iter().any(|row| row.value == "mini" && row.active),
        "the selection is where the cursor should open"
    );
}

/// A model with no efforts still has a Default row — the caller decides
/// whether to open the dialog at all, and upstream refuses with a toast
/// before it gets here.
#[test]
fn a_model_with_no_efforts_yields_the_default_row_alone() {
    let rows = rows([], None);

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].value, DEFAULT);
}
