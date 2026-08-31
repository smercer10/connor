use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, ErrorKind};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ropey::Rope;

use crate::grapheme;

/// Lines examined by convention detection: plenty for any real file, a
/// bound for absurd ones.
const DETECT_LINE_CAP: usize = 10_000;

/// Documents get process-unique ids: a stable identity for the crash
/// journal while tab indices shift as tabs open and close.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

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

/// One rope mutation in byte offsets and (row, byte-column) points, captured
/// before the mutation applied — the coordinates an incremental parser needs
/// to shift its tree. Plain data, so the document stays parser-agnostic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Splice {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_end_byte: usize,
    pub start_point: (usize, usize),
    pub old_end_point: (usize, usize),
    pub new_end_point: (usize, usize),
}

/// Pending splices past this are dropped and flagged: a consumer that far
/// behind reparses from scratch anyway, so the log stays bounded while a
/// background tab reloads repeatedly.
const SPLICE_CAP: usize = 256;

/// The region a reload replaced, in char indices: `[prefix, old_suffix_start)`
/// of the old text became `[prefix, new_suffix_start)` of the new — the
/// bounds views need to re-anchor positions by content.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChangeSpan {
    pub prefix: usize,
    pub old_suffix_start: usize,
    pub new_suffix_start: usize,
}

/// What `check_disk` found and did.
pub enum DiskCheck {
    /// Self-save, spurious event, or unreadable file; buffer untouched.
    Unchanged,
    /// Clean buffer swapped to the disk content; `old` is the text as it
    /// stood, for re-anchoring views.
    Reloaded { old: Rope, span: ChangeSpan },
    /// Dirty buffer kept its text; the conflict flag is up.
    Conflict,
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
    /// History position at the last save; 0 is the loaded state, and
    /// `usize::MAX` marks a saved state no history position reaches — it was
    /// truncated out of an abandoned redo branch.
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
        if self.saved_index > self.index {
            self.saved_index = usize::MAX;
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
    id: u64,
    rope: Rope,
    path: Option<PathBuf>,
    lossy: bool,
    line_ending: LineEnding,
    indent: IndentStyle,
    history: History,
    /// Bumped on every mutation (edit, undo, redo) and never repeated, so
    /// the crash journal can tell "changed since last snapshot" apart from
    /// a history index that undo walked back to.
    revision: u64,
    /// Restored from the crash journal and not yet saved.
    recovered: bool,
    /// FNV-1a of the bytes last seen on disk (loaded or saved); `None` when
    /// nothing is on disk yet. Lets a watch event tell an external change
    /// from our own save or a spurious wake.
    disk_hash: Option<u64>,
    /// Disk changed under a dirty buffer; cleared when buffer and disk
    /// reconverge (a save, or disk restored to the baseline).
    conflict: bool,
    /// Splices since the last `take_splices`, recorded only while a
    /// consumer tracks them — an untracked document pays one branch per
    /// edit and nothing more.
    pending: Vec<Splice>,
    track_splices: bool,
    /// The log hit `SPLICE_CAP` and was dropped; the consumer must rebuild
    /// rather than replay.
    splices_overflowed: bool,
}

impl Document {
    pub fn empty() -> Self {
        Document {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            rope: Rope::new(),
            path: None,
            lossy: false,
            line_ending: LineEnding::Lf,
            indent: IndentStyle::Spaces(4),
            history: History::default(),
            revision: 0,
            recovered: false,
            disk_hash: None,
            conflict: false,
            pending: Vec::new(),
            track_splices: false,
            splices_overflowed: false,
        }
    }

    /// Opens `path`. A missing file yields an empty document carrying that
    /// path, so saving can create it. Invalid UTF-8 loads lossily (bad bytes
    /// become U+FFFD) and is flagged so the status line can say so.
    pub fn open(path: PathBuf) -> io::Result<Self> {
        let (rope, lossy, disk_hash) = match File::open(&path) {
            Ok(file) => match Rope::from_reader(BufReader::new(file)) {
                // The load is verbatim, so the rope's chunks are the file's
                // bytes and hashing them here skips a second read.
                Ok(rope) => {
                    let hash = fnv1a(rope.chunks().map(str::as_bytes));
                    (rope, false, Some(hash))
                }
                Err(e) if e.kind() == ErrorKind::InvalidData => {
                    let bytes = std::fs::read(&path)?;
                    let hash = fnv1a(std::iter::once(bytes.as_slice()));
                    let rope = Rope::from_str(&String::from_utf8_lossy(&bytes));
                    (rope, true, Some(hash))
                }
                Err(e) => return Err(e),
            },
            Err(e) if e.kind() == ErrorKind::NotFound => (Rope::new(), false, None),
            Err(e) => return Err(e),
        };
        Ok(Document {
            line_ending: detect_line_ending(&rope),
            indent: detect_indent(&rope),
            rope,
            path: Some(path),
            lossy,
            disk_hash,
            ..Document::empty()
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

    #[cfg(test)]
    pub fn set_conflict(&mut self, conflict: bool) {
        self.conflict = conflict;
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Names a path-less buffer (or renames one) so `save` has somewhere to
    /// write; the next save creates or overwrites that file.
    pub fn set_path(&mut self, path: PathBuf) {
        self.path = Some(path);
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

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn recovered(&self) -> bool {
        self.recovered
    }

    /// Writes the text to the document's path atomically: an interrupted
    /// save leaves the old file intact, never a truncated one. The rope is
    /// written verbatim, so line endings and the trailing newline (or its
    /// absence) survive exactly as loaded. On success this position becomes
    /// the saved state and the lossy flag clears — the file now holds what
    /// the buffer shows.
    pub fn save(&mut self) -> io::Result<()> {
        let path = self.path.as_deref().ok_or_else(no_file_name)?;
        write_atomic(path, &self.rope)?;
        self.history.saved_index = self.history.index;
        // Later typing must not coalesce into a pre-save group, or dirty()
        // would report clean with post-save edits applied.
        self.history.open_kind = None;
        self.lossy = false;
        // The rope's chunks are exactly the bytes write_atomic just put on
        // disk, and the buffer now matches the file.
        self.disk_hash = Some(fnv1a(self.rope.chunks().map(str::as_bytes)));
        self.conflict = false;
        self.recovered = false;
        Ok(())
    }

    pub fn conflict(&self) -> bool {
        self.conflict
    }

    /// Re-reads the file after a watch event and reconciles the buffer with
    /// it. Never loses buffer text: a clean buffer reloads in place (as an
    /// undoable edit spanning just the changed region), a dirty one keeps
    /// its text and raises the conflict flag instead. Read errors leave
    /// everything untouched — deletion is usually a transient step of a
    /// rewrite, and the buffer is the user's copy of the data. `caret` is
    /// the viewing caret as it stands, recorded so undoing the reload
    /// restores it. A conflicted buffer undone back to clean stays stale
    /// until the next disk event or save — accepted gap.
    pub fn check_disk(&mut self, caret: Caret) -> DiskCheck {
        let Some(path) = self.path.as_deref() else {
            return DiskCheck::Unchanged;
        };
        let Ok(bytes) = fs::read(path) else {
            return DiskCheck::Unchanged;
        };
        let hash = fnv1a(std::iter::once(bytes.as_slice()));
        if Some(hash) == self.disk_hash {
            self.conflict = false;
            return DiskCheck::Unchanged;
        }
        if self.dirty() {
            self.conflict = true;
            return DiskCheck::Conflict;
        }
        let text = String::from_utf8_lossy(&bytes);
        let lossy = matches!(text, Cow::Owned(_));
        let span = change_span(&self.rope, &text);
        let old = self.rope.clone();
        let middle: String = text
            .chars()
            .skip(span.prefix)
            .take(span.new_suffix_start - span.prefix)
            .collect();
        // Distinct bytes can decode to identical chars (two lossy loads,
        // say); an empty edit would still open a no-op undo step.
        if span.prefix != span.old_suffix_start || !middle.is_empty() {
            self.edit(
                span.prefix..span.old_suffix_start,
                &middle,
                caret,
                EditKind::Other,
            );
            self.history.saved_index = self.history.index;
            self.history.open_kind = None;
        }
        self.line_ending = detect_line_ending(&self.rope);
        self.indent = detect_indent(&self.rope);
        self.lossy = lossy;
        self.disk_hash = Some(hash);
        self.conflict = false;
        DiskCheck::Reloaded { old, span }
    }

    /// Splices crash-journal `text` over the loaded content as one
    /// undoable edit that leaves the buffer dirty — the journal holds
    /// unsaved work, and undo walks back to what the disk held. Returns
    /// false when the journal already matches the buffer: nothing was
    /// unsaved after all, and the buffer stays clean and unmarked.
    pub fn restore_journal(&mut self, text: &str) -> bool {
        let span = change_span(&self.rope, text);
        let middle: String = text
            .chars()
            .skip(span.prefix)
            .take(span.new_suffix_start - span.prefix)
            .collect();
        if span.prefix == span.old_suffix_start && middle.is_empty() {
            return false;
        }
        let caret = Caret {
            cursor: 0,
            anchor: None,
        };
        self.edit(
            span.prefix..span.old_suffix_start,
            &middle,
            caret,
            EditKind::Other,
        );
        self.line_ending = detect_line_ending(&self.rope);
        self.indent = detect_indent(&self.rope);
        self.recovered = true;
        true
    }

    /// Replaces `range` with `text` — the single mutation entry point, so
    /// undo recording and coalescing see every change. `caret` is the caret
    /// as it stood before the edit; `kind` hints coalescing. Returns the
    /// cursor after the edit, snapped to a cluster boundary in case the
    /// insertion merged with a following combining mark.
    pub fn edit(&mut self, range: Range<usize>, text: &str, caret: Caret, kind: EditKind) -> usize {
        let deleted = self.rope.slice(range.clone()).to_string();
        self.splice(range.clone(), text);
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

    /// The one place the rope mutates: replaces `range` with `text`, records
    /// the splice for any tracking consumer, and bumps the revision. Undo
    /// and redo replay history through here too, so the log misses nothing.
    fn splice(&mut self, range: Range<usize>, text: &str) {
        if self.track_splices && !self.splices_overflowed {
            if self.pending.len() >= SPLICE_CAP {
                // Once overflowed the log is useless, so recording stays off
                // until `take_splices` resets the flag.
                self.pending.clear();
                self.splices_overflowed = true;
            } else {
                let start_byte = self.rope.char_to_byte(range.start);
                let old_end_byte = self.rope.char_to_byte(range.end);
                let start_point = self.point_of(start_byte);
                let new_end_point = match text.rfind('\n') {
                    None => (start_point.0, start_point.1 + text.len()),
                    Some(last) => {
                        let rows = text.bytes().filter(|&b| b == b'\n').count();
                        (start_point.0 + rows, text.len() - (last + 1))
                    }
                };
                self.pending.push(Splice {
                    start_byte,
                    old_end_byte,
                    new_end_byte: start_byte + text.len(),
                    start_point,
                    old_end_point: self.point_of(old_end_byte),
                    new_end_point,
                });
            }
        }
        self.rope.remove(range.clone());
        self.rope.insert(range.start, text);
        self.revision += 1;
    }

    /// (row, byte-column) of a byte offset, pre-mutation.
    fn point_of(&self, byte: usize) -> (usize, usize) {
        let row = self.rope.byte_to_line(byte);
        (row, byte - self.rope.line_to_byte(row))
    }

    /// Starts recording splices for `take_splices`; in force for the rest of
    /// the document's life.
    pub fn track_splices(&mut self) {
        self.track_splices = true;
    }

    /// Drains the recorded splices, oldest first. The flag reports that the
    /// log overflowed and was dropped: what remains is incomplete and the
    /// consumer must rebuild from the rope instead of replaying.
    pub fn take_splices(&mut self) -> (std::vec::Drain<'_, Splice>, bool) {
        let overflowed = std::mem::take(&mut self.splices_overflowed);
        (self.pending.drain(..), overflowed)
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
        let index = self.history.index;
        let group = &mut self.history.groups[index];
        let at = group.edit.at;
        let end = at + group.edit.inserted.chars().count();
        let caret_before = group.caret_before;
        // Taken and put back rather than cloned: undoing a large splice (a
        // reload, a replace-all) must not copy its text.
        let deleted = std::mem::take(&mut group.edit.deleted);
        self.splice(at..end, &deleted);
        self.history.groups[index].edit.deleted = deleted;
        Some(caret_before)
    }

    /// Re-applies the next undone group and hands back the caret to restore.
    pub fn redo(&mut self) -> Option<Caret> {
        self.history.open_kind = None;
        let index = self.history.index;
        let group = self.history.groups.get_mut(index)?;
        let at = group.edit.at;
        let end = at + group.edit.deleted.chars().count();
        let cursor = group.cursor_after;
        let inserted = std::mem::take(&mut group.edit.inserted);
        self.splice(at..end, &inserted);
        self.history.groups[index].edit.inserted = inserted;
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

/// FNV-1a 64. Hand-rolled because the baseline comparison needs a hash
/// that folds byte-at-a-time — the same bytes must hash equally whether fed
/// as rope chunks or one read buffer, which `std::hash::Hasher` does not
/// promise across chunkings. The input isn't adversarial.
fn fnv1a<'a>(chunks: impl Iterator<Item = &'a [u8]>) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for chunk in chunks {
        for &byte in chunk {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
    }
    hash
}

/// The common prefix and suffix between the buffer and freshly read text,
/// as a `ChangeSpan` in chars. The suffix scan is capped so the regions
/// never overlap when text was purely inserted or deleted.
fn change_span(old: &Rope, new: &str) -> ChangeSpan {
    let old_len = old.len_chars();
    let new_len = new.chars().count();
    let mut prefix = 0;
    for (a, b) in old.chars().zip(new.chars()) {
        if a != b {
            break;
        }
        prefix += 1;
    }
    let limit = old_len.min(new_len) - prefix;
    let mut suffix = 0;
    for (a, b) in old.chars_at(old_len).reversed().zip(new.chars().rev()) {
        if a != b || suffix == limit {
            break;
        }
        suffix += 1;
    }
    ChangeSpan {
        prefix,
        old_suffix_start: old_len - suffix,
        new_suffix_start: new_len - suffix,
    }
}

fn no_file_name() -> io::Error {
    io::Error::other("no file name")
}

/// The real file a save must land on: symlinks followed so the target is
/// replaced rather than the link, path made absolute so the temp file lands
/// beside it. A missing file resolves within its parent directory.
fn resolve_target(path: &Path) -> io::Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(target) => Ok(target),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let name = path.file_name().ok_or_else(no_file_name)?;
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            Ok(fs::canonicalize(parent)?.join(name))
        }
        Err(e) => Err(e),
    }
}

/// Creates the temp file the save writes into, beside the target so the
/// rename never crosses filesystems. `create_new` guarantees a fresh file;
/// collisions (a dead editor's leftovers) move on to the next suffix.
fn create_temp(dir: &Path, name: &OsStr) -> io::Result<(PathBuf, File)> {
    let mut attempt = 0;
    loop {
        let mut temp_name = OsString::from(".");
        temp_name.push(name);
        temp_name.push(format!(".connor-{}-{attempt}.tmp", std::process::id()));
        let temp_path = dir.join(temp_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists && attempt < 100 => attempt += 1,
            Err(e) => return Err(e),
        }
    }
}

/// Write-to-temp-then-rename: a crash at any point leaves either the old
/// file or the new one on disk, never a mix or a truncation.
fn write_atomic(path: &Path, rope: &Rope) -> io::Result<()> {
    let target = resolve_target(path)?;
    let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else {
        return Err(no_file_name());
    };
    let (temp_path, file) = create_temp(dir, name)?;
    let result = write_and_swap(file, &temp_path, &target, dir, rope);
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn write_and_swap(
    file: File,
    temp_path: &Path,
    target: &Path,
    dir: &Path,
    rope: &Rope,
) -> io::Result<()> {
    // The rename carries the temp file's permissions, so an existing
    // target's must be copied over first.
    if let Ok(meta) = fs::metadata(target) {
        file.set_permissions(meta.permissions())?;
    }
    let mut writer = BufWriter::new(file);
    rope.write_to(&mut writer)?;
    let file = writer
        .into_inner()
        .map_err(io::IntoInnerError::into_error)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temp_path, target)?;
    // Syncing the directory makes the rename itself durable across power
    // loss. Best-effort: failure (or a platform that can't open a
    // directory) costs durability, never integrity.
    if let Ok(dir) = File::open(dir) {
        let _ = dir.sync_all();
    }
    Ok(())
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

    /// A fresh scratch directory per test: tests run in parallel, so each
    /// needs its own.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("connor-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_writes_exact_bytes_and_leaves_no_temp_file() {
        let dir = scratch_dir("save-exact");
        let path = dir.join("f.txt");
        std::fs::write(&path, "a\r\nb").unwrap();
        let mut doc = Document::open(path.clone()).unwrap();
        doc.edit(0..0, "X", caret(0), EditKind::Insert);
        assert!(doc.dirty());
        doc.save().unwrap();
        assert!(!doc.dirty());
        // CRLF and the absent trailing newline both survive.
        assert_eq!(std::fs::read(&path).unwrap(), b"Xa\r\nb");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_creates_a_missing_file() {
        let dir = scratch_dir("save-create");
        let path = dir.join("new.txt");
        let mut doc = Document::open(path.clone()).unwrap();
        doc.edit(0..0, "hi\n", caret(0), EditKind::Insert);
        doc.save().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hi\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_without_a_path_errors() {
        let mut doc = Document::empty();
        doc.edit(0..0, "a", caret(0), EditKind::Insert);
        assert!(doc.save().is_err());
        assert!(doc.dirty());
    }

    #[test]
    fn undo_and_redo_cross_the_saved_state() {
        let dir = scratch_dir("save-undo");
        let mut doc = Document::open(dir.join("f.txt")).unwrap();
        doc.edit(0..0, "a", caret(0), EditKind::Insert);
        doc.save().unwrap();
        doc.break_undo_group();
        doc.edit(1..1, "b", caret(1), EditKind::Insert);
        assert!(doc.dirty());
        doc.undo();
        assert!(!doc.dirty()); // back at the saved state
        doc.undo();
        assert!(doc.dirty()); // before it
        doc.redo();
        assert!(!doc.dirty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn typing_after_save_does_not_coalesce_across_it() {
        let dir = scratch_dir("save-coalesce");
        let mut doc = Document::open(dir.join("f.txt")).unwrap();
        doc.edit(0..0, "a", caret(0), EditKind::Insert);
        doc.save().unwrap();
        doc.edit(1..1, "b", caret(1), EditKind::Insert);
        assert!(doc.dirty());
        doc.undo();
        assert_eq!(doc.rope().to_string(), "a");
        assert!(!doc.dirty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn truncating_the_saved_state_out_of_a_redo_branch_stays_dirty() {
        let dir = scratch_dir("save-truncate");
        let mut doc = Document::open(dir.join("f.txt")).unwrap();
        doc.edit(0..0, "a", caret(0), EditKind::Insert);
        doc.save().unwrap();
        doc.undo();
        doc.edit(0..0, "b", caret(0), EditKind::Insert);
        assert!(doc.dirty()); // same history position, different content
        doc.undo();
        assert!(doc.dirty()); // the saved state is unreachable now
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_clears_lossy() {
        let dir = scratch_dir("save-lossy");
        let path = dir.join("g.bin");
        std::fs::write(&path, b"ok\xFFbad").unwrap();
        let mut doc = Document::open(path.clone()).unwrap();
        assert!(doc.lossy());
        doc.save().unwrap();
        assert!(!doc.lossy());
        assert_eq!(std::fs::read(&path).unwrap(), "ok\u{FFFD}bad".as_bytes());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn save_preserves_permissions_and_follows_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch_dir("save-unix");
        let target = dir.join("real.sh");
        std::fs::write(&target, "old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = dir.join("link.sh");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let mut doc = Document::open(link.clone()).unwrap();
        doc.edit(0..3, "new", caret(3), EditKind::Other);
        doc.save().unwrap();

        assert!(link.symlink_metadata().unwrap().is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        std::fs::remove_dir_all(&dir).unwrap();
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

    #[test]
    fn fnv1a_is_chunk_invariant() {
        let whole = fnv1a(std::iter::once(b"abcd".as_slice()));
        let split = fnv1a([b"a".as_slice(), b"bc".as_slice(), b"d".as_slice()].into_iter());
        assert_eq!(whole, split);
        assert_ne!(whole, fnv1a(std::iter::once(b"abce".as_slice())));
    }

    fn span(old: &str, new: &str) -> ChangeSpan {
        change_span(&Rope::from_str(old), new)
    }

    #[test]
    fn change_span_finds_the_replaced_middle() {
        assert_eq!(
            span("axxxb", "ayb"),
            ChangeSpan {
                prefix: 1,
                old_suffix_start: 4,
                new_suffix_start: 2
            }
        );
        assert_eq!(
            span("abc", "xyz"),
            ChangeSpan {
                prefix: 0,
                old_suffix_start: 3,
                new_suffix_start: 3
            }
        );
    }

    #[test]
    fn change_span_handles_pure_insertion_and_deletion() {
        // "hello world" -> "hello brave world": the suffix scan would match
        // six chars but is capped so the regions never overlap.
        assert_eq!(
            span("hello world", "hello brave world"),
            ChangeSpan {
                prefix: 6,
                old_suffix_start: 6,
                new_suffix_start: 12
            }
        );
        assert_eq!(
            span("abcabc", "abc"),
            ChangeSpan {
                prefix: 3,
                old_suffix_start: 6,
                new_suffix_start: 3
            }
        );
        assert_eq!(
            span("aba", "aa"),
            ChangeSpan {
                prefix: 1,
                old_suffix_start: 2,
                new_suffix_start: 1
            }
        );
    }

    #[test]
    fn check_disk_reloads_a_clean_buffer_in_place() {
        let dir = scratch_dir("reload-clean");
        let path = dir.join("f.txt");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let mut doc = Document::open(path.clone()).unwrap();
        std::fs::write(&path, "one\nrewritten\ntwo\n").unwrap();
        let check = doc.check_disk(caret(0));
        assert!(matches!(check, DiskCheck::Reloaded { .. }));
        assert_eq!(doc.rope().to_string(), "one\nrewritten\ntwo\n");
        assert!(!doc.dirty());
        assert!(!doc.conflict());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn check_disk_redetects_conventions_and_lossiness() {
        let dir = scratch_dir("reload-detect");
        let path = dir.join("f.txt");
        std::fs::write(&path, "a\nb\n").unwrap();
        let mut doc = Document::open(path.clone()).unwrap();
        assert_eq!(doc.line_ending(), LineEnding::Lf);
        std::fs::write(&path, b"a\r\nb\r\n\xFF\r\n").unwrap();
        assert!(matches!(
            doc.check_disk(caret(0)),
            DiskCheck::Reloaded { .. }
        ));
        assert_eq!(doc.line_ending(), LineEnding::Crlf);
        assert!(doc.lossy());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn check_disk_flags_a_dirty_buffer_instead_of_reloading() {
        let dir = scratch_dir("reload-dirty");
        let path = dir.join("f.txt");
        std::fs::write(&path, "base\n").unwrap();
        let mut doc = Document::open(path.clone()).unwrap();
        doc.edit(0..0, "mine ", caret(0), EditKind::Insert);
        std::fs::write(&path, "theirs\n").unwrap();
        assert!(matches!(doc.check_disk(caret(0)), DiskCheck::Conflict));
        assert_eq!(doc.rope().to_string(), "mine base\n");
        assert!(doc.conflict());
        // Disk restored to the baseline: the conflict dissolves.
        std::fs::write(&path, "base\n").unwrap();
        assert!(matches!(doc.check_disk(caret(0)), DiskCheck::Unchanged));
        assert!(!doc.conflict());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_clears_the_conflict_and_suppresses_its_own_event() {
        let dir = scratch_dir("reload-save");
        let path = dir.join("f.txt");
        std::fs::write(&path, "base\n").unwrap();
        let mut doc = Document::open(path.clone()).unwrap();
        doc.edit(0..0, "mine ", caret(0), EditKind::Insert);
        std::fs::write(&path, "theirs\n").unwrap();
        assert!(matches!(doc.check_disk(caret(0)), DiskCheck::Conflict));
        doc.save().unwrap();
        assert!(!doc.conflict());
        // The watch event our own save fires must find nothing to do.
        assert!(matches!(doc.check_disk(caret(0)), DiskCheck::Unchanged));
        assert_eq!(doc.rope().to_string(), "mine base\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn check_disk_reload_is_undoable_back_to_the_buffer_text() {
        let dir = scratch_dir("reload-undo");
        let path = dir.join("f.txt");
        std::fs::write(&path, "one\n").unwrap();
        let mut doc = Document::open(path.clone()).unwrap();
        std::fs::write(&path, "two\n").unwrap();
        assert!(matches!(
            doc.check_disk(caret(0)),
            DiskCheck::Reloaded { .. }
        ));
        doc.undo();
        assert_eq!(doc.rope().to_string(), "one\n");
        assert!(doc.dirty()); // recovered text no longer matches disk
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn check_disk_leaves_a_deleted_file_alone() {
        let dir = scratch_dir("reload-deleted");
        let path = dir.join("f.txt");
        std::fs::write(&path, "keep\n").unwrap();
        let mut doc = Document::open(path.clone()).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(doc.check_disk(caret(0)), DiskCheck::Unchanged));
        assert_eq!(doc.rope().to_string(), "keep\n");
        assert!(!doc.conflict());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ids_are_unique_across_documents() {
        assert_ne!(Document::empty().id(), Document::empty().id());
    }

    #[test]
    fn revision_moves_on_every_mutation_and_only_on_mutation() {
        let mut doc = Document::from_str("ab");
        let start = doc.revision();
        doc.edit(0..0, "x", caret(0), EditKind::Insert);
        assert_eq!(doc.revision(), start + 1);
        doc.undo();
        assert_eq!(doc.revision(), start + 2);
        assert!(doc.undo().is_none()); // nothing left to undo
        assert_eq!(doc.revision(), start + 2);
        doc.redo();
        assert_eq!(doc.revision(), start + 3);
        assert!(doc.redo().is_none()); // nothing left to redo
        assert_eq!(doc.revision(), start + 3);
    }

    #[test]
    fn restore_journal_leaves_a_dirty_marked_buffer_undoable_to_disk() {
        let dir = scratch_dir("restore-basic");
        let path = dir.join("f.txt");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let mut doc = Document::open(path.clone()).unwrap();
        assert!(doc.restore_journal("one\nedited\ntwo\n"));
        assert_eq!(doc.rope().to_string(), "one\nedited\ntwo\n");
        assert!(doc.dirty());
        assert!(doc.recovered());
        doc.undo();
        assert_eq!(doc.rope().to_string(), "one\ntwo\n");
        assert!(!doc.dirty()); // back at what disk holds
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn restore_journal_with_matching_text_is_a_clean_no_op() {
        let mut doc = Document::from_str("same\n");
        assert!(!doc.restore_journal("same\n"));
        assert!(!doc.dirty());
        assert!(!doc.recovered());
    }

    #[test]
    fn restore_journal_fills_an_empty_document() {
        let mut doc = Document::empty();
        assert!(doc.restore_journal("lost\r\nwork\r\n"));
        assert_eq!(doc.rope().to_string(), "lost\r\nwork\r\n");
        assert!(doc.dirty());
        assert_eq!(doc.line_ending(), LineEnding::Crlf); // conventions re-detected
    }

    #[test]
    fn save_clears_recovered() {
        let dir = scratch_dir("restore-save");
        let path = dir.join("f.txt");
        std::fs::write(&path, "base\n").unwrap();
        let mut doc = Document::open(path.clone()).unwrap();
        assert!(doc.restore_journal("changed\n"));
        doc.save().unwrap();
        assert!(!doc.recovered());
        assert_eq!(std::fs::read(&path).unwrap(), b"changed\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn splices_of(doc: &mut Document) -> (Vec<Splice>, bool) {
        let (drain, overflowed) = doc.take_splices();
        (drain.collect(), overflowed)
    }

    #[test]
    fn untracked_documents_record_no_splices() {
        let mut doc = Document::from_str("ab");
        doc.edit(0..0, "x", caret(0), EditKind::Insert);
        doc.undo();
        let (splices, overflowed) = splices_of(&mut doc);
        assert!(splices.is_empty());
        assert!(!overflowed);
    }

    #[test]
    fn edits_record_byte_correct_splices() {
        let mut doc = Document::from_str("a日\nb");
        doc.track_splices();
        // Replace the wide char (bytes 1..4) with two lines of text.
        doc.edit(1..2, "x\nyz", caret(1), EditKind::Other);
        assert_eq!(doc.rope().to_string(), "ax\nyz\nb");
        let (splices, overflowed) = splices_of(&mut doc);
        assert!(!overflowed);
        assert_eq!(
            splices,
            vec![Splice {
                start_byte: 1,
                old_end_byte: 4,
                new_end_byte: 5,
                start_point: (0, 1),
                old_end_point: (0, 4),
                new_end_point: (1, 2),
            }]
        );
    }

    #[test]
    fn undo_and_redo_record_their_splices() {
        let mut doc = Document::from_str("ab\ncd");
        doc.track_splices();
        doc.edit(3..5, "x", caret(3), EditKind::Other);
        let edit = splices_of(&mut doc).0[0];
        assert_eq!(edit.start_point, (1, 0));

        doc.undo();
        let (splices, _) = splices_of(&mut doc);
        assert_eq!(
            splices,
            vec![Splice {
                start_byte: 3,
                old_end_byte: 4,
                new_end_byte: 5,
                start_point: (1, 0),
                old_end_point: (1, 1),
                new_end_point: (1, 2),
            }]
        );
        assert_eq!(doc.rope().to_string(), "ab\ncd");

        doc.redo();
        let (splices, _) = splices_of(&mut doc);
        assert_eq!(splices, vec![edit]);
        assert_eq!(doc.rope().to_string(), "ab\nx");
    }

    #[test]
    fn reload_records_a_single_splice() {
        let dir = scratch_dir("splice-reload");
        let path = dir.join("f.txt");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let mut doc = Document::open(path.clone()).unwrap();
        doc.track_splices();
        std::fs::write(&path, "one\nTWO\n").unwrap();
        assert!(matches!(
            doc.check_disk(caret(0)),
            DiskCheck::Reloaded { .. }
        ));
        let (splices, overflowed) = splices_of(&mut doc);
        assert!(!overflowed);
        assert_eq!(splices.len(), 1);
        assert_eq!(splices[0].start_byte, 4);
        assert_eq!(splices[0].start_point, (1, 0));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn splice_log_overflow_drops_and_flags() {
        let mut doc = Document::from_str("");
        doc.track_splices();
        for _ in 0..SPLICE_CAP + 5 {
            doc.edit(0..0, "x", caret(0), EditKind::Other);
            doc.break_undo_group();
        }
        let (splices, overflowed) = splices_of(&mut doc);
        assert!(splices.is_empty());
        assert!(overflowed);
        // The next edit records normally again.
        doc.edit(0..0, "y", caret(0), EditKind::Other);
        let (splices, overflowed) = splices_of(&mut doc);
        assert_eq!(splices.len(), 1);
        assert!(!overflowed);
    }

    #[test]
    fn crlf_terminators_count_as_line_breaks_in_splice_points() {
        let mut doc = Document::from_str("ab\r\ncd");
        doc.track_splices();
        doc.edit(5..6, "e\r\nf", caret(5), EditKind::Other);
        let (splices, _) = splices_of(&mut doc);
        assert_eq!(
            splices,
            vec![Splice {
                start_byte: 5,
                old_end_byte: 6,
                new_end_byte: 9,
                start_point: (1, 1),
                old_end_point: (1, 2),
                new_end_point: (2, 1),
            }]
        );
    }
}
