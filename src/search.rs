//! Incremental smart-case search and replace, driven from the status-line
//! prompt. Owns the query, the match set, and the caret to restore on
//! cancel.

use std::fmt::Write as _;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ropey::Rope;

use crate::doc::{Caret, Document, EditKind};
use crate::grapheme;
use crate::view::View;

const FIND_LABEL: &str = "find: ";
const REPLACE_LABEL: &str = "replace with: ";

pub enum Outcome {
    Pending,
    /// Enter in the find field: close, the cursor stays where it is.
    Accept,
    /// Esc: close, the origin caret and scroll are restored.
    Cancel,
    /// Alt+A replaced this many matches: close, report the count.
    ReplacedAll(usize),
}

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Find,
    Replace,
}

/// Borrowed view of the match set for the draw pass.
pub struct Highlights<'a> {
    /// Ascending char indices of match starts.
    pub starts: &'a [usize],
    /// Every match's char length.
    pub len: usize,
    /// Index into `starts` of the match the cursor sits on.
    pub current: Option<usize>,
}

pub struct SearchPrompt {
    query: String,
    replacement: String,
    /// Which field Tab has the keys editing.
    focus: Focus,
    /// Where the search began: what Esc restores, and where query edits
    /// re-anchor from — so shrinking the query jumps back naturally.
    origin: Caret,
    origin_scroll: (usize, usize),
    /// Ascending char indices of non-overlapping match starts; reused
    /// across recomputes.
    matches: Vec<usize>,
    /// The query's char count, which is every match's length too (the case
    /// fold is one-to-one).
    qlen: usize,
    /// Index into `matches` of the match the cursor sits on.
    current: Option<usize>,
}

impl SearchPrompt {
    pub fn new(view: &View) -> SearchPrompt {
        SearchPrompt {
            query: String::new(),
            replacement: String::new(),
            focus: Focus::Find,
            origin: Caret {
                cursor: view.cursor,
                anchor: view.anchor,
            },
            origin_scroll: (view.scroll_line, view.scroll_col),
            matches: Vec::new(),
            qlen: 0,
            current: None,
        }
    }

    /// Feeds one keypress; navigation and cancel move the view, the replace
    /// keys edit the document. Unrecognized keys are ignored so a stray
    /// chord can't dismiss the prompt.
    pub fn key(&mut self, key: &KeyEvent, doc: &mut Document, view: &mut View) -> Outcome {
        match key.code {
            KeyCode::Enter => match self.focus {
                Focus::Find => return Outcome::Accept,
                Focus::Replace => self.replace_one(doc, view),
            },
            KeyCode::Esc => {
                self.restore_origin(doc, view);
                return Outcome::Cancel;
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Find => Focus::Replace,
                    Focus::Replace => Focus::Find,
                };
            }
            KeyCode::Up => self.step(doc, view, self.matches.len().wrapping_sub(1)),
            KeyCode::Down => self.step(doc, view, 1),
            KeyCode::Char('a' | 'A') if key.modifiers.contains(KeyModifiers::ALT) => {
                let n = self.replace_all(doc, view);
                if n > 0 {
                    return Outcome::ReplacedAll(n);
                }
            }
            KeyCode::Backspace => {
                let field = match self.focus {
                    Focus::Find => &mut self.query,
                    Focus::Replace => &mut self.replacement,
                };
                if field.pop().is_some() && self.focus == Focus::Find {
                    self.research(doc, view);
                }
            }
            // Ctrl- and Alt-modified characters are chords, not input.
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                match self.focus {
                    Focus::Find => {
                        self.query.push(ch);
                        self.research(doc, view);
                    }
                    Focus::Replace => self.replacement.push(ch),
                }
            }
            _ => {}
        }
        Outcome::Pending
    }

    /// Writes the focused field, the match counter, and the key hints.
    pub fn render(&self, notice: &mut String) {
        notice.clear();
        match self.focus {
            Focus::Find => {
                notice.push_str(FIND_LABEL);
                notice.push_str(&self.query);
            }
            Focus::Replace => {
                notice.push_str(REPLACE_LABEL);
                notice.push_str(&self.replacement);
            }
        }
        if let Some(i) = self.current {
            let _ = write!(notice, " · {}/{}", i + 1, self.matches.len());
        } else if !self.query.is_empty() {
            notice.push_str(" · no matches");
        }
        notice.push_str(match self.focus {
            Focus::Find => " · ↑↓ next/prev · tab replace · esc",
            Focus::Replace => " · enter one · alt+a all · tab find · esc",
        });
    }

    /// Chars of the rendered notice before the caret: the label plus the
    /// focused field, with the counter and hints trailing after.
    pub fn caret_chars(&self) -> usize {
        let (label, field) = match self.focus {
            Focus::Find => (FIND_LABEL, &self.query),
            Focus::Replace => (REPLACE_LABEL, &self.replacement),
        };
        label.chars().count() + field.chars().count()
    }

    pub fn highlights(&self) -> Highlights<'_> {
        Highlights {
            starts: &self.matches,
            len: self.qlen,
            current: self.current,
        }
    }

    /// Re-derives the match set after the document changed underneath the
    /// prompt (an external reload). The cursor stays; `current` re-picks
    /// the first match at or after it.
    pub fn refresh(&mut self, doc: &Document, view: &View) {
        find_matches(doc.rope(), &self.query, &mut self.matches);
        self.qlen = self.query.chars().count();
        self.current = (!self.matches.is_empty())
            .then(|| self.matches.partition_point(|&s| s < view.cursor) % self.matches.len());
    }

    /// The query changed: recompute matches and jump to the first one at or
    /// after the origin. A miss leaves the cursor where it stands.
    fn research(&mut self, doc: &Document, view: &mut View) {
        find_matches(doc.rope(), &self.query, &mut self.matches);
        self.qlen = self.query.chars().count();
        if self.matches.is_empty() {
            self.current = None;
            return;
        }
        let from = self.origin.cursor;
        self.current = Some(self.matches.partition_point(|&s| s < from) % self.matches.len());
        self.goto_current(doc, view);
    }

    /// Moves `current` by `delta` matches, wrapping.
    fn step(&mut self, doc: &Document, view: &mut View, delta: usize) {
        let Some(i) = self.current else {
            return;
        };
        self.current = Some((i + delta) % self.matches.len());
        self.goto_current(doc, view);
    }

    /// Replaces the current match (its own undo step) and advances to the
    /// first match at or after the inserted text's end — so a replacement
    /// containing the query can't revisit itself — wrapping past the last.
    fn replace_one(&mut self, doc: &mut Document, view: &mut View) {
        let Some(i) = self.current else {
            return;
        };
        let start = self.matches[i];
        let caret = Caret {
            cursor: view.cursor,
            anchor: view.anchor,
        };
        let after = doc.edit(
            start..start + self.qlen,
            &self.replacement,
            caret,
            EditKind::Other,
        );
        view.set_caret(Caret {
            cursor: after,
            anchor: None,
        });
        find_matches(doc.rope(), &self.query, &mut self.matches);
        if self.matches.is_empty() {
            self.current = None;
            return;
        }
        self.current = Some(self.matches.partition_point(|&s| s < after) % self.matches.len());
        self.goto_current(doc, view);
    }

    /// Replaces every match with one `edit` spanning first to last — one
    /// undo step, no per-match history — and returns the count. The cursor
    /// lands where the current match was, remapped through the edit.
    fn replace_all(&mut self, doc: &mut Document, view: &mut View) -> usize {
        let n = self.matches.len();
        if n == 0 {
            return 0;
        }
        let rlen = self.replacement.chars().count();
        let first = self.matches[0];
        let last_end = self.matches[n - 1] + self.qlen;
        let mut text = String::new();
        let mut from = first;
        for &s in &self.matches {
            text.extend(doc.rope().slice(from..s).chars());
            text.push_str(&self.replacement);
            from = s + self.qlen;
        }
        let caret = Caret {
            cursor: view.cursor,
            anchor: view.anchor,
        };
        doc.edit(first..last_end, &text, caret, EditKind::Other);
        // Non-overlapping ascending matches make matches[k] ≥ k·qlen, so
        // the remap can't underflow.
        let k = self.current.unwrap_or(0);
        let target = self.matches[k] - k * self.qlen + k * rlen;
        view.set_caret(Caret {
            cursor: grapheme::snap_to_boundary(doc.rope().slice(..), target),
            anchor: None,
        });
        self.matches.clear();
        self.current = None;
        n
    }

    fn goto_current(&self, doc: &Document, view: &mut View) {
        if let Some(i) = self.current {
            view.set_caret(Caret {
                cursor: grapheme::snap_to_boundary(doc.rope().slice(..), self.matches[i]),
                anchor: None,
            });
        }
    }

    /// Puts the caret and scroll back where Ctrl+F found them, clamped in
    /// case replacements shortened the document.
    fn restore_origin(&self, doc: &Document, view: &mut View) {
        let slice = doc.rope().slice(..);
        let clamp = |pos: usize| grapheme::snap_to_boundary(slice, pos.min(slice.len_chars()));
        view.set_caret(Caret {
            cursor: clamp(self.origin.cursor),
            anchor: self.origin.anchor.map(clamp),
        });
        view.scroll_line = self.origin_scroll.0;
        view.scroll_col = self.origin_scroll.1;
    }
}

/// One-to-one case fold: `Ф`→`ф`, but `ß` stays (its lowercase expands to
/// two chars, and equal match and query lengths are load-bearing).
fn simple_fold(ch: char) -> char {
    let mut lower = ch.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(folded), None) => folded,
        _ => ch,
    }
}

/// Collects the char indices of every non-overlapping match, ascending.
/// Smart case: an all-lowercase query matches case-insensitively, one
/// uppercase char makes it exact.
fn find_matches(rope: &Rope, query: &str, out: &mut Vec<usize>) {
    out.clear();
    if query.is_empty() {
        return;
    }
    let exact = query.chars().any(char::is_uppercase);
    let fold = |ch: char| if exact { ch } else { simple_fold(ch) };
    let qlen = query.chars().count();
    let mut rest = query.chars();
    let first = fold(rest.next().unwrap());
    let rest = rest.as_str();

    let len = rope.len_chars();
    let mut iter = rope.chars();
    let mut pos = 0;
    while pos + qlen <= len {
        // Cloning the chunk cursor is cheap; the first-char gate keeps the
        // clone-and-verify off most positions.
        let mut probe = iter.clone();
        let hit = fold(probe.next().unwrap()) == first
            && rest
                .chars()
                .all(|qc| probe.next().is_some_and(|dc| fold(dc) == fold(qc)));
        let advance = if hit {
            out.push(pos);
            qlen
        } else {
            1
        };
        pos += advance;
        for _ in 0..advance {
            iter.next();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_query(prompt: &mut SearchPrompt, doc: &mut Document, view: &mut View, text: &str) {
        for ch in text.chars() {
            prompt.key(&press(KeyCode::Char(ch)), doc, view);
        }
    }

    fn matches_of(text: &str, query: &str) -> Vec<usize> {
        let mut out = Vec::new();
        find_matches(&Rope::from_str(text), query, &mut out);
        out
    }

    #[test]
    fn lowercase_query_matches_any_case() {
        assert_eq!(matches_of("Foo foo FOO", "foo"), vec![0, 4, 8]);
        assert_eq!(matches_of("Фыва фыва", "фыва"), vec![0, 5]);
    }

    #[test]
    fn uppercase_in_the_query_makes_it_exact() {
        assert_eq!(matches_of("Foo foo FOO", "Foo"), vec![0]);
    }

    #[test]
    fn positions_are_char_indices() {
        assert_eq!(matches_of("日本語 ab 日本語", "日本語"), vec![0, 7]);
    }

    #[test]
    fn matches_do_not_overlap() {
        assert_eq!(matches_of("aaa", "aa"), vec![0]);
        assert_eq!(matches_of("aaaa", "aa"), vec![0, 2]);
    }

    #[test]
    fn empty_query_and_match_at_document_end() {
        assert_eq!(matches_of("abc", ""), Vec::<usize>::new());
        assert_eq!(matches_of("abc", "bc"), vec![1]);
        assert_eq!(matches_of("abc", "abcd"), Vec::<usize>::new());
    }

    #[test]
    fn sharp_s_does_not_cross_match_ss() {
        assert_eq!(matches_of("straße", "strasse"), Vec::<usize>::new());
        assert_eq!(matches_of("straße", "straße"), vec![0]);
    }

    #[test]
    fn typing_jumps_to_the_first_match_after_the_origin() {
        let mut doc = Document::from_str("ab ab ab");
        let mut view = View::test_at(4, 0, 0); // inside the second "ab"
        let mut prompt = SearchPrompt::new(&view);
        type_query(&mut prompt, &mut doc, &mut view, "ab");
        assert_eq!(view.cursor, 6); // third "ab": first at/after origin 4
        assert_eq!(prompt.current, Some(2));
    }

    #[test]
    fn extending_the_query_re_anchors_from_the_origin() {
        let mut doc = Document::from_str("ax ay ax");
        let mut view = View::test_at(0, 0, 0);
        let mut prompt = SearchPrompt::new(&view);
        type_query(&mut prompt, &mut doc, &mut view, "ay");
        assert_eq!(view.cursor, 3);
        // Backspace to "a": jumps back to the first match, not onward.
        prompt.key(&press(KeyCode::Backspace), &mut doc, &mut view);
        assert_eq!(view.cursor, 0);
    }

    #[test]
    fn navigation_wraps_both_ways() {
        let mut doc = Document::from_str("ab ab ab");
        let mut view = View::test_at(0, 0, 0);
        let mut prompt = SearchPrompt::new(&view);
        type_query(&mut prompt, &mut doc, &mut view, "ab");
        assert_eq!(view.cursor, 0);
        prompt.key(&press(KeyCode::Up), &mut doc, &mut view);
        assert_eq!(view.cursor, 6); // wrapped to the last match
        prompt.key(&press(KeyCode::Down), &mut doc, &mut view);
        assert_eq!(view.cursor, 0); // and back around
        prompt.key(&press(KeyCode::Down), &mut doc, &mut view);
        assert_eq!(view.cursor, 3);
    }

    #[test]
    fn a_miss_leaves_the_cursor_and_reports_no_matches() {
        let mut doc = Document::from_str("ab ab");
        let mut view = View::test_at(3, 0, 0);
        let mut prompt = SearchPrompt::new(&view);
        type_query(&mut prompt, &mut doc, &mut view, "ab");
        assert_eq!(view.cursor, 3);
        type_query(&mut prompt, &mut doc, &mut view, "z");
        assert_eq!(view.cursor, 3); // "abz" misses: no jump anywhere
        let mut notice = String::new();
        prompt.render(&mut notice);
        assert_eq!(
            notice,
            "find: abz · no matches · ↑↓ next/prev · tab replace · esc"
        );
    }

    #[test]
    fn esc_restores_caret_and_scroll_enter_accepts() {
        let mut doc = Document::from_str("start\nab ab ab abable");
        let mut view = View::test_at(2, 1, 3);
        view.anchor = Some(4);
        let mut prompt = SearchPrompt::new(&view);
        view.anchor = None; // as the Ctrl+F handler does
        type_query(&mut prompt, &mut doc, &mut view, "able");
        assert_eq!(view.cursor, 17);
        assert!(matches!(
            prompt.key(&press(KeyCode::Esc), &mut doc, &mut view),
            Outcome::Cancel
        ));
        assert_eq!(view.cursor, 2);
        assert_eq!(view.anchor, Some(4));
        assert_eq!((view.scroll_line, view.scroll_col), (1, 3));

        let mut view = View::test_at(0, 0, 0);
        let mut prompt = SearchPrompt::new(&view);
        type_query(&mut prompt, &mut doc, &mut view, "able");
        assert!(matches!(
            prompt.key(&press(KeyCode::Enter), &mut doc, &mut view),
            Outcome::Accept
        ));
        assert_eq!(view.cursor, 17); // accepted: the cursor stays put
    }

    #[test]
    fn render_shows_the_counter_and_caret_chars_track_the_query() {
        let mut doc = Document::from_str("ab ab ab");
        let mut view = View::test_at(0, 0, 0);
        let mut prompt = SearchPrompt::new(&view);
        let mut notice = String::new();
        prompt.render(&mut notice);
        assert_eq!(notice, "find:  · ↑↓ next/prev · tab replace · esc");
        type_query(&mut prompt, &mut doc, &mut view, "ab");
        prompt.key(&press(KeyCode::Down), &mut doc, &mut view);
        prompt.render(&mut notice);
        assert_eq!(notice, "find: ab · 2/3 · ↑↓ next/prev · tab replace · esc");
        assert_eq!(prompt.caret_chars(), 8);
    }

    #[test]
    fn refresh_recomputes_matches_without_moving_the_cursor() {
        let mut doc = Document::from_str("ab ab");
        let mut view = View::test_at(0, 0, 0);
        let mut prompt = SearchPrompt::new(&view);
        type_query(&mut prompt, &mut doc, &mut view, "ab");
        assert_eq!(prompt.matches, vec![0, 3]);

        // The document changed underneath (an external reload).
        let doc = Document::from_str("ab zz ab ab");
        view.cursor = 4;
        prompt.refresh(&doc, &view);
        assert_eq!(prompt.matches, vec![0, 6, 9]);
        assert_eq!(prompt.current, Some(1)); // first match at/after cursor 4
        assert_eq!(view.cursor, 4);
    }

    fn alt(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::ALT)
    }

    #[test]
    fn tab_routes_typing_to_the_replace_field_and_back() {
        let mut doc = Document::from_str("ab ab");
        let mut view = View::test_at(0, 0, 0);
        let mut prompt = SearchPrompt::new(&view);
        type_query(&mut prompt, &mut doc, &mut view, "ab");
        prompt.key(&press(KeyCode::Tab), &mut doc, &mut view);
        type_query(&mut prompt, &mut doc, &mut view, "xy");
        assert_eq!(prompt.query, "ab");
        assert_eq!(prompt.replacement, "xy");
        let mut notice = String::new();
        prompt.render(&mut notice);
        assert_eq!(
            notice,
            "replace with: xy · 1/2 · enter one · alt+a all · tab find · esc"
        );
        assert_eq!(prompt.caret_chars(), 16);

        prompt.key(&press(KeyCode::Tab), &mut doc, &mut view);
        type_query(&mut prompt, &mut doc, &mut view, "c");
        assert_eq!(prompt.query, "abc");
        assert_eq!(prompt.replacement, "xy");
    }

    #[test]
    fn replace_one_replaces_advances_and_wraps() {
        let mut doc = Document::from_str("ab cd ab ab");
        let mut view = View::test_at(0, 0, 0);
        let mut prompt = SearchPrompt::new(&view);
        type_query(&mut prompt, &mut doc, &mut view, "ab");
        prompt.key(&press(KeyCode::Tab), &mut doc, &mut view);
        type_query(&mut prompt, &mut doc, &mut view, "xyz");

        prompt.key(&press(KeyCode::Enter), &mut doc, &mut view);
        assert_eq!(doc.rope().to_string(), "xyz cd ab ab");
        assert_eq!(view.cursor, 7); // advanced to the next match

        // Skip to the last match, replace it: the advance wraps around.
        prompt.key(&press(KeyCode::Up), &mut doc, &mut view);
        prompt.key(&press(KeyCode::Enter), &mut doc, &mut view);
        assert_eq!(doc.rope().to_string(), "xyz cd ab xyz");
        assert_eq!(view.cursor, 7);

        // Each replace is its own undo step.
        assert!(doc.undo().is_some());
        assert_eq!(doc.rope().to_string(), "xyz cd ab ab");
        assert!(doc.undo().is_some());
        assert_eq!(doc.rope().to_string(), "ab cd ab ab");
    }

    #[test]
    fn replacing_the_last_match_reports_no_matches_and_stays_open() {
        let mut doc = Document::from_str("ab");
        let mut view = View::test_at(0, 0, 0);
        let mut prompt = SearchPrompt::new(&view);
        type_query(&mut prompt, &mut doc, &mut view, "ab");
        prompt.key(&press(KeyCode::Tab), &mut doc, &mut view);
        // An empty replacement deletes.
        assert!(matches!(
            prompt.key(&press(KeyCode::Enter), &mut doc, &mut view),
            Outcome::Pending
        ));
        assert_eq!(doc.rope().to_string(), "");
        let mut notice = String::new();
        prompt.render(&mut notice);
        assert_eq!(
            notice,
            "replace with:  · no matches · enter one · alt+a all · tab find · esc"
        );
    }

    #[test]
    fn a_replacement_containing_the_query_cannot_loop() {
        let mut doc = Document::from_str("a b");
        let mut view = View::test_at(0, 0, 0);
        let mut prompt = SearchPrompt::new(&view);
        type_query(&mut prompt, &mut doc, &mut view, "a");
        prompt.key(&press(KeyCode::Tab), &mut doc, &mut view);
        type_query(&mut prompt, &mut doc, &mut view, "aa");
        prompt.key(&press(KeyCode::Enter), &mut doc, &mut view);
        assert_eq!(doc.rope().to_string(), "aa b");
        // The freshly inserted "aa" is behind the advance point: current
        // wrapped back to it rather than running away forward.
        assert_eq!(view.cursor, 0);
        prompt.key(&press(KeyCode::Enter), &mut doc, &mut view);
        assert_eq!(doc.rope().to_string(), "aaa b");
    }

    #[test]
    fn replace_all_is_one_undo_step_and_reports_the_count() {
        let mut doc = Document::from_str("ab x ab y ab");
        let mut view = View::test_at(0, 0, 0);
        let mut prompt = SearchPrompt::new(&view);
        type_query(&mut prompt, &mut doc, &mut view, "ab");
        prompt.key(&press(KeyCode::Down), &mut doc, &mut view); // current: 2nd
        prompt.key(&press(KeyCode::Tab), &mut doc, &mut view);
        type_query(&mut prompt, &mut doc, &mut view, "long");
        assert!(matches!(
            prompt.key(&alt('a'), &mut doc, &mut view),
            Outcome::ReplacedAll(3)
        ));
        assert_eq!(doc.rope().to_string(), "long x long y long");
        assert_eq!(view.cursor, 7); // where the current match was, remapped

        assert!(doc.undo().is_some());
        assert_eq!(doc.rope().to_string(), "ab x ab y ab");
        assert!(doc.undo().is_none()); // one step reverted everything
    }

    #[test]
    fn replace_all_with_no_matches_is_a_quiet_no_op() {
        let mut doc = Document::from_str("cd");
        let mut view = View::test_at(1, 0, 0);
        let mut prompt = SearchPrompt::new(&view);
        type_query(&mut prompt, &mut doc, &mut view, "ab");
        assert!(matches!(
            prompt.key(&alt('a'), &mut doc, &mut view),
            Outcome::Pending
        ));
        assert_eq!(doc.rope().to_string(), "cd");
        assert_eq!(view.cursor, 1);
    }

    #[test]
    fn esc_after_replacements_clamps_the_restored_caret() {
        let mut doc = Document::from_str("xx ab");
        let mut view = View::test_at(5, 0, 0); // at the document's end
        let mut prompt = SearchPrompt::new(&view);
        type_query(&mut prompt, &mut doc, &mut view, "ab");
        prompt.key(&press(KeyCode::Tab), &mut doc, &mut view);
        prompt.key(&press(KeyCode::Enter), &mut doc, &mut view); // delete "ab"
        prompt.key(&press(KeyCode::Esc), &mut doc, &mut view);
        assert_eq!(view.cursor, 3); // origin 5 clamped into the shorter doc
    }

    #[test]
    fn stray_chords_do_not_edit_the_query() {
        let mut doc = Document::from_str("ab");
        let mut view = View::test_at(0, 0, 0);
        let mut prompt = SearchPrompt::new(&view);
        prompt.key(
            &KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &mut doc,
            &mut view,
        );
        prompt.key(
            &KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT),
            &mut doc,
            &mut view,
        );
        assert!(prompt.query.is_empty());
    }
}
