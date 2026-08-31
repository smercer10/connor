//! The fuzzy file picker's state: the walked file list, the query, and the
//! ranked matches. Pure state — the walker thread and the overlay drawing
//! live elsewhere.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::project::FileBatch;

pub enum Outcome {
    Pending,
    Open(PathBuf),
    Cancel,
}

pub struct Picker {
    root: PathBuf,
    generation: u64,
    cancel: Arc<AtomicBool>,
    query: String,
    pattern: Pattern,
    /// Every walked path, root-relative, in walk order.
    paths: Vec<String>,
    /// `(score, index into paths)`, best first; reused across keystrokes.
    matched: Vec<(u32, u32)>,
    /// Index into `matched`.
    selected: usize,
    walking: bool,
    matcher: Matcher,
    charbuf: Vec<char>,
}

impl Picker {
    pub fn new(root: PathBuf, generation: u64, cancel: Arc<AtomicBool>) -> Picker {
        Picker {
            root,
            generation,
            cancel,
            query: String::new(),
            pattern: Pattern::parse("", CaseMatching::Smart, Normalization::Smart),
            paths: Vec::new(),
            matched: Vec::new(),
            selected: 0,
            walking: true,
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            charbuf: Vec::new(),
        }
    }

    /// Folds a walker batch in; false means nothing on screen changes —
    /// the batch belonged to a previous walk.
    pub fn absorb(&mut self, batch: FileBatch) -> bool {
        if batch.generation != self.generation {
            return false;
        }
        self.walking = !batch.done;
        let start = self.paths.len();
        self.paths.extend(batch.paths);
        self.score_from(start);
        true
    }

    /// Feeds one keypress: printable characters extend the query, ↑↓ move
    /// the selection. Anything else is ignored so a stray chord can't
    /// dismiss the picker.
    pub fn key(&mut self, key: &KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Enter => {
                if let Some(&(_, i)) = self.matched.get(self.selected) {
                    self.dismiss();
                    return Outcome::Open(self.root.join(&self.paths[i as usize]));
                }
            }
            KeyCode::Esc => {
                self.dismiss();
                return Outcome::Cancel;
            }
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(self.matched.len().max(1) - 1),
            KeyCode::Backspace => {
                if self.query.pop().is_some() {
                    self.rescore();
                }
            }
            // Ctrl- and Alt-modified characters are chords, not input.
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(ch);
                self.narrow();
            }
            _ => {}
        }
        Outcome::Pending
    }

    /// Appends a paste flattened to a single line: control characters,
    /// line breaks included, are dropped.
    pub fn paste(&mut self, text: &str) {
        self.query
            .extend(text.chars().filter(|ch| !ch.is_control()));
        self.narrow();
    }

    /// Stops the walk; closing the picker for any reason calls this.
    pub fn dismiss(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Reranks everything under the current query; the price of a query
    /// that may have widened (backspace).
    fn rescore(&mut self) {
        self.pattern = Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
        self.matched.clear();
        self.score_from(0);
        self.selected = 0;
    }

    /// Reranks only the current matches: extending the query can only
    /// shrink the set, so after the first keystroke's full scan each
    /// further one touches survivors alone — the picker stays responsive
    /// on tens of thousands of files without a matcher thread.
    fn narrow(&mut self) {
        self.pattern = Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
        let mut keep = 0;
        for j in 0..self.matched.len() {
            let i = self.matched[j].1;
            let haystack = Utf32Str::new(&self.paths[i as usize], &mut self.charbuf);
            if let Some(score) = self.pattern.score(haystack, &mut self.matcher) {
                self.matched[keep] = (score, i);
                keep += 1;
            }
        }
        self.matched.truncate(keep);
        self.sort();
        self.selected = 0;
    }

    /// Scores `paths[start..]` into `matched`, keeping it sorted. An empty
    /// query lists everything in walk order without a matcher pass.
    fn score_from(&mut self, start: usize) {
        if self.query.is_empty() {
            self.matched
                .extend((start..self.paths.len()).map(|i| (0, i as u32)));
            return;
        }
        for i in start..self.paths.len() {
            let haystack = Utf32Str::new(&self.paths[i], &mut self.charbuf);
            if let Some(score) = self.pattern.score(haystack, &mut self.matcher) {
                self.matched.push((score, i as u32));
            }
        }
        self.sort();
    }

    fn sort(&mut self) {
        self.matched
            .sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// The path shown at rank `i`.
    pub fn shown(&self, i: usize) -> &str {
        &self.paths[self.matched[i].1 as usize]
    }

    pub fn matched_len(&self) -> usize {
        self.matched.len()
    }

    pub fn total(&self) -> usize {
        self.paths.len()
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn walking(&self) -> bool {
        self.walking
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker(paths: &[&str]) -> Picker {
        let mut p = Picker::new(PathBuf::from("/r"), 1, Arc::new(AtomicBool::new(false)));
        p.absorb(FileBatch {
            generation: 1,
            paths: paths.iter().map(|s| s.to_string()).collect(),
            done: true,
        });
        p
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_str(p: &mut Picker, text: &str) {
        for ch in text.chars() {
            assert!(matches!(p.key(&press(KeyCode::Char(ch))), Outcome::Pending));
        }
    }

    fn shown_all(p: &Picker) -> Vec<&str> {
        (0..p.matched_len()).map(|i| p.shown(i)).collect()
    }

    const PATHS: &[&str] = &["src/main.rs", "src/draw.rs", "README.md"];

    #[test]
    fn an_empty_query_lists_everything_in_walk_order() {
        let p = picker(PATHS);
        assert_eq!(shown_all(&p), PATHS);
        assert_eq!(p.matched_len(), p.total());
        assert!(!p.walking());
    }

    #[test]
    fn typing_filters_and_backspace_re_expands() {
        let mut p = picker(PATHS);
        type_str(&mut p, "drrs");
        assert_eq!(shown_all(&p), ["src/draw.rs"]);
        p.key(&press(KeyCode::Backspace));
        p.key(&press(KeyCode::Backspace));
        p.key(&press(KeyCode::Backspace));
        p.key(&press(KeyCode::Backspace));
        assert_eq!(shown_all(&p), PATHS);
    }

    #[test]
    fn a_filename_query_ranks_that_file_first() {
        let mut p = picker(&["src/other.rs", "docs/draw.md", "src/draw.rs"]);
        type_str(&mut p, "draw.rs");
        assert_eq!(p.shown(0), "src/draw.rs");
    }

    #[test]
    fn a_query_edit_resets_the_selection() {
        let mut p = picker(PATHS);
        p.key(&press(KeyCode::Down));
        assert_eq!(p.selected(), 1);
        type_str(&mut p, "r");
        assert_eq!(p.selected(), 0);
    }

    #[test]
    fn the_selection_clamps_at_both_ends() {
        let mut p = picker(PATHS);
        p.key(&press(KeyCode::Up));
        assert_eq!(p.selected(), 0);
        for _ in 0..10 {
            p.key(&press(KeyCode::Down));
        }
        assert_eq!(p.selected(), PATHS.len() - 1);
    }

    #[test]
    fn enter_opens_the_selection_joined_to_the_root_and_stops_the_walk() {
        let mut p = picker(PATHS);
        p.key(&press(KeyCode::Down));
        match p.key(&press(KeyCode::Enter)) {
            Outcome::Open(path) => assert_eq!(path, PathBuf::from("/r/src/draw.rs")),
            _ => panic!("expected Open"),
        }
        assert!(p.cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn enter_on_an_empty_list_stays_pending() {
        let mut p = picker(PATHS);
        type_str(&mut p, "zzzz");
        assert_eq!(p.matched_len(), 0);
        assert!(matches!(p.key(&press(KeyCode::Enter)), Outcome::Pending));
        assert!(matches!(p.key(&press(KeyCode::Down)), Outcome::Pending));
    }

    #[test]
    fn esc_cancels_and_stops_the_walk() {
        let mut p = picker(PATHS);
        assert!(matches!(p.key(&press(KeyCode::Esc)), Outcome::Cancel));
        assert!(p.cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn a_stale_batch_changes_nothing() {
        let mut p = picker(PATHS);
        let absorbed = p.absorb(FileBatch {
            generation: 9,
            paths: vec!["ghost.rs".to_string()],
            done: false,
        });
        assert!(!absorbed);
        assert_eq!(p.total(), PATHS.len());
        assert!(!p.walking());
    }

    #[test]
    fn a_live_batch_reranks_under_the_current_query() {
        let mut p = Picker::new(PathBuf::from("/r"), 1, Arc::new(AtomicBool::new(false)));
        assert!(p.absorb(FileBatch {
            generation: 1,
            paths: vec!["notes/draw.txt".to_string()],
            done: false,
        }));
        assert!(p.walking());
        type_str(&mut p, "draw.rs");
        assert_eq!(p.matched_len(), 0);
        assert!(p.absorb(FileBatch {
            generation: 1,
            paths: vec!["src/draw.rs".to_string()],
            done: true,
        }));
        assert_eq!(shown_all(&p), ["src/draw.rs"]);
        assert!(!p.walking());
    }

    #[test]
    fn stray_chords_are_pending_and_change_nothing() {
        let mut p = picker(PATHS);
        for key in [
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            press(KeyCode::F(5)),
            press(KeyCode::Left),
        ] {
            assert!(matches!(p.key(&key), Outcome::Pending));
        }
        assert_eq!(p.query(), "");
        assert_eq!(shown_all(&p), PATHS);
    }

    #[test]
    fn a_paste_flattens_to_one_line_and_filters() {
        let mut p = picker(PATHS);
        p.paste("dr\naw");
        assert_eq!(p.query(), "draw");
        assert_eq!(shown_all(&p), ["src/draw.rs"]);
    }
}
