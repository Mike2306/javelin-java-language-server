//! Scope resolution over whole fixture files.
//!
//! The unit tests in `javelin-parser` pin one construct each against a source
//! string written for it. This file asks the question a user asks: with the
//! cursor *here*, what does this name mean? `Shadowing.java` binds one name four
//! times over, so every answer is only right for one reason.

use javelin_document::{DocumentId, TextDocument};
use javelin_integration_tests::fixture;
use javelin_parser::{BindingKind, JavaParser, ScopeTree};

struct File {
    source: String,
    scopes: ScopeTree,
}

fn file(name: &str) -> File {
    let source = fixture(&format!("java/{name}.java"));
    let document = TextDocument::new(
        DocumentId::new(format!("file:///{name}.java")),
        1,
        source.clone(),
    );
    let tree = JavaParser::new()
        .expect("grammar")
        .parse(&document)
        .expect("parse");
    let scopes = ScopeTree::build(&document, &tree);
    File { source, scopes }
}

impl File {
    /// The offset just after `marker`, which is where the cursor sits once the
    /// user has finished typing it.
    fn after(&self, marker: &str) -> u32 {
        let at = self
            .source
            .find(marker)
            .unwrap_or_else(|| panic!("{marker:?} is not in the fixture"));
        (at + marker.len()) as u32
    }

    fn kind_of(&self, marker: &str, name: &str) -> Option<BindingKind> {
        self.scopes
            .resolve(self.after(marker), name)
            .map(|binding| binding.kind)
    }
}

#[test]
fn one_name_bound_four_times_resolves_to_the_innermost_binding_each_time() {
    let f = file("Shadowing");
    assert_eq!(
        f.kind_of("int fromBlock =", "value"),
        Some(BindingKind::LocalVariable),
        "inside the innermost block, the block-local wins"
    );
    assert_eq!(
        f.kind_of("int fromParameter =", "value"),
        Some(BindingKind::Parameter),
        "in the method body outside that block, the parameter wins"
    );
    assert_eq!(
        f.kind_of("int inAnotherMethod() {", "value"),
        Some(BindingKind::Field),
        "in another method, only the field is in scope"
    );
    assert_eq!(
        f.kind_of("value -> value", "value"),
        Some(BindingKind::Parameter),
        "in a lambda, the lambda parameter wins over the field"
    );
}

#[test]
fn the_binding_a_name_resolves_to_points_at_the_declaration_that_won() {
    let f = file("Shadowing");
    let block_local = f
        .scopes
        .resolve(f.after("int fromBlock ="), "value")
        .expect("bound");
    assert_eq!(
        &f.source[block_local.span.start_byte as usize..block_local.span.end_byte as usize],
        "int value = 3;"
    );
    assert_eq!(
        &f.source
            [block_local.name_span.start_byte as usize..block_local.name_span.end_byte as usize],
        "value"
    );
    let declared_on = f.source[..f.source.find("int value = 3;").expect("the block-local")]
        .matches('\n')
        .count() as u32;
    assert_eq!(
        block_local.name_span.range.start.line, declared_on,
        "the LSP range must name the line the local was declared on"
    );
}

#[test]
fn a_local_is_not_visible_above_its_own_declaration() {
    let f = file("Shadowing");
    // `fromParameter` is declared before the block; `fromBlock` inside it.
    assert!(
        f.scopes
            .resolve(f.after("int inMethod("), "fromBlock")
            .is_none()
    );
    assert!(
        f.scopes
            .resolve(f.after("int fromBlock"), "fromBlock")
            .is_some()
    );
}

#[test]
fn a_field_is_visible_above_its_own_declaration() {
    let f = file("Shadowing");
    let field = f.after("private int value");
    let use_site = f.after("int inAnotherMethod() {");
    assert!(
        use_site < field,
        "the fixture must declare the field below the method that reads it"
    );
    assert_eq!(
        f.kind_of("int inAnotherMethod() {", "value"),
        Some(BindingKind::Field)
    );
}

#[test]
fn everything_visible_at_a_point_is_listed_once_per_name() {
    let f = file("Shadowing");
    let visible: Vec<_> = f
        .scopes
        .visible_at(f.after("int fromBlock ="))
        .into_iter()
        .map(|binding| (binding.name.as_str(), binding.kind))
        .collect();
    assert_eq!(
        visible,
        vec![
            ("value", BindingKind::LocalVariable),
            ("fromBlock", BindingKind::LocalVariable),
            ("fromParameter", BindingKind::LocalVariable),
            ("Shadowing", BindingKind::TypeDeclaration),
        ],
        "innermost first, one entry per name, the shadowed field left out"
    );
}

#[test]
fn a_half_typed_file_still_binds_what_it_managed_to_parse() {
    let f = file("Broken");
    assert_eq!(
        f.kind_of("int x =", "x"),
        Some(BindingKind::LocalVariable),
        "the truncated statement still declared a local"
    );
    assert_eq!(
        f.kind_of("int x =", "Broken"),
        Some(BindingKind::TypeDeclaration),
        "and the unclosed class still declared a type"
    );
}
