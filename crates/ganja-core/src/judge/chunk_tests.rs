use std::time::{Duration, Instant};

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use super::{MERGE_MAX, S_MAX, SPLIT_MAX, Segment, segments};

/// The vectors, as the measurement's harness wrote them and a generator over
/// the same segmenter extended them.
const VECTORS: &str = include_str!("chunk-vectors.json");

/// How many of [`VECTORS`] the harness itself wrote, before the generator's
/// branch cases.
const FROZEN_VECTORS: usize = 12;

/// Where the harness's own file ends inside [`VECTORS`]: its bytes are this
/// prefix followed by `"\n]"`.
const FROZEN_PREFIX: usize = 192_961;

/// The sha256 of the harness's own file, whole.
const FROZEN_SHA256: &str = "2d6e3e237423ec4901c1ab8224782f6aa0e50d3eb7a3cfded3fb1be3384a72c5";

/// One vector, in the harness's own field order.
#[derive(Deserialize)]
struct Vector {
    name: String,
    input: String,
    input_sha256: String,
    segments: Vec<(usize, usize)>,
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn vectors() -> Vec<Vector> {
    serde_json::from_str(VECTORS).expect("the committed vectors parse")
}

/// What every segmentation holds, whatever the input: contiguous, covering,
/// non-empty, at most [`SPLIT_MAX`] bytes, cut on char boundaries.
fn invariants(name: &str, content: &str, cut: &[Segment]) {
    if content.is_empty() {
        assert!(cut.is_empty(), "{name}: empty input has no segment");
        return;
    }
    assert_eq!(cut.first().map(|s| s.start), Some(0), "{name}: the first segment starts at 0");
    assert_eq!(
        cut.last().map(|s| s.end),
        Some(content.len()),
        "{name}: the last segment ends at the content's end"
    );
    for pair in cut.windows(2) {
        assert_eq!(pair[0].end, pair[1].start, "{name}: segments are contiguous: {pair:?}");
    }
    for segment in cut {
        assert!(segment.start < segment.end, "{name}: {segment:?} is empty");
        assert!(
            segment.end - segment.start <= SPLIT_MAX,
            "{name}: {segment:?} is {} bytes",
            segment.end - segment.start
        );
        assert!(
            content.is_char_boundary(segment.start) && content.is_char_boundary(segment.end),
            "{name}: {segment:?} cuts inside a character"
        );
    }
}

/// Criterion 43: the port cuts every committed input exactly where the
/// measurement's segmenter cut it — the harness's own vectors and the branch
/// cases a generator `#[path]`-including that segmenter unmodified added
/// (random text over `a`, `é`, a 4-byte emoji, `.?!` and the four blank bytes
/// up to 20 KiB; a 4-byte character at every hard cut; `?`/`!` before `\t`
/// and `\r`; units of exactly 4,096 and 4,097 bytes; the level-0 fallback
/// inside a long line; a layout above fifty-two segments).
#[test]
fn the_port_cuts_every_committed_vector_where_the_measured_segmenter_did() {
    let vectors = vectors();
    assert_eq!(vectors.len(), 40, "twelve from the harness and twenty-eight branch cases");

    for vector in &vectors {
        assert_eq!(
            hex(vector.input.as_bytes()),
            vector.input_sha256,
            "{}: the committed input is the one the vector was cut from",
            vector.name
        );
        let cut = segments(&vector.input);
        invariants(&vector.name, &vector.input, &cut);
        let got: Vec<(usize, usize)> = cut.iter().map(|s| (s.start, s.end)).collect();
        assert_eq!(got, vector.segments, "{}: the port disagrees with the vector", vector.name);
    }
}

/// The first twelve vectors are the harness's own file, byte for byte: the
/// generator read them back and wrote them first, so its output begins with
/// exactly that file minus its closing bracket.
#[test]
fn the_committed_vectors_begin_with_the_harness_file_byte_for_byte() {
    let mut frozen = VECTORS.as_bytes()[..FROZEN_PREFIX].to_vec();
    frozen.extend_from_slice(b"\n]");

    assert_eq!(hex(&frozen), FROZEN_SHA256);
    let names: Vec<String> =
        vectors().into_iter().take(FROZEN_VECTORS).map(|vector| vector.name).collect();
    assert_eq!(
        names,
        [
            "empty",
            "one-line",
            "blank-only",
            "leading-blank",
            "merge-501x10",
            "merge-exact-2048",
            "mid-block-alone",
            "split-lines-30x300",
            "split-sentences",
            "hard-cut-multibyte",
            "crlf-blank-lines",
            "mixed-multibyte",
        ]
    );
}

/// A layout the contract cannot send whole exists among the vectors, so the
/// cap is tested against a real cut rather than an assumed one.
#[test]
fn some_committed_layout_has_more_segments_than_are_sent() {
    let most = vectors().iter().map(|vector| vector.segments.len()).max().unwrap_or(0);

    assert!(most > S_MAX, "the largest vector has {most} segments");
}

/// Criterion 47, first half: fifty-three units of 1,025 bytes — reachable only
/// through an MCP `output_limit` above 50 KiB — are fifty-three segments,
/// because two units together are 2,050 bytes, over the merge target.
#[test]
fn fifty_three_units_of_1025_bytes_are_fifty_three_segments() {
    let content = format!("{}\n\n", "u".repeat(1023)).repeat(53);
    assert_eq!(content.len(), 54_325);

    let cut = segments(&content);
    invariants("53 x 1025", &content, &cut);
    assert_eq!(cut.len(), 53);
    assert!(cut.iter().all(|s| s.end - s.start == 1025 && 2 * 1025 > MERGE_MAX));
}

/// Criterion 48: the cut search is linear. A megabyte and a half of short
/// lines is one block the split walks line end by line end; the rescan the
/// rule was first written with took seconds on it.
#[test]
fn a_megabyte_and_a_half_of_short_lines_segments_in_under_a_second() {
    let content: String = (0..200_000).map(|index| format!("l{index:06}\n")).collect();
    assert!(content.len() >= 1_536 * 1024, "{} bytes", content.len());

    let started = Instant::now();
    let cut = segments(&content);
    let took = started.elapsed();

    invariants("1.5 MiB of lines", &content, &cut);
    assert!(took < Duration::from_secs(1), "segmenting took {took:?}");
}

/// A cut never lands inside a character. With one to three ASCII bytes and
/// then only 4-byte characters — no line end, no sentence end — the first
/// hard cut lands inside a character and falls back to its start; after it
/// every piece starts on a character and 4,096 is a multiple of four, so the
/// rest land between characters. With 3-byte characters every hard cut lands
/// inside one, and every piece falls back a byte.
#[test]
fn a_hard_cut_inside_a_character_falls_back_to_its_start() {
    for offset in 1..=3 {
        let content = format!("{}{}", "a".repeat(offset), "😀".repeat(3000));
        let cut = segments(&content);

        invariants("emoji", &content, &cut);
        let first = cut.first().map(|s| s.end).unwrap_or_default();
        assert_eq!(first, offset + 4 * ((SPLIT_MAX - offset) / 4), "offset {offset}");
        assert!(first < SPLIT_MAX, "offset {offset}: the first piece stopped short");
    }

    let content = "€".repeat(4000);
    let cut = segments(&content);
    invariants("euro", &content, &cut);
    assert!(
        cut[..cut.len() - 1].iter().all(|s| s.end - s.start == SPLIT_MAX - 1),
        "every full piece fell back one byte: {cut:?}"
    );
}
