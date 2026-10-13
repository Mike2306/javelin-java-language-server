//! A single open document and the edits applied to it.

use std::sync::Arc;

use crate::line_index::{BytePosition, LineIndex, Position};

/// Identifies a document. A URI string rather than a path, because the editor
/// owns the naming and not every open buffer is a file on disk.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DocumentId(Arc<str>);

impl DocumentId {
    pub fn new(uri: impl AsRef<str>) -> Self {
        Self(Arc::from(uri.as_ref()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl Range {
    pub fn new(start: Position, end: Position) -> Self {
        Self { start, end }
    }
}

/// One edit from the editor. `range: None` means the whole document was replaced.
#[derive(Debug, Clone)]
pub struct Change {
    pub range: Option<Range>,
    pub text: String,
}

impl Change {
    pub fn full(text: impl Into<String>) -> Self {
        Self {
            range: None,
            text: text.into(),
        }
    }

    pub fn incremental(range: Range, text: impl Into<String>) -> Self {
        Self {
            range: Some(range),
            text: text.into(),
        }
    }
}

/// One applied edit, described the way an incremental parser needs it: where
/// the edit started, where it used to end, and where it now ends.
///
/// Positions are [`BytePosition`]s rather than LSP [`Position`]s, because the
/// consumer is Tree-sitter and its `Point.column` counts bytes. The conversion
/// also has to happen here and nowhere else: every edit in a batch is resolved
/// against the text its predecessor produced, and those intermediate texts do
/// not outlive [`TextDocument::with_changes_reporting`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextEdit {
    pub start_byte: u32,
    pub old_end_byte: u32,
    pub new_end_byte: u32,
    pub start_position: BytePosition,
    pub old_end_position: BytePosition,
    pub new_end_position: BytePosition,
}

/// The edits that took a document from `base_version` to the next version.
///
/// The base version travels with the edits because applying them to a tree
/// parsed from any other version produces a silently *wrong* tree rather than a
/// failure. Carrying it lets the parser check instead of trust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditBatch {
    pub base_version: i32,
    pub edits: Vec<TextEdit>,
}

/// An immutable snapshot of a document at one version.
///
/// Snapshots are never mutated in place. Applying a change produces a new
/// `TextDocument`, so any analysis already holding the old one keeps a coherent
/// view of the source it was started against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextDocument {
    id: DocumentId,
    version: i32,
    text: Arc<str>,
    line_index: Arc<LineIndex>,
}

impl TextDocument {
    pub fn new(id: DocumentId, version: i32, text: impl Into<String>) -> Self {
        let text: Arc<str> = Arc::from(text.into());
        let line_index = Arc::new(LineIndex::new(&text));
        Self {
            id,
            version,
            text,
            line_index,
        }
    }

    pub fn id(&self) -> &DocumentId {
        &self.id
    }

    pub fn version(&self) -> i32 {
        self.version
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn line_index(&self) -> &LineIndex {
        &self.line_index
    }

    pub fn offset_at(&self, position: Position) -> u32 {
        self.line_index.offset(&self.text, position)
    }

    pub fn position_at(&self, offset: u32) -> Position {
        self.line_index.position(&self.text, offset)
    }

    /// Apply a batch of edits, producing the next version of this document.
    ///
    /// Edits are applied in the order the editor sent them; each one is resolved
    /// against the text produced by its predecessor, as the protocol requires.
    pub fn with_changes(&self, version: i32, changes: &[Change]) -> TextDocument {
        self.with_changes_reporting(version, changes).0
    }

    /// As [`with_changes`](Self::with_changes), but also reporting the byte and
    /// point ranges each change touched, in application order.
    ///
    /// ponytail: the text is rebuilt as a flat `String` and the line table from
    /// scratch per change. That is the right trade while documents are single
    /// files; switch to a rope if profiling ever shows edit cost dominating in
    /// large generated sources.
    pub fn with_changes_reporting(
        &self,
        version: i32,
        changes: &[Change],
    ) -> (TextDocument, EditBatch) {
        let mut text = String::from(&*self.text);
        let mut index = (*self.line_index).clone();
        let mut edits = Vec::with_capacity(changes.len());

        for change in changes {
            // A full replacement is an edit like any other: it spans everything.
            let (start, old_end) = match change.range {
                None => (0, text.len()),
                Some(range) => {
                    let start = index.offset(&text, range.start) as usize;
                    let end = index.offset(&text, range.end) as usize;
                    if start <= end {
                        (start, end)
                    } else {
                        (end, start)
                    }
                }
            };
            let start_position = index.byte_position(start as u32);
            let old_end_position = index.byte_position(old_end as u32);

            text.replace_range(start..old_end, &change.text);
            index = LineIndex::new(&text);

            let new_end = start + change.text.len();
            edits.push(TextEdit {
                start_byte: start as u32,
                old_end_byte: old_end as u32,
                new_end_byte: new_end as u32,
                start_position,
                old_end_position,
                new_end_position: index.byte_position(new_end as u32),
            });
        }

        let document = TextDocument {
            id: self.id.clone(),
            version,
            text: Arc::from(text),
            line_index: Arc::new(index),
        };
        let batch = EditBatch {
            base_version: self.version,
            edits,
        };
        (document, batch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> TextDocument {
        TextDocument::new(DocumentId::new("file:///A.java"), 1, text)
    }

    #[test]
    fn a_full_replacement_swaps_the_whole_buffer() {
        let updated = doc("old").with_changes(2, &[Change::full("new")]);
        assert_eq!(updated.text(), "new");
        assert_eq!(updated.version(), 2);
    }

    #[test]
    fn an_incremental_edit_only_touches_its_range() {
        let original = doc("class A {\n  int x;\n}\n");
        let range = Range::new(Position::new(1, 6), Position::new(1, 7));
        let updated = original.with_changes(2, &[Change::incremental(range, "count")]);
        assert_eq!(updated.text(), "class A {\n  int count;\n}\n");
    }

    #[test]
    fn later_edits_in_a_batch_see_earlier_ones() {
        let original = doc("ab\n");
        let changes = vec![
            Change::incremental(Range::new(Position::new(0, 1), Position::new(0, 1)), "X"),
            Change::incremental(Range::new(Position::new(0, 2), Position::new(0, 2)), "Y"),
        ];
        assert_eq!(original.with_changes(2, &changes).text(), "aXYb\n");
    }

    #[test]
    fn the_previous_snapshot_is_untouched_by_an_edit() {
        let original = doc("one");
        let updated = original.with_changes(2, &[Change::full("two")]);
        assert_eq!(original.text(), "one");
        assert_eq!(updated.text(), "two");
    }

    #[test]
    fn the_line_index_tracks_edits_that_add_lines() {
        let updated = doc("a").with_changes(2, &[Change::full("a\nb\nc")]);
        assert_eq!(updated.position_at(4), Position::new(2, 0));
    }

    fn edits(original: &TextDocument, changes: &[Change]) -> Vec<TextEdit> {
        original.with_changes_reporting(2, changes).1.edits
    }

    #[test]
    fn a_single_character_insertion_reports_an_empty_old_range() {
        let original = doc("class A {\n  int x;\n}\n");
        let at = Position::new(1, 6);
        let reported = edits(&original, &[Change::incremental(Range::new(at, at), "y")]);
        assert_eq!(
            reported,
            vec![TextEdit {
                start_byte: 16,
                old_end_byte: 16,
                new_end_byte: 17,
                start_position: BytePosition::new(1, 6),
                old_end_position: BytePosition::new(1, 6),
                new_end_position: BytePosition::new(1, 7),
            }]
        );
    }

    #[test]
    fn an_edit_after_a_multibyte_character_reports_byte_columns_not_utf16_ones() {
        // The emoji is 4 bytes but 2 UTF-16 code units, so what the editor calls
        // character 14 is byte 16.
        let original = doc("String s = \"🚀\";\n");
        let at = Position::new(0, 14); // between the emoji and the closing quote
        let reported = edits(&original, &[Change::incremental(Range::new(at, at), "ä")]);
        let edit = reported[0];
        assert_eq!(edit.start_byte, 16);
        assert_eq!(edit.start_position, BytePosition::new(0, 16));
        assert_eq!(edit.old_end_byte, 16);
        assert_eq!(edit.new_end_byte, 18, "ä is two bytes");
        assert_eq!(edit.new_end_position, BytePosition::new(0, 18));
    }

    #[test]
    fn an_edit_spanning_lines_reports_the_line_it_ends_on() {
        let original = doc("a\nb\n");
        let whole = Range::new(Position::new(0, 0), Position::new(1, 1));
        let reported = edits(&original, &[Change::incremental(whole, "x\ny\nz")]);
        let edit = reported[0];
        assert_eq!(edit.old_end_position, BytePosition::new(1, 1));
        assert_eq!(edit.new_end_position, BytePosition::new(2, 1));
    }

    #[test]
    fn a_deletion_reports_a_new_end_equal_to_its_start() {
        let original = doc("abcd");
        let range = Range::new(Position::new(0, 1), Position::new(0, 3));
        let edit = edits(&original, &[Change::incremental(range, "")])[0];
        assert_eq!(
            (edit.start_byte, edit.old_end_byte, edit.new_end_byte),
            (1, 3, 1)
        );
    }

    #[test]
    fn a_full_replacement_reports_one_edit_spanning_both_texts() {
        let original = doc("old\ntext\n");
        let edit = edits(&original, &[Change::full("new")])[0];
        assert_eq!(
            (edit.start_byte, edit.old_end_byte, edit.new_end_byte),
            (0, 9, 3)
        );
        assert_eq!(edit.start_position, BytePosition::new(0, 0));
        assert_eq!(edit.old_end_position, BytePosition::new(2, 0));
        assert_eq!(edit.new_end_position, BytePosition::new(0, 3));
    }

    #[test]
    fn each_edit_in_a_batch_is_reported_against_its_predecessors_text() {
        let original = doc("ab\n");
        let changes = vec![
            Change::incremental(Range::new(Position::new(0, 1), Position::new(0, 1)), "XX"),
            Change::incremental(Range::new(Position::new(0, 4), Position::new(0, 4)), "Y"),
        ];
        let reported = edits(&original, &changes);
        assert_eq!(reported.len(), 2);
        assert_eq!(reported[0].start_byte, 1);
        // Offset 4 only exists once the first edit has widened the line.
        assert_eq!(reported[1].start_byte, 4);
        assert_eq!(reported[1].new_end_byte, 5);
    }

    #[test]
    fn the_edit_batch_names_the_version_it_was_computed_from() {
        let original = doc("a");
        let (updated, batch) = original.with_changes_reporting(7, &[Change::full("b")]);
        assert_eq!(batch.base_version, 1);
        assert_eq!(updated.version(), 7);
    }

    #[test]
    fn a_reversed_range_is_normalised_rather_than_panicking() {
        let original = doc("abcd");
        let backwards = Range::new(Position::new(0, 3), Position::new(0, 1));
        assert_eq!(
            original
                .with_changes(2, &[Change::incremental(backwards, "-")])
                .text(),
            "a-d"
        );
    }
}
