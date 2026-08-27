//! The config dialect this build has left, and the only place it is still
//! decoded.
//!
//! Spec: none of its own. What is here was the whole of the loader's own read
//! until `ganja.toml` became the config (**D536**), moved rather than
//! rewritten, so a file that loaded before the format change loads the same
//! way here.
//!
//! Two callers, and no third: `ganja config migrate`, which reads a legacy
//! file in order to write the `ganja.toml` that replaces it, and `ganja config
//! import-opencode`, which reads *upstream's* files — those are still JSONC,
//! are somebody else's format, and are not migrating anywhere, which is why
//! [`parse_options`] is public and the dialect it describes has to keep
//! agreeing with what upstream writes.
//!
//! The loader itself no longer comes here. A discovered
//! [`crate::config::LEGACY_FILES`] entry is
//! [`crate::config::ConfigError::Legacy`] — the refusal, not the read — and
//! this module is what makes that refusal answerable rather than merely
//! final.

use std::{fs, path::Path};

use super::{Config, ConfigError};

/// How a file in the legacy dialect is parsed.
///
/// Comments and trailing commas are the JSONC dialect upstream accepts, and
/// are why `.json` is parsed by the same reader. Everything else the crate
/// would tolerate by default — single quotes, hexadecimal numbers, missing
/// commas, unquoted keys — is refused, because a file that loads here and
/// nowhere else is a file that has stopped being JSON.
///
/// Public because two readers of the same dialect live outside this crate:
/// `ganja config import-opencode`, reading the files upstream writes, and
/// `ganja config migrate`, reading the ones this build has left behind. Both
/// *call* this rather than keeping a copy of it, so the looser-or-stricter
/// question has one answer by construction.
#[must_use]
pub fn parse_options() -> jsonc_parser::ParseOptions {
    jsonc_parser::ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

/// Reads one file in the legacy dialect as the [`Config`] this build would
/// have loaded from it.
///
/// The decode goes straight into [`Config`] from `jsonc-parser`'s `serde`
/// feature over its token stream, which sees keys in document order — never
/// through a value type that sorts, because permission rules are evaluated
/// last-match-wins and the order a document spelled its keys in is the answer
/// to which rule decides. The `Option` is what makes an empty file, or one
/// holding nothing but comments, an empty config rather than a type error
/// about `null`.
///
/// Then the loader's own seven post-decode refusals run, through the same
/// private `checked` the TOML path uses. That is the whole point of reading a
/// legacy file *here* rather than in the command that converts one:
/// a source this build would decline at launch is declined at the read, not
/// translated cleanly into a `ganja.toml` whose first launch refuses it.
///
/// # Errors
///
/// Returns [`ConfigError`] for a file that cannot be read, is not valid in the
/// dialect, does not describe a config, or fails one of those seven checks.
/// Unlike the loader's own read, an absent file is an error here: nothing
/// discovers its way into this function, so every path that arrives was asked
/// for by name.
pub fn read(path: &Path) -> Result<Config, ConfigError> {
    let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_owned(),
        source,
    })?;

    let config = jsonc_parser::parse_to_serde_value::<Option<Config>>(&text, &parse_options())
        .map(Option::unwrap_or_default)
        .map_err(|error| ConfigError::Parse {
            path: path.to_owned(),
            message: error.to_string(),
        })?;

    super::checked(path, config)
}

#[cfg(test)]
#[path = "legacy_tests.rs"]
mod tests;
