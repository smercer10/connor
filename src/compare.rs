//! The buffer beside its baseline, line for line: the marks the gutter
//! already holds, stood up as aligned bands and scrolled from one position
//! so the two panes can never drift apart. Pure state — the panes are drawn
//! in `draw`, and nothing here reads a file or runs a process.

use crossterm::event::{KeyCode, KeyEvent};
use ropey::Rope;

use crate::diff::{self, Band, Diff};
use crate::doc::{Disk, Document};

/// Nothing will ever stand beside this buffer: it is outside a repository,
/// or has no path yet. Said in the status line when the toggle declines to
/// open, and in the pane when a tab switch lands on one.
pub const NOTHING: &str = "nothing in HEAD to compare against";

/// Past the size cap `diff` puts on everything it does. A file this big is
/// generated rather than reviewed, and saying so beats claiming HEAD has
/// never seen it.
pub const TOO_BIG: &str = "file too large to diff";

/// The file a conflict is about could not be read: it was deleted, or its
/// permissions changed, between the change landing and the view opening.
pub const UNREADABLE: &str = "can't read the file on disk";

/// The choice a conflict view offers, in the status line — the resolutions
/// are the view's, so nothing else has a place to say them.
pub const RESOLVE_HINT: &str = "(k)eep yours · (t)ake disk · (esc) cancel";

/// Said in the pane while the first lookup is still out — a few
/// milliseconds after open, and the one moment "no baseline yet" would
/// otherwise read as "every line is new".
const READING: &str = "reading HEAD…";

/// Why a side-by-side view cannot open on this document, if it cannot. One
/// answer for the toggle's refusal and for the pane a tab switch lands on.
pub fn refusal(doc: &Document, diff: &Diff) -> Option<&'static str> {
    if !diff.in_repo() {
        Some(NOTHING)
    } else if doc.rope().len_bytes() > diff::MAX_LEN {
        Some(TOO_BIG)
    } else {
        None
    }
}

pub enum Outcome {
    Pending,
    /// Esc: the view closes and the buffer is exactly where it was.
    Close,
    /// Keep the buffer: the file on disk is left untouched until a save.
    KeepMine,
    /// Take the disk version over the unsaved edits, once confirmed.
    TakeDisk,
}

pub struct Compare {
    /// The baseline of a HEAD view, owned: a ropey clone is copy-on-write
    /// rather than a copy, and holding it keeps this a view of two texts
    /// rather than of a `Diff`. Empty in a disk view, where `disk` holds the
    /// text the buffer stands against.
    head: Rope,
    /// The file as it was read when a disk view opened: what the panes show
    /// and, if the disk version is taken, exactly what lands in the buffer.
    /// `None` in a HEAD view — and the only thing that tells the two apart.
    disk: Option<Disk>,
    /// Whether the baseline is HEAD's content or the empty stand-in a file
    /// HEAD has never seen gets.
    tracked: bool,
    bands: Vec<Band>,
    /// The rows the differing bands start at: the header's count and place,
    /// and both jumps, from one sorted list.
    changed: Vec<usize>,
    /// What the bands were built from. Hunks are a pure function of the
    /// baseline and the revision they were computed from, so these name them
    /// exactly and staleness costs three comparisons a frame.
    doc_id: u64,
    head_gen: u64,
    hunk_rev: Option<u64>,
    note: Option<&'static str>,
    top: usize,
    scroll_col: usize,
    /// The change last jumped to. Repeated jumps must step even when the
    /// view is clamped against the bottom and `top` can no longer tell two
    /// changes apart; any scroll of your own drops it.
    at: Option<usize>,
}

impl Compare {
    /// The shell both openings start from: what fills it is the whole of
    /// what tells a HEAD view from a disk one.
    fn blank() -> Compare {
        Compare {
            head: Rope::new(),
            disk: None,
            tracked: false,
            bands: Vec::new(),
            changed: Vec::new(),
            doc_id: 0,
            head_gen: 0,
            hunk_rev: None,
            note: None,
            top: 0,
            scroll_col: 0,
            at: None,
        }
    }

    /// Opens with `line` — the cursor's — at the top, so the view starts
    /// where the reader already is.
    pub fn new(doc: &Document, diff: &Diff, line: usize, rows: usize) -> Compare {
        let mut compare = Compare::blank();
        compare.build(doc, diff);
        compare.top = compare.clamp(compare.row_of(line), rows);
        compare
    }

    /// Opens the buffer against the file on disk rather than HEAD: the
    /// conflict a dirty buffer's file changing underneath it raises, stood
    /// up so it can be resolved. Needs no repository and no lookup — the
    /// read is already done — so it opens where a HEAD view could not.
    pub fn disk(doc: &Document, disk: Disk, line: usize, rows: usize) -> Compare {
        let mut compare = Compare::blank();
        compare.build_disk(doc, disk);
        compare.top = compare.clamp(compare.row_of(line), rows);
        compare
    }

    /// Rebuilds a disk view against a fresh read: the file moved again while
    /// the view was up, and a pane showing what is no longer there is one a
    /// resolution cannot be made from. Keeps your place, like a moved
    /// baseline does.
    pub fn refresh_disk(&mut self, doc: &Document, disk: Disk, rows: usize) {
        self.build_disk(doc, disk);
        self.top = self.clamp(self.top, rows);
        self.at = None;
    }

    /// Rebuilds when the marks moved: a new tab under the view, a baseline
    /// that has been looked up again, or a diff that has since landed.
    /// Called before each frame, so a rebuild never lands on the render
    /// path. The text itself is read live from the document, so an edit
    /// arriving ahead of its diff shows through at once and only the
    /// alignment lags — the same bargain the gutter already makes.
    pub fn sync(&mut self, doc: &Document, diff: &Diff, rows: usize) {
        if self.disk.is_some() || !self.stale(doc, diff) {
            return;
        }
        let same_doc = self.doc_id == doc.id();
        self.build(doc, diff);
        // A different file starts at its own top; the same one keeps its
        // place under a baseline that moved.
        self.top = if same_doc {
            self.clamp(self.top, rows)
        } else {
            0
        };
        self.at = None;
    }

    /// Whether what the view was built from has moved. The view holds the
    /// baseline itself, so a lookup landing changes what it shows even when
    /// the marks it produced did not — a clean file's stay empty either
    /// way, and the gutter is right not to repaint for that.
    pub fn stale(&self, doc: &Document, diff: &Diff) -> bool {
        self.disk.is_none()
            && (self.doc_id != doc.id()
                || self.head_gen != diff.head_gen()
                || self.hunk_rev != diff.hunk_rev())
    }

    fn build(&mut self, doc: &Document, diff: &Diff) {
        self.doc_id = doc.id();
        self.head_gen = diff.head_gen();
        self.hunk_rev = diff.hunk_rev();
        self.tracked = diff.head().is_some();
        self.head = diff.head().cloned().unwrap_or_else(Rope::new);
        self.note = refusal(doc, diff).or((diff.head_gen() == 0).then_some(READING));
        self.bands.clear();
        self.changed.clear();
        if self.note.is_none() {
            // A file HEAD has never seen holds no marks: the gutter shows
            // an empty column for it on purpose, since a bar against every
            // row of a new file says nothing. Beside an empty pane it says
            // everything, so the view asks for its own answer — a diff
            // against nothing, which is the cheapest one there is.
            let absent;
            let hunks = if self.tracked {
                diff.hunks()
            } else {
                absent = diff::hunks(&self.head, doc.rope());
                &absent
            };
            self.bands = diff::bands(hunks, self.head.len_lines(), doc.rope().len_lines());
            self.changed
                .extend(self.bands.iter().filter(|b| !b.same).map(|b| b.row));
        }
    }

    /// The two sides' own read of the file, for a disk view: no repository,
    /// no lookup and no staleness tags, since nothing can move under it but
    /// the file itself — and that arrives through `refresh_disk`.
    fn build_disk(&mut self, doc: &Document, disk: Disk) {
        self.doc_id = doc.id();
        self.note = (doc.rope().len_bytes() > diff::MAX_LEN
            || disk.text.len_bytes() > diff::MAX_LEN)
            .then_some(TOO_BIG);
        self.bands.clear();
        self.changed.clear();
        if self.note.is_none() {
            let hunks = diff::hunks(&disk.text, doc.rope());
            self.bands = diff::bands(&hunks, disk.text.len_lines(), doc.rope().len_lines());
            self.changed
                .extend(self.bands.iter().filter(|b| !b.same).map(|b| b.row));
        }
        self.disk = Some(disk);
    }

    /// The text in the left pane, whichever kind of view this is.
    pub fn baseline(&self) -> &Rope {
        match &self.disk {
            Some(disk) => &disk.text,
            None => &self.head,
        }
    }

    /// The file this view stands the buffer against, when it is a conflict
    /// being resolved rather than a diff being read.
    pub fn resolving(&self) -> Option<&Disk> {
        self.disk.as_ref()
    }

    pub fn doc_id(&self) -> u64 {
        self.doc_id
    }

    pub fn tracked(&self) -> bool {
        self.tracked
    }

    pub fn bands(&self) -> &[Band] {
        &self.bands
    }

    /// What to say instead of panes, when there is nothing to show.
    pub fn note(&self) -> Option<&'static str> {
        self.note
    }

    pub fn top(&self) -> usize {
        self.top
    }

    pub fn scroll_col(&self) -> usize {
        self.scroll_col
    }

    pub fn rows(&self) -> usize {
        diff::rows(&self.bands)
    }

    pub fn changes(&self) -> usize {
        self.changed.len()
    }

    /// Which change the view stands on, 1-based; zero above the first.
    pub fn at(&self) -> usize {
        self.current().map_or(0, |i| i + 1)
    }

    /// The change the view stands on: the one it was last sent to, or —
    /// after a scroll of your own — the last one at or above the top row.
    fn current(&self) -> Option<usize> {
        match self.at {
            Some(i) => Some(i),
            None => self
                .changed
                .partition_point(|&row| row <= self.top)
                .checked_sub(1),
        }
    }

    /// Feeds one unmodified keypress. Anything unrecognized is swallowed,
    /// so plain typing can't leak into the document behind the view.
    /// `clipped` is the last frame's answer to whether anything ran past a
    /// pane's right edge — nothing here knows a line's width without
    /// walking it, so that is what bounds scrolling right.
    pub fn key(&mut self, key: &KeyEvent, rows: usize, clipped: bool) -> Outcome {
        let page = rows.max(1) as isize;
        if self.disk.is_some() {
            match key.code {
                KeyCode::Char('k' | 'K') => return Outcome::KeepMine,
                KeyCode::Char('t' | 'T') => return Outcome::TakeDisk,
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc => return Outcome::Close,
            KeyCode::Up => self.scroll_by(-1, rows),
            KeyCode::Down => self.scroll_by(1, rows),
            KeyCode::PageUp => self.scroll_by(-page, rows),
            KeyCode::PageDown => self.scroll_by(page, rows),
            KeyCode::Home => {
                self.scroll_by(isize::MIN, rows);
                self.scroll_col = 0;
            }
            KeyCode::End => self.scroll_by(isize::MAX, rows),
            KeyCode::Left => self.scroll_col = self.scroll_col.saturating_sub(1),
            KeyCode::Right if clipped => self.scroll_col += 1,
            _ => {}
        }
        Outcome::Pending
    }

    pub fn scroll_by(&mut self, delta: isize, rows: usize) {
        self.top = self.clamp(self.top.saturating_add_signed(delta), rows);
        self.at = None;
    }

    /// Moves to the change after the one the view stands on, wrapping.
    pub fn next_change(&mut self, rows: usize) {
        let len = self.changed.len();
        if len > 0 {
            self.jump(self.current().map_or(0, |i| (i + 1) % len), rows);
        }
    }

    /// Moves to the change before the one the view stands on, wrapping.
    pub fn prev_change(&mut self, rows: usize) {
        let len = self.changed.len();
        if len > 0 {
            self.jump(
                self.current().map_or(len - 1, |i| (i + len - 1) % len),
                rows,
            );
        }
    }

    fn jump(&mut self, i: usize, rows: usize) {
        self.top = self.clamp(self.changed[i], rows);
        self.at = Some(i);
    }

    /// The last row the view may sit on: far enough to show the end, never
    /// past it.
    fn clamp(&self, row: usize, rows: usize) -> usize {
        row.min(self.rows().saturating_sub(rows.max(1)))
    }

    /// The view row showing buffer `line`, or the nearest one below it when
    /// the line sits in a run that HEAD alone has.
    fn row_of(&self, line: usize) -> usize {
        let i = self.bands.partition_point(|b| b.buffer.end <= line);
        match self.bands.get(i) {
            Some(b) if b.buffer.start <= line => b.row + (line - b.buffer.start),
            Some(b) => b.row,
            None => self.rows().saturating_sub(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;

    use super::*;
    use crate::diff::hunks;

    /// A view of `buffer` against `head`, opened on `line`.
    fn view_of(head: &str, buffer: &str, line: usize, rows: usize) -> (Document, Diff, Compare) {
        let doc = Document::from_str(buffer);
        let baseline = Rope::from_str(head);
        let found = hunks(&baseline, doc.rope());
        let diff = Diff::test_baseline(Some(baseline), found);
        let compare = Compare::new(&doc, &diff, line, rows);
        (doc, diff, compare)
    }

    /// A view of `buffer` against the file, as a conflict stands it up.
    fn disk_view(disk: &str, buffer: &str, line: usize, rows: usize) -> (Document, Compare) {
        let doc = Document::from_str(buffer);
        let compare = Compare::disk(&doc, on_disk(disk, 1), line, rows);
        (doc, compare)
    }

    fn on_disk(text: &str, hash: u64) -> Disk {
        Disk {
            text: Rope::from_str(text),
            hash,
            lossy: false,
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn lines(n: usize) -> String {
        (0..n).map(|i| format!("line {i}\n")).collect()
    }

    #[test]
    fn opening_lands_on_the_line_the_cursor_was_on() {
        let head = lines(60);
        let (_, _, compare) = view_of(&head, &head, 30, 10);
        assert_eq!(compare.top(), 30);
        // Never past the end: the last page still shows the last line.
        let (_, _, compare) = view_of(&head, &head, 59, 10);
        assert_eq!(compare.top(), compare.rows() - 10);
    }

    #[test]
    fn changes_walk_forward_and_back_and_wrap() {
        let head = lines(60);
        let buffer = head
            .replace("line 5\n", "LINE 5\n")
            .replace("line 30\n", "LINE 30\n");
        let (_, _, mut compare) = view_of(&head, &buffer, 0, 10);
        assert_eq!(compare.changes(), 2);
        assert_eq!(compare.at(), 0);

        compare.next_change(10);
        assert_eq!((compare.top(), compare.at()), (5, 1));
        compare.next_change(10);
        assert_eq!((compare.top(), compare.at()), (30, 2));
        // Past the last: back around to the first.
        compare.next_change(10);
        assert_eq!((compare.top(), compare.at()), (5, 1));
        compare.prev_change(10);
        assert_eq!((compare.top(), compare.at()), (30, 2));
        compare.prev_change(10);
        assert_eq!((compare.top(), compare.at()), (5, 1));
        // Before the first: around to the last.
        compare.prev_change(10);
        assert_eq!((compare.top(), compare.at()), (30, 2));
    }

    #[test]
    fn a_change_below_the_last_page_is_still_stepped_through() {
        // Clamped against the bottom, two changes can share a top row; the
        // walk must still step off the one it is on rather than stick.
        let head = lines(20);
        let buffer = head
            .replace("line 17\n", "LINE 17\n")
            .replace("line 19\n", "LINE 19\n");
        let (_, _, mut compare) = view_of(&head, &buffer, 0, 20);
        assert_eq!(compare.changes(), 2);
        compare.next_change(20);
        let first = compare.top();
        assert_eq!(compare.at(), 1);
        compare.next_change(20);
        assert_eq!(compare.top(), first, "both clamp to the same row");
        assert_eq!(compare.at(), 2, "but the walk moved on");
    }

    #[test]
    fn a_buffer_matching_its_baseline_has_nothing_to_walk() {
        let head = lines(10);
        let (_, _, mut compare) = view_of(&head, &head, 0, 5);
        assert_eq!(compare.changes(), 0);
        compare.next_change(5);
        compare.prev_change(5);
        assert_eq!(compare.top(), 0);
        assert_eq!(compare.at(), 0);
    }

    #[test]
    fn scrolling_stops_at_both_ends() {
        let head = lines(60);
        let (_, _, mut compare) = view_of(&head, &head, 0, 10);
        let last = compare.rows() - 10;
        compare.scroll_by(-5, 10);
        assert_eq!(compare.top(), 0);
        compare.scroll_by(isize::MAX, 10);
        assert_eq!(compare.top(), last);
        compare.key(&press(KeyCode::End), 10, false);
        assert_eq!(compare.top(), last);
        compare.key(&press(KeyCode::Home), 10, false);
        assert_eq!(compare.top(), 0);
        compare.key(&press(KeyCode::PageDown), 10, false);
        assert_eq!(compare.top(), 10);
        compare.key(&press(KeyCode::Up), 10, false);
        assert_eq!(compare.top(), 9);
    }

    #[test]
    fn a_view_shorter_than_the_screen_never_scrolls() {
        let head = lines(3);
        let (_, _, mut compare) = view_of(&head, &head, 0, 40);
        compare.key(&press(KeyCode::End), 40, false);
        assert_eq!(compare.top(), 0);
    }

    #[test]
    fn a_file_the_baseline_has_never_seen_reads_as_added_whole() {
        // The gutter holds no marks for one, on purpose; beside an empty
        // pane the whole file is the answer.
        let doc = Document::from_str("a\nb\n");
        let diff = Diff::test_baseline(None, Vec::new());
        let compare = Compare::new(&doc, &diff, 0, 10);
        assert!(!compare.tracked());
        assert_eq!(compare.note(), None);
        assert_eq!(compare.changes(), 1);
        assert_eq!(compare.bands()[0].buffer, 0..2);
        assert!(compare.bands()[0].head.is_empty());
    }

    #[test]
    fn nothing_to_compare_against_leaves_a_note_and_no_panes() {
        let doc = Document::from_str("a\nb\n");
        // Outside a repository: no baseline will ever arrive.
        let compare = Compare::new(&doc, &Diff::new(None), 0, 10);
        assert_eq!(compare.note(), Some(NOTHING));
        assert!(compare.bands().is_empty());
        assert_eq!(compare.rows(), 0);

        // Inside one, but no lookup has answered yet — which must not read
        // as "HEAD has never seen this file".
        let compare = Compare::new(&doc, &Diff::test_marks(Vec::new()), 0, 10);
        assert_eq!(compare.note(), Some(READING));
        assert!(compare.bands().is_empty());

        // Past the cap, HEAD has seen it and the answer must not pretend
        // otherwise.
        let big = Document::from_str(&"x\n".repeat(diff::MAX_LEN));
        let diff = Diff::test_baseline(Some(Rope::new()), Vec::new());
        assert_eq!(refusal(&big, &diff), Some(TOO_BIG));
        assert_eq!(Compare::new(&big, &diff, 0, 10).note(), Some(TOO_BIG));
    }

    #[test]
    fn a_baseline_landing_on_a_clean_file_still_asks_for_a_repaint() {
        // The marks are the same empty list before and after — the gutter
        // is right not to repaint for that — but the view has only just
        // been handed something to stand the buffer against.
        let doc = Document::from_str("a\n");
        let waiting = Diff::test_marks(Vec::new());
        let compare = Compare::new(&doc, &waiting, 0, 10);
        assert_eq!(compare.note(), Some(READING));
        let landed = Diff::test_baseline(Some(Rope::from_str("a\n")), Vec::new());
        assert!(compare.stale(&doc, &landed));

        // And again when HEAD moves to a text the buffer also matches:
        // same marks, same revision, a different baseline behind them.
        let compare = Compare::new(&doc, &landed, 0, 10);
        assert!(!compare.stale(&doc, &landed));
        let mut moved = Diff::test_baseline(Some(Rope::from_str("a\n")), Vec::new());
        moved.bump_head_gen();
        assert!(compare.stale(&doc, &moved));
    }

    #[test]
    fn esc_closes_and_every_other_key_is_swallowed() {
        let head = lines(10);
        let (_, _, mut compare) = view_of(&head, &head, 0, 5);
        assert!(matches!(
            compare.key(&press(KeyCode::Esc), 5, false),
            Outcome::Close
        ));
        // Typing must not leak into the document behind the view.
        for code in [KeyCode::Char('a'), KeyCode::Enter, KeyCode::Backspace] {
            assert!(matches!(
                compare.key(&press(code), 5, false),
                Outcome::Pending
            ));
        }
        assert_eq!(compare.top(), 0);
    }

    #[test]
    fn scrolling_right_waits_for_something_to_be_cut_off() {
        let head = lines(10);
        let (_, _, mut compare) = view_of(&head, &head, 0, 5);
        compare.key(&press(KeyCode::Right), 5, false);
        assert_eq!(compare.scroll_col(), 0, "nothing is past the edge");
        compare.key(&press(KeyCode::Right), 5, true);
        compare.key(&press(KeyCode::Right), 5, true);
        assert_eq!(compare.scroll_col(), 2);
        compare.key(&press(KeyCode::Left), 5, false);
        assert_eq!(compare.scroll_col(), 1);
        compare.key(&press(KeyCode::Home), 5, false);
        assert_eq!(compare.scroll_col(), 0);
    }

    #[test]
    fn a_baseline_that_moved_rebuilds_the_view_and_keeps_your_place() {
        let head = lines(60);
        let (doc, _, mut compare) = view_of(&head, &head, 0, 10);
        compare.scroll_by(20, 10);
        assert_eq!(compare.changes(), 0);

        // A commit landed: HEAD is a different text now.
        let moved = head.replace("line 40\n", "LINE 40\n");
        let baseline = Rope::from_str(&moved);
        let found = hunks(&baseline, doc.rope());
        let mut after = Diff::test_baseline(Some(baseline), found);
        after.bump_head_gen();
        compare.sync(&doc, &after, 10);
        assert_eq!(compare.changes(), 1);
        assert_eq!(compare.top(), 20, "the same file keeps its place");
    }

    #[test]
    fn another_file_under_the_view_starts_at_its_own_top() {
        let head = lines(60);
        let (_, _, mut compare) = view_of(&head, &head, 0, 10);
        compare.scroll_by(20, 10);
        let other = Document::from_str("one\ntwo\n");
        let baseline = Rope::from_str("one\n");
        let found = hunks(&baseline, other.rope());
        compare.sync(&other, &Diff::test_baseline(Some(baseline), found), 10);
        assert_eq!(compare.top(), 0);
        assert_eq!(compare.changes(), 1);
    }

    #[test]
    fn a_disk_view_stands_the_buffer_against_the_file_it_read() {
        let disk = lines(60);
        let buffer = disk.replace("line 5\n", "LINE 5\n");
        let (_, compare) = disk_view(&disk, &buffer, 0, 10);
        // No repository, no lookup, no waiting: the read is the baseline.
        assert_eq!(compare.note(), None);
        assert_eq!(compare.changes(), 1);
        assert_eq!(compare.baseline().to_string(), disk);
        assert_eq!(compare.resolving().map(|d| d.hash), Some(1));
    }

    #[test]
    fn a_disk_view_is_never_rebuilt_by_the_diff_behind_it() {
        let disk = lines(20);
        let (doc, mut compare) = disk_view(&disk, &disk, 0, 5);
        compare.scroll_by(3, 5);
        // A HEAD lookup landing underneath says nothing about the file.
        let mut landed = Diff::test_baseline(Some(Rope::from_str("elsewhere\n")), Vec::new());
        landed.bump_head_gen();
        assert!(!compare.stale(&doc, &landed));
        compare.sync(&doc, &landed, 5);
        assert_eq!(compare.top(), 3);
        assert_eq!(compare.baseline().to_string(), disk);
    }

    #[test]
    fn the_resolutions_belong_to_the_disk_view_alone() {
        let disk = lines(10);
        let (_, mut compare) = disk_view(&disk, &disk, 0, 5);
        assert!(matches!(
            compare.key(&press(KeyCode::Char('k')), 5, false),
            Outcome::KeepMine
        ));
        assert!(matches!(
            compare.key(&press(KeyCode::Char('T')), 5, false),
            Outcome::TakeDisk
        ));
        // Reading the view still works while the choice stands open.
        compare.key(&press(KeyCode::Down), 5, false);
        assert_eq!(compare.top(), 1);
        assert!(matches!(
            compare.key(&press(KeyCode::Esc), 5, false),
            Outcome::Close
        ));

        // In a HEAD view they are typing, and typing is swallowed.
        let (_, _, mut head) = view_of(&disk, &disk, 0, 5);
        for code in [KeyCode::Char('k'), KeyCode::Char('t')] {
            assert!(matches!(head.key(&press(code), 5, false), Outcome::Pending));
        }
    }

    #[test]
    fn a_file_that_moved_again_rebuilds_the_view_and_keeps_your_place() {
        let disk = lines(60);
        let (doc, mut compare) = disk_view(&disk, &disk, 0, 10);
        compare.scroll_by(20, 10);
        assert_eq!(compare.changes(), 0);

        let moved = disk.replace("line 40\n", "LINE 40\n");
        compare.refresh_disk(&doc, on_disk(&moved, 2), 10);
        assert_eq!(compare.changes(), 1);
        assert_eq!(compare.top(), 20, "the same file keeps its place");
        assert_eq!(
            compare.resolving().map(|d| d.hash),
            Some(2),
            "and a resolution would apply what the panes now show"
        );
    }

    #[test]
    fn either_side_past_the_cap_notes_but_still_resolves() {
        let big = "x\n".repeat(diff::MAX_LEN);
        let doc = Document::from_str("small\n");
        let mut compare = Compare::disk(&doc, on_disk(&big, 1), 0, 10);
        assert_eq!(compare.note(), Some(TOO_BIG));
        assert!(compare.bands().is_empty());
        // Refusing to open would leave the conflict with no way out, and
        // the choice is whole-file either way.
        assert!(matches!(
            compare.key(&press(KeyCode::Char('t')), 10, false),
            Outcome::TakeDisk
        ));

        let doc = Document::from_str(&big);
        assert_eq!(
            Compare::disk(&doc, on_disk("small\n", 1), 0, 10).note(),
            Some(TOO_BIG)
        );
    }

    #[test]
    fn a_quiet_document_costs_no_rebuild() {
        let head = lines(10);
        let (doc, diff, mut compare) = view_of(&head, &head, 0, 5);
        compare.scroll_by(3, 5);
        compare.sync(&doc, &diff, 5);
        assert_eq!(compare.top(), 3);
    }
}
