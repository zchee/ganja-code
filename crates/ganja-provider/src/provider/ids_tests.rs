use super::{derived, render_v4};
use crate::protocol::MessageId;

/// The derivation is a **hash of one input**, so a fixed message id derives a
/// fixed value — pinned as a literal, because the id every live `claude`
/// process and every composed cursor blob is filed under is exactly this
/// string, and a change to the seed or the layout re-mints all of them.
#[test]
fn a_fixed_message_id_derives_a_fixed_value() {
    let id = MessageId::from("019841e1-0000-7000-8000-000000000001".to_owned());

    assert_eq!(derived(&id), "a6f47bf8-9725-440f-a526-60476cd6867f");
}

#[test]
fn two_message_ids_derive_two_values_and_one_derives_one() {
    let first = MessageId::from("019841e1-0000-7000-8000-000000000001".to_owned());
    let second = MessageId::from("019841e1-0000-7000-8000-000000000002".to_owned());

    assert_ne!(derived(&first), derived(&second));
    assert_eq!(derived(&first), derived(&first));
}

#[test]
fn a_derived_value_is_shaped_like_a_v4_uuid() {
    let rendered = derived(&MessageId::from("anything".to_owned()));

    assert_eq!(rendered.len(), 36);
    assert_eq!(
        rendered
            .chars()
            .enumerate()
            .filter(|(_, glyph)| *glyph == '-')
            .map(|(at, _)| at)
            .collect::<Vec<_>>(),
        vec![8, 13, 18, 23],
        "the 8-4-4-4-12 grouping"
    );
    assert!(rendered.chars().all(|glyph| glyph == '-' || glyph.is_ascii_hexdigit()));
    assert_eq!(rendered.as_bytes()[14], b'4', "the version nibble");
    assert!(matches!(rendered.as_bytes()[19], b'8' | b'9' | b'a' | b'b'), "the variant nibble");
}

/// The version and variant bits are stamped whatever arrived, which is what
/// lets a hash and a random draw render to the same shape.
#[test]
fn the_version_and_variant_nibbles_are_stamped_over_whatever_the_bytes_held() {
    assert_eq!(render_v4([0x00; 16]), "00000000-0000-4000-8000-000000000000");
    assert_eq!(render_v4([0xff; 16]), "ffffffff-ffff-4fff-bfff-ffffffffffff");
}
