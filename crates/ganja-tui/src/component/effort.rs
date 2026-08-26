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
#[path = "effort_tests.rs"]
mod tests;
