//! The project tree's state: the walked files, their derived directories,
//! expansion, and selection. Pure state — the walker thread and the sidebar
//! drawing live elsewhere.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use crossterm::event::{KeyCode, KeyEvent};

use crate::project::FileBatch;

pub enum Outcome {
    Pending,
    Open(PathBuf),
    /// Esc: focus returns to the editor; the sidebar stays.
    FocusEditor,
}

const SEP: char = std::path::MAIN_SEPARATOR;
const SEP_B: u8 = std::path::MAIN_SEPARATOR as u8;

/// One node: a walked file, or a directory derived from the files' paths —
/// the walk emits files only, and deriving directories keeps the tree
/// showing exactly what the picker shows.
#[derive(PartialEq, Eq)]
struct Entry {
    /// Root-relative, native separators, as the walk spells them.
    path: String,
    /// Byte offset of the last component, so drawing slices the name
    /// without allocating.
    name_at: u32,
    depth: u16,
    dir: bool,
}

impl Entry {
    fn new(path: String, dir: bool) -> Entry {
        let name_at = path.rfind(SEP).map_or(0, |i| i + 1) as u32;
        let depth = path.bytes().filter(|&b| b == SEP_B).count() as u16;
        Entry {
            path,
            name_at,
            depth,
            dir,
        }
    }
}

/// DFS preorder with directories before files at each level: compare
/// components in lockstep, a directory component sorting before a file one.
/// A parent therefore lands immediately before its descendants, which stay
/// contiguous — what the visibility scan relies on.
fn tree_cmp(a: &Entry, b: &Entry) -> Ordering {
    let mut ai = a.path.split(SEP).peekable();
    let mut bi = b.path.split(SEP).peekable();
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let x_file = ai.peek().is_none() && !a.dir;
                let y_file = bi.peek().is_none() && !b.dir;
                match x_file.cmp(&y_file).then_with(|| x.cmp(y)) {
                    Ordering::Equal => {}
                    ord => return ord,
                }
            }
        }
    }
}

/// Builds a whole entry list from scratch — the refresh path, which needs
/// the complete picture to tell whether anything changed at all.
fn build(files: Vec<String>) -> (Vec<Entry>, HashSet<String>) {
    let mut dirs: HashSet<String> = HashSet::new();
    for f in &files {
        for (i, b) in f.bytes().enumerate() {
            if b == SEP_B && !dirs.contains(&f[..i]) {
                dirs.insert(f[..i].to_string());
            }
        }
    }
    let mut entries = Vec::with_capacity(files.len() + dirs.len());
    entries.extend(dirs.iter().map(|d| Entry::new(d.clone(), true)));
    for f in files {
        entries.push(Entry::new(f, false));
    }
    entries.sort_unstable_by(tree_cmp);
    (entries, dirs)
}

/// Merges two lists already sorted by `tree_cmp` into one.
fn merge(a: Vec<Entry>, b: Vec<Entry>) -> Vec<Entry> {
    if b.is_empty() {
        return a;
    }
    if a.is_empty() {
        return b;
    }
    let mut out = Vec::with_capacity(a.len() + b.len());
    let mut a = a.into_iter().peekable();
    let mut b = b.into_iter().peekable();
    loop {
        match (a.peek(), b.peek()) {
            (Some(x), Some(y)) => {
                if tree_cmp(x, y) != Ordering::Greater {
                    out.push(a.next().unwrap());
                } else {
                    out.push(b.next().unwrap());
                }
            }
            (Some(_), None) => {
                out.extend(a);
                return out;
            }
            (None, _) => {
                out.extend(b);
                return out;
            }
        }
    }
}

/// One row's worth of data for drawing; everything borrows from the tree.
pub struct Row<'a> {
    pub name: &'a str,
    /// Root-relative, as the walk spells it — what the project-wide change
    /// marks are keyed by.
    pub path: &'a str,
    pub depth: usize,
    pub dir: bool,
    pub expanded: bool,
    /// The file being edited.
    pub active: bool,
}

pub struct Tree {
    root: PathBuf,
    generation: u64,
    cancel: Arc<AtomicBool>,
    walking: bool,
    /// Every derived directory path — the dedupe for incremental inserts.
    dirs: HashSet<String>,
    /// A refresh walk accumulates here and swaps in on its done batch, so
    /// the tree never shrinks and regrows on screen mid-walk.
    pending: Vec<String>,
    refreshing: bool,
    /// A change arrived while a refresh walk was in flight; the main loop
    /// spawns the follow-up once the tree is idle.
    pub rerun: bool,
    /// Files plus derived directories, DFS order, dirs first per level.
    entries: Vec<Entry>,
    /// Expanded directory paths; empty means only the top level shows.
    expanded: HashSet<String>,
    /// Indices into `entries`, display order.
    visible: Vec<u32>,
    /// Index into `visible`.
    selected: usize,
    scroll: usize,
    /// Root-relative path of the edited file, when it lives under the root.
    active: Option<String>,
    /// The tab path `active` was computed from — the syscall-free guard.
    active_src: Option<PathBuf>,
    /// Reveal the active file once the walk delivers it.
    reveal_pending: bool,
}

impl Tree {
    pub fn new(root: PathBuf, generation: u64, cancel: Arc<AtomicBool>) -> Tree {
        // Canonical, so stripping it from a canonicalized tab path works.
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        Tree {
            root,
            generation,
            cancel,
            walking: true,
            dirs: HashSet::new(),
            pending: Vec::new(),
            refreshing: false,
            rerun: false,
            entries: Vec::new(),
            expanded: HashSet::new(),
            visible: Vec::new(),
            selected: 0,
            scroll: 0,
            active: None,
            active_src: None,
            reveal_pending: true,
        }
    }

    /// Folds a walker batch in; false means nothing on screen changes — a
    /// stale generation, a still-accumulating refresh, or a refresh that
    /// rebuilt an identical tree.
    pub fn absorb(&mut self, batch: FileBatch, text_h: usize) -> bool {
        if batch.generation != self.generation {
            return false;
        }
        if self.refreshing {
            self.pending.extend(batch.paths);
            if !batch.done {
                return false;
            }
            self.refreshing = false;
            let (entries, dirs) = build(std::mem::take(&mut self.pending));
            if entries == self.entries {
                return false;
            }
            let sel = self.selected_entry().map(|e| e.path.clone());
            self.entries = entries;
            self.dirs = dirs;
            self.expanded.retain(|p| self.dirs.contains(p));
            self.recompute_visible();
            self.select_path_or_clamp(sel.as_deref());
            self.ensure_visible(text_h);
            return true;
        }
        self.walking = !batch.done;
        self.insert(batch.paths, text_h);
        if self.reveal_pending && (self.reveal_active(text_h) || !self.walking) {
            self.reveal_pending = false;
        }
        true
    }

    /// Starts consuming a fresh walk's batches as a refresh; call only when
    /// no walk is in flight.
    pub fn begin_refresh(&mut self, generation: u64, cancel: Arc<AtomicBool>) {
        self.generation = generation;
        self.cancel = cancel;
        self.refreshing = true;
        self.rerun = false;
        self.pending.clear();
    }

    /// Whether a walk is in flight — the gate on spawning a refresh.
    pub fn busy(&self) -> bool {
        self.walking || self.refreshing
    }

    /// Stops the in-flight walk; closing the sidebar calls this.
    pub fn dismiss(&self) {
        self.cancel.store(true, AtomicOrdering::Relaxed);
    }

    /// Feeds one unmodified keypress. Anything unrecognized is swallowed so
    /// plain typing can't leak into the document while the tree has focus.
    pub fn key(&mut self, key: &KeyEvent, text_h: usize) -> Outcome {
        match key.code {
            KeyCode::Esc => return Outcome::FocusEditor,
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.select_down(1),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.visible.len().saturating_sub(1),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(text_h.max(1)),
            KeyCode::PageDown => self.select_down(text_h.max(1)),
            KeyCode::Enter => return self.activate(text_h),
            KeyCode::Right => match self.selected_entry() {
                Some(e) if e.dir && !self.expanded.contains(&e.path) => {
                    let path = e.path.clone();
                    self.set_expanded(&path, true, text_h);
                }
                // Already expanded: step into the first child, if any.
                Some(e) if e.dir => {
                    let depth = e.depth;
                    let next = self.selected + 1;
                    if self
                        .visible
                        .get(next)
                        .is_some_and(|&i| self.entries[i as usize].depth > depth)
                    {
                        self.selected = next;
                    }
                }
                _ => {}
            },
            KeyCode::Left => match self.selected_entry() {
                Some(e) if e.dir && self.expanded.contains(&e.path) => {
                    let path = e.path.clone();
                    self.set_expanded(&path, false, text_h);
                }
                Some(e) if e.depth > 0 => {
                    let parent = e.path[..e.name_at as usize - 1].to_string();
                    if let Some(i) = self.visible_index(&parent) {
                        self.selected = i;
                    }
                }
                _ => {}
            },
            _ => return Outcome::Pending,
        }
        self.ensure_visible(text_h);
        Outcome::Pending
    }

    /// A click on body row `row` (0-based below the tab bar) selects it;
    /// a file opens, a directory toggles.
    pub fn click(&mut self, row: usize, text_h: usize) -> Outcome {
        let i = self.scroll + row;
        if i >= self.visible.len() {
            return Outcome::Pending;
        }
        self.selected = i;
        self.activate(text_h)
    }

    /// The wheel moves the viewport only, like the editor's.
    pub fn scroll_by(&mut self, delta: isize, text_h: usize) {
        let max = self.visible.len().saturating_sub(text_h.max(1));
        self.scroll = self.scroll.saturating_add_signed(delta).min(max);
    }

    /// The edited file changed; returns whether the mark moved. Guarded by
    /// a lexical compare of the tab path, so the per-iteration call site is
    /// syscall-free when nothing changed.
    pub fn set_active(&mut self, path: Option<&Path>) -> bool {
        if self.active_src.as_deref() == path {
            return false;
        }
        self.active_src = path.map(Path::to_path_buf);
        let rel = path.and_then(|p| {
            crate::tabs::canonical(p)
                .strip_prefix(&self.root)
                .ok()
                .and_then(Path::to_str)
                .map(str::to_string)
        });
        if rel == self.active {
            return false;
        }
        self.active = rel;
        true
    }

    /// Expands the active file's ancestors and selects it; true when found.
    pub fn reveal_active(&mut self, text_h: usize) -> bool {
        let Some(active) = &self.active else {
            return false;
        };
        if !self.entries.iter().any(|e| !e.dir && &e.path == active) {
            return false;
        }
        let active = active.clone();
        for (i, b) in active.bytes().enumerate() {
            if b == SEP_B {
                self.expanded.insert(active[..i].to_string());
            }
        }
        self.recompute_visible();
        if let Some(i) = self.visible_index(&active) {
            self.selected = i;
        }
        self.ensure_visible(text_h);
        true
    }

    fn selected_entry(&self) -> Option<&Entry> {
        self.visible
            .get(self.selected)
            .map(|&i| &self.entries[i as usize])
    }

    fn select_down(&mut self, by: usize) {
        self.selected = (self.selected + by).min(self.visible.len().saturating_sub(1));
    }

    /// Enter or a click on the selection: open a file, toggle a directory.
    fn activate(&mut self, text_h: usize) -> Outcome {
        let Some(e) = self.selected_entry() else {
            return Outcome::Pending;
        };
        if !e.dir {
            return Outcome::Open(self.root.join(&e.path));
        }
        let path = e.path.clone();
        let expand = !self.expanded.contains(&path);
        self.set_expanded(&path, expand, text_h);
        Outcome::Pending
    }

    fn set_expanded(&mut self, path: &str, on: bool, text_h: usize) {
        if on {
            self.expanded.insert(path.to_string());
        } else {
            self.expanded.remove(path);
        }
        let sel = self.selected_entry().map(|e| e.path.clone());
        self.recompute_visible();
        self.select_path_or_clamp(sel.as_deref());
        self.ensure_visible(text_h);
    }

    fn visible_index(&self, path: &str) -> Option<usize> {
        self.visible
            .iter()
            .position(|&i| self.entries[i as usize].path == path)
    }

    /// Folds one walked batch into the sorted entries: only the batch is
    /// sorted, then merged in — a streaming walk costs each batch its own
    /// size, not a rebuild of everything so far. State mutation —
    /// allocation is fine here, unlike drawing.
    fn insert(&mut self, paths: Vec<String>, text_h: usize) {
        let sel = self.selected_entry().map(|e| e.path.clone());
        let mut new = Vec::with_capacity(paths.len());
        for f in &paths {
            for (i, b) in f.bytes().enumerate() {
                if b == SEP_B && !self.dirs.contains(&f[..i]) {
                    self.dirs.insert(f[..i].to_string());
                    new.push(Entry::new(f[..i].to_string(), true));
                }
            }
        }
        for f in paths {
            new.push(Entry::new(f, false));
        }
        new.sort_unstable_by(tree_cmp);
        let old = std::mem::take(&mut self.entries);
        self.entries = merge(old, new);
        self.recompute_visible();
        self.select_path_or_clamp(sel.as_deref());
        self.ensure_visible(text_h);
    }

    /// Descendants sit contiguously after their parent, so one linear scan
    /// that skips collapsed subtrees yields the display order.
    fn recompute_visible(&mut self) {
        self.visible.clear();
        let mut i = 0;
        while i < self.entries.len() {
            self.visible.push(i as u32);
            let e = &self.entries[i];
            if e.dir && !self.expanded.contains(&e.path) {
                i = self.subtree_end(i);
            } else {
                i += 1;
            }
        }
    }

    /// The first index after `entries[i]`'s descendants.
    fn subtree_end(&self, i: usize) -> usize {
        let p = &self.entries[i].path;
        let mut j = i + 1;
        while j < self.entries.len() {
            let q = &self.entries[j].path;
            if q.len() > p.len() && q.as_bytes()[p.len()] == SEP_B && q.starts_with(p.as_str()) {
                j += 1;
            } else {
                break;
            }
        }
        j
    }

    /// Re-selects `sel` where it survived, else its nearest visible
    /// ancestor (a collapse pulls the selection up), else clamps the index.
    fn select_path_or_clamp(&mut self, sel: Option<&str>) {
        if let Some(path) = sel {
            let mut target = path;
            loop {
                if let Some(i) = self.visible_index(target) {
                    self.selected = i;
                    return;
                }
                match target.rfind(SEP) {
                    Some(k) => target = &target[..k],
                    None => break,
                }
            }
        }
        self.selected = self.selected.min(self.visible.len().saturating_sub(1));
    }

    fn ensure_visible(&mut self, text_h: usize) {
        let text_h = text_h.max(1);
        self.scroll = self.scroll.min(self.visible.len().saturating_sub(text_h));
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + text_h {
            self.scroll = self.selected + 1 - text_h;
        }
    }

    /// The directories the tree currently derives, root-relative — the set
    /// the sidebar's watcher mirrors.
    pub fn dir_paths(&self) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .filter(|e| e.dir)
            .map(|e| e.path.as_str())
    }

    pub fn visible_len(&self) -> usize {
        self.visible.len()
    }

    /// The row shown at display position `i`.
    pub fn row(&self, i: usize) -> Row<'_> {
        let e = &self.entries[self.visible[i] as usize];
        Row {
            name: &e.path[e.name_at as usize..],
            path: &e.path,
            depth: usize::from(e.depth),
            dir: e.dir,
            expanded: e.dir && self.expanded.contains(&e.path),
            active: self.active.as_deref() == Some(e.path.as_str()),
        }
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn walking(&self) -> bool {
        self.walking
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    const H: usize = 10;

    fn batch(generation: u64, paths: &[&str], done: bool) -> FileBatch {
        FileBatch {
            generation,
            paths: paths.iter().map(|s| s.to_string()).collect(),
            done,
        }
    }

    fn tree(paths: &[&str]) -> Tree {
        let mut t = Tree::new(PathBuf::from("/r"), 1, Arc::new(AtomicBool::new(false)));
        assert!(t.absorb(batch(1, paths, true), H));
        t
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// The visible rows as indented names, directories marked with `/`.
    fn shown(t: &Tree) -> Vec<String> {
        (0..t.visible_len())
            .map(|i| {
                let r = t.row(i);
                format!(
                    "{}{}{}",
                    "  ".repeat(r.depth),
                    r.name,
                    if r.dir { "/" } else { "" }
                )
            })
            .collect()
    }

    fn selected_name(t: &Tree) -> String {
        t.row(t.selected()).name.to_string()
    }

    const PATHS: &[&str] = &["b.txt", "a/z.rs", "a/b/c.rs"];

    #[test]
    fn only_the_top_level_is_visible_at_first() {
        let t = tree(PATHS);
        assert_eq!(shown(&t), ["a/", "b.txt"]);
        assert!(!t.walking());
    }

    #[test]
    fn files_derive_their_ancestor_directories_dirs_first_alphabetical() {
        let mut t = tree(PATHS);
        t.key(&press(KeyCode::Enter), H);
        assert_eq!(shown(&t), ["a/", "  b/", "  z.rs", "b.txt"]);
        t.key(&press(KeyCode::Down), H);
        t.key(&press(KeyCode::Enter), H);
        assert_eq!(shown(&t), ["a/", "  b/", "    c.rs", "  z.rs", "b.txt"]);
    }

    #[test]
    fn expanding_and_collapsing_reshapes_the_visible_rows() {
        let mut t = tree(PATHS);
        t.key(&press(KeyCode::Enter), H);
        assert_eq!(t.visible_len(), 4);
        t.key(&press(KeyCode::Enter), H);
        assert_eq!(shown(&t), ["a/", "b.txt"]);
        assert_eq!(selected_name(&t), "a");
    }

    #[test]
    fn enter_opens_a_file_joined_to_the_root() {
        let mut t = tree(PATHS);
        t.key(&press(KeyCode::Down), H);
        match t.key(&press(KeyCode::Enter), H) {
            Outcome::Open(path) => assert_eq!(path, PathBuf::from("/r/b.txt")),
            _ => panic!("expected Open"),
        }
    }

    #[test]
    fn right_expands_and_steps_into_a_directory() {
        let mut t = tree(PATHS);
        t.key(&press(KeyCode::Right), H);
        assert_eq!(shown(&t), ["a/", "  b/", "  z.rs", "b.txt"]);
        assert_eq!(selected_name(&t), "a");
        t.key(&press(KeyCode::Right), H);
        assert_eq!(selected_name(&t), "b");
    }

    #[test]
    fn left_jumps_from_a_file_to_its_parent_and_collapses_it() {
        let mut t = tree(PATHS);
        t.key(&press(KeyCode::Enter), H);
        t.key(&press(KeyCode::Down), H);
        t.key(&press(KeyCode::Down), H);
        assert_eq!(selected_name(&t), "z.rs");
        t.key(&press(KeyCode::Left), H);
        assert_eq!(selected_name(&t), "a");
        t.key(&press(KeyCode::Left), H);
        assert_eq!(shown(&t), ["a/", "b.txt"]);
    }

    #[test]
    fn esc_hands_focus_back_and_stray_keys_are_pending() {
        let mut t = tree(PATHS);
        assert!(matches!(
            t.key(&press(KeyCode::Esc), H),
            Outcome::FocusEditor
        ));
        for code in [KeyCode::Char('x'), KeyCode::Tab, KeyCode::F(5)] {
            assert!(matches!(t.key(&press(code), H), Outcome::Pending));
        }
        assert_eq!(shown(&t), ["a/", "b.txt"]);
        assert_eq!(t.selected(), 0);
    }

    #[test]
    fn keys_on_an_empty_tree_stay_pending() {
        let mut t = tree(&[]);
        for code in [
            KeyCode::Enter,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::End,
        ] {
            assert!(matches!(t.key(&press(code), H), Outcome::Pending));
        }
        assert_eq!(t.visible_len(), 0);
    }

    #[test]
    fn later_batches_merge_into_place_and_the_selection_follows_its_path() {
        let mut t = Tree::new(PathBuf::from("/r"), 1, Arc::new(AtomicBool::new(false)));
        assert!(t.absorb(batch(1, &["src/z.rs", "b.txt"], false), H));
        t.key(&press(KeyCode::Down), H);
        assert_eq!(selected_name(&t), "b.txt");
        assert!(t.absorb(batch(1, &["src/a.rs", "a.txt"], true), H));
        assert_eq!(selected_name(&t), "b.txt");
        t.key(&press(KeyCode::Up), H);
        t.key(&press(KeyCode::Up), H);
        t.key(&press(KeyCode::Enter), H);
        assert_eq!(shown(&t), ["src/", "  a.rs", "  z.rs", "a.txt", "b.txt"]);
    }

    #[test]
    fn a_stale_batch_changes_nothing() {
        let mut t = tree(PATHS);
        assert!(!t.absorb(batch(9, &["ghost.rs"], true), H));
        assert_eq!(shown(&t), ["a/", "b.txt"]);
    }

    #[test]
    fn a_refresh_buffers_batches_until_done_then_swaps() {
        let mut t = tree(PATHS);
        t.key(&press(KeyCode::Enter), H);
        t.begin_refresh(2, Arc::new(AtomicBool::new(false)));
        assert!(t.busy());
        assert!(!t.absorb(batch(2, &["b.txt", "a/z.rs"], false), H));
        assert_eq!(t.visible_len(), 4);
        assert!(t.absorb(batch(2, &["a/new.rs"], true), H));
        assert!(!t.busy());
        assert_eq!(shown(&t), ["a/", "  new.rs", "  z.rs", "b.txt"]);
    }

    #[test]
    fn a_refresh_preserves_expansion_and_selection_by_path() {
        let mut t = tree(&["a/x.rs", "a/y.rs", "b.txt"]);
        t.key(&press(KeyCode::Enter), H);
        t.key(&press(KeyCode::Down), H);
        t.key(&press(KeyCode::Down), H);
        assert_eq!(selected_name(&t), "y.rs");
        t.begin_refresh(2, Arc::new(AtomicBool::new(false)));
        assert!(t.absorb(
            batch(2, &["a/x.rs", "a/new.rs", "a/y.rs", "b.txt"], true),
            H
        ));
        assert_eq!(shown(&t), ["a/", "  new.rs", "  x.rs", "  y.rs", "b.txt"]);
        assert_eq!(selected_name(&t), "y.rs");
    }

    #[test]
    fn a_refresh_with_identical_files_reports_no_change() {
        let mut t = tree(PATHS);
        t.begin_refresh(2, Arc::new(AtomicBool::new(false)));
        // Walk order may differ between runs; only the tree matters.
        assert!(!t.absorb(batch(2, &["a/b/c.rs", "b.txt", "a/z.rs"], true), H));
    }

    #[test]
    fn a_vanished_selection_falls_back_to_its_parent() {
        let mut t = tree(PATHS);
        t.key(&press(KeyCode::Enter), H);
        t.key(&press(KeyCode::Down), H);
        t.key(&press(KeyCode::Down), H);
        assert_eq!(selected_name(&t), "z.rs");
        t.begin_refresh(2, Arc::new(AtomicBool::new(false)));
        assert!(t.absorb(batch(2, &["b.txt", "a/b/c.rs"], true), H));
        assert_eq!(selected_name(&t), "a");
    }

    #[test]
    fn a_wholly_vanished_selection_clamps_to_a_neighbour() {
        let mut t = tree(PATHS);
        t.key(&press(KeyCode::Down), H);
        assert_eq!(selected_name(&t), "b.txt");
        t.begin_refresh(2, Arc::new(AtomicBool::new(false)));
        assert!(t.absorb(batch(2, &["a/z.rs"], true), H));
        assert_eq!(selected_name(&t), "a");
    }

    #[test]
    fn set_active_marks_the_edited_file_and_reveal_expands_to_it() {
        let mut t = tree(PATHS);
        assert!(t.set_active(Some(Path::new("/r/a/b/c.rs"))));
        // A repeat with the same tab path is a guarded no-op.
        assert!(!t.set_active(Some(Path::new("/r/a/b/c.rs"))));
        assert!(t.reveal_active(H));
        assert_eq!(selected_name(&t), "c.rs");
        assert!(t.row(t.selected()).active);
        assert_eq!(shown(&t), ["a/", "  b/", "    c.rs", "  z.rs", "b.txt"]);
    }

    #[test]
    fn an_active_path_outside_the_root_marks_nothing() {
        let mut t = tree(PATHS);
        assert!(t.set_active(Some(Path::new("/r/a/b/c.rs"))));
        assert!(t.set_active(Some(Path::new("/elsewhere/x.rs"))));
        assert!(!t.reveal_active(H));
        assert!((0..t.visible_len()).all(|i| !t.row(i).active));
    }

    #[test]
    fn the_reveal_waits_for_the_walk_to_deliver_the_active_file() {
        let mut t = Tree::new(PathBuf::from("/r"), 1, Arc::new(AtomicBool::new(false)));
        t.set_active(Some(Path::new("/r/a/b/c.rs")));
        assert!(t.absorb(batch(1, &["b.txt"], false), H));
        assert_eq!(shown(&t), ["b.txt"]);
        assert!(t.absorb(batch(1, &["a/z.rs", "a/b/c.rs"], true), H));
        assert_eq!(selected_name(&t), "c.rs");
        assert!(t.row(t.selected()).active);
    }

    #[test]
    fn scroll_keeps_the_selection_inside_the_window() {
        let files: Vec<String> = (0..20).map(|i| format!("f{i:02}.rs")).collect();
        let refs: Vec<&str> = files.iter().map(String::as_str).collect();
        let mut t = tree(&refs);
        t.key(&press(KeyCode::End), 5);
        assert_eq!(t.selected(), 19);
        assert_eq!(t.scroll(), 15);
        t.key(&press(KeyCode::Home), 5);
        assert_eq!(t.scroll(), 0);
        t.key(&press(KeyCode::PageDown), 5);
        assert_eq!(t.selected(), 5);
        assert_eq!(t.scroll(), 1);
    }

    #[test]
    fn the_wheel_moves_the_viewport_only_and_clamps() {
        let files: Vec<String> = (0..20).map(|i| format!("f{i:02}.rs")).collect();
        let refs: Vec<&str> = files.iter().map(String::as_str).collect();
        let mut t = tree(&refs);
        t.scroll_by(100, 5);
        assert_eq!(t.scroll(), 15);
        assert_eq!(t.selected(), 0);
        t.scroll_by(-100, 5);
        assert_eq!(t.scroll(), 0);
    }

    #[test]
    fn a_click_selects_and_opens_a_file_or_toggles_a_directory() {
        let mut t = tree(PATHS);
        assert!(matches!(t.click(0, H), Outcome::Pending));
        assert_eq!(shown(&t), ["a/", "  b/", "  z.rs", "b.txt"]);
        match t.click(3, H) {
            Outcome::Open(path) => assert_eq!(path, PathBuf::from("/r/b.txt")),
            _ => panic!("expected Open"),
        }
        assert_eq!(selected_name(&t), "b.txt");
        assert!(matches!(t.click(9, H), Outcome::Pending));
        assert_eq!(selected_name(&t), "b.txt");
    }

    #[test]
    fn dismiss_stops_the_walk() {
        let cancel = Arc::new(AtomicBool::new(false));
        let t = Tree::new(PathBuf::from("/r"), 1, Arc::clone(&cancel));
        t.dismiss();
        assert!(cancel.load(AtomicOrdering::Relaxed));
    }
}
