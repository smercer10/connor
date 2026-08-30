//! The open files: each tab pairs a document with its view, exactly one
//! active at a time.

use crate::doc::Document;
use crate::view::View;

/// One open file and the view looking at it.
pub struct Tab {
    pub doc: Document,
    pub view: View,
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
            tabs: docs
                .into_iter()
                .map(|doc| Tab {
                    doc,
                    view: View::default(),
                })
                .collect(),
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
        self.tabs.push(Tab {
            doc,
            view: View::default(),
        });
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
