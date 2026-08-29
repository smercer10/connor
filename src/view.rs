use crate::doc::Document;
use crate::grapheme::{self, RopeGraphemes};

/// Everything that belongs to one view of a document rather than to the
/// document itself: cursor, sticky column, and scroll position. A document
/// shown in two places would have two of these.
#[derive(Default)]
pub struct View {
    /// Char index into the rope. Always on a grapheme-cluster boundary and
    /// never past a line's terminator (it may sit at a line's end, after the
    /// last cluster).
    pub cursor: usize,
    /// The visual column vertical movement aims for, so the cursor springs
    /// back out wide after crossing short lines. Set by the first vertical
    /// move, cleared by any horizontal one.
    goal_col: Option<usize>,
    pub scroll_line: usize,
    /// Leftmost visible visual column of the text area.
    pub scroll_col: usize,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum CharClass {
    Whitespace,
    Word,
    Punct,
}

/// A cluster's class is its first scalar's: enough to make word movement
/// land where code editors land it.
fn classify(ch: char) -> CharClass {
    if ch.is_whitespace() {
        CharClass::Whitespace
    } else if ch.is_alphanumeric() || ch == '_' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

impl View {
    pub fn line(&self, doc: &Document) -> usize {
        doc.rope().char_to_line(self.cursor)
    }

    pub fn move_left(&mut self, doc: &Document) {
        self.goal_col = None;
        self.cursor = grapheme::prev_grapheme_boundary(doc.rope().slice(..), self.cursor);
    }

    pub fn move_right(&mut self, doc: &Document) {
        self.goal_col = None;
        self.cursor = grapheme::next_grapheme_boundary(doc.rope().slice(..), self.cursor);
    }

    pub fn move_home(&mut self, doc: &Document) {
        self.goal_col = None;
        self.cursor = doc.line_start(self.line(doc));
    }

    pub fn move_end(&mut self, doc: &Document) {
        self.goal_col = None;
        self.cursor = doc.line_end(self.line(doc));
    }

    pub fn move_doc_start(&mut self) {
        self.goal_col = None;
        self.cursor = 0;
    }

    pub fn move_doc_end(&mut self, doc: &Document) {
        self.goal_col = None;
        self.cursor = doc.rope().len_chars();
    }

    pub fn move_up(&mut self, doc: &Document) {
        self.move_vertical(doc, -1);
    }

    pub fn move_down(&mut self, doc: &Document) {
        self.move_vertical(doc, 1);
    }

    /// Pages shift the viewport a whole text area and carry the cursor with
    /// it, keeping its screen row; `scroll_to_cursor` then clamps at the
    /// document's edges.
    pub fn page_up(&mut self, doc: &Document, text_h: usize) {
        self.scroll_line = self.scroll_line.saturating_sub(text_h);
        self.move_vertical(doc, -(text_h as isize));
    }

    pub fn page_down(&mut self, doc: &Document, text_h: usize) {
        self.scroll_line = (self.scroll_line + text_h).min(doc.line_count() - 1);
        self.move_vertical(doc, text_h as isize);
    }

    fn move_vertical(&mut self, doc: &Document, delta: isize) {
        let line = self.line(doc);
        let goal = *self
            .goal_col
            .get_or_insert_with(|| vcol(doc, line, self.cursor));
        let target = line.saturating_add_signed(delta).min(doc.line_count() - 1);
        self.cursor = char_at_vcol(doc, target, goal);
    }

    /// To the start of the next word: skip the run of like-classed clusters
    /// under the cursor, then any whitespace. Crosses lines because `\n` is
    /// whitespace.
    pub fn move_word_right(&mut self, doc: &Document) {
        self.goal_col = None;
        let slice = doc.rope().slice(..);
        let len = slice.len_chars();
        let mut idx = self.cursor;
        if idx < len {
            let class = classify(slice.char(idx));
            while idx < len && classify(slice.char(idx)) == class {
                idx = grapheme::next_grapheme_boundary(slice, idx);
            }
        }
        while idx < len && classify(slice.char(idx)) == CharClass::Whitespace {
            idx = grapheme::next_grapheme_boundary(slice, idx);
        }
        self.cursor = idx;
    }

    /// To the start of the previous word: skip whitespace behind the cursor,
    /// then the run of like-classed clusters before it.
    pub fn move_word_left(&mut self, doc: &Document) {
        self.goal_col = None;
        let slice = doc.rope().slice(..);
        let mut idx = self.cursor;
        while idx > 0 {
            let prev = grapheme::prev_grapheme_boundary(slice, idx);
            if classify(slice.char(prev)) != CharClass::Whitespace {
                break;
            }
            idx = prev;
        }
        if idx > 0 {
            let class = classify(slice.char(grapheme::prev_grapheme_boundary(slice, idx)));
            while idx > 0 {
                let prev = grapheme::prev_grapheme_boundary(slice, idx);
                if classify(slice.char(prev)) != class {
                    break;
                }
                idx = prev;
            }
        }
        self.cursor = idx;
    }

    /// The cursor's visual column on its line. O(prefix): fine for any real
    /// line, lags only on a pathological single line of hundreds of MB (the
    /// future fix, if ever needed, is a cached line-offset anchor).
    pub fn vcol(&self, doc: &Document) -> usize {
        vcol(doc, self.line(doc), self.cursor)
    }

    /// Scrolls just far enough that the cursor's whole cluster is visible.
    /// Call after every movement and resize.
    pub fn scroll_to_cursor(&mut self, doc: &Document, text_w: usize, text_h: usize) {
        if text_w == 0 || text_h == 0 {
            return;
        }
        let line = self.line(doc);
        self.scroll_line = self
            .scroll_line
            .clamp(line.saturating_sub(text_h - 1), line);

        let vcol = vcol(doc, line, self.cursor);
        let width = self.cursor_width(doc, line, vcol);
        if vcol + width > self.scroll_col + text_w {
            self.scroll_col = vcol + width - text_w;
        }
        if vcol < self.scroll_col {
            self.scroll_col = vcol;
        }
    }

    /// Columns the cursor occupies: its cluster's width, or one at line end.
    fn cursor_width(&self, doc: &Document, line: usize, vcol: usize) -> usize {
        let end = doc.line_end(line);
        if self.cursor >= end {
            return 1;
        }
        let slice = doc.rope().slice(..);
        let next = grapheme::next_grapheme_boundary(slice, self.cursor).min(end);
        let mut buf = [0; 16];
        let cluster = grapheme::grapheme_str(slice, self.cursor..next, &mut buf);
        grapheme::grapheme_width(cluster, vcol)
    }
}

/// Visual column of `char_idx` on `line`: the summed widths of the clusters
/// before it.
fn vcol(doc: &Document, line: usize, char_idx: usize) -> usize {
    let start = doc.line_start(line);
    let slice = doc.rope().slice(start..doc.line_end(line));
    let mut buf = [0; 16];
    let mut col = 0;
    for range in RopeGraphemes::new(slice) {
        if start + range.end > char_idx {
            break;
        }
        let cluster = grapheme::grapheme_str(slice, range, &mut buf);
        col += grapheme::grapheme_width(cluster, col);
    }
    col
}

/// The cluster on `line` whose span contains visual column `goal`, or the
/// line's end when the line is shorter.
fn char_at_vcol(doc: &Document, line: usize, goal: usize) -> usize {
    let start = doc.line_start(line);
    let end = doc.line_end(line);
    let slice = doc.rope().slice(start..end);
    let mut buf = [0; 16];
    let mut col = 0;
    for range in RopeGraphemes::new(slice) {
        let cluster = grapheme::grapheme_str(slice, range.clone(), &mut buf);
        let width = grapheme::grapheme_width(cluster, col);
        if col + width > goal {
            return start + range.start;
        }
        col += width;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view_at(cursor: usize) -> View {
        View {
            cursor,
            ..View::default()
        }
    }

    #[test]
    fn left_and_right_step_whole_clusters() {
        let doc = Document::from_str("ae\u{301}\u{1F1E6}\u{1F1FA}b");
        let mut view = View::default();
        let stops = [1, 3, 5, 6, 6];
        for stop in stops {
            view.move_right(&doc);
            assert_eq!(view.cursor, stop);
        }
        for stop in [5, 3, 1, 0, 0] {
            view.move_left(&doc);
            assert_eq!(view.cursor, stop);
        }
    }

    #[test]
    fn right_at_line_end_wraps_to_next_line_start() {
        let doc = Document::from_str("ab\r\ncd");
        let mut view = view_at(2); // end of line 0
        view.move_right(&doc);
        assert_eq!(view.cursor, 4); // start of line 1
        view.move_left(&doc);
        assert_eq!(view.cursor, 2); // back to end of line 0, not inside \r\n
    }

    #[test]
    fn home_and_end_use_terminator_aware_bounds() {
        let doc = Document::from_str("ab\r\ncd\r\n");
        let mut view = view_at(5);
        view.move_end(&doc);
        assert_eq!(view.cursor, 6);
        view.move_home(&doc);
        assert_eq!(view.cursor, 4);
    }

    #[test]
    fn vertical_movement_keeps_the_goal_column() {
        // Line 1 is short, line 2 is long again: the cursor springs back out.
        let doc = Document::from_str("abcdef\nab\nabcdef");
        let mut view = view_at(4); // col 4 on line 0
        view.move_down(&doc);
        assert_eq!(view.cursor, 9); // clamped to end of "ab"
        view.move_down(&doc);
        assert_eq!(view.cursor, 14); // col 4 on line 2
        view.move_up(&doc);
        view.move_up(&doc);
        assert_eq!(view.cursor, 4);
    }

    #[test]
    fn goal_column_lands_on_wide_cluster_starts() {
        // "日本" occupies columns 0-3; goal col 3 is inside 本.
        let doc = Document::from_str("abcd\n日本\nabcd");
        let mut view = view_at(3);
        view.move_down(&doc);
        assert_eq!(view.cursor, 6); // start of 本
        view.move_down(&doc);
        assert_eq!(view.cursor, 11); // col 3 restored
    }

    #[test]
    fn goal_column_survives_a_tab() {
        // A tab at column 0 spans columns 0-7; goal 4 lands on the tab.
        let doc = Document::from_str("abcdefgh\n\tx\nabcdefgh");
        let mut view = view_at(4);
        view.move_down(&doc);
        assert_eq!(view.cursor, 9); // the tab itself
        view.move_down(&doc);
        assert_eq!(view.cursor, 16);
    }

    #[test]
    fn horizontal_movement_clears_the_goal_column() {
        let doc = Document::from_str("abcdef\nab\nabcdef");
        let mut view = view_at(4);
        view.move_down(&doc);
        view.move_left(&doc);
        assert_eq!(view.cursor, 8);
        view.move_down(&doc);
        assert_eq!(view.cursor, 11); // new goal is col 1, not 4
    }

    #[test]
    fn vertical_movement_clamps_at_document_edges() {
        let doc = Document::from_str("ab\ncd");
        let mut view = view_at(1);
        view.move_up(&doc);
        assert_eq!(view.line(&doc), 0);
        view.move_down(&doc);
        view.move_down(&doc);
        assert_eq!(view.line(&doc), 1);
    }

    #[test]
    fn document_jumps() {
        let doc = Document::from_str("ab\ncd\nef");
        let mut view = View::default();
        view.move_doc_end(&doc);
        assert_eq!(view.cursor, 8);
        view.move_doc_start();
        assert_eq!(view.cursor, 0);
    }

    #[test]
    fn word_right_stops_at_word_starts() {
        let doc = Document::from_str("foo_bar baz, qux\nnext");
        let mut view = View::default();
        let stops = [8, 11, 13, 17, 21];
        for stop in stops {
            view.move_word_right(&doc);
            assert_eq!(view.cursor, stop);
        }
        view.move_word_right(&doc);
        assert_eq!(view.cursor, 21); // end of document, stays put
    }

    #[test]
    fn word_left_mirrors_word_right() {
        let doc = Document::from_str("foo_bar baz, qux\nnext");
        let mut view = view_at(21);
        for stop in [17, 13, 11, 8, 0] {
            view.move_word_left(&doc);
            assert_eq!(view.cursor, stop);
        }
        view.move_word_left(&doc);
        assert_eq!(view.cursor, 0);
    }

    #[test]
    fn pages_move_viewport_and_cursor_together() {
        let text: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let doc = Document::from_str(&text);
        let mut view = View::default();
        view.page_down(&doc, 10);
        view.scroll_to_cursor(&doc, 80, 10);
        assert_eq!(view.line(&doc), 10);
        assert_eq!(view.scroll_line, 10);
        view.page_up(&doc, 10);
        view.scroll_to_cursor(&doc, 80, 10);
        assert_eq!(view.line(&doc), 0);
        assert_eq!(view.scroll_line, 0);
    }

    #[test]
    fn page_movement_clamps_at_the_last_line() {
        let doc = Document::from_str("a\nb\nc");
        let mut view = View::default();
        view.page_down(&doc, 50);
        view.scroll_to_cursor(&doc, 80, 10);
        assert_eq!(view.line(&doc), 2);
        assert!(view.scroll_line <= 2);
    }

    #[test]
    fn scrolling_tracks_the_cursor_on_all_four_edges() {
        let text: String = (0..50).map(|_| "abcdefghijklmnopqrstuvwxyz\n").collect();
        let doc = Document::from_str(&text);
        let mut view = view_at(doc.line_start(30));
        view.scroll_to_cursor(&doc, 10, 10);
        assert_eq!(view.scroll_line, 21); // cursor on the bottom row

        view.cursor = doc.line_start(5);
        view.scroll_to_cursor(&doc, 10, 10);
        assert_eq!(view.scroll_line, 5); // cursor on the top row

        view.cursor = doc.line_start(5) + 20;
        view.scroll_to_cursor(&doc, 10, 10);
        assert_eq!(view.scroll_col, 11); // cursor at the right edge

        view.cursor = doc.line_start(5) + 3;
        view.scroll_to_cursor(&doc, 10, 10);
        assert_eq!(view.scroll_col, 3); // cursor at the left edge
    }

    #[test]
    fn scrolling_keeps_a_wide_cluster_fully_visible() {
        let doc = Document::from_str("aaaa日z");
        let mut view = view_at(4); // the wide cluster, columns 4-5
        view.scroll_to_cursor(&doc, 5, 1);
        assert_eq!(view.scroll_col, 1); // columns 1-5 visible: 日 fits whole
    }

    #[test]
    fn degenerate_text_area_is_ignored() {
        let doc = Document::from_str("abc");
        let mut view = view_at(2);
        view.scroll_to_cursor(&doc, 0, 0);
        assert_eq!(view.scroll_line, 0);
        assert_eq!(view.scroll_col, 0);
    }
}
