use std::borrow::Cow;
use std::fs::File;
use std::io::{self, BufReader, ErrorKind};
use std::ops::Range;
use std::path::PathBuf;

use ropey::Rope;

use crate::grapheme;

/// Lines examined by convention detection: plenty for any real file, a
/// bound for absurd ones.
const DETECT_LINE_CAP: usize = 10_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineEnding {
    Lf,
    Crlf,
}

impl LineEnding {
    pub fn as_str(self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::Crlf => "\r\n",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IndentStyle {
    Tabs,
    Spaces(u8),
}

impl IndentStyle {
    /// What the Tab key inserts.
    pub fn as_str(self) -> &'static str {
        match self {
            IndentStyle::Tabs => "\t",
            IndentStyle::Spaces(n) => &"        "[..usize::from(n)],
        }
    }
}

/// The majority terminator among the file's lines; ties and empty files
/// fall back to LF.
fn detect_line_ending(rope: &Rope) -> LineEnding {
    let mut crlf = 0;
    let mut lf = 0;
    // Every line but the last ends in '\n'; a '\r' before it makes CRLF.
    for line in 0..rope.len_lines().saturating_sub(1).min(DETECT_LINE_CAP) {
        let end = rope.line_to_char(line + 1);
        if end >= 2 && rope.char(end - 2) == '\r' {
            crlf += 1;
        } else {
            lf += 1;
        }
    }
    if crlf > lf {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    }
}

/// Tabs when more lines open with a tab than with spaces; otherwise spaces,
/// their width the most common growth in leading spaces from one
/// space-indented line to the next (how deeper nesting reveals the step).
/// Undetectable width falls back to 4.
fn detect_indent(rope: &Rope) -> IndentStyle {
    let mut tabs = 0;
    let mut spaces = 0;
    let mut steps = [0usize; 9];
    let mut prev_width = 0;
    for line in rope.lines().take(rope.len_lines().min(DETECT_LINE_CAP)) {
        match line.chars().next() {
            Some('\t') => tabs += 1,
            Some(' ') => spaces += 1,
            _ => continue,
        }
        let width = line.chars().take_while(|&ch| ch == ' ').take(64).count();
        if width > 0 {
            let step = width.saturating_sub(prev_width);
            if (1..=8).contains(&step) {
                steps[step] += 1;
            }
            prev_width = width;
        }
    }
    if tabs > spaces {
        return IndentStyle::Tabs;
    }
    // Smallest step wins ties: a two-level jump must not read as one step.
    let mut best = (4, 0);
    for (step, &count) in steps.iter().enumerate().skip(1) {
        if count > best.1 {
            best = (step, count);
        }
    }
    IndentStyle::Spaces(best.0 as u8)
}

/// A cursor plus the selection anchor: what undo must restore for the
/// edit's site to look untouched.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Caret {
    pub cursor: usize,
    pub anchor: Option<usize>,
}

/// The user gesture behind an edit — the hint coalescing uses to fold a
/// burst of typing into one undo step. `Other` never coalesces.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    Insert,
    Backspace,
    Delete,
    Other,
}

/// One atomic replacement in char indices: `deleted` stood at `at`,
/// `inserted` stands there now. Replays in either direction.
struct Edit {
    at: usize,
    deleted: String,
    inserted: String,
}

impl Edit {
    /// Folds `next` into this edit when it continues the same run — typing
    /// extending the insertion's tail, Backspace eating leftward from its
    /// start, Delete eating rightward at its position.
    fn try_merge(&mut self, next: &Edit, kind: EditKind) -> bool {
        match kind {
            EditKind::Insert if next.deleted.is_empty() => {
                if next.at != self.at + self.inserted.chars().count() {
                    return false;
                }
                self.inserted.push_str(&next.inserted);
                true
            }
            EditKind::Backspace if next.inserted.is_empty() => {
                if next.at + next.deleted.chars().count() != self.at {
                    return false;
                }
                self.at = next.at;
                self.deleted.insert_str(0, &next.deleted);
                true
            }
            EditKind::Delete if next.inserted.is_empty() => {
                if next.at != self.at {
                    return false;
                }
                self.deleted.push_str(&next.deleted);
                true
            }
            _ => false,
        }
    }
}

/// The unit of undo: one edit plus the carets to restore on either side.
struct EditGroup {
    edit: Edit,
    caret_before: Caret,
    cursor_after: usize,
}

#[derive(Default)]
struct History {
    groups: Vec<EditGroup>,
    /// `groups[..index]` are applied to the rope.
    index: usize,
    /// The kind that may still extend `groups[index - 1]`; `None` when the
    /// run is closed.
    open_kind: Option<EditKind>,
    /// History position at the last save; 0 is the loaded state. Saving
    /// (issue #4) must invalidate this when the saved position is truncated
    /// out of an abandoned redo branch.
    saved_index: usize,
}

impl History {
    fn record(&mut self, edit: Edit, caret_before: Caret, cursor_after: usize, kind: EditKind) {
        // A selection replace never coalesces, and neither does anything
        // when a redo branch is pending or the run was broken.
        if self.index == self.groups.len()
            && self.open_kind == Some(kind)
            && caret_before.anchor.is_none()
            && let Some(last) = self.groups.last_mut()
            && last.edit.try_merge(&edit, kind)
        {
            last.cursor_after = cursor_after;
            return;
        }
        self.groups.truncate(self.index);
        self.groups.push(EditGroup {
            edit,
            caret_before,
            cursor_after,
        });
        self.index = self.groups.len();
        self.open_kind = (kind != EditKind::Other).then_some(kind);
    }
}

/// One open file: the text plus everything that belongs to the file rather
/// than to a view of it (path, undo history, how it was loaded, its
/// conventions).
pub struct Document {
    rope: Rope,
    path: Option<PathBuf>,
    lossy: bool,
    line_ending: LineEnding,
    indent: IndentStyle,
    history: History,
}

impl Document {
    pub fn empty() -> Self {
        Document {
            rope: Rope::new(),
            path: None,
            lossy: false,
            line_ending: LineEnding::Lf,
            indent: IndentStyle::Spaces(4),
            history: History::default(),
        }
    }

    /// Opens `path`. A missing file yields an empty document carrying that
    /// path, so saving can create it. Invalid UTF-8 loads lossily (bad bytes
    /// become U+FFFD) and is flagged so the status line can say so.
    pub fn open(path: PathBuf) -> io::Result<Self> {
        let (rope, lossy) = match File::open(&path) {
            Ok(file) => match Rope::from_reader(BufReader::new(file)) {
                Ok(rope) => (rope, false),
                Err(e) if e.kind() == ErrorKind::InvalidData => {
                    let bytes = std::fs::read(&path)?;
                    (Rope::from_str(&String::from_utf8_lossy(&bytes)), true)
                }
                Err(e) => return Err(e),
            },
            Err(e) if e.kind() == ErrorKind::NotFound => (Rope::new(), false),
            Err(e) => return Err(e),
        };
        Ok(Document {
            line_ending: detect_line_ending(&rope),
            indent: detect_indent(&rope),
            rope,
            path: Some(path),
            lossy,
            history: History::default(),
        })
    }

    #[cfg(test)]
    pub fn from_str(text: &str) -> Self {
        let rope = Rope::from_str(text);
        Document {
            line_ending: detect_line_ending(&rope),
            indent: detect_indent(&rope),
            rope,
            ..Document::empty()
        }
    }

    #[cfg(test)]
    pub fn set_lossy(&mut self, lossy: bool) {
        self.lossy = lossy;
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    pub fn name(&self) -> Cow<'_, str> {
        match &self.path {
            Some(path) => path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy(),
            None => Cow::Borrowed("[No Name]"),
        }
    }

    /// Dirty means the history stands somewhere other than the last saved
    /// position, so undoing back to the loaded state clears it.
    pub fn dirty(&self) -> bool {
        self.history.index != self.history.saved_index
    }

    /// Replaces `range` with `text` — the single mutation entry point, so
    /// undo recording and coalescing see every change. `caret` is the caret
    /// as it stood before the edit; `kind` hints coalescing. Returns the
    /// cursor after the edit, snapped to a cluster boundary in case the
    /// insertion merged with a following combining mark.
    pub fn edit(&mut self, range: Range<usize>, text: &str, caret: Caret, kind: EditKind) -> usize {
        let deleted = self.rope.slice(range.clone()).to_string();
        self.rope.remove(range.clone());
        self.rope.insert(range.start, text);
        let cursor =
            grapheme::snap_to_boundary(self.rope.slice(..), range.start + text.chars().count());
        let edit = Edit {
            at: range.start,
            deleted,
            inserted: text.to_owned(),
        };
        self.history.record(edit, caret, cursor, kind);
        cursor
    }

    /// Closes the open typing run so the next edit starts a fresh undo step.
    /// Called on every movement key.
    pub fn break_undo_group(&mut self) {
        self.history.open_kind = None;
    }

    /// Reverts the latest applied group and hands back the caret to restore —
    /// anchor included, so undoing a selection replace revives the selection.
    pub fn undo(&mut self) -> Option<Caret> {
        self.history.open_kind = None;
        self.history.index = self.history.index.checked_sub(1)?;
        let group = &self.history.groups[self.history.index];
        let end = group.edit.at + group.edit.inserted.chars().count();
        self.rope.remove(group.edit.at..end);
        self.rope.insert(group.edit.at, &group.edit.deleted);
        Some(group.caret_before)
    }

    /// Re-applies the next undone group and hands back the caret to restore.
    pub fn redo(&mut self) -> Option<Caret> {
        self.history.open_kind = None;
        let group = self.history.groups.get(self.history.index)?;
        let end = group.edit.at + group.edit.deleted.chars().count();
        let cursor = group.cursor_after;
        self.rope.remove(group.edit.at..end);
        self.rope.insert(group.edit.at, &group.edit.inserted);
        self.history.index += 1;
        Some(Caret {
            cursor,
            anchor: None,
        })
    }

    pub fn lossy(&self) -> bool {
        self.lossy
    }

    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    pub fn indent(&self) -> IndentStyle {
        self.indent
    }

    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    pub fn line_start(&self, line: usize) -> usize {
        self.rope.line_to_char(line)
    }

    /// The char index just past the line's text — before its `\n` or `\r\n`
    /// terminator, which the cursor never enters and the screen never shows.
    pub fn line_end(&self, line: usize) -> usize {
        let start = self.rope.line_to_char(line);
        let mut end = self.rope.line_to_char(line + 1);
        if end > start && self.rope.char(end - 1) == '\n' {
            end -= 1;
            if end > start && self.rope.char(end - 1) == '\r' {
                end -= 1;
            }
        }
        end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_document_has_one_empty_line() {
        let doc = Document::empty();
        assert_eq!(doc.line_count(), 1);
        assert_eq!(doc.line_start(0), 0);
        assert_eq!(doc.line_end(0), 0);
        assert_eq!(doc.name(), "[No Name]");
        assert!(!doc.dirty());
    }

    #[test]
    fn line_end_stops_before_lf_and_crlf() {
        let doc = Document::from_str("ab\ncd\r\nef");
        assert_eq!(doc.line_count(), 3);
        assert_eq!(doc.line_end(0), 2);
        assert_eq!(doc.line_start(1), 3);
        assert_eq!(doc.line_end(1), 5);
        assert_eq!(doc.line_start(2), 7);
        assert_eq!(doc.line_end(2), 9);
    }

    #[test]
    fn trailing_newline_yields_an_empty_last_line() {
        let doc = Document::from_str("ab\n");
        assert_eq!(doc.line_count(), 2);
        assert_eq!(doc.line_start(1), 3);
        assert_eq!(doc.line_end(1), 3);
    }

    #[test]
    fn empty_lines_terminate_correctly() {
        let doc = Document::from_str("\n\r\n");
        assert_eq!(doc.line_count(), 3);
        assert_eq!(doc.line_end(0), 0);
        assert_eq!(doc.line_start(1), 1);
        assert_eq!(doc.line_end(1), 1);
    }

    #[test]
    fn line_ending_detection_takes_the_majority() {
        assert_eq!(
            Document::from_str("a\nb\nc\n").line_ending(),
            LineEnding::Lf
        );
        let crlf = Document::from_str("a\r\nb\r\nc");
        assert_eq!(crlf.line_ending(), LineEnding::Crlf);
        let mixed = Document::from_str("a\r\nb\nc\r\nd\r\n");
        assert_eq!(mixed.line_ending(), LineEnding::Crlf);
        let tie = Document::from_str("a\r\nb\nc");
        assert_eq!(tie.line_ending(), LineEnding::Lf);
        assert_eq!(Document::empty().line_ending(), LineEnding::Lf);
        assert_eq!(LineEnding::Crlf.as_str(), "\r\n");
    }

    #[test]
    fn indent_detection_votes_tabs_versus_spaces() {
        let tabs = Document::from_str("fn x\n\ta\n\tb\n");
        assert_eq!(tabs.indent(), IndentStyle::Tabs);
        let spaces = Document::from_str("fn x\n  a\n  b\n");
        assert_eq!(spaces.indent(), IndentStyle::Spaces(2));
        let majority = Document::from_str("\ta\n\tb\n  c\n");
        assert_eq!(majority.indent(), IndentStyle::Tabs);
        assert_eq!(IndentStyle::Tabs.as_str(), "\t");
        assert_eq!(IndentStyle::Spaces(3).as_str(), "   ");
    }

    #[test]
    fn indent_width_follows_nesting_steps() {
        let four = Document::from_str("a\n    b\n        c\n    d\n");
        assert_eq!(four.indent(), IndentStyle::Spaces(4));
        let two = Document::from_str("x\n  a\n    b\n      c\n");
        assert_eq!(two.indent(), IndentStyle::Spaces(2));
    }

    #[test]
    fn undetectable_indent_falls_back_to_four_spaces() {
        assert_eq!(Document::empty().indent(), IndentStyle::Spaces(4));
        let flat = Document::from_str("flat\nlines\nonly\n");
        assert_eq!(flat.indent(), IndentStyle::Spaces(4));
    }

    fn caret(cursor: usize) -> Caret {
        Caret {
            cursor,
            anchor: None,
        }
    }

    #[test]
    fn edit_replaces_a_range_and_returns_the_cursor() {
        let mut doc = Document::from_str("hello");
        let cursor = doc.edit(1..3, "EY", caret(3), EditKind::Other);
        assert_eq!(doc.rope().to_string(), "hEYlo");
        assert_eq!(cursor, 3);
        assert!(doc.dirty());
    }

    #[test]
    fn a_typing_run_undoes_and_redoes_as_one_step() {
        let mut doc = Document::empty();
        doc.edit(0..0, "a", caret(0), EditKind::Insert);
        doc.edit(1..1, "b", caret(1), EditKind::Insert);
        doc.edit(2..2, "c", caret(2), EditKind::Insert);
        assert_eq!(doc.rope().to_string(), "abc");
        assert_eq!(doc.undo(), Some(caret(0)));
        assert_eq!(doc.rope().to_string(), "");
        assert!(!doc.dirty());
        assert_eq!(doc.redo(), Some(caret(3)));
        assert_eq!(doc.rope().to_string(), "abc");
        assert!(doc.dirty());
    }

    #[test]
    fn backspace_and_delete_runs_coalesce() {
        let mut doc = Document::from_str("abcd");
        doc.edit(3..4, "", caret(4), EditKind::Backspace);
        doc.edit(2..3, "", caret(3), EditKind::Backspace);
        assert_eq!(doc.rope().to_string(), "ab");
        assert_eq!(doc.undo(), Some(caret(4)));
        assert_eq!(doc.rope().to_string(), "abcd");

        doc.edit(0..1, "", caret(0), EditKind::Delete);
        doc.edit(0..1, "", caret(0), EditKind::Delete);
        assert_eq!(doc.rope().to_string(), "cd");
        assert_eq!(doc.undo(), Some(caret(0)));
        assert_eq!(doc.rope().to_string(), "abcd");
    }

    #[test]
    fn breaking_the_group_splits_a_typing_run() {
        let mut doc = Document::empty();
        doc.edit(0..0, "a", caret(0), EditKind::Insert);
        doc.break_undo_group();
        doc.edit(1..1, "b", caret(1), EditKind::Insert);
        assert_eq!(doc.undo(), Some(caret(1)));
        assert_eq!(doc.rope().to_string(), "a");
        assert_eq!(doc.undo(), Some(caret(0)));
        assert_eq!(doc.rope().to_string(), "");
    }

    #[test]
    fn kind_changes_and_gaps_do_not_coalesce() {
        let mut doc = Document::empty();
        doc.edit(0..0, "ab", caret(0), EditKind::Insert);
        doc.edit(1..2, "", caret(2), EditKind::Backspace);
        assert_eq!(doc.rope().to_string(), "a");
        doc.undo();
        assert_eq!(doc.rope().to_string(), "ab");

        // An insert away from the run's tail starts its own group.
        doc.edit(0..0, "x", caret(0), EditKind::Insert);
        assert_eq!(doc.rope().to_string(), "xab");
        doc.undo();
        assert_eq!(doc.rope().to_string(), "ab");
    }

    #[test]
    fn selection_replace_is_its_own_step_and_undo_revives_the_selection() {
        let mut doc = Document::from_str("hello");
        let sel = Caret {
            cursor: 4,
            anchor: Some(1),
        };
        let cursor = doc.edit(1..4, "X", sel, EditKind::Other);
        assert_eq!(doc.rope().to_string(), "hXo");
        doc.edit(cursor..cursor, "y", caret(cursor), EditKind::Insert);
        assert_eq!(doc.rope().to_string(), "hXyo");
        assert_eq!(doc.undo(), Some(caret(2)));
        assert_eq!(doc.undo(), Some(sel));
        assert_eq!(doc.rope().to_string(), "hello");
    }

    #[test]
    fn a_new_edit_clears_the_redo_branch() {
        let mut doc = Document::empty();
        doc.edit(0..0, "a", caret(0), EditKind::Insert);
        doc.undo();
        doc.edit(0..0, "b", caret(0), EditKind::Insert);
        assert_eq!(doc.redo(), None);
        assert_eq!(doc.rope().to_string(), "b");
    }

    #[test]
    fn undo_and_redo_on_an_empty_history_are_no_ops() {
        let mut doc = Document::from_str("ab");
        assert_eq!(doc.undo(), None);
        assert_eq!(doc.redo(), None);
        assert_eq!(doc.rope().to_string(), "ab");
        assert!(!doc.dirty());
    }

    #[test]
    fn insert_merging_with_a_combining_mark_snaps_the_cursor() {
        let mut doc = Document::from_str("\u{301}x");
        let cursor = doc.edit(0..0, "e", caret(0), EditKind::Insert);
        assert_eq!(doc.rope().to_string(), "e\u{301}x");
        assert_eq!(cursor, 2); // past the whole e-plus-accent cluster
    }

    #[test]
    fn open_reads_missing_and_lossy_files() {
        let dir = std::env::temp_dir().join(format!("connor-doc-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let valid = dir.join("valid.txt");
        std::fs::write(&valid, "hello\nworld\n").unwrap();
        let doc = Document::open(valid).unwrap();
        assert_eq!(doc.rope().to_string(), "hello\nworld\n");
        assert!(!doc.lossy());
        assert_eq!(doc.name(), "valid.txt");

        let missing = dir.join("missing.txt");
        let doc = Document::open(missing.clone()).unwrap();
        assert_eq!(doc.line_count(), 1);
        assert_eq!(doc.name(), "missing.txt");
        assert!(!missing.exists());

        let garbage = dir.join("garbage.bin");
        std::fs::write(&garbage, b"ok\xFF\xFEbad\n").unwrap();
        let doc = Document::open(garbage).unwrap();
        assert!(doc.lossy());
        assert_eq!(doc.rope().to_string(), "ok\u{FFFD}\u{FFFD}bad\n");
        assert!(!doc.dirty());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
