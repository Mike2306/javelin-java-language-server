//! The set of documents the editor currently has open.

use std::collections::HashMap;
use std::sync::Arc;

use crate::text::{Change, DocumentId, EditBatch, TextDocument};

/// Owns every open document and hands out shared snapshots.
///
/// The store is not internally synchronised: the server keeps it behind a single
/// lock held only for the length of a document update, so background analysis
/// works from the `Arc` snapshot it took and never contends on this map.
#[derive(Debug, Default)]
pub struct DocumentStore {
    documents: HashMap<DocumentId, Arc<TextDocument>>,
}

impl DocumentStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(
        &mut self,
        id: DocumentId,
        version: i32,
        text: impl Into<String>,
    ) -> Arc<TextDocument> {
        let document = Arc::new(TextDocument::new(id.clone(), version, text));
        self.documents.insert(id, Arc::clone(&document));
        document
    }

    /// Apply editor changes. Returns `None` for a document that is not open,
    /// which happens legitimately when notifications race a close.
    pub fn change(
        &mut self,
        id: &DocumentId,
        version: i32,
        changes: &[Change],
    ) -> Option<Arc<TextDocument>> {
        self.change_reporting(id, version, changes)
            .map(|(document, _)| document)
    }

    /// As [`change`](Self::change), but also reporting what the changes touched
    /// so a caller holding the previous syntax tree can reparse incrementally.
    pub fn change_reporting(
        &mut self,
        id: &DocumentId,
        version: i32,
        changes: &[Change],
    ) -> Option<(Arc<TextDocument>, EditBatch)> {
        let current = self.documents.get(id)?;
        let (updated, edits) = current.with_changes_reporting(version, changes);
        let updated = Arc::new(updated);
        self.documents.insert(id.clone(), Arc::clone(&updated));
        Some((updated, edits))
    }

    pub fn close(&mut self, id: &DocumentId) -> Option<Arc<TextDocument>> {
        self.documents.remove(id)
    }

    pub fn get(&self, id: &DocumentId) -> Option<Arc<TextDocument>> {
        self.documents.get(id).map(Arc::clone)
    }

    pub fn len(&self) -> usize {
        self.documents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<TextDocument>> {
        self.documents.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_survive_open_change_close() {
        let mut store = DocumentStore::new();
        let id = DocumentId::new("file:///A.java");
        store.open(id.clone(), 1, "class A {}");
        assert_eq!(store.len(), 1);

        let changed = store.change(&id, 2, &[Change::full("class B {}")]);
        assert_eq!(
            changed.map(|d| d.text().to_owned()),
            Some("class B {}".to_owned())
        );

        store.close(&id);
        assert!(store.is_empty());
        assert!(store.get(&id).is_none());
    }

    #[test]
    fn changing_an_unopened_document_is_ignored() {
        let mut store = DocumentStore::new();
        let id = DocumentId::new("file:///Gone.java");
        assert!(store.change(&id, 2, &[Change::full("x")]).is_none());
    }

    #[test]
    fn a_snapshot_taken_before_an_edit_keeps_its_own_text() {
        let mut store = DocumentStore::new();
        let id = DocumentId::new("file:///A.java");
        let before = store.open(id.clone(), 1, "v1");
        store.change(&id, 2, &[Change::full("v2")]);
        assert_eq!(before.text(), "v1");
        assert_eq!(store.get(&id).map(|d| d.version()), Some(2));
    }
}
