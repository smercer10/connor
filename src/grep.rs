//! Project-wide search: a background grep that walks the project with the
//! picker's ignore rules, and the overlay state its hits stream into. Pure
//! state plus the worker — the overlay drawing lives elsewhere.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::search::simple_fold;
use crate::watch::AppEvent;

/// One matching line; `col` is the char index of the match in the line.
pub struct Hit {
    pub line: u32,
    pub col: u32,
    pub preview: String,
}

/// Every hit found in one file, root-relative; never split across batches,
/// so hits arrive grouped by construction.
pub struct FileHits {
    pub path: String,
    pub hits: Vec<Hit>,
}

/// A chunk of search hits; `done` marks the search's last chunk, and
/// `truncated` that the global cap ended it early.
pub struct HitBatch {
    pub generation: u64,
    pub files: Vec<FileHits>,
    pub done: bool,
    pub truncated: bool,
}

/// Files larger than this are skipped: they are artifacts, not code, and
/// reading them would starve the stream.
const MAX_FILE_LEN: u64 = 4 << 20;

/// A NUL among this many leading bytes marks a file binary (grep's
/// convention) and skips it.
const SNIFF: usize = 1024;

/// Chars of preview stored per hit; a minified line must not ship whole.
const PREVIEW_MAX: usize = 200;

/// Chars kept ahead of the match when a long line's preview is windowed.
const PREVIEW_BEFORE: usize = 24;

/// Hits per file before the rest of that file is left unscanned.
const FILE_HITS: usize = 100;

/// Hits per search before it stops: an over-broad query saturates and ends
/// instead of swallowing the repo.
const MAX_HITS: usize = 2000;

/// Flush pending hits at this count, or at `FLUSH_EVERY` — whichever comes
/// first, so sparse matches still paint promptly.
const FLUSH_HITS: usize = 128;
const FLUSH_EVERY: Duration = Duration::from_millis(25);

/// The pause after a query edit before its search spawns; each edit re-arms
/// it, so one crawl serves a typing burst.
const RESTART: Duration = Duration::from_millis(50);

/// The query prepared once: smart case as in-buffer search — any uppercase
/// makes matching exact, else chars are pre-folded for caseless compare.
struct Needle {
    chars: Vec<char>,
    exact: bool,
}

impl Needle {
    fn new(query: &str) -> Needle {
        let exact = query.chars().any(char::is_uppercase);
        let fold = |ch| if exact { ch } else { simple_fold(ch) };
        Needle {
            chars: query.chars().map(fold).collect(),
            exact,
        }
    }
}

/// The char index of the first match in `line`, sharing in-buffer search's
/// semantics. The first-char gate keeps the clone-and-verify off most
/// positions.
fn find_in_line(line: &str, needle: &Needle) -> Option<usize> {
    let (&first, rest) = needle.chars.split_first()?;
    let fold = |ch| if needle.exact { ch } else { simple_fold(ch) };
    let mut iter = line.chars();
    let mut pos = 0;
    loop {
        let mut probe = iter.clone();
        if fold(probe.next()?) == first
            && rest
                .iter()
                .all(|&qc| probe.next().is_some_and(|dc| fold(dc) == qc))
        {
            return Some(pos);
        }
        iter.next();
        pos += 1;
    }
}

/// The stored slice of a matching line: control chars flattened to spaces
/// so drawn widths stay honest, at most `PREVIEW_MAX` chars, windowed so a
/// match deep in a long line stays inside what's stored (`…` marks the
/// cut).
fn preview(line: &str, col: usize) -> String {
    let skip = if col < PREVIEW_MAX {
        0
    } else {
        col - PREVIEW_BEFORE
    };
    let mut out = String::new();
    if skip > 0 {
        out.push('…');
    }
    out.extend(
        line.chars()
            .skip(skip)
            .take(PREVIEW_MAX)
            .map(|ch| if ch.is_control() { ' ' } else { ch }),
    );
    out
}

/// Greps one file: oversized and binary files skipped, the rest decoded
/// lossily and scanned for the first match per line, up to `FILE_HITS`.
fn grep_file(path: &Path, buf: &mut Vec<u8>, needle: &Needle) -> Option<Vec<Hit>> {
    let mut file = File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_FILE_LEN {
        return None;
    }
    buf.clear();
    file.read_to_end(buf).ok()?;
    if buf.iter().take(SNIFF).any(|&b| b == 0) {
        return None;
    }
    let text = String::from_utf8_lossy(buf);
    let mut hits = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if let Some(col) = find_in_line(line, needle) {
            hits.push(Hit {
                line: (i + 1) as u32,
                col: col as u32,
                preview: preview(line, col),
            });
            if hits.len() == FILE_HITS {
                break;
            }
        }
    }
    (!hits.is_empty()).then_some(hits)
}

/// Runs the search on its own thread, streaming batches into `tx`. Returns
/// the cancel flag; setting it ends the walk within one entry. The thread
/// owns no editor state and also exits when the receiver is gone.
pub fn spawn_search(
    root: PathBuf,
    query: String,
    generation: u64,
    tx: Sender<AppEvent>,
) -> Arc<AtomicBool> {
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    thread::spawn(move || search(&root, &query, generation, &flag, &tx));
    cancel
}

/// The search body: the picker's walk — hidden files skipped, `.gitignore`
/// honored, symlinks not followed — with each file grepped in place.
fn search(root: &Path, query: &str, generation: u64, cancel: &AtomicBool, tx: &Sender<AppEvent>) {
    let needle = Needle::new(query);
    let mut files: Vec<FileHits> = Vec::new();
    let mut buf = Vec::new();
    let mut pending = 0;
    let mut total = 0;
    let mut truncated = false;
    let mut last_flush = Instant::now();
    if !needle.chars.is_empty() {
        for entry in ignore::WalkBuilder::new(root).build() {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let Ok(entry) = entry else { continue };
            if entry.depth() == 0 || entry.file_type().is_none_or(|t| t.is_dir()) {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(root) else {
                continue;
            };
            let Some(rel) = rel.to_str() else { continue };
            if let Some(mut hits) = grep_file(entry.path(), &mut buf, &needle) {
                if total + hits.len() >= MAX_HITS {
                    hits.truncate(MAX_HITS - total);
                    truncated = true;
                }
                total += hits.len();
                pending += hits.len();
                files.push(FileHits {
                    path: rel.to_string(),
                    hits,
                });
                if truncated {
                    break;
                }
            }
            if pending > 0 && (pending >= FLUSH_HITS || last_flush.elapsed() >= FLUSH_EVERY) {
                let batch = HitBatch {
                    generation,
                    files: std::mem::take(&mut files),
                    done: false,
                    truncated: false,
                };
                if tx.send(AppEvent::Hits(batch)).is_err() {
                    return;
                }
                pending = 0;
                last_flush = Instant::now();
            }
        }
    }
    let _ = tx.send(AppEvent::Hits(HitBatch {
        generation,
        files,
        done: true,
        truncated,
    }));
}

pub enum Outcome {
    Pending,
    /// Jump to the 1-based `line` (caret at char `col`) of `path`.
    Open {
        path: PathBuf,
        line: usize,
        col: usize,
    },
    Cancel,
}

/// One drawable row, borrowed from the state.
pub enum Row<'a> {
    /// A file's header: its root-relative path.
    File(&'a str),
    Hit {
        line: u32,
        preview: &'a str,
        selected: bool,
    },
}

pub struct Grep {
    root: PathBuf,
    generation: u64,
    cancel: Arc<AtomicBool>,
    query: String,
    /// Hits grouped by file, in arrival order.
    files: Vec<FileHits>,
    /// Each file's header display row; its hit rows follow it.
    starts: Vec<u32>,
    /// Total hits across `files`.
    hits: usize,
    /// Global hit index — file headers are labels, not targets.
    selected: usize,
    searching: bool,
    truncated: bool,
    /// When the current query's search should spawn; each edit re-arms it.
    restart: Option<Instant>,
}

impl Grep {
    pub fn new(root: PathBuf) -> Grep {
        Grep {
            root,
            // Walk generations start at 1; 0 can never match a batch.
            generation: 0,
            cancel: Arc::default(),
            query: String::new(),
            files: Vec::new(),
            starts: Vec::new(),
            hits: 0,
            selected: 0,
            searching: false,
            truncated: false,
            restart: None,
        }
    }

    /// Feeds one keypress: printable characters extend the query, ↑↓ move
    /// the selection. Anything else is ignored so a stray chord can't
    /// dismiss the overlay.
    pub fn key(&mut self, key: &KeyEvent, now: Instant) -> Outcome {
        match key.code {
            KeyCode::Enter => {
                if self.selected < self.hits {
                    let f = self.file_of_hit(self.selected);
                    let file = &self.files[f];
                    let hit = &file.hits[self.selected - (self.starts[f] as usize - f)];
                    self.dismiss();
                    return Outcome::Open {
                        path: self.root.join(&file.path),
                        line: hit.line as usize,
                        col: hit.col as usize,
                    };
                }
            }
            KeyCode::Esc => {
                self.dismiss();
                return Outcome::Cancel;
            }
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(self.hits.max(1) - 1),
            KeyCode::Backspace => {
                if self.query.pop().is_some() {
                    self.edited(now);
                }
            }
            // Ctrl- and Alt-modified characters are chords, not input.
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(ch);
                self.edited(now);
            }
            _ => {}
        }
        Outcome::Pending
    }

    /// Appends a paste flattened to a single line: control characters,
    /// line breaks included, are dropped.
    pub fn paste(&mut self, text: &str, now: Instant) {
        let flat: String = text.chars().filter(|ch| !ch.is_control()).collect();
        if !flat.is_empty() {
            self.query.push_str(&flat);
            self.edited(now);
        }
    }

    /// A query edit: the running search stops now, its results leave the
    /// screen, and the next search waits out the restart pause.
    fn edited(&mut self, now: Instant) {
        self.cancel.store(true, Ordering::Relaxed);
        self.files.clear();
        self.starts.clear();
        self.hits = 0;
        self.selected = 0;
        self.searching = false;
        self.truncated = false;
        self.restart = (!self.query.is_empty()).then(|| now + RESTART);
    }

    /// The armed restart's due time, for the main loop's wake computation.
    pub fn deadline(&self) -> Option<Instant> {
        self.restart
    }

    /// Disarms a due restart and hands back the query to search for.
    pub fn take_restart(&mut self, now: Instant) -> Option<String> {
        if self.restart.is_some_and(|t| t <= now) {
            self.restart = None;
            return Some(self.query.clone());
        }
        None
    }

    /// Adopts the search just spawned for this overlay's query.
    pub fn begin(&mut self, generation: u64, cancel: Arc<AtomicBool>) {
        self.generation = generation;
        self.cancel = cancel;
        self.searching = true;
    }

    /// Folds a hit batch in; false means nothing on screen changes — the
    /// batch belonged to a previous search.
    pub fn absorb(&mut self, batch: HitBatch) -> bool {
        if batch.generation != self.generation {
            return false;
        }
        self.searching = !batch.done;
        self.truncated |= batch.truncated;
        for file in batch.files {
            self.starts.push((self.files.len() + self.hits) as u32);
            self.hits += file.hits.len();
            self.files.push(file);
        }
        true
    }

    /// Stops the search; closing the overlay for any reason calls this.
    pub fn dismiss(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// The file whose hit block holds global hit `h`.
    fn file_of_hit(&self, h: usize) -> usize {
        let (mut lo, mut hi) = (0, self.files.len());
        while hi - lo > 1 {
            let mid = lo.midpoint(hi);
            // `starts[f] - f` is the count of hits in files before `f`.
            if self.starts[mid] as usize - mid <= h {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Total drawable rows: a header per file plus a row per hit.
    pub fn display_len(&self) -> usize {
        self.files.len() + self.hits
    }

    /// The row shown at display index `i`.
    pub fn row(&self, i: usize) -> Row<'_> {
        let f = self.starts.partition_point(|&s| s as usize <= i) - 1;
        let file = &self.files[f];
        let offset = i - self.starts[f] as usize;
        if offset == 0 {
            return Row::File(&file.path);
        }
        let hit = &file.hits[offset - 1];
        Row::Hit {
            line: hit.line,
            preview: &hit.preview,
            selected: self.starts[f] as usize - f + offset - 1 == self.selected,
        }
    }

    /// The selected hit's display row; meaningless while there are none.
    pub fn selected_display_row(&self) -> usize {
        // Hit `h` in file `f` sits below `f + 1` headers.
        self.selected + self.file_of_hit(self.selected) + 1
    }

    pub fn hit_count(&self) -> usize {
        self.hits
    }

    pub fn searching(&self) -> bool {
        self.searching
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc;

    fn needle(query: &str) -> Needle {
        Needle::new(query)
    }

    #[test]
    fn a_lowercase_query_matches_any_case() {
        assert_eq!(find_in_line("The FIND spot", &needle("find")), Some(4));
        assert_eq!(find_in_line("Немного Текста", &needle("текст")), Some(8));
    }

    #[test]
    fn an_uppercase_query_matches_exactly() {
        assert_eq!(find_in_line("find or Find", &needle("Find")), Some(8));
        assert_eq!(find_in_line("find only", &needle("Find")), None);
    }

    #[test]
    fn the_column_counts_chars_not_bytes() {
        assert_eq!(find_in_line("ééé x", &needle("x")), Some(4));
    }

    #[test]
    fn an_empty_query_never_matches() {
        assert_eq!(find_in_line("anything", &needle("")), None);
    }

    #[test]
    fn a_short_line_is_stored_whole_with_controls_flattened() {
        assert_eq!(preview("a\tb", 0), "a b");
    }

    #[test]
    fn a_match_deep_in_a_long_line_stays_inside_the_preview() {
        let line = format!("{}needle{}", "x".repeat(500), "y".repeat(500));
        let p = preview(&line, 500);
        assert!(p.starts_with('…'));
        assert!(p.contains("needle"));
        assert!(p.chars().count() <= PREVIEW_MAX + 1);
    }

    /// A fresh scratch directory per test: tests run in parallel, so each
    /// needs its own.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("connor-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn put(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// Runs the search synchronously and returns its batches.
    fn search_all(root: &Path, query: &str, cancel: &AtomicBool) -> Vec<HitBatch> {
        let (tx, rx) = mpsc::channel();
        search(root, query, 7, cancel, &tx);
        drop(tx);
        rx.into_iter()
            .map(|ev| match ev {
                AppEvent::Hits(batch) => batch,
                _ => unreachable!(),
            })
            .collect()
    }

    /// Flattens a full run to `(path, line, col)` triples, sorted by path.
    fn found(root: &Path, query: &str) -> Vec<(String, u32, u32)> {
        let batches = search_all(root, query, &AtomicBool::new(false));
        assert!(batches.iter().all(|b| b.generation == 7));
        assert!(batches.last().unwrap().done);
        assert!(batches.iter().rev().skip(1).all(|b| !b.done));
        let mut out: Vec<(String, u32, u32)> = batches
            .into_iter()
            .flat_map(|b| b.files)
            .flat_map(|f| {
                f.hits
                    .into_iter()
                    .map(move |h| (f.path.clone(), h.line, h.col))
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn hits_carry_one_based_lines_and_char_columns() {
        let dir = scratch_dir("grep-lines");
        put(&dir.join("a.rs"), "one\n  needle\nnée needle\n");
        assert_eq!(
            found(&dir, "needle"),
            [("a.rs".to_string(), 2, 2), ("a.rs".to_string(), 3, 4)]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignored_hidden_binary_and_oversized_files_are_skipped() {
        let dir = scratch_dir("grep-skip");
        fs::create_dir_all(dir.join(".git")).unwrap();
        put(&dir.join(".gitignore"), "target/\n");
        put(&dir.join("src/kept.rs"), "needle\n");
        put(&dir.join("target/out.rs"), "needle\n");
        put(&dir.join(".hidden.rs"), "needle\n");
        put(&dir.join("blob.bin"), "needle\0needle\n");
        put(&dir.join("fat.txt"), &"needle\n".repeat(1 << 20));
        assert_eq!(found(&dir, "needle"), [("src/kept.rs".to_string(), 1, 0)]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_query_finds_nothing_without_walking() {
        let dir = scratch_dir("grep-empty");
        put(&dir.join("a.rs"), "text\n");
        let batches = search_all(&dir, "", &AtomicBool::new(false));
        assert_eq!(batches.len(), 1);
        assert!(batches[0].done && batches[0].files.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_preset_cancel_flag_ends_the_search_before_any_batch() {
        let dir = scratch_dir("grep-cancel");
        put(&dir.join("a.rs"), "needle\n");
        let batches = search_all(&dir, "needle", &AtomicBool::new(true));
        assert!(batches.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_stops_contributing_at_its_hit_cap() {
        let dir = scratch_dir("grep-filecap");
        put(&dir.join("a.rs"), &"needle\n".repeat(FILE_HITS + 50));
        assert_eq!(found(&dir, "needle").len(), FILE_HITS);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_global_cap_ends_the_search_done_and_truncated() {
        let dir = scratch_dir("grep-globalcap");
        for i in 0..(MAX_HITS / FILE_HITS) + 2 {
            put(
                &dir.join(format!("f{i:02}.rs")),
                &"needle\n".repeat(FILE_HITS),
            );
        }
        let batches = search_all(&dir, "needle", &AtomicBool::new(false));
        let last = batches.last().unwrap();
        assert!(last.done && last.truncated);
        let total: usize = batches
            .iter()
            .flat_map(|b| &b.files)
            .map(|f| f.hits.len())
            .sum();
        assert_eq!(total, MAX_HITS);
        let _ = fs::remove_dir_all(&dir);
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn hit(line: u32, preview: &str) -> Hit {
        Hit {
            line,
            col: 0,
            preview: preview.to_string(),
        }
    }

    fn batch(generation: u64, files: Vec<FileHits>, done: bool) -> HitBatch {
        HitBatch {
            generation,
            files,
            done,
            truncated: false,
        }
    }

    /// A grep mid-search on generation 1 with two files of (2, 1) hits.
    fn grep_two_files() -> Grep {
        let mut g = Grep::new(PathBuf::from("/r"));
        g.begin(1, Arc::default());
        assert!(g.absorb(batch(
            1,
            vec![
                FileHits {
                    path: "src/a.rs".to_string(),
                    hits: vec![hit(3, "aaa"), hit(9, "bbb")],
                },
                FileHits {
                    path: "src/b.rs".to_string(),
                    hits: vec![hit(1, "ccc")],
                },
            ],
            false,
        )));
        g
    }

    fn type_str(g: &mut Grep, text: &str, now: Instant) {
        for ch in text.chars() {
            assert!(matches!(
                g.key(&press(KeyCode::Char(ch)), now),
                Outcome::Pending
            ));
        }
    }

    #[test]
    fn an_edit_cancels_clears_and_arms_the_restart() {
        let mut g = grep_two_files();
        let cancel = Arc::clone(&g.cancel);
        let now = Instant::now();
        type_str(&mut g, "q", now);
        assert!(cancel.load(Ordering::Relaxed));
        assert_eq!(g.display_len(), 0);
        assert!(!g.searching());
        assert_eq!(g.deadline(), Some(now + RESTART));
        assert_eq!(g.take_restart(now), None);
        assert_eq!(g.take_restart(now + RESTART), Some("q".to_string()));
        assert_eq!(g.deadline(), None);
    }

    #[test]
    fn backspacing_to_empty_arms_no_restart() {
        let mut g = Grep::new(PathBuf::from("/r"));
        let now = Instant::now();
        type_str(&mut g, "q", now);
        g.key(&press(KeyCode::Backspace), now);
        assert_eq!(g.query(), "");
        assert_eq!(g.deadline(), None);
    }

    #[test]
    fn a_stale_batch_changes_nothing() {
        let mut g = grep_two_files();
        assert!(!g.absorb(batch(
            9,
            vec![FileHits {
                path: "ghost.rs".to_string(),
                hits: vec![hit(1, "zzz")],
            }],
            true,
        )));
        assert_eq!(g.display_len(), 5);
        assert!(g.searching());
    }

    #[test]
    fn streaming_appends_grouped_and_keeps_the_selection() {
        let mut g = grep_two_files();
        g.key(&press(KeyCode::Down), Instant::now());
        assert_eq!(g.selected, 1);
        assert!(g.absorb(batch(
            1,
            vec![FileHits {
                path: "src/c.rs".to_string(),
                hits: vec![hit(7, "ddd")],
            }],
            true,
        )));
        assert_eq!(g.selected, 1);
        assert!(!g.searching());
        assert_eq!(g.display_len(), 7);
        assert!(matches!(g.row(5), Row::File("src/c.rs")));
    }

    #[test]
    fn a_truncated_batch_sticks() {
        let mut g = grep_two_files();
        let mut b = batch(1, Vec::new(), true);
        b.truncated = true;
        assert!(g.absorb(b));
        assert!(g.truncated());
    }

    #[test]
    fn rows_interleave_headers_and_hits() {
        let g = grep_two_files();
        assert_eq!(g.display_len(), 5);
        assert_eq!(g.hit_count(), 3);
        assert!(matches!(g.row(0), Row::File("src/a.rs")));
        assert!(matches!(
            g.row(2),
            Row::Hit {
                line: 9,
                preview: "bbb",
                selected: false
            }
        ));
        assert!(matches!(g.row(3), Row::File("src/b.rs")));
        assert!(matches!(
            g.row(4),
            Row::Hit {
                line: 1,
                preview: "ccc",
                selected: false
            }
        ));
    }

    #[test]
    fn the_selection_moves_hit_to_hit_across_groups_and_clamps() {
        let mut g = grep_two_files();
        let now = Instant::now();
        g.key(&press(KeyCode::Up), now);
        assert_eq!(g.selected_display_row(), 1);
        g.key(&press(KeyCode::Down), now);
        g.key(&press(KeyCode::Down), now);
        // The third hit sits below both headers.
        assert_eq!(g.selected_display_row(), 4);
        assert!(matches!(g.row(4), Row::Hit { selected: true, .. }));
        g.key(&press(KeyCode::Down), now);
        assert_eq!(g.selected_display_row(), 4);
    }

    #[test]
    fn enter_jumps_to_the_selection_joined_to_the_root_and_stops_the_search() {
        let mut g = grep_two_files();
        let now = Instant::now();
        g.key(&press(KeyCode::Down), now);
        match g.key(&press(KeyCode::Enter), now) {
            Outcome::Open { path, line, col } => {
                assert_eq!(path, PathBuf::from("/r/src/a.rs"));
                assert_eq!((line, col), (9, 0));
            }
            _ => panic!("expected Open"),
        }
        assert!(g.cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn enter_without_hits_stays_pending() {
        let mut g = Grep::new(PathBuf::from("/r"));
        let now = Instant::now();
        assert!(matches!(
            g.key(&press(KeyCode::Enter), now),
            Outcome::Pending
        ));
        assert!(matches!(
            g.key(&press(KeyCode::Down), now),
            Outcome::Pending
        ));
    }

    #[test]
    fn esc_cancels_and_stops_the_search() {
        let mut g = grep_two_files();
        assert!(matches!(
            g.key(&press(KeyCode::Esc), Instant::now()),
            Outcome::Cancel
        ));
        assert!(g.cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn stray_chords_are_pending_and_change_nothing() {
        let mut g = grep_two_files();
        let now = Instant::now();
        for key in [
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT),
            press(KeyCode::F(5)),
            press(KeyCode::Left),
        ] {
            assert!(matches!(g.key(&key, now), Outcome::Pending));
        }
        assert_eq!(g.query(), "");
        assert_eq!(g.display_len(), 5);
        assert_eq!(g.deadline(), None);
    }

    #[test]
    fn a_paste_flattens_to_one_line_and_arms_the_restart() {
        let mut g = Grep::new(PathBuf::from("/r"));
        let now = Instant::now();
        g.paste("nee\ndle", now);
        assert_eq!(g.query(), "needle");
        assert_eq!(g.deadline(), Some(now + RESTART));
    }
}
