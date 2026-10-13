//! document → parser, on real Java sources.
//!
//! Single-crate behaviour is covered by unit tests. What is verified here is
//! that an edit made through the document store produces a tree the parser
//! agrees with, on files that are not toy strings.

use javelin_document::{Change, DocumentId, DocumentStore, Position, Range};
use javelin_integration_tests::fixture;
use javelin_parser::JavaParser;

#[test]
fn a_well_formed_fixture_parses_cleanly() {
    let mut parser = JavaParser::new().expect("grammar");
    let document = javelin_document::TextDocument::new(
        DocumentId::new("file:///Simple.java"),
        1,
        fixture("java/Simple.java"),
    );
    let tree = parser.parse(&document).expect("parse");
    assert!(!tree.has_errors());
    assert_eq!(tree.root().kind(), "program");
}

#[test]
fn a_truncated_fixture_still_yields_a_tree() {
    let mut parser = JavaParser::new().expect("grammar");
    let document = javelin_document::TextDocument::new(
        DocumentId::new("file:///Broken.java"),
        1,
        fixture("java/Broken.java"),
    );
    let tree = parser
        .parse(&document)
        .expect("parse must not fail outright");
    assert!(tree.has_errors(), "the fixture is deliberately unparseable");
    assert!(
        tree.root().child_count() > 0,
        "recovery must still produce structure"
    );
}

#[test]
fn editing_through_the_store_feeds_the_parser_the_new_text() {
    let mut store = DocumentStore::new();
    let mut parser = JavaParser::new().expect("grammar");
    let id = DocumentId::new("file:///Simple.java");

    let opened = store.open(id.clone(), 1, fixture("java/Simple.java"));
    let before = parser.parse(&opened).expect("initial parse");
    assert!(!before.has_errors());

    // Delete the closing brace of `doubled()`, mid-edit.
    let text = opened.text();
    let brace = text.rfind('}').expect("a closing brace") as u32;
    let range = Range::new(opened.position_at(brace), opened.position_at(brace + 1));
    let edited = store
        .change(&id, 2, &[Change::incremental(range, "")])
        .expect("open");

    let after = parser.parse(&edited).expect("reparse");
    assert_eq!(after.version(), 2);
    assert!(
        after.has_errors(),
        "removing a brace must be visible to the parser"
    );
}

#[test]
fn positions_in_a_non_ascii_file_survive_a_round_trip() {
    let source = fixture("java/Unicode.java");
    let document =
        javelin_document::TextDocument::new(DocumentId::new("file:///Unicode.java"), 1, source);

    let rocket = document.text().find('🚀').expect("the rocket") as u32;
    let position = document.position_at(rocket);
    assert_eq!(document.offset_at(position), rocket);

    // The character after the rocket is two UTF-16 units further along.
    let after = Position::new(position.line, position.character + 2);
    assert_eq!(document.offset_at(after), rocket + '🚀'.len_utf8() as u32);
}
