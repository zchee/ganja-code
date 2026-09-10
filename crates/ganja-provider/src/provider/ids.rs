//! Ids two wires derive the same way from a transcript's own message ids.
//!
//! One function and its renderer, and they are here rather than in either
//! wire because **two** wires now key on them. D553 wrote [`derived`] inside
//! cursor's `history` to give a history blob a `message_id` that survives
//! compaction; **D556** keys a held `claude` process by
//! `derived(messages[0].id)` for the same property — a name that is stable
//! for as long as the conversation's first message is, and that changes
//! exactly when compaction replaces it.
//!
//! Nothing here reaches a vendor's API or knows what a wire is. It is the
//! hash and the layout, so that the two wires cannot drift into deriving the
//! same id two ways.

use std::fmt::Write as _;

use sha2::{Digest as _, Sha256};

use crate::protocol::MessageId;

/// What [`derived`] hashes ahead of the id.
///
/// A domain separator: the same message id derives a different value here
/// than it would under any other seed, so two derivations that happen to
/// share this function cannot collide by construction rather than by luck.
/// Its bytes are cursor's own from D553 and must not change — a new spelling
/// re-mints every id every live `claude` process and every composed cursor
/// blob is filed under.
const DERIVATION_SEED: &str = "ganja-cursor-message:";

/// A stable id derived from a transcript message's own `id`: the first
/// sixteen bytes of `sha256(seed ‖ id)` rendered as a v4-shaped UUID — the
/// reference's `deterministicUuid` layout (`proxy.ts:1341-1351`) from a
/// different seed.
///
/// Derived rather than minted so the same message derives the same value on
/// every request, and from the transcript's id rather than a turn index and
/// its text so the value survives compaction, where an index shift re-mints
/// every id the reference has.
///
/// Three consumers: each cursor history user blob's `message_id`, cursor's
/// run request `conversation_id` (both derived from a message's id — D553),
/// and **D556**'s held-process key, `derived(messages[0].id)`. An action's own
/// id is never derived — that one is `cursor::request::fresh_id`'s random
/// one, the reference's shape.
#[must_use]
pub fn derived(id: &MessageId) -> String {
    let digest = Sha256::new().chain_update(DERIVATION_SEED).chain_update(id.as_str()).finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);

    render_v4(bytes)
}

/// Sixteen bytes rendered as a v4 UUID: the version and variant nibbles
/// stamped, then lowercase hex in the 8-4-4-4-12 grouping.
///
/// The bits are stamped whatever the bytes were, because the two producers
/// disagree about where their bytes come from — `cursor::request::fresh_id`'s
/// are random and [`derived`]'s are a hash — so the two ids on a run request
/// are the same *shape* by construction, which is the reference's own
/// arrangement (`proxy.ts:849` mints one, `:1341-1351` derives the other,
/// both to this layout).
#[must_use]
pub fn render_v4(mut bytes: [u8; 16]) -> String {
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let mut rendered = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            rendered.push('-');
        }
        write!(rendered, "{byte:02x}").expect("writing hex into a String cannot fail");
    }

    rendered
}

#[cfg(test)]
#[path = "ids_tests.rs"]
mod tests;
