//! The buffer against its HEAD content: a line diff, kept off the render
//! path, marked beside the line numbers. HEAD comes from shelling out to
//! `git` — the only external process connor runs — so no `git`, no
//! repository, or a file absent from HEAD all mean no marks, never an
//! error and never a stall.

use std::ffi::OsString;
use std::io::Read as _;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;

use imara_diff::{
    Algorithm, Diff as LineDiff, IndentHeuristic, IndentLevel, InternedInput, TokenSource,
};
use ropey::{Rope, RopeSlice};

use crate::doc::Document;
use crate::project;
use crate::watch::AppEvent;

/// Buffers and HEAD blobs past this get no marks at all: a file this size
/// is generated, not reviewed, and `grep` caps a searched file the same way.
pub const MAX_LEN: usize = 4 << 20;

/// Buffers under this re-diff on the event path; bigger ones go to a worker
/// so no frame blocks on one. Measured, not guessed: a histogram diff of
/// real source with one line in a hundred rewritten runs at ~60 MB/s in
/// release, so 256 KiB is ~4 ms — comfortably inside a frame that also has
/// a reparse to pay for, and every keystroke pays it.
const SYNC_DIFF_LIMIT: usize = 256 * 1024;

/// The bar marking a row whose content differs — in the gutter's one
/// column, and in each pane of the side-by-side view.
pub const BAR: char = '▍';

/// What a marked row says about HEAD.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change {
    Added,
    Changed,
    /// Lines removed just above this row.
    RemovedAbove,
    /// Lines removed past the end of the buffer, so the mark hangs under
    /// the last row rather than over a row that no longer exists.
    RemovedBelow,
}

impl Change {
    /// A bar beside rows whose content is there; a rule along the edge the
    /// removed lines left for content that isn't.
    pub fn glyph(self) -> char {
        match self {
            Change::Added | Change::Changed => BAR,
            Change::RemovedAbove => '▔',
            Change::RemovedBelow => '▁',
        }
    }

    /// A `Cell::fg` code: dark green, yellow and red, from the base ANSI
    /// palette only so the terminal theme keeps light and dark readable —
    /// `syntax`'s rule.
    pub fn color(self) -> u8 {
        match self {
            Change::Added => 3,
            Change::Changed => 4,
            Change::RemovedAbove | Change::RemovedBelow => 2,
        }
    }
}

/// A run of buffer lines carrying one mark, in line indices. Hunks are
/// sorted and non-overlapping, so the draw pass walks them with a single
/// advancing index.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hunk {
    pub start: usize,
    pub end: usize,
    pub kind: Change,
    /// The HEAD lines this hunk replaced, empty for a pure insertion. The
    /// gutter has one column and no use for them; `bands` needs them to
    /// stand the two texts side by side.
    pub head_start: usize,
    pub head_end: usize,
}

impl Hunk {
    /// The buffer lines the hunk actually covers. `start..end` is a mark
    /// range, and for a removal it is a synthetic one-row marker on the
    /// edge the lines left — the run itself is empty, at `start` when the
    /// gap closed inside the buffer and past `end` when it ran off the end.
    fn buffer(&self) -> Range<usize> {
        match self.kind {
            Change::Added | Change::Changed => self.start..self.end,
            Change::RemovedAbove => self.start..self.start,
            Change::RemovedBelow => self.end..self.end,
        }
    }
}

/// The lines of a rope without their terminators. Stripping them is what
/// makes the comparison line-ending agnostic — a CRLF buffer against an LF
/// blob differs by content, not by convention, the same conclusion `git
/// diff` reaches by normalizing the working side — and it matches connor's
/// model of the line ending as a whole-document property.
struct Lines<'a>(RopeSlice<'a>);

struct LineIter<'a>(ropey::iter::Lines<'a>);

impl<'a> Iterator for LineIter<'a> {
    type Item = RopeSlice<'a>;

    fn next(&mut self) -> Option<RopeSlice<'a>> {
        self.0.next().map(strip_terminator)
    }
}

impl<'a> TokenSource for Lines<'a> {
    type Token = RopeSlice<'a>;
    type Tokenizer = LineIter<'a>;

    fn tokenize(&self) -> LineIter<'a> {
        LineIter(self.0.lines())
    }

    fn estimate_tokens(&self) -> u32 {
        self.0.len_lines() as u32
    }
}

/// The line without its `\n` or `\r\n`, the way `Document::line_end` bounds
/// a line.
pub fn strip_terminator(line: RopeSlice<'_>) -> RopeSlice<'_> {
    let mut end = line.len_chars();
    if end > 0 && line.char(end - 1) == '\n' {
        end -= 1;
        if end > 0 && line.char(end - 1) == '\r' {
            end -= 1;
        }
    }
    line.slice(..end)
}

/// The buffer's lines against HEAD's, as gutter marks; empty when they
/// agree. Pure, and the only part of this module #38 and #40 need.
pub fn hunks(head: &Rope, buffer: &Rope) -> Vec<Hunk> {
    let input = InternedInput::new(Lines(head.slice(..)), Lines(buffer.slice(..)));
    let mut diff = LineDiff::compute(Algorithm::Histogram, &input);
    // The indent heuristic is what puts an inserted block's mark on the
    // lines a reader would call inserted; `postprocess_lines` is the same
    // call, but it wants tokens that are byte slices.
    diff.postprocess_with_heuristic(
        &input,
        IndentHeuristic::new(|token| IndentLevel::for_ascii_line(input.interner[token].bytes(), 8)),
    );
    let last = buffer.len_lines() - 1;
    diff.hunks()
        .map(|hunk| {
            let (start, end) = (hunk.after.start as usize, hunk.after.end as usize);
            let (head_start, head_end) = (hunk.before.start as usize, hunk.before.end as usize);
            let (start, end, kind) = if !hunk.after.is_empty() {
                let kind = if hunk.before.is_empty() {
                    Change::Added
                } else {
                    Change::Changed
                };
                (start, end, kind)
            } else if start > last {
                (last, last + 1, Change::RemovedBelow)
            } else {
                (start, start + 1, Change::RemovedAbove)
            };
            Hunk {
                start,
                end,
                kind,
                head_start,
                head_end,
            }
        })
        .collect()
}

/// One aligned band of a side-by-side view: the HEAD lines and the buffer
/// lines that stand level with each other, starting at view row `row`.
/// Bands are sorted, tile the view without gaps, and between them every
/// line of each side appears exactly once. Either range may be empty — an
/// insertion has no HEAD lines, a deletion no buffer lines — so a band is
/// as tall as its longer side and the shorter one pads with blank rows.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Band {
    pub row: usize,
    pub head: Range<usize>,
    pub buffer: Range<usize>,
    /// Whether the two sides agree here: the context between changes.
    pub same: bool,
}

impl Band {
    pub fn height(&self) -> usize {
        self.head.len().max(self.buffer.len())
    }
}

/// The two texts stood side by side, from the marks the gutter already
/// holds — no second diff, so opening the view costs one pass over the
/// hunks however large the file. Line counts are `Rope::len_lines`, the
/// same count the diff tokenized, so the trailing empty line each rope
/// carries lands opposite its twin.
pub fn bands(hunks: &[Hunk], head_lines: usize, buffer_lines: usize) -> Vec<Band> {
    let mut bands = Vec::with_capacity(2 * hunks.len() + 1);
    let mut row = 0;
    let (mut h, mut b) = (0, 0);
    for hunk in hunks {
        let head = hunk.head_start.min(head_lines)..hunk.head_end.min(head_lines);
        let buffer = hunk.buffer();
        let buffer = buffer.start.min(buffer_lines)..buffer.end.min(buffer_lines);
        // The run before the hunk is untouched, so it is the same length on
        // both sides; the shorter one governs if a clamp above shortened it.
        let context = head
            .start
            .saturating_sub(h)
            .min(buffer.start.saturating_sub(b));
        push(&mut bands, &mut row, h..h + context, b..b + context, true);
        h += context;
        b += context;
        push(&mut bands, &mut row, head.clone(), buffer.clone(), false);
        h = h.max(head.end);
        b = b.max(buffer.end);
    }
    push(&mut bands, &mut row, h..head_lines, b..buffer_lines, true);
    bands
}

/// Appends a band and advances the row cursor past it. An empty one — two
/// empty ranges — is not a band at all and is dropped, so the list stays
/// free of zero-height entries a row lookup would have to skip.
fn push(
    bands: &mut Vec<Band>,
    row: &mut usize,
    head: Range<usize>,
    buffer: Range<usize>,
    same: bool,
) {
    let band = Band {
        row: *row,
        head,
        buffer,
        same,
    };
    let height = band.height();
    if height > 0 {
        bands.push(band);
        *row += height;
    }
}

/// Rows the aligned view occupies.
pub fn rows(bands: &[Band]) -> usize {
    bands.last().map_or(0, |b| b.row + b.height())
}

/// What a finished job brought home.
enum Done {
    /// A `git` lookup: HEAD's content, `None` when the file has none, and
    /// where HEAD lives when the job was asked to find out.
    Head {
        head: Option<Rope>,
        git_dir: Option<PathBuf>,
    },
    /// A re-diff against the baseline already held.
    Rediff,
}

/// A finished lookup or diff on its way back to the main loop.
pub struct DiffDone {
    pub doc_id: u64,
    generation: u64,
    /// The document revision the hunks were computed from.
    revision: u64,
    hunks: Vec<Hunk>,
    kind: Done,
}

/// One document's standing against HEAD.
pub struct Diff {
    /// Whether the document sits inside a git project, decided once with no
    /// subprocess. False keeps this inert for the document's whole life:
    /// connor never runs `git` for it, and its gutter is exactly as wide as
    /// it was before any of this existed.
    in_repo: bool,
    /// HEAD's content; `None` before the first lookup finishes and when the
    /// file has none.
    head: Option<Rope>,
    /// Bumped by every lookup that lands, so a view holding the baseline
    /// knows to take it again. Zero means none has answered yet, which is
    /// what tells "still reading" from "HEAD has never seen this file".
    head_gen: u64,
    /// HEAD has never been looked up, or may have moved since it was.
    stale: bool,
    git_dir: Option<PathBuf>,
    hunks: Vec<Hunk>,
    /// The revision `hunks` were computed from.
    hunk_rev: Option<u64>,
    /// Tags background work so a superseded result is dropped on arrival.
    generation: u64,
    inflight: bool,
}

impl Diff {
    pub fn new(path: Option<&Path>) -> Diff {
        Diff {
            in_repo: path.is_some_and(project::in_repo),
            head: None,
            head_gen: 0,
            stale: true,
            git_dir: None,
            hunks: Vec::new(),
            hunk_rev: None,
            generation: 0,
            inflight: false,
        }
    }

    /// Whether the gutter carries a mark column for this document. True for
    /// everything inside a repository, tracked or not: deciding it on the
    /// lookup's answer instead would shift the text a column sideways one
    /// frame after every open.
    pub fn in_repo(&self) -> bool {
        self.in_repo
    }

    pub fn hunks(&self) -> &[Hunk] {
        &self.hunks
    }

    /// HEAD's content, once a lookup has answered; `None` while one is in
    /// flight and when HEAD has never seen this file. `head_gen` tells
    /// those two apart.
    pub fn head(&self) -> Option<&Rope> {
        self.head.as_ref()
    }

    /// Which lookup the baseline came from; zero before the first answers.
    pub fn head_gen(&self) -> u64 {
        self.head_gen
    }

    /// The document revision `hunks` were computed from. With `head_gen` it
    /// names the marks exactly, so a view can tell when they moved without
    /// comparing the list.
    pub fn hunk_rev(&self) -> Option<u64> {
        self.hunk_rev
    }

    /// The directory holding this file's HEAD, once a lookup has found it —
    /// the only place a commit or a branch switch shows up, since neither
    /// touches the working tree.
    pub fn git_dir(&self) -> Option<&Path> {
        self.git_dir.as_deref()
    }

    /// HEAD may have moved; the next pump looks again. Clearing nothing
    /// keeps the current marks on screen until the new answer lands.
    pub fn mark_stale(&mut self) {
        self.stale = true;
    }

    /// Catches the marks up with the document: looks HEAD up when it has
    /// never been looked up or may have moved, and re-diffs when the buffer
    /// has changed under them — in place when cheap, on a worker when not.
    /// Called before each frame of the active tab and whenever HEAD moves;
    /// a quiet document is a no-op. One job at a time, so a typing burst
    /// cannot stack threads.
    pub fn pump(&mut self, doc: &Document, tx: &Sender<AppEvent>) {
        if !self.in_repo || self.inflight {
            return;
        }
        let Some(path) = doc.path() else {
            return;
        };
        let rope = doc.rope();
        if rope.len_bytes() > MAX_LEN {
            // Nothing to show and nothing to spend: a buffer edited back
            // under the cap picks up again on the next pump.
            self.hunks.clear();
            self.hunk_rev = None;
            return;
        }
        if self.stale {
            // Cleared at the spawn, not on arrival, so a HEAD move that
            // lands mid-flight is not swallowed by the reply to the last one.
            self.stale = false;
            self.generation += 1;
            spawn_head(
                path.to_path_buf(),
                self.git_dir.is_none(),
                rope.clone(),
                doc.id(),
                self.generation,
                doc.revision(),
                tx.clone(),
            );
            self.inflight = true;
            return;
        }
        let Some(head) = &self.head else {
            return;
        };
        if self.hunk_rev == Some(doc.revision()) {
            return;
        }
        if rope.len_bytes() <= SYNC_DIFF_LIMIT {
            self.hunks = hunks(head, rope);
            self.hunk_rev = Some(doc.revision());
        } else {
            self.generation += 1;
            spawn_rediff(
                head.clone(),
                rope.clone(),
                doc.id(),
                self.generation,
                doc.revision(),
                tx.clone(),
            );
            self.inflight = true;
        }
    }

    /// Installs a finished job if it is still the awaited one, then pumps
    /// again to cover whatever the document did while it ran. Returns
    /// whether the marks changed — a re-lookup that finds the same content
    /// changes nothing and must not cost a repaint.
    pub fn absorb(&mut self, done: DiffDone, doc: &Document, tx: &Sender<AppEvent>) -> bool {
        if !self.inflight || done.generation != self.generation {
            return false;
        }
        self.inflight = false;
        if let Done::Head { head, git_dir } = done.kind {
            self.head = head;
            self.head_gen += 1;
            if git_dir.is_some() {
                self.git_dir = git_dir;
            }
        }
        let changed = self.hunks != done.hunks;
        self.hunks = done.hunks;
        self.hunk_rev = Some(done.revision);
        self.pump(doc, tx);
        changed
    }

    /// A comparison standing at a fixed baseline whose lookup has answered,
    /// so a test can build the side-by-side view without a repository
    /// behind it. `head` of `None` is a file HEAD has never seen.
    #[cfg(test)]
    pub fn test_baseline(head: Option<Rope>, hunks: Vec<Hunk>) -> Diff {
        Diff {
            in_repo: true,
            head,
            head_gen: 1,
            hunks,
            hunk_rev: Some(0),
            ..Diff::new(None)
        }
    }

    /// A lookup landed on a baseline that had moved.
    #[cfg(test)]
    pub fn bump_head_gen(&mut self) {
        self.head_gen += 1;
    }

    /// A comparison standing at a fixed set of marks, so a test can draw or
    /// navigate them without a repository behind it.
    #[cfg(test)]
    pub fn test_marks(hunks: Vec<Hunk>) -> Diff {
        Diff {
            in_repo: true,
            hunks,
            ..Diff::new(None)
        }
    }

    /// The line to jump to for the change after `line`, wrapping; `None`
    /// when the buffer matches HEAD.
    pub fn next_change(&self, line: usize) -> Option<usize> {
        if self.hunks.is_empty() {
            return None;
        }
        let i = self.hunks.partition_point(|h| h.start <= line);
        Some(self.hunks[i % self.hunks.len()].start)
    }

    /// The line to jump to for the change before `line`, wrapping.
    pub fn prev_change(&self, line: usize) -> Option<usize> {
        if self.hunks.is_empty() {
            return None;
        }
        let i = self.hunks.partition_point(|h| h.start < line);
        Some(self.hunks[(i + self.hunks.len() - 1) % self.hunks.len()].start)
    }
}

/// Looks HEAD up and diffs against it on a worker: the `git` process, the
/// read and the diff all stay off the event path. No cancel flag — unlike a
/// walk or a parse there is no progress to poll, the work is bounded by
/// `MAX_LEN`, and a superseded result is already dropped by generation on
/// arrival. The thread owns no editor state and exits when the receiver is
/// gone.
fn spawn_head(
    path: PathBuf,
    want_git_dir: bool,
    rope: Rope,
    doc_id: u64,
    generation: u64,
    revision: u64,
    tx: Sender<AppEvent>,
) {
    thread::spawn(move || {
        let git_dir = if want_git_dir { git_dir(&path) } else { None };
        let head = head_blob(&path);
        let found = head.as_ref().map(|h| hunks(h, &rope)).unwrap_or_default();
        let _ = tx.send(AppEvent::Diffed(DiffDone {
            doc_id,
            generation,
            revision,
            hunks: found,
            kind: Done::Head { head, git_dir },
        }));
    });
}

/// Re-diffs a buffer too big to diff on the event path against the baseline
/// already in hand.
fn spawn_rediff(
    head: Rope,
    rope: Rope,
    doc_id: u64,
    generation: u64,
    revision: u64,
    tx: Sender<AppEvent>,
) {
    thread::spawn(move || {
        let found = hunks(&head, &rope);
        let _ = tx.send(AppEvent::Diffed(DiffDone {
            doc_id,
            generation,
            revision,
            hunks: found,
            kind: Done::Rediff,
        }));
    });
}

/// A `git` invocation rooted at `dir`, wired so nothing it does can reach a
/// terminal in raw mode and nothing in the environment can point it at a
/// different repository. The project-wide scan builds on this too.
pub fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        // A connor launched from a hook inherits these, and they name
        // whatever repository the hook is running for.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    cmd
}

fn dir_of(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// HEAD's content for `path`, or `None` when there is none to compare
/// against: no `git` on the machine, a path outside a repository, a file
/// absent from HEAD, a blob past the cap, or one that isn't text.
fn head_blob(path: &Path) -> Option<Rope> {
    // `HEAD:./name` resolves against git's own directory, so no repository
    // root or relative path has to be computed here; the `HEAD:` prefix
    // also means a filename starting with `-` is never read as a flag, and
    // an `OsString` keeps a name that isn't UTF-8 intact.
    let mut spec = OsString::from("HEAD:./");
    spec.push(path.file_name()?);
    let mut child = git(dir_of(path))
        .args(["cat-file", "blob"])
        .arg(spec)
        .spawn()
        .ok()?;
    let mut out = Vec::new();
    // Capped rather than read whole, and the pipe closes before the wait:
    // an oversized blob leaves `git` writing into a closed pipe, which
    // ends it and fails the status below, instead of deadlocking us.
    let read = child
        .stdout
        .take()
        .map(|stdout| stdout.take(MAX_LEN as u64 + 1).read_to_end(&mut out));
    let ok = child.wait().is_ok_and(|status| status.success());
    if !ok || !matches!(read, Some(Ok(_))) || out.len() > MAX_LEN {
        return None;
    }
    Some(Rope::from_str(&String::from_utf8(out).ok()?))
}

/// Where the file's HEAD actually lives — under `.git/worktrees/<name>` in
/// a linked worktree, not the `.git` file beside the checkout — so the
/// watch that notices a commit sits on the right directory.
fn git_dir(path: &Path) -> Option<PathBuf> {
    let out = git(dir_of(path))
        .args(["rev-parse", "--absolute-git-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let dir = PathBuf::from(text.trim_end_matches(['\n', '\r']));
    dir.is_dir().then_some(dir)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    fn marks(head: &str, buffer: &str) -> Vec<Hunk> {
        hunks(&Rope::from_str(head), &Rope::from_str(buffer))
    }

    fn hunk(start: usize, end: usize, kind: Change, head: Range<usize>) -> Hunk {
        Hunk {
            start,
            end,
            kind,
            head_start: head.start,
            head_end: head.end,
        }
    }

    #[test]
    fn a_buffer_matching_head_has_no_marks() {
        assert_eq!(marks("a\nb\nc\n", "a\nb\nc\n"), []);
        assert_eq!(marks("", ""), []);
    }

    #[test]
    fn an_inserted_run_marks_the_lines_it_added() {
        assert_eq!(
            marks("a\nb\n", "a\nx\ny\nb\n"),
            [hunk(1, 3, Change::Added, 1..1)]
        );
    }

    #[test]
    fn a_rewritten_line_marks_changed_not_added_and_removed() {
        assert_eq!(
            marks("a\nb\nc\n", "a\nB\nc\n"),
            [hunk(1, 2, Change::Changed, 1..2)]
        );
    }

    #[test]
    fn a_removed_run_marks_the_row_it_left_behind() {
        // "b" and "c" are gone from between "a" and "d"; the mark sits on
        // the row that now follows the gap.
        assert_eq!(
            marks("a\nb\nc\nd\n", "a\nd\n"),
            [hunk(1, 2, Change::RemovedAbove, 1..3)]
        );
    }

    #[test]
    fn a_removal_running_off_the_end_marks_under_the_last_row() {
        // Nothing follows the gap, so the mark hangs below the last row
        // rather than over a row that no longer exists.
        assert_eq!(
            marks("a\nb\n", "a"),
            [hunk(0, 1, Change::RemovedBelow, 1..3)]
        );
    }

    #[test]
    fn a_dropped_trailing_newline_reads_as_a_removal() {
        assert_eq!(marks("a\n", "a"), [hunk(0, 1, Change::RemovedBelow, 1..2)]);
    }

    #[test]
    fn line_endings_are_not_content() {
        // git normalizes the working side before comparing; stripping
        // terminators reaches the same answer without running the filters.
        assert_eq!(marks("a\nb\n", "a\r\nb\r\n"), []);
        assert_eq!(marks("a\r\nb\r\n", "a\nb\n"), []);
        assert_eq!(
            marks("a\nb\n", "a\r\nB\r\n"),
            [hunk(1, 2, Change::Changed, 1..2)]
        );
    }

    #[test]
    fn an_emptied_buffer_marks_the_removal_at_its_top() {
        assert_eq!(
            marks("a\nb\n", ""),
            [hunk(0, 1, Change::RemovedAbove, 0..2)]
        );
    }

    #[test]
    fn a_file_absent_from_head_is_added_whole() {
        assert_eq!(marks("", "a\nb\n"), [hunk(0, 2, Change::Added, 0..0)]);
    }

    #[test]
    fn edits_far_apart_stay_separate_hunks() {
        // The distinction from `change_span`, which collapses these into
        // one region spanning nearly the whole document.
        let head: String = (0..30).map(|i| format!("line {i}\n")).collect();
        let buffer = head
            .replace("line 2\n", "LINE 2\n")
            .replace("line 27\n", "LINE 27\n");
        assert_eq!(
            marks(&head, &buffer),
            [
                hunk(2, 3, Change::Changed, 2..3),
                hunk(27, 28, Change::Changed, 27..28),
            ]
        );
    }

    #[test]
    fn hunks_come_back_sorted_and_non_overlapping() {
        let head: String = (0..40).map(|i| format!("line {i}\n")).collect();
        let mut buffer = head.replace("line 5\n", "");
        buffer = buffer.replace("line 20\n", "LINE 20\nextra\n");
        buffer.push_str("tail\n");
        let found = marks(&head, &buffer);
        assert!(found.len() >= 3, "{found:?}");
        for pair in found.windows(2) {
            assert!(pair[0].end <= pair[1].start, "{found:?}");
        }
        for h in &found {
            assert!(h.start < h.end, "{h:?}");
        }
    }

    fn aligned(head: &str, buffer: &str) -> Vec<Band> {
        let (head, buffer) = (Rope::from_str(head), Rope::from_str(buffer));
        bands(&hunks(&head, &buffer), head.len_lines(), buffer.len_lines())
    }

    fn band(row: usize, head: Range<usize>, buffer: Range<usize>, same: bool) -> Band {
        Band {
            row,
            head,
            buffer,
            same,
        }
    }

    #[test]
    fn matching_texts_are_one_band_of_context() {
        // Three lines each: the two written plus the empty one every rope
        // carries past its last terminator.
        assert_eq!(aligned("a\nb\n", "a\nb\n"), [band(0, 0..3, 0..3, true)]);
    }

    #[test]
    fn an_insertion_pads_the_head_side() {
        // Two new lines with nothing opposite them, so the band is two
        // rows tall and HEAD contributes none of them.
        let found = aligned("a\nb\n", "a\nx\ny\nb\n");
        assert_eq!(
            found,
            [
                band(0, 0..1, 0..1, true),
                band(1, 1..1, 1..3, false),
                band(3, 1..3, 3..5, true),
            ]
        );
        assert_eq!(rows(&found), 5);
    }

    #[test]
    fn a_deletion_pads_the_buffer_side() {
        let found = aligned("a\nb\nc\nd\n", "a\nd\n");
        assert_eq!(
            found,
            [
                band(0, 0..1, 0..1, true),
                band(1, 1..3, 1..1, false),
                band(3, 3..5, 1..3, true),
            ]
        );
        assert_eq!(rows(&found), 5);
    }

    #[test]
    fn a_rewrite_stands_the_two_versions_level() {
        assert_eq!(
            aligned("a\nb\nc\n", "a\nB\nc\n"),
            [
                band(0, 0..1, 0..1, true),
                band(1, 1..2, 1..2, false),
                band(2, 2..4, 2..4, true),
            ]
        );
    }

    #[test]
    fn a_removal_off_the_end_sits_past_the_last_buffer_line() {
        // The gutter collapses this to a marker row under the last line;
        // the view wants the run itself, empty and at the very end.
        assert_eq!(
            aligned("a\nb\n", "a"),
            [band(0, 0..1, 0..1, true), band(1, 1..3, 1..1, false)]
        );
    }

    #[test]
    fn a_file_absent_from_head_reads_as_added_whole() {
        assert_eq!(
            aligned("", "a\nb\n"),
            [band(0, 0..0, 0..2, false), band(2, 0..1, 2..3, true)]
        );
    }

    #[test]
    fn bands_tile_the_view_and_spend_every_line_once() {
        let head: String = (0..40).map(|i| format!("line {i}\n")).collect();
        let mut buffer = head.replace("line 5\n", "");
        buffer = buffer.replace("line 20\n", "LINE 20\nextra\n");
        buffer.push_str("tail\n");
        let (head_rope, buffer_rope) = (Rope::from_str(&head), Rope::from_str(&buffer));
        let found = aligned(&head, &buffer);
        let (mut row, mut h, mut b) = (0, 0, 0);
        for band in &found {
            assert_eq!(band.row, row, "{found:?}");
            assert!(band.height() > 0, "{found:?}");
            // Every line of each side, in order, exactly once.
            assert_eq!(band.head.start, h, "{found:?}");
            assert_eq!(band.buffer.start, b, "{found:?}");
            row += band.height();
            h = band.head.end;
            b = band.buffer.end;
        }
        assert_eq!(h, head_rope.len_lines());
        assert_eq!(b, buffer_rope.len_lines());
        assert_eq!(rows(&found), row);
        // The view is never shorter than either text: nothing is hidden.
        assert!(row >= head_rope.len_lines().max(buffer_rope.len_lines()));
    }

    #[test]
    fn next_change_walks_forward_and_wraps() {
        let diff = Diff::test_marks(vec![
            hunk(2, 4, Change::Added, 2..2),
            hunk(10, 11, Change::Changed, 10..11),
        ]);
        assert_eq!(diff.next_change(0), Some(2));
        assert_eq!(diff.next_change(2), Some(10)); // inside a hunk: on to the next
        assert_eq!(diff.next_change(3), Some(10));
        assert_eq!(diff.next_change(10), Some(2)); // past the last: back around
        assert_eq!(diff.next_change(99), Some(2));
    }

    #[test]
    fn prev_change_walks_back_and_wraps() {
        let diff = Diff::test_marks(vec![
            hunk(2, 4, Change::Added, 2..2),
            hunk(10, 11, Change::Changed, 10..11),
        ]);
        assert_eq!(diff.prev_change(99), Some(10));
        assert_eq!(diff.prev_change(10), Some(2));
        assert_eq!(diff.prev_change(2), Some(10)); // before the first: around
        assert_eq!(diff.prev_change(0), Some(10));
    }

    #[test]
    fn a_clean_buffer_has_nowhere_to_jump() {
        let diff = Diff::test_marks(Vec::new());
        assert_eq!(diff.next_change(0), None);
        assert_eq!(diff.prev_change(0), None);
    }

    #[test]
    fn a_buffer_outside_a_repository_never_runs_git() {
        let (tx, rx) = mpsc::channel();
        let mut diff = Diff::new(None);
        assert!(!diff.in_repo());
        let doc = Document::from_str("a\nb\n");
        diff.pump(&doc, &tx);
        assert!(!diff.inflight);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_pathless_buffer_never_runs_git() {
        let (tx, _rx) = mpsc::channel();
        let mut diff = Diff::test_marks(Vec::new());
        let doc = Document::from_str("a\nb\n");
        diff.pump(&doc, &tx);
        assert!(!diff.inflight);
    }

    #[test]
    fn a_buffer_past_the_cap_drops_its_marks_and_spends_nothing() {
        let (tx, _rx) = mpsc::channel();
        let mut diff = Diff::test_marks(vec![hunk(0, 1, Change::Added, 0..0)]);
        let mut doc = Document::from_str(&"x\n".repeat(MAX_LEN / 2 + 1));
        doc.set_path(PathBuf::from("huge.rs"));
        diff.pump(&doc, &tx);
        assert_eq!(diff.hunks(), []);
        assert!(!diff.inflight);
    }

    fn done(generation: u64, revision: u64, hunks: Vec<Hunk>) -> DiffDone {
        DiffDone {
            doc_id: 0,
            generation,
            revision,
            hunks,
            kind: Done::Rediff,
        }
    }

    #[test]
    fn a_superseded_result_is_dropped() {
        let (tx, _rx) = mpsc::channel();
        let doc = Document::from_str("a\n");
        let mut diff = Diff::test_marks(vec![hunk(0, 1, Change::Added, 0..0)]);
        diff.inflight = true;
        diff.generation = 2;
        assert!(!diff.absorb(done(1, 0, Vec::new()), &doc, &tx));
        assert_eq!(diff.hunks(), [hunk(0, 1, Change::Added, 0..0)]);
        assert!(diff.inflight, "the awaited job is still out");
    }

    #[test]
    fn a_result_matching_the_marks_on_screen_costs_no_repaint() {
        let (tx, _rx) = mpsc::channel();
        let doc = Document::from_str("a\n");
        let same = vec![hunk(0, 1, Change::Added, 0..0)];
        let mut diff = Diff::test_marks(same.clone());
        diff.inflight = true;
        diff.generation = 1;
        assert!(!diff.absorb(done(1, 0, same), &doc, &tx));

        diff.inflight = true;
        diff.generation = 2;
        assert!(diff.absorb(
            done(2, 0, vec![hunk(4, 5, Change::Changed, 4..5)]),
            &doc,
            &tx
        ));
        assert_eq!(diff.hunks(), [hunk(4, 5, Change::Changed, 4..5)]);
    }

    #[test]
    fn head_moving_during_a_lookup_is_not_answered_by_the_old_reply() {
        let (tx, _rx) = mpsc::channel();
        let doc = Document::from_str("a\n");
        let mut diff = Diff::test_marks(Vec::new());
        diff.inflight = true;
        diff.generation = 1;
        diff.stale = false; // the spawn cleared it
        diff.mark_stale(); // HEAD moved while the lookup was out
        diff.absorb(done(1, 0, Vec::new()), &doc, &tx);
        assert!(diff.stale, "the next pump must look again");
    }

    /// A throwaway repository holding one committed file, or `None` when
    /// this machine has no usable `git` — the same condition the feature
    /// degrades under, so the test degrades with it.
    fn scratch_repo(name: &str, content: &str) -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!("connor-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok()?;
        let file = dir.join("tracked.txt");
        std::fs::write(&file, content).ok()?;
        let run = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args([
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        (run(&["init"]) && run(&["add", "tracked.txt"]) && run(&["commit", "-m", "in"]))
            .then_some(dir)
    }

    #[test]
    fn a_committed_file_reads_back_its_head_content() {
        let Some(dir) = scratch_repo("head-blob", "a\nb\n") else {
            return;
        };
        let file = dir.join("tracked.txt");
        assert_eq!(head_blob(&file), Some(Rope::from_str("a\nb\n")));
        // The gutter's whole job, end to end: edit the working copy and the
        // marks describe it against what was committed.
        std::fs::write(&file, "a\nB\nc\n").unwrap();
        let buffer = Rope::from_str(&std::fs::read_to_string(&file).unwrap());
        assert_eq!(
            hunks(&head_blob(&file).unwrap(), &buffer),
            [hunk(1, 3, Change::Changed, 1..2)]
        );
        assert!(git_dir(&file).is_some_and(|d| d.ends_with(".git")));
        assert!(project::in_repo(&file));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_the_repository_has_never_seen_has_no_head_content() {
        let Some(dir) = scratch_repo("untracked", "a\n") else {
            return;
        };
        let new = dir.join("untracked.txt");
        std::fs::write(&new, "a\n").unwrap();
        assert_eq!(head_blob(&new), None);
        // Still in a repository, so the gutter keeps its mark column.
        assert!(project::in_repo(&new));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_path_outside_any_repository_has_no_head_content() {
        let dir = std::env::temp_dir().join(format!("connor-norepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("loose.txt");
        std::fs::write(&file, "a\n").unwrap();
        assert_eq!(head_blob(&file), None);
        assert_eq!(git_dir(&file), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
