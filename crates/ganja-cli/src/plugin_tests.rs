use super::{approx, grouped};

/// The reference output's own roundings, row by row: `< 20` below
/// twenty, nearest ten below a thousand, tenths of k above with a clean
/// `.0` dropped.
#[test]
fn token_estimates_round_the_way_the_reference_output_does() {
    assert_eq!(approx(7), "< 20");
    assert_eq!(approx(19), "< 20");
    assert_eq!(approx(20), "~20");
    assert_eq!(approx(38), "~40");
    assert_eq!(approx(44), "~40");
    assert_eq!(approx(946), "~950");
    assert_eq!(approx(1_020), "~1k");
    assert_eq!(approx(1_120), "~1.1k");
    assert_eq!(approx(3_840), "~3.8k");
}

/// The always-on total keeps its digits, grouped in threes.
#[test]
fn the_total_groups_its_thousands() {
    assert_eq!(grouped(7), "7");
    assert_eq!(grouped(950), "950");
    assert_eq!(grouped(1_620), "1,620");
    assert_eq!(grouped(1_234_567), "1,234,567");
}
