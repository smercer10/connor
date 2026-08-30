use std::ops::Range;

use ropey::Rope;

use crate::doc::{Caret, ChangeSpan, Document, EditKind};
use crate::grapheme::{self, RopeGraphemes};

/// Where a char position lands after a reload: positions in the common
/// prefix stay, positions in the common suffix shift with the length delta,
/// and positions inside the changed region clamp to its start.
fn remap(pos: usize, span: &ChangeSpan) -> usize {
    if pos <= span.prefix {
        pos
    } else if pos >= span.old_suffix_start {
        pos - span.old_suffix_start + span.new_suffix_start
    } else {
        span.prefix
    }
}

/// Everything that belongs to one view of a document rather than to the
/// document itself: cursor, selection, sticky column, and scroll position.
/// A document shown in two places would have two of these.
#[derive(Default)]
pub struct View {
    /// Char index into the rope. Always on a grapheme-cluster boundary and
    /// never past a line's terminator (it may sit at a line's end, after the
    /// last cluster).
    pub cursor: usize,
    /// The selection's fixed end; the cursor is the moving end. `None` means
    /// no selection.
    pub anchor: Option<usize>,
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
    #[cfg(test)]
    pub fn test_at(cursor: usize, scroll_line: usize, scroll_col: usize) -> View {
        View {
            cursor,
            anchor: None,
            goal_col: None,
            scroll_line,
            scroll_col,
        }
    }

    #[cfg(test)]
    pub fn with_anchor(mut self, anchor: usize) -> View {
        self.anchor = Some(anchor);
        self
    }

    pub fn line(&self, doc: &Document) -> usize {
        doc.rope().char_to_line(self.cursor)
    }

    /// The selected char range, normalized so start ≤ end. A zero-width
    /// selection behaves as none everywhere.
    pub fn selection(&self) -> Option<Range<usize>> {
        let anchor = self.anchor?;
        (anchor != self.cursor).then(|| anchor.min(self.cursor)..anchor.max(self.cursor))
    }

    /// Called before every movement key: an extending move drops the anchor
    /// where the cursor stands, a plain move dissolves the selection.
    pub fn begin_or_clear_selection(&mut self, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
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

    /// Jumps to 1-based `line`, clamped to the document; cursor at its start.
    pub fn move_to_line(&mut self, doc: &Document, line: usize) {
        self.goal_col = None;
        self.cursor = doc.line_start(line.clamp(1, doc.line_count()) - 1);
    }

    /// A mouse press: places the cursor like a movement key, extending the
    /// selection when shift is held.
    pub fn click(
        &mut self,
        doc: &Document,
        gutter_w: usize,
        text_h: usize,
        x: u16,
        y: u16,
        extend: bool,
    ) {
        self.goal_col = None;
        self.begin_or_clear_selection(extend);
        self.cursor = self.hit(doc, gutter_w, text_h, x, y, false);
    }

    /// A drag with the button held: the press's anchor stays, the cursor
    /// follows the pointer.
    pub fn drag(&mut self, doc: &Document, gutter_w: usize, text_h: usize, x: u16, y: u16) {
        self.goal_col = None;
        self.begin_or_clear_selection(true);
        self.cursor = self.hit(doc, gutter_w, text_h, x, y, true);
    }

    /// Double-click: selects the run of like-classed clusters under the
    /// cursor.
    pub fn select_word(&mut self, doc: &Document) {
        let range = word_at(doc, self.cursor);
        self.goal_col = None;
        self.anchor = Some(range.start);
        self.cursor = range.end;
    }

    /// The wheel moves the viewport, not the cursor; the caller must skip
    /// the cursor snap for this event or it scrolls straight back.
    pub fn scroll_wheel(&mut self, doc: &Document, delta: isize) {
        self.scroll_line = self
            .scroll_line
            .saturating_add_signed(delta)
            .min(doc.line_count() - 1);
    }

    /// Maps a screen cell to a char index against the viewport as drawn.
    /// A drag overshoots one line past the text area's top and bottom edges,
    /// so a drag held there keeps advancing the selection and the caller's
    /// `scroll_to_cursor` pulls the viewport along — autoscroll with no
    /// timers.
    fn hit(
        &self,
        doc: &Document,
        gutter_w: usize,
        text_h: usize,
        x: u16,
        y: u16,
        drag: bool,
    ) -> usize {
        let y = usize::from(y);
        let overshoot = usize::from(drag);
        let line = if y == 0 {
            self.scroll_line.saturating_sub(overshoot)
        } else if y <= text_h {
            self.scroll_line + y - 1
        } else {
            self.scroll_line + text_h.saturating_sub(1) + overshoot
        };
        let goal = self.scroll_col + usize::from(x).saturating_sub(gutter_w);
        char_at_vcol(doc, line.min(doc.line_count() - 1), goal)
    }

    /// Restores a caret handed back by undo or redo.
    pub fn set_caret(&mut self, caret: Caret) {
        self.goal_col = None;
        self.cursor = caret.cursor;
        self.anchor = caret.anchor;
    }

    /// Re-anchors this view by content after its document reloaded from
    /// disk: `old` is the text as it stood, `new` the document's rope now.
    /// The scroll anchor follows the same mapping as the cursor, so an
    /// agent edit above the viewport doesn't shift what's on screen.
    pub fn remap_after_reload(&mut self, old: &Rope, new: &Rope, span: &ChangeSpan) {
        self.goal_col = None;
        self.cursor = grapheme::snap_to_boundary(new.slice(..), remap(self.cursor, span));
        // A selection straddling the change collapses to equal ends, which
        // selection() already reads as none.
        self.anchor = self
            .anchor
            .map(|a| grapheme::snap_to_boundary(new.slice(..), remap(a, span)));
        let line = self.scroll_line.min(old.len_lines() - 1);
        self.scroll_line = new.char_to_line(remap(old.line_to_char(line), span));
    }

    /// The range a pending edit replaces: the selection, or nothing at the
    /// cursor.
    fn edit_range(&self) -> Range<usize> {
        self.selection().unwrap_or(self.cursor..self.cursor)
    }

    fn apply_edit(&mut self, doc: &mut Document, range: Range<usize>, text: &str, kind: EditKind) {
        let caret = Caret {
            cursor: self.cursor,
            anchor: self.anchor,
        };
        self.goal_col = None;
        self.anchor = None;
        self.cursor = doc.edit(range, text, caret, kind);
    }

    /// Kind for an insertion: replacing a selection is its own undo step,
    /// plain typing joins the open run.
    fn insert_kind(range: &Range<usize>) -> EditKind {
        if range.is_empty() {
            EditKind::Insert
        } else {
            EditKind::Other
        }
    }

    pub fn insert_char(&mut self, doc: &mut Document, ch: char) {
        let range = self.edit_range();
        let kind = View::insert_kind(&range);
        let mut buf = [0; 4];
        self.apply_edit(doc, range, ch.encode_utf8(&mut buf), kind);
    }

    pub fn insert_tab(&mut self, doc: &mut Document) {
        let range = self.edit_range();
        let kind = View::insert_kind(&range);
        let text = doc.indent().as_str();
        self.apply_edit(doc, range, text, kind);
    }

    /// Enter: the detected terminator plus a copy of the current line's
    /// leading whitespace — only the part before the edit point, so Enter
    /// pressed inside the indentation doesn't deepen it. One edit, so a
    /// selection replace and the newline undo together.
    pub fn insert_newline(&mut self, doc: &mut Document) {
        let range = self.edit_range();
        let line_start = doc.line_start(doc.rope().char_to_line(range.start));
        let mut text = String::from(doc.line_ending().as_str());
        text.extend(
            doc.rope()
                .slice(line_start..range.start)
                .chars()
                .take_while(|&ch| ch == ' ' || ch == '\t'),
        );
        self.apply_edit(doc, range, &text, EditKind::Other);
    }

    /// Inserts pasted text over the selection (or at the cursor) as one
    /// edit, so any paste is a single undo step. Terminators are normalized
    /// first: terminals send `\r` for line breaks in a bracketed paste, and
    /// a bare `\r` is not a line break in this rope.
    pub fn paste(&mut self, doc: &mut Document, text: &str) {
        let range = self.edit_range();
        if text.is_empty() && range.is_empty() {
            return;
        }
        let text = normalize_eol(text, doc.line_ending().as_str());
        self.apply_edit(doc, range, &text, EditKind::Other);
    }

    pub fn backspace(&mut self, doc: &mut Document) {
        let (range, kind) = match self.selection() {
            Some(sel) => (sel, EditKind::Other),
            None => {
                let prev = grapheme::prev_grapheme_boundary(doc.rope().slice(..), self.cursor);
                if prev == self.cursor {
                    return;
                }
                (prev..self.cursor, EditKind::Backspace)
            }
        };
        self.apply_edit(doc, range, "", kind);
    }

    pub fn delete(&mut self, doc: &mut Document) {
        let (range, kind) = match self.selection() {
            Some(sel) => (sel, EditKind::Other),
            None => {
                let next = grapheme::next_grapheme_boundary(doc.rope().slice(..), self.cursor);
                if next == self.cursor {
                    return;
                }
                (self.cursor..next, EditKind::Delete)
            }
        };
        self.apply_edit(doc, range, "", kind);
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

/// The run of like-classed clusters containing `pos`, clamped to its line so
/// a whitespace run never swallows the terminator; at a line's end, the run
/// just before it. Empty on an empty line.
fn word_at(doc: &Document, pos: usize) -> Range<usize> {
    let line = doc.rope().char_to_line(pos);
    let line_start = doc.line_start(line);
    let line_end = doc.line_end(line);
    if line_start == line_end {
        return pos..pos;
    }
    let slice = doc.rope().slice(..);
    let seed = if pos < line_end {
        pos
    } else {
        grapheme::prev_grapheme_boundary(slice, line_end)
    };
    let class = classify(slice.char(seed));
    let mut start = seed;
    while start > line_start {
        let prev = grapheme::prev_grapheme_boundary(slice, start);
        if classify(slice.char(prev)) != class {
            break;
        }
        start = prev;
    }
    let mut end = seed;
    while end < line_end && classify(slice.char(end)) == class {
        end = grapheme::next_grapheme_boundary(slice, end);
    }
    start..end
}

/// Rewrites `\r\n`, bare `\r` and `\n` to `eol`, leaving everything else
/// untouched.
fn normalize_eol(text: &str, eol: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                chars.next_if_eq(&'\n');
                out.push_str(eol);
            }
            '\n' => out.push_str(eol),
            _ => out.push(ch),
        }
    }
    out
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
    fn move_to_line_clamps_and_lands_at_line_starts() {
        let doc = Document::from_str("ab\ncd\nef");
        let mut view = view_at(1);
        view.move_to_line(&doc, 2);
        assert_eq!(view.cursor, 3);
        view.move_to_line(&doc, 999);
        assert_eq!(view.cursor, 6);
        view.move_to_line(&doc, 0);
        assert_eq!(view.cursor, 0);
    }

    #[test]
    fn move_to_line_resets_the_goal_column() {
        let doc = Document::from_str("abcdef\nab\nabcdef");
        let mut view = view_at(4);
        view.move_down(&doc); // goal column 4, clamped to end of "ab"
        view.move_to_line(&doc, 2);
        view.move_down(&doc);
        assert_eq!(view.cursor, 10); // start of line 2, not old goal col 4
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
    fn insert_advances_past_the_typed_char() {
        let mut doc = Document::from_str("bc");
        let mut view = View::default();
        view.insert_char(&mut doc, 'a');
        assert_eq!(doc.rope().to_string(), "abc");
        assert_eq!(view.cursor, 1);
    }

    #[test]
    fn typing_over_a_selection_replaces_it() {
        let mut doc = Document::from_str("hello");
        let mut view = view_at(1).with_anchor(4); // "ell", cursor at its start
        view.insert_char(&mut doc, 'X');
        assert_eq!(doc.rope().to_string(), "hXo");
        assert_eq!(view.cursor, 2);
        assert_eq!(view.anchor, None);
    }

    #[test]
    fn backspace_and_delete_remove_whole_clusters() {
        let mut doc = Document::from_str("ae\u{301}b");
        let mut view = view_at(3);
        view.backspace(&mut doc); // e plus its accent
        assert_eq!(doc.rope().to_string(), "ab");
        assert_eq!(view.cursor, 1);
        view.delete(&mut doc);
        assert_eq!(doc.rope().to_string(), "a");
        assert_eq!(view.cursor, 1);
    }

    #[test]
    fn backspace_at_a_line_start_removes_the_whole_crlf() {
        let mut doc = Document::from_str("ab\r\ncd");
        let mut view = view_at(4);
        view.backspace(&mut doc);
        assert_eq!(doc.rope().to_string(), "abcd");
        assert_eq!(view.cursor, 2);
    }

    #[test]
    fn backspace_and_delete_eat_a_selection_whole() {
        let mut doc = Document::from_str("hello");
        let mut view = view_at(4).with_anchor(1);
        view.backspace(&mut doc);
        assert_eq!(doc.rope().to_string(), "ho");
        assert_eq!(view.cursor, 1);
        assert_eq!(view.anchor, None);
    }

    #[test]
    fn edits_at_the_document_edges_are_no_ops() {
        let mut doc = Document::from_str("a");
        let mut view = view_at(0);
        view.backspace(&mut doc);
        let mut view = view_at(1);
        view.delete(&mut doc);
        assert_eq!(doc.rope().to_string(), "a");
        assert!(!doc.dirty());
    }

    #[test]
    fn enter_copies_the_leading_whitespace() {
        let mut doc = Document::from_str("    foo");
        let mut view = view_at(7);
        view.insert_newline(&mut doc);
        assert_eq!(doc.rope().to_string(), "    foo\n    ");
        assert_eq!(view.cursor, 12);
    }

    #[test]
    fn enter_inside_the_indentation_copies_only_the_prefix() {
        let mut doc = Document::from_str("\t\tfoo");
        let mut view = view_at(1);
        view.insert_newline(&mut doc);
        assert_eq!(doc.rope().to_string(), "\t\n\t\tfoo");
        assert_eq!(view.cursor, 3);
    }

    #[test]
    fn enter_on_a_crlf_document_inserts_crlf() {
        let mut doc = Document::from_str("ab\r\ncd");
        let mut view = view_at(6);
        view.insert_newline(&mut doc);
        assert_eq!(doc.rope().to_string(), "ab\r\ncd\r\n");
        assert_eq!(view.cursor, 8);
    }

    #[test]
    fn enter_replaces_a_selection_then_indents_from_the_result() {
        let mut doc = Document::from_str("  ab\n  cd");
        let mut view = view_at(8).with_anchor(3); // "b\n  c"
        view.insert_newline(&mut doc);
        assert_eq!(doc.rope().to_string(), "  a\n  d");
        assert_eq!(view.cursor, 6);
    }

    #[test]
    fn tab_inserts_the_detected_indent() {
        let mut doc = Document::from_str("\tx\n");
        let mut view = View::default();
        view.insert_tab(&mut doc);
        assert_eq!(doc.rope().to_string(), "\t\tx\n");

        let mut doc = Document::from_str("  x\n  y\n");
        let mut view = View::default();
        view.insert_tab(&mut doc);
        assert_eq!(doc.rope().to_string(), "    x\n  y\n");

        let mut doc = Document::empty();
        let mut view = View::default();
        view.insert_tab(&mut doc);
        assert_eq!(doc.rope().to_string(), "    ");
    }

    #[test]
    fn edits_clear_the_goal_column() {
        let mut doc = Document::from_str("abcdef\nab\nabcdef");
        let mut view = view_at(4);
        view.move_down(&doc); // goal 4, clamped to end of "ab"
        view.insert_char(&mut doc, 'x');
        view.move_down(&doc); // new goal is col 3, not the stale 4
        assert_eq!(view.cursor, 14);
    }

    #[test]
    fn a_typing_burst_undoes_as_one_step_through_the_view() {
        let mut doc = Document::empty();
        let mut view = View::default();
        for ch in "hi!".chars() {
            view.insert_char(&mut doc, ch);
        }
        assert_eq!(doc.rope().to_string(), "hi!");
        view.set_caret(doc.undo().unwrap());
        assert_eq!(doc.rope().to_string(), "");
        assert_eq!(view.cursor, 0);
        view.set_caret(doc.redo().unwrap());
        assert_eq!(doc.rope().to_string(), "hi!");
        assert_eq!(view.cursor, 3);
    }

    #[test]
    fn paste_at_the_cursor_inserts() {
        let mut doc = Document::from_str("ad");
        let mut view = view_at(1);
        view.paste(&mut doc, "bc");
        assert_eq!(doc.rope().to_string(), "abcd");
        assert_eq!(view.cursor, 3);
    }

    #[test]
    fn paste_replaces_the_selection_as_one_undo_step() {
        let mut doc = Document::from_str("hello world");
        let mut view = view_at(5).with_anchor(0);
        view.paste(&mut doc, "goodbye\ncruel");
        assert_eq!(doc.rope().to_string(), "goodbye\ncruel world");
        assert_eq!(view.selection(), None);
        view.set_caret(doc.undo().unwrap());
        assert_eq!(doc.rope().to_string(), "hello world");
        assert_eq!(view.selection(), Some(0..5)); // undo revives the selection
        assert!(doc.undo().is_none());
    }

    #[test]
    fn paste_normalizes_line_endings_to_the_documents() {
        let mut doc = Document::from_str("a\nb\n");
        let mut view = View::default();
        view.paste(&mut doc, "x\r\ny\rz\n");
        assert_eq!(doc.rope().to_string(), "x\ny\nz\na\nb\n");

        let mut doc = Document::from_str("a\r\nb\r\n");
        let mut view = View::default();
        view.paste(&mut doc, "x\ny\r");
        assert_eq!(doc.rope().to_string(), "x\r\ny\r\na\r\nb\r\n");
    }

    #[test]
    fn an_empty_paste_leaves_no_undo_step() {
        let mut doc = Document::from_str("ab");
        let mut view = view_at(1);
        view.paste(&mut doc, "");
        assert!(doc.undo().is_none());
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

    #[test]
    fn remap_keeps_prefix_shifts_suffix_clamps_middle() {
        let span = ChangeSpan {
            prefix: 2,
            old_suffix_start: 5,
            new_suffix_start: 7,
        };
        assert_eq!(remap(0, &span), 0);
        assert_eq!(remap(2, &span), 2);
        assert_eq!(remap(3, &span), 2); // inside the change: clamps
        assert_eq!(remap(5, &span), 7);
        assert_eq!(remap(6, &span), 8);
    }

    #[test]
    fn remap_after_reload_re_anchors_the_cursor_by_content() {
        // Two lines inserted above: the cursor and viewport follow their
        // line's content down.
        let old = Rope::from_str("a\nb\nc\nd\n");
        let new = Rope::from_str("x\ny\na\nb\nc\nd\n");
        let span = ChangeSpan {
            prefix: 0,
            old_suffix_start: 0,
            new_suffix_start: 4,
        };
        let mut view = View::test_at(4, 2, 0); // cursor on "c", "c" at top
        view.remap_after_reload(&old, &new, &span);
        assert_eq!(new.char_to_line(view.cursor), 4); // still on "c"
        assert_eq!(view.scroll_line, 4);
    }

    #[test]
    fn remap_after_reload_snaps_to_a_cluster_boundary() {
        // The reload appended a combining mark that merges with the char
        // before the cursor; the cursor may not stay inside the cluster.
        let old = Rope::from_str("ae");
        let new = Rope::from_str("ae\u{301}");
        let span = ChangeSpan {
            prefix: 2,
            old_suffix_start: 2,
            new_suffix_start: 3,
        };
        let mut view = View::test_at(2, 0, 0);
        view.remap_after_reload(&old, &new, &span);
        assert_eq!(view.cursor, 3);
    }

    #[test]
    fn selection_straddling_the_change_dissolves() {
        let old = Rope::from_str("aXYZb");
        let new = Rope::from_str("ab");
        let span = ChangeSpan {
            prefix: 1,
            old_suffix_start: 4,
            new_suffix_start: 1,
        };
        let mut view = View::test_at(3, 0, 0).with_anchor(2);
        view.remap_after_reload(&old, &new, &span);
        assert_eq!(view.selection(), None);
        assert!(view.cursor <= new.len_chars());
    }

    #[test]
    fn click_maps_screen_cell_through_scroll_and_gutter() {
        let doc = Document::from_str("zero\none\ntwo\nthree\nfour\n");
        let mut view = View::test_at(0, 2, 1);
        // Row 2 of the text area is line 3; x 5 past a 3-wide gutter with
        // the leftmost column scrolled off is visual column 3.
        view.click(&doc, 3, 10, 5, 2, false);
        assert_eq!(view.cursor, 16); // "three" starts at 13
    }

    #[test]
    fn click_below_the_last_line_lands_on_it() {
        let doc = Document::from_str("a\nb");
        let mut view = View::default();
        view.click(&doc, 2, 10, 2, 9, false);
        assert_eq!(view.cursor, 2);
    }

    #[test]
    fn click_past_line_end_clamps_to_line_end() {
        let doc = Document::from_str("ab\ncdef");
        let mut view = View::default();
        view.click(&doc, 2, 10, 40, 1, false);
        assert_eq!(view.cursor, 2);
    }

    #[test]
    fn click_in_the_gutter_lands_at_the_leftmost_visible_column() {
        let doc = Document::from_str("abcdef");
        let mut view = View::test_at(0, 0, 2);
        view.click(&doc, 3, 10, 1, 1, false);
        assert_eq!(view.cursor, 2);
    }

    #[test]
    fn click_on_a_wide_cluster_or_tab_lands_on_its_start() {
        let doc = Document::from_str("日本\n\tx");
        let mut view = View::default();
        view.click(&doc, 2, 10, 3, 1, false); // second column of 日
        assert_eq!(view.cursor, 0);
        view.click(&doc, 2, 10, 7, 2, false); // inside the tab's 8 columns
        assert_eq!(view.cursor, 3);
    }

    #[test]
    fn shift_click_extends_and_plain_click_dissolves_the_selection() {
        let doc = Document::from_str("hello");
        let mut view = view_at(1);
        view.click(&doc, 2, 10, 6, 1, true);
        assert_eq!(view.selection(), Some(1..4));
        view.click(&doc, 2, 10, 2, 1, false);
        assert_eq!(view.selection(), None);
        assert_eq!(view.cursor, 0);
    }

    #[test]
    fn click_resets_the_goal_column() {
        let doc = Document::from_str("abcd\nx\nabcd");
        let mut view = view_at(3);
        view.move_down(&doc); // goal column 3, clamped to "x"'s end
        assert_eq!(view.cursor, 6);
        view.click(&doc, 2, 10, 2, 2, false); // start of "x"
        view.move_down(&doc);
        assert_eq!(view.cursor, 7); // aims for the clicked column, not 3
    }

    #[test]
    fn drag_keeps_the_anchor_and_follows_the_pointer() {
        let doc = Document::from_str("hello\nworld");
        let mut view = View::default();
        view.click(&doc, 2, 10, 3, 1, false);
        view.drag(&doc, 2, 10, 5, 2);
        assert_eq!(view.selection(), Some(1..9));
        view.drag(&doc, 2, 10, 4, 1);
        assert_eq!(view.selection(), Some(1..2));
    }

    #[test]
    fn drag_at_the_edges_overshoots_one_line() {
        let doc = Document::from_str("a\nb\nc\nd\ne\nf");
        let mut view = View::test_at(4, 2, 0); // lines 2..=3 visible
        view.drag(&doc, 2, 2, 2, 0);
        assert_eq!(view.cursor, 2); // one line above the viewport
        view.scroll_to_cursor(&doc, 10, 2);
        assert_eq!(view.scroll_line, 1);
        view.drag(&doc, 2, 2, 2, 3);
        assert_eq!(view.cursor, 6); // one line below it
        view.scroll_to_cursor(&doc, 10, 2);
        assert_eq!(view.scroll_line, 2);
    }

    #[test]
    fn select_word_takes_word_punct_and_whitespace_runs() {
        let doc = Document::from_str("foo_bar, baz");
        for (cursor, range) in [(2, 0..7), (7, 7..8), (8, 8..9), (10, 9..12)] {
            let mut view = view_at(cursor);
            view.select_word(&doc);
            assert_eq!(view.selection(), Some(range));
        }
    }

    #[test]
    fn select_word_at_line_end_takes_the_word_before() {
        let doc = Document::from_str("foo bar\nx");
        let mut view = view_at(7);
        view.select_word(&doc);
        assert_eq!(view.selection(), Some(4..7));
    }

    #[test]
    fn select_word_on_an_empty_line_selects_nothing() {
        let doc = Document::from_str("a\n\nb");
        let mut view = view_at(2);
        view.select_word(&doc);
        assert_eq!(view.selection(), None);
    }

    #[test]
    fn word_selection_stays_on_its_line() {
        let doc = Document::from_str("a \n b");
        let mut view = view_at(1);
        view.select_word(&doc);
        assert_eq!(view.selection(), Some(1..2));
    }

    #[test]
    fn scroll_wheel_moves_viewport_only_and_clamps() {
        let doc = Document::from_str("a\nb\nc\nd\ne");
        let mut view = View::default();
        view.scroll_wheel(&doc, 3);
        assert_eq!(view.scroll_line, 3);
        assert_eq!(view.cursor, 0);
        view.scroll_wheel(&doc, 100);
        assert_eq!(view.scroll_line, 4);
        view.scroll_wheel(&doc, -100);
        assert_eq!(view.scroll_line, 0);
    }

    #[test]
    fn typing_replaces_a_mouse_selection() {
        let mut doc = Document::from_str("hello");
        let mut view = View::default();
        view.click(&doc, 2, 10, 3, 1, false);
        view.drag(&doc, 2, 10, 6, 1);
        view.insert_char(&mut doc, 'x');
        assert_eq!(doc.rope().to_string(), "hxo");
        assert_eq!(view.selection(), None);
    }
}
