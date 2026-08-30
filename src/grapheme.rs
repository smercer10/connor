//! Grapheme-cluster boundaries, iteration and display widths over rope
//! slices. `GraphemeCursor` wants contiguous `&str`s; ropes store text in
//! chunks, so every entry point here feeds the cursor chunk by chunk.

use std::ops::Range;

use ropey::RopeSlice;
use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete};
use unicode_width::UnicodeWidthStr;

pub const TAB_WIDTH: usize = 8;

/// The char index of the first cluster boundary after `char_idx`, or the
/// slice's end when there is none.
pub fn next_grapheme_boundary(slice: RopeSlice, char_idx: usize) -> usize {
    let byte_idx = slice.char_to_byte(char_idx);
    let mut cursor = GraphemeCursor::new(byte_idx, slice.len_bytes(), true);
    let (mut chunk, mut chunk_start, _, _) = slice.chunk_at_byte(byte_idx);
    loop {
        match cursor.next_boundary(chunk, chunk_start) {
            Ok(Some(boundary)) => return slice.byte_to_char(boundary),
            Ok(None) => return slice.len_chars(),
            Err(GraphemeIncomplete::NextChunk) => {
                chunk_start += chunk.len();
                chunk = slice.chunk_at_byte(chunk_start).0;
            }
            Err(GraphemeIncomplete::PreContext(byte)) => {
                let (context, context_start, _, _) = slice.chunk_at_byte(byte - 1);
                cursor.provide_context(context, context_start);
            }
            _ => unreachable!(),
        }
    }
}

/// The char index of the last cluster boundary before `char_idx`, or 0.
pub fn prev_grapheme_boundary(slice: RopeSlice, char_idx: usize) -> usize {
    let byte_idx = slice.char_to_byte(char_idx);
    let mut cursor = GraphemeCursor::new(byte_idx, slice.len_bytes(), true);
    let (mut chunk, mut chunk_start, _, _) = slice.chunk_at_byte(byte_idx);
    loop {
        match cursor.prev_boundary(chunk, chunk_start) {
            Ok(Some(boundary)) => return slice.byte_to_char(boundary),
            Ok(None) => return 0,
            Err(GraphemeIncomplete::PrevChunk) => {
                let (prev, prev_start, _, _) = slice.chunk_at_byte(chunk_start - 1);
                chunk = prev;
                chunk_start = prev_start;
            }
            Err(GraphemeIncomplete::PreContext(byte)) => {
                let (context, context_start, _, _) = slice.chunk_at_byte(byte - 1);
                cursor.provide_context(context, context_start);
            }
            _ => unreachable!(),
        }
    }
}

/// The nearest cluster boundary at or after `char_idx` — the identity when
/// already on one. Guards callers whose index can land mid-cluster, e.g. an
/// insert that merges with a following combining mark.
pub fn snap_to_boundary(slice: RopeSlice, char_idx: usize) -> usize {
    if char_idx == 0 {
        return 0;
    }
    next_grapheme_boundary(slice, prev_grapheme_boundary(slice, char_idx))
}

/// Yields each cluster of a slice as a char range, front to back. Walks the
/// chunks linearly — amortized O(bytes), no per-cluster tree lookups — which
/// is what rendering and word scans want.
pub struct RopeGraphemes<'a> {
    slice: RopeSlice<'a>,
    chunk: &'a str,
    chunk_start: usize,
    cursor: GraphemeCursor,
    char_idx: usize,
}

impl<'a> RopeGraphemes<'a> {
    pub fn new(slice: RopeSlice<'a>) -> Self {
        let (chunk, chunk_start, _, _) = slice.chunk_at_byte(0);
        RopeGraphemes {
            slice,
            chunk,
            chunk_start,
            cursor: GraphemeCursor::new(0, slice.len_bytes(), true),
            char_idx: 0,
        }
    }
}

impl Iterator for RopeGraphemes<'_> {
    type Item = Range<usize>;

    fn next(&mut self) -> Option<Range<usize>> {
        let start = self.char_idx;
        let mut consumed_from = self.cursor.cur_cursor();
        loop {
            match self.cursor.next_boundary(self.chunk, self.chunk_start) {
                Ok(Some(boundary)) => {
                    let local =
                        &self.chunk[consumed_from - self.chunk_start..boundary - self.chunk_start];
                    self.char_idx += local.chars().count();
                    return Some(start..self.char_idx);
                }
                Ok(None) => return None,
                Err(GraphemeIncomplete::NextChunk) => {
                    let local = &self.chunk[consumed_from - self.chunk_start..];
                    self.char_idx += local.chars().count();
                    self.chunk_start += self.chunk.len();
                    consumed_from = self.chunk_start;
                    self.chunk = self.slice.chunk_at_byte(self.chunk_start).0;
                }
                Err(GraphemeIncomplete::PreContext(byte)) => {
                    let (context, context_start, _, _) = self.slice.chunk_at_byte(byte - 1);
                    self.cursor.provide_context(context, context_start);
                }
                _ => unreachable!(),
            }
        }
    }
}

/// Copies the cluster at `range` into `buf` and returns it as `&str` without
/// heap allocation. Clusters too long for the buffer (only exotic ZWJ
/// sequences) come back as U+FFFD, matching what a `Cell` can hold.
pub fn grapheme_str<'b>(slice: RopeSlice, range: Range<usize>, buf: &'b mut [u8; 16]) -> &'b str {
    let mut len = 0;
    for ch in slice.slice(range).chars() {
        if len + ch.len_utf8() > buf.len() {
            return "\u{FFFD}";
        }
        ch.encode_utf8(&mut buf[len..]);
        len += ch.len_utf8();
    }
    str::from_utf8(&buf[..len]).unwrap_or("\u{FFFD}")
}

/// Display columns for one cluster drawn at visual column `at_col` (tabs run
/// to the next tab stop, so their width depends on where they start).
/// Control clusters render as U+FFFD and degenerate zero-width clusters are
/// clamped, so nothing occupies zero columns.
pub fn grapheme_width(grapheme: &str, at_col: usize) -> usize {
    if grapheme == "\t" {
        return TAB_WIDTH - at_col % TAB_WIDTH;
    }
    if grapheme.chars().next().is_some_and(char::is_control) {
        return 1;
    }
    grapheme.width().max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ropey::Rope;

    fn boundaries(text: &str) -> Vec<usize> {
        let rope = Rope::from_str(text);
        let mut out = vec![0];
        let mut idx = 0;
        while idx < rope.len_chars() {
            idx = next_grapheme_boundary(rope.slice(..), idx);
            out.push(idx);
        }
        out
    }

    #[test]
    fn ascii_boundaries_are_per_char() {
        assert_eq!(boundaries("abc"), vec![0, 1, 2, 3]);
    }

    #[test]
    fn combining_marks_stay_attached() {
        // e + acute, a + grave + combining grave below
        assert_eq!(boundaries("e\u{301}a\u{300}\u{316}"), vec![0, 2, 5]);
    }

    #[test]
    fn crlf_is_one_cluster() {
        assert_eq!(boundaries("a\r\nb"), vec![0, 1, 3, 4]);
    }

    #[test]
    fn flag_and_zwj_emoji_are_single_clusters() {
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
        assert_eq!(boundaries("\u{1F1E6}\u{1F1FA}"), vec![0, 2]);
        assert_eq!(boundaries(family), vec![0, family.chars().count()]);
    }

    #[test]
    fn prev_boundary_mirrors_next() {
        let rope = Rope::from_str("e\u{301}a\r\nb");
        let slice = rope.slice(..);
        assert_eq!(prev_grapheme_boundary(slice, slice.len_chars()), 5);
        assert_eq!(prev_grapheme_boundary(slice, 5), 3);
        assert_eq!(prev_grapheme_boundary(slice, 3), 2);
        assert_eq!(prev_grapheme_boundary(slice, 2), 0);
        assert_eq!(prev_grapheme_boundary(slice, 0), 0);
    }

    #[test]
    fn boundaries_cross_chunk_seams() {
        // Big enough that ropey splits chunks; a combining pair sits right
        // after a long ASCII run so some seam falls near or inside clusters.
        let mut text = "x".repeat(2000);
        text.push_str("e\u{301}");
        text.push_str(&"y".repeat(2000));
        let rope = Rope::from_str(&text);
        let slice = rope.slice(..);
        assert!(rope.chunks().count() > 1, "test needs a multi-chunk rope");
        assert_eq!(next_grapheme_boundary(slice, 2000), 2002);
        assert_eq!(prev_grapheme_boundary(slice, 2002), 2000);
        // Walking the whole rope lands exactly at the end.
        let mut idx = 0;
        let mut count = 0;
        while idx < slice.len_chars() {
            idx = next_grapheme_boundary(slice, idx);
            count += 1;
        }
        assert_eq!(idx, slice.len_chars());
        assert_eq!(count, 4001);
    }

    #[test]
    fn snap_is_identity_on_boundaries_and_snaps_forward_inside_clusters() {
        let rope = Rope::from_str("ae\u{301}b");
        let slice = rope.slice(..);
        for idx in [0, 1, 3, 4] {
            assert_eq!(snap_to_boundary(slice, idx), idx);
        }
        assert_eq!(snap_to_boundary(slice, 2), 3); // between e and its accent
    }

    #[test]
    fn iterator_matches_one_shot_boundaries() {
        let mut text = "a".repeat(1500);
        text.push_str("日本\t🙂 e\u{301}\r\n");
        let rope = Rope::from_str(&text);
        let ranges: Vec<_> = RopeGraphemes::new(rope.slice(..)).collect();
        let mut expected = Vec::new();
        let mut idx = 0;
        while idx < rope.len_chars() {
            let next = next_grapheme_boundary(rope.slice(..), idx);
            expected.push(idx..next);
            idx = next;
        }
        assert_eq!(ranges, expected);
    }

    #[test]
    fn grapheme_str_copies_and_clips() {
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
        let rope = Rope::from_str(&format!("ab日e\u{301}{family}"));
        let slice = rope.slice(..);
        let mut buf = [0; 16];
        assert_eq!(grapheme_str(slice, 0..1, &mut buf), "a");
        assert_eq!(grapheme_str(slice, 2..3, &mut buf), "日");
        assert_eq!(grapheme_str(slice, 3..5, &mut buf), "e\u{301}");
        assert_eq!(
            grapheme_str(slice, 5..5 + family.chars().count(), &mut buf),
            "\u{FFFD}"
        );
    }

    #[test]
    fn tab_width_runs_to_the_next_stop() {
        assert_eq!(grapheme_width("\t", 0), 8);
        assert_eq!(grapheme_width("\t", 3), 5);
        assert_eq!(grapheme_width("\t", 7), 1);
        assert_eq!(grapheme_width("\t", 8), 8);
    }

    #[test]
    fn widths_for_wide_control_and_zero_width_clusters() {
        assert_eq!(grapheme_width("a", 0), 1);
        assert_eq!(grapheme_width("日", 0), 2);
        assert_eq!(grapheme_width("e\u{301}", 0), 1);
        assert_eq!(grapheme_width("\r", 0), 1);
        assert_eq!(grapheme_width("\u{301}", 0), 1); // lone combining mark
    }
}
