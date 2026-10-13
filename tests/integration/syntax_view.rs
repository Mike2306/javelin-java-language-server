//! The syntax view of a real file, asserted as a rendered outline.
//!
//! The unit tests in `javelin-parser` pin one construct each against a source
//! string written for it. This file asks the opposite question: given a whole
//! Java file, is the shape that comes out the shape a reader would draw? An
//! outline answers that in a form a human can check; a hundred struct literals
//! would assert the same thing and hide it.

use javelin_document::{DocumentId, TextDocument};
use javelin_integration_tests::{fixture, outline};
use javelin_parser::{Decl, DeclKind, FileSyntax, JavaParser};

const FIXTURES: [&str; 10] = [
    "Simple",
    "Unicode",
    "Broken",
    "Generics",
    "Lambdas",
    "Records",
    "Nested",
    "Annotations",
    "Enums",
    "Javadoc",
];

fn source(name: &str) -> String {
    fixture(&format!("java/{name}.java"))
}

fn view(name: &str) -> FileSyntax {
    let document = TextDocument::new(
        DocumentId::new(format!("file:///{name}.java")),
        1,
        source(name),
    );
    let tree = JavaParser::new()
        .expect("grammar")
        .parse(&document)
        .expect("parse");
    FileSyntax::build(&document, &tree)
}

/// The first declaration reachable by walking the given names downward.
fn find<'a>(decls: &'a [Decl], path: &[&str]) -> &'a Decl {
    let (head, rest) = path.split_first().expect("a non-empty path");
    let found = decls
        .iter()
        .find(|decl| decl.name == *head)
        .unwrap_or_else(|| panic!("no declaration named {head}"));
    if rest.is_empty() {
        found
    } else {
        find(&found.children, rest)
    }
}

fn walk(decls: &[Decl], visit: &mut impl FnMut(&Decl)) {
    for decl in decls {
        visit(decl);
        walk(&decl.parameters, visit);
        walk(&decl.children, visit);
    }
}

#[test]
fn a_record_shows_its_components_its_constructors_and_the_type_nested_in_it() {
    assert_eq!(
        outline(&view("Records")),
        "\
package com.example
record Records
  component x: int
  component label: String
  constructor Records()
  constructor Records(x: int)
  method doubled(): int
  record Nested
    component a: double
    component b: double
"
    );
}

#[test]
fn nested_inner_local_and_anonymous_classes_appear_where_they_were_written() {
    assert_eq!(
        outline(&view("Nested")),
        "\
package com.example
class Nested
  class Static
    field inStatic: int
  class Inner
    field inInner: int
  method go(): void
    class Local
      field inLocal: int
    local anonymous: Runnable
      anonymous Runnable
        method run(): void
          local inAnonymous: int
"
    );
}

#[test]
fn an_enum_lists_its_constants_a_constant_body_and_its_ordinary_members() {
    assert_eq!(
        outline(&view("Enums")),
        "\
package com.example
enum Enums
  constant FIRST
  constant SECOND
  constant THIRD
    method weight(): int
  field value: int
  constructor Enums()
  constructor Enums(value: int)
  method weight(): int
"
    );
}

#[test]
fn a_lambda_body_hides_neither_the_declarations_in_it_nor_the_ones_after_it() {
    assert_eq!(
        outline(&view("Lambdas")),
        "\
package com.example
import java.util.List
import java.util.function.BiFunction
class Lambdas
  field noop: Runnable
  method run(names: List<String>): void
    local add: BiFunction<Integer, Integer, Integer>
    local shouted: String
    local reference: Runnable
  method run2(): void
"
    );
}

#[test]
fn generic_signatures_are_captured_exactly_as_the_author_wrote_them() {
    let view = view("Generics");
    let class = &view.declarations[0];
    assert_eq!(
        class.type_parameters.as_deref(),
        Some("<T extends Comparable<T>, U>")
    );

    let index = find(&view.declarations, &["Generics", "index"]);
    assert_eq!(index.type_parameters.as_deref(), Some("<R>"));
    assert_eq!(index.declared_type.as_deref(), Some("java.util.Map<T, R>"));
    assert_eq!(
        index.parameters[0].declared_type.as_deref(),
        Some("java.util.function.Function<? super T, R>"),
        "a wildcard bound is text, not something to resolve"
    );

    let copy = find(&view.declarations, &["Generics", "copy"]);
    assert_eq!(copy.parameters[1].declared_type.as_deref(), Some("E[]"));
}

#[test]
fn annotations_are_captured_as_written_on_whatever_carries_them() {
    let view = view("Annotations");
    let marker = find(&view.declarations, &["Annotations", "Marker"]);
    assert_eq!(marker.kind, DeclKind::AnnotationType);
    assert_eq!(
        find(&marker.children, &["count"]).declared_type.as_deref(),
        Some("int")
    );

    let field = find(&view.declarations, &["Annotations", "annotated"]);
    assert_eq!(field.modifiers, vec!["@Marker(\"field\")", "private"]);

    let go = find(&view.declarations, &["Annotations", "go"]);
    assert_eq!(
        go.modifiers,
        vec![
            "@Marker(value = \"method\", count = 2)",
            "@SafeVarargs",
            "final"
        ]
    );
    let rest = &go.parameters[0];
    assert!(rest.varargs);
    assert_eq!(
        rest.declared_type.as_deref(),
        Some("String"),
        "no ... in the type"
    );
    assert_eq!(rest.modifiers, vec!["@Marker(\"param\")"]);
}

#[test]
fn doc_comments_are_carried_verbatim_and_ordinary_block_comments_are_not() {
    let view = view("Javadoc");
    let class = &view.declarations[0];
    assert_eq!(
        class.doc.as_deref(),
        Some("/**\n * A documented type.\n */")
    );
    assert_eq!(
        find(&view.declarations, &["Javadoc", "count"])
            .doc
            .as_deref(),
        Some("/** A documented field. */")
    );
    assert!(
        find(&view.declarations, &["Javadoc", "ignored"])
            .doc
            .is_none()
    );

    let add = find(&view.declarations, &["Javadoc", "add"]);
    let doc = add.doc.as_deref().expect("the method is documented");
    assert!(
        doc.contains("@param by how much"),
        "tags stay in the raw text, unparsed: {doc}"
    );
}

#[test]
fn a_file_caught_mid_keystroke_still_yields_what_did_parse() {
    assert_eq!(
        outline(&view("Broken")),
        "\
package com.example
class Broken
  method go(): void
    local x: int
"
    );
}

#[test]
fn every_name_span_slices_the_name_out_of_the_source_it_came_from() {
    for name in FIXTURES {
        let source = source(name);
        let mut checked = 0;
        walk(&view(name).declarations, &mut |decl| {
            if decl.name.is_empty() {
                return;
            }
            let bytes = decl.name_span.start_byte as usize..decl.name_span.end_byte as usize;
            assert_eq!(
                source.get(bytes),
                Some(decl.name.as_str()),
                "{name}: {:?} {} has a name span that does not cover its name",
                decl.kind,
                decl.name
            );
            assert!(
                decl.span.start_byte <= decl.name_span.start_byte
                    && decl.name_span.end_byte <= decl.span.end_byte,
                "{name}: {} has a name outside its own declaration",
                decl.name
            );
            checked += 1;
        });
        assert!(checked > 0, "{name} declared nothing");
    }
}
