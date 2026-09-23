//! How one tool result is cut into the segments the judge sends (**D567**).
//!
//! The rule is the one the measurement ran: a result is not sent whole, it is
//! cut into segments of at most [`SPLIT_MAX`] bytes, each judged on its own,
//! and the result fires when any segment does. A segment is small enough that
//! a planted passage is most of what the model is shown, which is what made
//! the measured recall possible; the vectors next to this file hold the cut
//! the measurement made, byte for byte, and `chunk_tests.rs` holds this port to
//! them.
//!
//! Input: the bytes of the content exactly as the judge sends them. Output:
//! contiguous, non-overlapping `[start, end)` byte ranges covering the whole
//! input, every cut on a UTF-8 char boundary.
//!
//! 1. Blocks. A line is *blank* when it holds only ` `, `\t`, `\r` (and its
//!    `\n`). A block is a maximal run of non-blank lines together with the
//!    blank lines that follow it; blank lines at the very start belong to the
//!    first block. So a new block starts at every non-blank line that follows
//!    a blank line.
//! 2. Oversized blocks. A block longer than [`SPLIT_MAX`] bytes is split into
//!    pieces of at most [`SPLIT_MAX`] bytes, greedily (each piece as long as
//!    possible), cutting first at line ends (after `\n`); a stretch with no
//!    line end that fits is cut at sentence ends (after one whitespace byte
//!    that follows `.`, `?` or `!`); a stretch with neither is hard-cut at the
//!    last char boundary at or before [`SPLIT_MAX`] bytes.
//! 3. Merge. Blocks and pieces, in order, are merged greedily: the next unit
//!    joins the current segment while the segment stays at most [`MERGE_MAX`]
//!    bytes; otherwise it starts a new segment. A unit of 2,049 to 4,096
//!    bytes is therefore a segment of its own.
//!
//! Only the first [`S_MAX`] segments are sent; the rest are counted.
//!
//! **One difference from the rule as first written, and it is not in the
//! output.** Step 2's search for the furthest cut that fits walks the cut
//! points once, forward, instead of rescanning every cut point for every
//! piece: the rescan is quadratic in the number of lines of a large block and
//! took seconds on a megabyte of short lines, which an MCP server's
//! `output_limit` can reach. The vectors prove the cut unchanged.

/// Greedy-merge target, in bytes.
pub(crate) const MERGE_MAX: usize = 2048;

/// A block above this many bytes is split, and no segment is longer.
pub(crate) const SPLIT_MAX: usize = 4096;

/// Segments per tool result that are sent; the rest are counted, not judged.
///
/// Fifty-two rather than the forty the measurement ran under, by the owner's
/// ruling: no measured result had more than forty, so every measured number
/// stands, and fifty-two is the count the 50 KiB clamp can never exceed — two
/// adjacent segments hold at least 2,049 bytes, so fifty-three need at least
/// 53,275 bytes, and a clamped result is at most 51,244. Only an MCP server
/// whose `output_limit` is raised above 50 KiB can leave a segment unsent.
pub(crate) const S_MAX: usize = 52;

/// One segment, as byte offsets into the content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Segment {
    /// The first byte.
    pub(crate) start: usize,
    /// One past the last byte.
    pub(crate) end: usize,
}

/// Whether `line` holds only blank bytes.
fn is_blank(line: &str) -> bool {
    line.bytes().all(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
}

/// Block start offsets (step 1). The first is always 0 for non-empty input.
fn block_starts(content: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut at = 0;
    let mut previous_blank = false;
    // Whether the current block has a non-blank line yet: leading blank lines
    // at the start of the content belong to the first block.
    let mut has_text = false;
    for line in content.split_inclusive('\n') {
        let blank = is_blank(line);
        if at == 0 {
            starts.push(0);
        } else if !blank && previous_blank && has_text {
            starts.push(at);
        }
        has_text |= !blank;
        previous_blank = blank;
        at += line.len();
    }

    starts
}

/// Cut points strictly inside `(start, end)`, ascending, at `level`: 0 is
/// after a `\n`, 1 is after the whitespace byte that follows `.`, `?` or `!`.
fn boundaries(content: &str, start: usize, end: usize, level: u8) -> Vec<usize> {
    let bytes = content.as_bytes();
    (start + 1..end)
        .filter(|&at| match level {
            0 => bytes[at - 1] == b'\n',
            _ => {
                at >= 2
                    && matches!(bytes[at - 1], b' ' | b'\t' | b'\n' | b'\r')
                    && matches!(bytes[at - 2], b'.' | b'?' | b'!')
            }
        })
        .collect()
}

/// The last char boundary at or before `at`.
fn floor_boundary(content: &str, mut at: usize) -> usize {
    while !content.is_char_boundary(at) {
        at -= 1;
    }

    at
}

/// Step 2: `[start, end)` split into pieces of at most [`SPLIT_MAX`] bytes.
///
/// The cut search is linear: `next` is the first cut point past the current
/// piece's start and `reach` the first past its budget, and both only move
/// forward, so every cut point is looked at a bounded number of times. The
/// piece is `[current, cuts[reach - 1])` when `reach > next` — the furthest
/// cut that fits, which is what the rule asks for — and otherwise the stretch
/// up to the next cut drops a level.
fn split(content: &str, start: usize, end: usize, level: u8, out: &mut Vec<(usize, usize)>) {
    if end - start <= SPLIT_MAX {
        out.push((start, end));
        return;
    }
    if level >= 2 {
        let mut current = start;
        while end - current > SPLIT_MAX {
            let cut = floor_boundary(content, current + SPLIT_MAX);
            out.push((current, cut));
            current = cut;
        }
        out.push((current, end));
        return;
    }

    let cuts = boundaries(content, start, end, level);
    let mut current = start;
    let mut next = 0;
    while end - current > SPLIT_MAX {
        while next < cuts.len() && cuts[next] <= current {
            next += 1;
        }
        let mut reach = next;
        while reach < cuts.len() && cuts[reach] - current <= SPLIT_MAX {
            reach += 1;
        }
        if reach > next {
            let cut = cuts[reach - 1];
            out.push((current, cut));
            current = cut;
            next = reach;
        } else {
            let stop = cuts.get(next).copied().unwrap_or(end);
            split(content, current, stop, level + 1, out);
            current = stop;
        }
    }
    if current < end {
        out.push((current, end));
    }
}

/// Every segment of `content`, in document order (steps 1–3). Empty content
/// has none.
pub(crate) fn segments(content: &str) -> Vec<Segment> {
    let starts = block_starts(content);
    let mut units = Vec::new();
    for (index, &start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(content.len());
        split(content, start, end, 0, &mut units);
    }

    let mut out: Vec<Segment> = Vec::new();
    for (start, end) in units {
        match out.last_mut() {
            Some(last) if end - last.start <= MERGE_MAX => last.end = end,
            _ => out.push(Segment { start, end }),
        }
    }

    out
}

#[cfg(test)]
#[path = "chunk_tests.rs"]
mod tests;
