//! The rows of the effort picker: upstream's flat list, "Default" first.
//!
//! Spec: upstream `packages/tui/src/component/dialog-variant.tsx`. The dialog
//! itself is the shared [`list::ListDialog`] — what makes an effort picker an
//! effort picker is only its rows, so the rows are what this module holds:
//! a fixed "Default" entry that clears the selection, then the active model's
//! catalog names in the catalog's order, with the active mark on whichever of
//! them the session is running (or on Default when it runs none).
//!
//! Upstream marks the clearing row with the literal value `"default"`
//! (`dialog-variant.tsx:13`) and stores the same word for a cleared selection
//! (`context/local.tsx:388`), so a catalog effort that happened to be named
//! `default` would collide there exactly as it does here — the collision is
//! ported, not invented.

use crate::component::list;

/// The row value that means "no effort", upstream's own marker.
pub const DEFAULT: &str = "default";

/// What the Default row is called on screen (`dialog-variant.tsx:14`).
const DEFAULT_TITLE: &str = "Default";

/// The picker's rows over `names`, with the active mark on `selected`.
///
/// `names` is the active model's effort roster in the order the catalog
/// keeps it; `None` for `selected` is a session running no effort, which is
/// what the Default row is for.
pub fn rows<'a>(
    names: impl IntoIterator<Item = &'a str>,
    selected: Option<&str>,
) -> Vec<list::Row> {
    std::iter::once(list::Row {
        value: DEFAULT.to_owned(),
        label: DEFAULT_TITLE.to_owned(),
        detail: None,
        active: selected.is_none(),
    })
    .chain(names.into_iter().map(|name| list::Row {
        value: name.to_owned(),
        label: name.to_owned(),
        detail: None,
        active: selected == Some(name),
    }))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT, rows};

    #[test]
    fn the_default_row_comes_first_and_is_active_when_nothing_is_selected() {
        let rows = rows(["max", "mini"], None);

        assert_eq!(
            rows.iter()
                .map(|row| row.value.as_str())
                .collect::<Vec<_>>(),
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
}
