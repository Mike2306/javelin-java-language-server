//! Java syntax parsing.
//!
//! Tree-sitter is the interactive parsing layer: it is error-tolerant, so a file
//! that is mid-keystroke still yields a usable tree, and it is fast enough to run
//! on every change. This crate is the only place allowed to know that, so the
//! grammar can be swapped without touching anything downstream.
//!
//! Scope today is parsing — from scratch on open, incrementally on every edit —
//! and a tree wrapper. Java *semantics* live in `javelin-semantic` and do not
//! exist yet.

mod call_sites;
mod expr;
mod folding_range;
mod occurrences;
mod queries;
mod scopes;
mod selection_range;
mod semantic_tokens;
mod syntax;

use javelin_core::{Error, Result};
use javelin_document::{BytePosition, EditBatch, TextDocument, TextEdit};

pub use call_sites::{CallSite, ChainLink, call_sites, chain_links};
pub use expr::{
    Expr, Literal, Selection, SelectionKind, case_constant_at, creation_at,
    explicit_constructor_call_at, member_selection_at, unqualified_call_at, value_at,
};
pub use folding_range::{FoldingRange, FoldingRangeKind, folding_ranges};
pub use occurrences::{
    TypeOccurrence, creation_name_spans, explicit_constructor_spans, identifier_spans,
    identifier_spans_in, member_name_spans, type_occurrences, unbound_expression_names,
};
pub use queries::{
    AnnotationElement, Candidate, Definition, DefinitionKind, annotation_element_at,
    completions_at, definition_at, expression_name_at, name_at, occurrences_at, qualified_name_at,
    signature_of,
};
pub use scopes::{Binding, BindingKind, Scope, ScopeKind, ScopeTree};
pub use selection_range::{SelectionRange, selection_ranges};
pub use semantic_tokens::{SemanticToken, SemanticTokenKind, semantic_tokens, semantic_tokens_in};
pub use syntax::{
    Decl, DeclKind, FileSyntax, ImportDecl, LambdaParameter, NamedType, PackageDecl, Span,
};

/// A parsed Java file, tied to the document version it was produced from.
///
/// The version is carried so a consumer can tell whether the tree it is holding
/// still describes the buffer the user is looking at.
#[derive(Debug, Clone)]
pub struct SyntaxTree {
    tree: tree_sitter::Tree,
    version: i32,
}

impl SyntaxTree {
    pub fn version(&self) -> i32 {
        self.version
    }

    pub fn inner(&self) -> &tree_sitter::Tree {
        &self.tree
    }

    pub fn root(&self) -> tree_sitter::Node<'_> {
        self.tree.root_node()
    }

    /// Whether the grammar had to recover anywhere.
    ///
    /// A `true` here is normal while typing and must never be treated as a
    /// reason to withhold results.
    pub fn has_errors(&self) -> bool {
        self.tree.root_node().has_error()
    }
}

/// Owns a Tree-sitter parser configured for Java.
///
/// Creating one loads the grammar, so it is worth reusing per worker thread
/// rather than per request.
pub struct JavaParser {
    inner: tree_sitter::Parser,
}

// `tree_sitter::Parser` is opaque, so print the useful part: which grammar.
impl std::fmt::Debug for JavaParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JavaParser")
            .field("language", &"java")
            .finish()
    }
}

impl JavaParser {
    pub fn new() -> Result<Self> {
        let mut inner = tree_sitter::Parser::new();
        inner
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .map_err(|err| Error::internal(format!("incompatible Java grammar: {err}")))?;
        Ok(Self { inner })
    }

    /// Parse a document from scratch.
    ///
    /// This is the `didOpen` path and the fallback for any caller without a
    /// previous tree. Use [`reparse`](Self::reparse) on the edit path.
    pub fn parse(&mut self, document: &TextDocument) -> Result<SyntaxTree> {
        self.run(document, None)
    }

    /// Reparse after edits, reusing the subtrees the edits did not touch.
    ///
    /// Tree-sitter only reuses correctly when it has been told which byte ranges
    /// moved: an old tree handed over without matching `InputEdit`s produces a
    /// silently *wrong* tree rather than an error. `edits` therefore carries the
    /// version it was computed from, and a tree from any other version falls
    /// back to a full parse.
    pub fn reparse(
        &mut self,
        previous: &SyntaxTree,
        edits: &EditBatch,
        document: &TextDocument,
    ) -> Result<SyntaxTree> {
        if previous.version != edits.base_version {
            tracing::debug!(
                uri = document.id().as_str(),
                tree_version = previous.version,
                base_version = edits.base_version,
                "edits do not describe the tree on hand; parsing from scratch"
            );
            return self.parse(document);
        }

        let mut edited = previous.tree.clone();
        for edit in &edits.edits {
            edited.edit(&input_edit(edit));
        }
        self.run(document, Some(&edited))
    }

    fn run(
        &mut self,
        document: &TextDocument,
        previous: Option<&tree_sitter::Tree>,
    ) -> Result<SyntaxTree> {
        let tree = self
            .inner
            .parse(document.text(), previous)
            .ok_or_else(|| Error::Parse {
                path: document.id().as_str().into(),
                message: "parser returned no tree (cancelled or grammar timeout)".into(),
            })?;
        Ok(SyntaxTree {
            tree,
            version: document.version(),
        })
    }
}

fn input_edit(edit: &TextEdit) -> tree_sitter::InputEdit {
    tree_sitter::InputEdit {
        start_byte: edit.start_byte as usize,
        old_end_byte: edit.old_end_byte as usize,
        new_end_byte: edit.new_end_byte as usize,
        start_position: point(edit.start_position),
        old_end_position: point(edit.old_end_position),
        new_end_position: point(edit.new_end_position),
    }
}

/// `Point.column` counts bytes, which is exactly what a [`BytePosition`] holds.
fn point(position: BytePosition) -> tree_sitter::Point {
    tree_sitter::Point {
        row: position.line as usize,
        column: position.column as usize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use javelin_document::{DocumentId, TextDocument};

    fn doc(text: &str) -> TextDocument {
        TextDocument::new(DocumentId::new("file:///A.java"), 1, text)
    }

    #[test]
    fn parses_a_well_formed_class() {
        let mut parser = JavaParser::new().unwrap();
        let tree = parser.parse(&doc("class A { int x; }")).unwrap();
        assert_eq!(tree.root().kind(), "program");
        assert!(!tree.has_errors());
    }

    #[test]
    fn a_half_typed_file_still_produces_a_tree() {
        let mut parser = JavaParser::new().unwrap();
        let tree = parser.parse(&doc("class A { int x")).unwrap();
        assert!(tree.has_errors(), "the grammar should report recovery");
        assert_eq!(
            tree.root().kind(),
            "program",
            "but still hand back a usable root"
        );
    }

    #[test]
    fn a_reparsed_tree_matches_a_fresh_parse_of_the_same_text() {
        let mut parser = JavaParser::new().unwrap();
        let document = doc("class A { int x; }");
        let previous = parser.parse(&document).unwrap();
        let at = javelin_document::Position::new(0, 14);
        let (next, edits) = document.with_changes_reporting(
            2,
            &[javelin_document::Change::incremental(
                javelin_document::Range::new(at, at),
                "count",
            )],
        );

        let incremental = parser.reparse(&previous, &edits, &next).unwrap();
        let fresh = parser.parse(&next).unwrap();
        assert_eq!(incremental.root().to_sexp(), fresh.root().to_sexp());
        assert_eq!(incremental.version(), 2);
    }

    #[test]
    fn edits_from_another_version_fall_back_to_a_full_parse() {
        let mut parser = JavaParser::new().unwrap();
        let stale = parser.parse(&doc("class A { int x; }")).unwrap();
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 9, "interface B {}");
        // Edits that describe neither text: applying them would corrupt the tree.
        let edits = javelin_document::EditBatch {
            base_version: 8,
            edits: Vec::new(),
        };

        let reparsed = parser.reparse(&stale, &edits, &document).unwrap();
        assert_eq!(
            reparsed.root().to_sexp(),
            parser.parse(&document).unwrap().root().to_sexp(),
            "the mismatch must be caught before the stale tree is reused"
        );
        assert_eq!(reparsed.version(), 9);
    }

    #[test]
    fn the_tree_remembers_the_version_it_came_from() {
        let mut parser = JavaParser::new().unwrap();
        let document = doc("class A {}");
        let next = document.with_changes(2, &[javelin_document::Change::full("class B {}")]);
        let first = parser.parse(&document).unwrap();
        let second = parser.parse(&next).unwrap();
        assert_eq!(first.version(), 1);
        assert_eq!(second.version(), 2);
    }
}
