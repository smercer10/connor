//! The open files: each tab pairs a document with its view, exactly one
//! active at a time.

use std::fs;
use std::path::{Path, PathBuf};

use crate::diff::Diff;
use crate::doc::Document;
use crate::status::Mark;
use crate::syntax::Syntax;
use crate::view::View;

/// One open file and the view looking at it, plus its syntax highlighter
/// when a grammar covers the file and its standing against HEAD.
pub struct Tab {
    pub doc: Document,
    pub view: View,
    pub syntax: Option<Syntax>,
    pub diff: Diff,
    /// How the file stands against HEAD on disk, from the project-wide
    /// scan; `Status::sync` owns it, and the label draws it.
    pub mark: Option<Mark>,
}

impl Tab {
    fn of(mut doc: Document) -> Tab {
        Tab {
            syntax: Syntax::new(&mut doc),
            diff: Diff::new(doc.path()),
            doc,
            view: View::default(),
            mark: None,
        }
    }
}

/// The tab strip. Never empty — closing the last tab means quitting, which
/// is the caller's decision, not this collection's.
pub struct Tabs {
    tabs: Vec<Tab>,
    active: usize,
}

impl Tabs {
    /// Wraps the documents in tabs, the first one active. An empty vec is a
    /// caller bug.
    pub fn new(docs: Vec<Document>) -> Tabs {
        debug_assert!(!docs.is_empty());
        Tabs {
            tabs: docs.into_iter().map(Tab::of).collect(),
            active: 0,
        }
    }

    pub fn all(&self) -> &[Tab] {
        &self.tabs
    }

    pub fn count(&self) -> usize {
        self.tabs.len()
    }

    pub fn get_mut(&mut self, index: usize) -> &mut Tab {
        &mut self.tabs[index]
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn activate(&mut self, index: usize) {
        self.active = index;
    }

    pub fn active(&self) -> &Tab {
        &self.tabs[self.active]
    }

    pub fn active_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    /// Activates the tab to the right, wrapping.
    pub fn next(&mut self) {
        self.active = (self.active + 1) % self.tabs.len();
    }

    /// Activates the tab to the left, wrapping.
    pub fn prev(&mut self) {
        self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
    }

    /// Appends a tab and activates it.
    pub fn push(&mut self, doc: Document) {
        self.tabs.push(Tab::of(doc));
        self.active = self.tabs.len() - 1;
    }

    /// Removes the active tab: its right-hand neighbour slides into its
    /// slot and becomes active, or the new last tab does. Keeping the
    /// collection non-empty is the caller's job.
    pub fn close_active(&mut self) {
        debug_assert!(self.tabs.len() > 1);
        self.tabs.remove(self.active);
        self.active = self.active.min(self.tabs.len() - 1);
    }

    pub fn any_dirty(&self) -> bool {
        self.tabs.iter().any(|tab| tab.doc.dirty())
    }

    /// Finds the tab holding `path`. Paths compare canonically where they
    /// resolve, so `./x` and `x` meet; one that doesn't exist yet falls
    /// back to lexical equality.
    pub fn find_by_path(&self, path: &Path) -> Option<usize> {
        let target = canonical(path);
        self.tabs
            .iter()
            .position(|tab| tab.doc.path().is_some_and(|p| canonical(p) == target))
    }
}

pub(crate) fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching_wraps_in_both_directions() {
        let mut tabs = Tabs::new(vec![
            Document::empty(),
            Document::empty(),
            Document::empty(),
        ]);
        tabs.next();
        assert_eq!(tabs.active_index(), 1);
        tabs.next();
        tabs.next();
        assert_eq!(tabs.active_index(), 0);
        tabs.prev();
        assert_eq!(tabs.active_index(), 2);
    }

    #[test]
    fn switching_a_lone_tab_stays_put() {
        let mut tabs = Tabs::new(vec![Document::empty()]);
        tabs.next();
        assert_eq!(tabs.active_index(), 0);
        tabs.prev();
        assert_eq!(tabs.active_index(), 0);
    }

    #[test]
    fn first_document_is_active() {
        let mut tabs = Tabs::new(vec![
            Document::from_str("first"),
            Document::from_str("second"),
        ]);
        assert_eq!(tabs.active_mut().doc.rope().to_string(), "first");
    }

    #[test]
    fn push_appends_and_activates() {
        let mut tabs = Tabs::new(vec![Document::from_str("first")]);
        tabs.push(Document::from_str("second"));
        assert_eq!(tabs.count(), 2);
        assert_eq!(tabs.active_index(), 1);
        assert_eq!(tabs.active_mut().doc.rope().to_string(), "second");
    }

    #[test]
    fn closing_a_middle_tab_activates_its_right_neighbour() {
        let mut tabs = Tabs::new(vec![
            Document::from_str("a"),
            Document::from_str("b"),
            Document::from_str("c"),
        ]);
        tabs.activate(1);
        tabs.close_active();
        assert_eq!(tabs.active_index(), 1);
        assert_eq!(tabs.active_mut().doc.rope().to_string(), "c");
    }

    #[test]
    fn closing_the_last_tab_activates_the_new_last() {
        let mut tabs = Tabs::new(vec![Document::from_str("a"), Document::from_str("b")]);
        tabs.activate(1);
        tabs.close_active();
        assert_eq!(tabs.active_index(), 0);
        assert_eq!(tabs.active_mut().doc.rope().to_string(), "a");
    }

    #[test]
    fn find_by_path_meets_spelling_variants_of_an_existing_file() {
        let dir = std::env::temp_dir().join(format!("connor-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.txt");
        std::fs::write(&path, "x").unwrap();

        let tabs = Tabs::new(vec![
            Document::empty(),
            Document::open(path.clone()).unwrap(),
        ]);
        assert_eq!(tabs.find_by_path(&path), Some(1));
        assert_eq!(tabs.find_by_path(&dir.join(".").join("f.txt")), Some(1));
        assert_eq!(tabs.find_by_path(&dir.join("other.txt")), None);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn find_by_path_falls_back_to_lexical_equality() {
        let mut doc = Document::from_str("");
        doc.set_path(PathBuf::from("no/such/file"));
        let tabs = Tabs::new(vec![doc]);
        assert_eq!(tabs.find_by_path(Path::new("no/such/file")), Some(0));
        assert_eq!(tabs.find_by_path(Path::new("no/such/other")), None);
    }

    #[test]
    fn any_dirty_scans_every_tab() {
        use crate::doc::{Caret, EditKind};
        let mut tabs = Tabs::new(vec![Document::from_str("a"), Document::from_str("b")]);
        assert!(!tabs.any_dirty());
        tabs.get_mut(1).doc.edit(
            0..0,
            "x",
            Caret {
                cursor: 0,
                anchor: None,
            },
            EditKind::Insert,
        );
        assert!(tabs.any_dirty());
    }
}
