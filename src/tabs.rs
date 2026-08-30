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

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn active(&self) -> &Tab {
        &self.tabs[self.active]
    }

    pub fn active_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    #[cfg(test)]
    pub fn test_activate(&mut self, index: usize) {
        self.active = index;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_document_is_active() {
        let mut tabs = Tabs::new(vec![
            Document::from_str("first"),
            Document::from_str("second"),
        ]);
        assert_eq!(tabs.active_mut().doc.rope().to_string(), "first");
    }
}
