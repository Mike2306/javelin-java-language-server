//! Where a file writes a name, for the reference index and the query that
//! reads it.
//!
//! Two walks, both over every token in the file. [`type_occurrences`] is the
//! workspace walk's: it records each plain name that could mean a type, and
//! what this file alone can say about which one. [`identifier_spans`] is the
//! request path's: every token spelled a given way, whatever position it is in,
//! for a caller that will resolve each one itself.
//!
//! What this module never does is decide what a name the file does not declare
//! means. That takes the file's package and imports asked against every type
//! the workspace and the classpath hold, and it is asked at query time rather
//! than here — the table it reads changes after the walk that recorded the
//! name.

use std::collections::HashMap;

use javelin_core::{CancellationToken, Cancelled};
use javelin_document::TextDocument;
use tree_sitter::Node;

use crate::SyntaxTree;
use crate::queries::{is_expression_name, is_identifier, is_simple_name, namespace, span, text};
use crate::scopes::{BindingKind, ScopeTree};
use crate::syntax::{FileSyntax, Span, declares_a_type};

/// One plain name written where a type could be meant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeOccurrence {
    /// As written, and the key a query looks occurrences up by.
    pub name: String,
    pub span: Span,
    /// The type this file itself says the name means, fully qualified — set
    /// when the scope tree binds the name to a type the file declares.
    /// `None` means this file cannot say, and the name is resolved later
    /// against the file's package and imports.
    pub fqn: Option<String>,
    /// Whether the grammar reads the position as a type — a
    /// `type_identifier`, or an annotation's name — rather than an
    /// expression where a type is one reading among several: `logger` in
    /// `logger.info()` may be a type and may be a field a supertype declares.
    /// Only a type position is one a diagnostic may judge.
    pub in_type_position: bool,
}

/// Every plain name in the file that could refer to a type.
///
/// The same filter and the same namespace order go-to-definition uses at a
/// cursor — `is_simple_name`, then `resolve_type` for a
/// `type_identifier` and `resolve_value` before `resolve_type` for a plain
/// `identifier` — so an occurrence is recorded exactly where definition would
/// have an answer that is a type:
///
/// | The token is | Recorded |
/// |---|---|
/// | bound to a type the file declares by a reachable name | with that fqn |
/// | bound to a local class | not at all |
/// | bound to a value or a type parameter | not at all |
/// | unbound, and the file declares a type of that name somewhere | not at all |
/// | unbound otherwise | with no fqn |
///
/// The fourth row is `javelin-semantic`'s own over-refusal: a nested type used
/// where the scope tree does not bind it is a name this file declares, and
/// resolving it against the classpath could answer someone else's type.
///
/// A type's own declaration name is an occurrence under the first row — the
/// scope tree binds a declaration to itself.
///
/// A segment of a qualified name — `java.util.List`, `Map.Entry`, an
/// import's — is recorded under the dotted text through it, by
/// `qualified_occurrence`'s rules.
pub fn type_occurrences(
    document: &TextDocument,
    tree: &SyntaxTree,
    scopes: &ScopeTree,
    syntax: &FileSyntax,
    token: &CancellationToken,
) -> Result<Vec<TypeOccurrence>, Cancelled> {
    token.check()?;
    // Keyed by where the declaration's name starts, which is what a binding
    // records of it: name spans are unique within a file.
    let fqns: HashMap<u32, String> = syntax
        .named_types()
        .into_iter()
        .map(|named| (named.decl.name_span.start_byte, named.fqn))
        .collect();
    let mut out = Vec::new();
    each_node(tree.root(), token, |node| {
        if !is_identifier(node) {
            return;
        }
        if !is_simple_name(node) {
            if let Some(segment) = qualified_occurrence(document, scopes, syntax, node) {
                out.push(segment);
            }
            return;
        }
        let Some(name) = text(document, node) else {
            return;
        };
        // Definition's first step, before any scope is asked: a declaration's
        // own name answers with that declaration. A constructor, a method or
        // a field is named there, not a type — `Greeter() {}` inside
        // `class Greeter` is not a use of the class. A type's own name stays,
        // and the scope tree binds it to itself below.
        let at = span(document, node);
        if syntax
            .declaration_at(at)
            .is_some_and(|decl| !declares_a_type(decl.kind))
        {
            return;
        }
        // `package p;` names a package, never a type.
        if node
            .parent()
            .is_some_and(|parent| parent.kind() == "package_declaration")
        {
            return;
        }
        let fqn = match namespace(scopes, node, name) {
            Some(binding) if binding.kind == BindingKind::TypeDeclaration => {
                match fqns.get(&binding.name_span.start_byte) {
                    Some(fqn) => Some(fqn.clone()),
                    // A local class: nothing outside this file can name it.
                    None => return,
                }
            }
            Some(_) => return,
            None if syntax.declares_type_named(name) => return,
            None => None,
        };
        let in_type_position = node.kind() == "type_identifier"
            || node.parent().is_some_and(|parent| {
                matches!(parent.kind(), "marker_annotation" | "annotation")
                    && parent.child_by_field_name("name") == Some(node)
            });
        out.push(TypeOccurrence {
            name: name.to_owned(),
            span: at,
            fqn,
            in_type_position,
        });
    })?;
    Ok(out)
}

/// A segment of a qualified name as an occurrence of the dotted text through
/// it — which the query resolves against the file's package and imports, as it
/// does a plain name — or `None` where definition would not answer there.
///
/// The last segment always; a segment before it only when it is capitalised,
/// which is what a type qualifying another looks like (`Map` in `Map.Entry`)
/// and what a package segment does not, so the table is not three rows per
/// import longer. And nothing whose first segment the file binds or declares
/// as a type, which `javelin_semantic::external_type_at` refuses too.
///
/// ponytail: a type qualifier spelled in lower case — `outer.Inner` — is not
/// recorded, so references on `outer` miss it there. The upgrade is recording
/// every segment and paying the rows.
fn qualified_occurrence(
    document: &TextDocument,
    scopes: &ScopeTree,
    syntax: &FileSyntax,
    node: Node<'_>,
) -> Option<TypeOccurrence> {
    let segment = crate::queries::qualified_segment(document, node)?;
    let name = text(document, node)?;
    if !segment.last && !name.starts_with(|c: char| c.is_uppercase()) {
        return None;
    }
    let first = segment.dotted.split('.').next()?;
    if syntax.declares_type_named(first)
        || scopes
            .resolve_type(node.start_byte() as u32, first)
            .is_some()
    {
        return None;
    }
    Some(TypeOccurrence {
        name: segment.dotted,
        span: span(document, node),
        fqn: None,
        // Only a name's last segment in a type position is one a
        // diagnostic may judge; an import is judged as an import.
        in_type_position: segment.in_type && segment.last,
    })
}

/// Every identifier token spelled `name`, in any position, in source order.
///
/// Unlike [`type_occurrences`] nothing is filtered by position: `other.name`
/// and `name()` are here too, because a caller that resolves each span the way
/// go-to-definition does — a member selection by its receiver, a call by its
/// arguments — is the one that can say what they mean.
pub fn identifier_spans(
    document: &TextDocument,
    tree: &SyntaxTree,
    name: &str,
    token: &CancellationToken,
) -> Result<Vec<Span>, Cancelled> {
    identifier_spans_in(document, tree, name, 0..=u32::MAX, token)
}

/// [`identifier_spans`] over the part of the file `within` covers, in bytes.
///
/// For a caller that knows where a name can be written — a local's scope —
/// and would otherwise walk a file of twenty thousand methods to find the
/// three that are in one of them. A subtree wholly outside the range is never
/// entered.
pub fn identifier_spans_in(
    document: &TextDocument,
    tree: &SyntaxTree,
    name: &str,
    within: std::ops::RangeInclusive<u32>,
    token: &CancellationToken,
) -> Result<Vec<Span>, Cancelled> {
    token.check()?;
    let text = document.text();
    let start = (*within.start() as usize).min(text.len());
    let mut end = (*within.end() as usize).saturating_add(1).min(text.len());
    // A range's last byte can be the first of a multi-byte character, and a
    // slice ending inside one is no slice at all.
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = Vec::new();
    if name.is_empty() || start >= end {
        return Ok(out);
    }
    // The spelling is found in the text and only then confirmed against the
    // tree: a substring search over a five-megabyte file is a fraction of a
    // walk over its millions of nodes, and one descent per hit — rejecting a
    // hit inside a longer name, a comment or a string — is bounded by the
    // tree's depth. Hits come out in source order, each once.
    let mut from = start;
    while let Some(found) = text.get(from..end).and_then(|rest| rest.find(name)) {
        token.check()?;
        let at = from + found;
        let until = at + name.len();
        from = until;
        let Some(node) = tree.root().descendant_for_byte_range(at, until) else {
            continue;
        };
        if is_identifier(node) && node.start_byte() == at && node.end_byte() == until {
            out.push(span(document, node));
        }
    }
    Ok(out)
}

/// Whether the name at `offset` is selected through a receiver — `x.name`,
/// `x.name()`, `x::name` — so no scope it is written in can capture it.
pub fn has_receiver(tree: &SyntaxTree, offset: u32) -> bool {
    let Some(node) = crate::queries::identifier_at(tree.root(), offset) else {
        return false;
    };
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "method_invocation" => {
            parent.child_by_field_name("name") == Some(node)
                && parent.child_by_field_name("object").is_some()
        }
        "field_access" => parent.child_by_field_name("field") == Some(node),
        "method_reference" => parent.child(0) != Some(node),
        _ => false,
    }
}

/// Whether `word` is written, as a whole word, inside a string literal or a
/// text block — `@MethodSource("word")`, a reflective lookup: uses a rename
/// cannot see and must not leave behind.
pub fn strings_mention(
    document: &TextDocument,
    tree: &SyntaxTree,
    word: &str,
    token: &CancellationToken,
) -> Result<bool, Cancelled> {
    mentioned_in(document, tree, word, &["string_literal"], token)
}

/// Whether `word` is written as a whole word inside a node of one of `kinds`,
/// found the way [`identifier_spans_in`] finds a spelling: text first, then
/// one walk up the tree per hit.
fn mentioned_in(
    document: &TextDocument,
    tree: &SyntaxTree,
    word: &str,
    kinds: &[&str],
    token: &CancellationToken,
) -> Result<bool, Cancelled> {
    let text = document.text();
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    if word.is_empty() {
        return Ok(false);
    }
    for (at, _) in text.match_indices(word) {
        token.check()?;
        let until = at + word.len();
        if text[..at].chars().next_back().is_some_and(is_word)
            || text[until..].chars().next().is_some_and(is_word)
        {
            continue;
        }
        let mut node = tree.root().descendant_for_byte_range(at, until);
        while let Some(current) = node {
            if kinds.contains(&current.kind()) {
                return Ok(true);
            }
            node = current.parent();
        }
    }
    Ok(false)
}

/// Every identifier that names a *selected member*: the name of a method
/// call, with a receiver or without, the field of a field access, and the
/// right-hand side of a method reference — the positions whose meaning
/// depends on a receiver's type, which is the semantic engine's to resolve.
///
/// A bare name that means an inherited *field* — `count` inside a subclass of
/// the file next door — is not a selection, so it is not here:
/// [`unbound_expression_names`] lists those, and a caller that wants both asks
/// for both.
pub fn member_name_spans(
    document: &TextDocument,
    tree: &SyntaxTree,
    token: &CancellationToken,
) -> Result<Vec<Span>, Cancelled> {
    let mut out = Vec::new();
    each_node(tree.root(), token, |node| {
        if node.kind() != "identifier" {
            return;
        }
        let Some(parent) = node.parent() else {
            return;
        };
        let selected = match parent.kind() {
            "method_invocation" => parent.child_by_field_name("name") == Some(node),
            "field_access" => parent.child_by_field_name("field") == Some(node),
            // `Type::member` — every child after the first names the member.
            "method_reference" => parent.child(0) != Some(node),
            // `@A(name = …)`: the element of `A` it names.
            "element_value_pair" => parent.child_by_field_name("key") == Some(node),
            // `case MON`: under an enum selector, the enum's constant.
            "switch_label" => crate::expr::case_selector(node).is_some(),
            _ => false,
        };
        if selected {
            out.push(span(document, node));
        }
    })?;
    Ok(out)
}

/// Every name in expression position the scope tree binds nothing of — a
/// field the file's types inherit, one a static import brings in, a type or a
/// package — for a caller that asks each one what it means.
pub fn unbound_expression_names(
    document: &TextDocument,
    tree: &SyntaxTree,
    scopes: &ScopeTree,
    token: &CancellationToken,
) -> Result<Vec<Span>, Cancelled> {
    let mut out = Vec::new();
    each_node(tree.root(), token, |node| {
        if is_expression_name(node)
            && let Some(name) = text(document, node)
            && scopes
                .resolve_value(node.start_byte() as u32, name)
                .is_none()
        {
            out.push(span(document, node));
        }
    })?;
    Ok(out)
}

/// The class name of every `new T(…)` in the file — [`crate::creation_at`]'s
/// positions — for a caller that asks each which constructor it selects.
pub fn creation_name_spans(
    document: &TextDocument,
    tree: &SyntaxTree,
    token: &CancellationToken,
) -> Result<Vec<Span>, Cancelled> {
    let mut out = Vec::new();
    each_node(tree.root(), token, |node| {
        if node.kind() == "type_identifier" && crate::expr::creation_named(node).is_some() {
            out.push(span(document, node));
        }
    })?;
    Ok(out)
}

/// The keyword of every unqualified `this(…)` and `super(…)`, and the name
/// of every enum constant, where [`crate::explicit_constructor_call_at`]
/// answers with `constants`.
pub fn explicit_constructor_spans(
    document: &TextDocument,
    tree: &SyntaxTree,
    token: &CancellationToken,
) -> Result<Vec<Span>, Cancelled> {
    let mut out = Vec::new();
    each_node(tree.root(), token, |node| {
        if let Some(at) = crate::expr::constructor_call_token(node, true) {
            out.push(span(document, at));
        }
    })?;
    Ok(out)
}

/// Every node under `root`, pre-order, which for a tree whose children lie in
/// source order is source order.
///
/// A cursor rather than recursion or a stack of collected children: this runs
/// over whole files — on the workspace walk's thread for every file, and on
/// the request loop for the open one — and a five-megabyte generated class is
/// millions of nodes, so the walk allocates nothing per node and its depth is
/// not the call stack's. `token` is polled per node, which is the unit of work
/// proportional to the file.
fn each_node<'t>(
    root: Node<'t>,
    token: &CancellationToken,
    mut visit: impl FnMut(Node<'t>),
) -> Result<(), Cancelled> {
    each_node_within(root, 0..=u32::MAX, token, &mut visit)
}

/// [`each_node`], never entering a subtree wholly outside `within`.
fn each_node_within<'t>(
    root: Node<'t>,
    within: std::ops::RangeInclusive<u32>,
    token: &CancellationToken,
    visit: &mut impl FnMut(Node<'t>),
) -> Result<(), Cancelled> {
    token.check()?;
    let outside = |node: Node<'_>| {
        (node.end_byte() as u32) < *within.start() || (node.start_byte() as u32) > *within.end()
    };
    let mut cursor = root.walk();
    loop {
        token.check()?;
        let node = cursor.node();
        let entered = !outside(node);
        if entered {
            visit(node);
        }
        if entered && cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JavaParser;
    use javelin_document::DocumentId;

    struct Fixture {
        source: String,
        document: TextDocument,
        tree: SyntaxTree,
    }

    fn fixture(source: &str) -> Fixture {
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 1, source);
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parse");
        Fixture {
            source: source.to_owned(),
            document,
            tree,
        }
    }

    #[test]
    fn has_receiver_tells_a_selection_from_a_bare_name() {
        let f =
            fixture("class A { void m() { go(); this.go(); x.f = f; Runnable r = this::go; } }");
        let at = |needle: &str| f.source.find(needle).expect("in source") as u32;
        assert!(!has_receiver(&f.tree, at("go();")));
        assert!(has_receiver(&f.tree, at("go(); x")));
        assert!(has_receiver(&f.tree, at("f = f")));
        assert!(!has_receiver(&f.tree, at("f;")));
        assert!(has_receiver(&f.tree, at("go; }")));
    }

    #[test]
    fn strings_mention_finds_whole_words_in_literals_only() {
        let f = fixture(
            "class A { @S(\"data\") void data() {} String s = \"metadata\"; // data\n\
             String t = \"\"\"\n  data\n  \"\"\"; }",
        );
        let token = CancellationToken::new();
        assert!(strings_mention(&f.document, &f.tree, "data", &token).unwrap());
        let g = fixture("class A { void data() {} String s = \"metadata\"; /* data */ }");
        assert!(!strings_mention(&g.document, &g.tree, "data", &token).unwrap());
    }

    impl Fixture {
        fn occurrences(&self) -> Vec<TypeOccurrence> {
            let scopes = ScopeTree::build(&self.document, &self.tree);
            let syntax = FileSyntax::build(&self.document, &self.tree);
            type_occurrences(
                &self.document,
                &self.tree,
                &scopes,
                &syntax,
                &CancellationToken::new(),
            )
            .expect("not cancelled")
        }

        /// `(name, byte offset, fqn)` per occurrence, readable in an assertion.
        fn summary(&self) -> Vec<(String, u32, Option<String>)> {
            self.occurrences()
                .into_iter()
                .map(|occurrence| (occurrence.name, occurrence.span.start_byte, occurrence.fqn))
                .collect()
        }

        fn named(&self, name: &str) -> Vec<(u32, Option<String>)> {
            self.summary()
                .into_iter()
                .filter(|(written, ..)| written == name)
                .map(|(_, at, fqn)| (at, fqn))
                .collect()
        }

        /// The byte offset of the `n`th (zero-based) occurrence of `needle`.
        fn nth(&self, needle: &str, n: usize) -> u32 {
            self.source
                .match_indices(needle)
                .nth(n)
                .unwrap_or_else(|| panic!("{needle:?} #{n} is not in the fixture"))
                .0 as u32
        }
    }

    #[test]
    fn a_type_bound_in_the_file_is_recorded_with_its_fqn_declaration_included() {
        let f = fixture("package p; class A {} class B { A a; }");
        let fqn = Some("p.A".to_owned());
        assert_eq!(
            f.named("A"),
            vec![(f.nth("A", 0), fqn.clone()), (f.nth("A a", 0), fqn)]
        );
    }

    #[test]
    fn a_nested_types_fqn_joins_on_a_dot() {
        let f = fixture("package p; class Outer { class Inner {} Inner i; }");
        assert_eq!(
            f.named("Inner"),
            vec![
                (f.nth("Inner", 0), Some("p.Outer.Inner".to_owned())),
                (f.nth("Inner i", 0), Some("p.Outer.Inner".to_owned())),
            ]
        );
    }

    #[test]
    fn an_unbound_name_is_recorded_with_no_fqn() {
        let f = fixture("class B { Greeter g = Greeter.create(); }");
        assert_eq!(
            f.named("Greeter"),
            vec![(f.nth("Greeter", 0), None), (f.nth("Greeter", 1), None)]
        );
    }

    /// A qualified name's last segment, and a capitalised segment before
    /// it, are recorded under the dotted text through them — an import's
    /// too, where a package segment and a first segment the file declares or
    /// binds are not.
    #[test]
    fn a_qualified_names_segments_are_recorded_by_their_dotted_text() {
        let f = fixture(
            "package p.q; import java.util.Map; class Outer { class In {} } \
             class B<T> { java.util.List l; Map.Entry e; Outer.In o; T.X t; }",
        );
        assert_eq!(f.named("java.util.Map"), vec![(f.nth("Map;", 0), None)]);
        assert_eq!(f.named("java.util.List"), vec![(f.nth("List l", 0), None)]);
        assert_eq!(f.named("Map.Entry"), vec![(f.nth("Entry", 0), None)]);
        assert_eq!(f.named("Map"), vec![(f.nth("Map.Entry", 0), None)]);
        for refused in ["java", "java.util", "p.q", "Outer.In", "T.X"] {
            assert_eq!(f.named(refused), Vec::new(), "{refused}");
        }
    }

    #[test]
    fn a_value_named_like_a_type_is_not_a_type_occurrence() {
        let f = fixture("class B { void m() { int Greeter = 1; use(Greeter); } }");
        assert_eq!(f.named("Greeter"), Vec::new());
    }

    #[test]
    fn a_type_parameter_is_not_a_type_occurrence() {
        let f = fixture("class Box<T> { T value; }");
        assert_eq!(f.named("T"), Vec::new());
    }

    #[test]
    fn a_local_class_is_not_recorded() {
        let f = fixture("class A { void m() { class Local {} Local l; } }");
        assert_eq!(f.named("Local"), Vec::new());
    }

    #[test]
    fn a_name_the_file_declares_but_does_not_bind_there_is_refused() {
        let f = fixture("class Base { class Inner {} } class Sub extends Base { Inner i; }");
        assert_eq!(
            f.named("Inner"),
            vec![(f.nth("Inner", 0), Some("Base.Inner".to_owned()))],
            "the declaration is recorded, the unbound use in Sub is not"
        );
    }

    /// What the diagnostics may judge: a name the grammar reads as a type, or
    /// an annotation's — never a receiver that might be an inherited field.
    #[test]
    fn a_type_position_is_a_type_identifier_or_an_annotation_name() {
        let f = fixture(
            "class A { Foo f; @Ann void m(Object x) { Bar b = new Baz(); logger.info(); \
             Object o = (Qux) x; } }",
        );
        let positions: Vec<(String, bool)> = f
            .occurrences()
            .into_iter()
            .map(|occurrence| (occurrence.name, occurrence.in_type_position))
            .filter(|(name, _)| name != "A" && name != "Object")
            .collect();
        assert_eq!(
            positions,
            vec![
                ("Foo".to_owned(), true),
                ("Ann".to_owned(), true),
                ("Bar".to_owned(), true),
                ("Baz".to_owned(), true),
                ("logger".to_owned(), false),
                ("Qux".to_owned(), true),
            ]
        );
    }

    #[test]
    fn a_selected_member_and_a_call_name_are_not_type_occurrences() {
        let f = fixture("class A { void m(Object x) { x.Foo = 1; Foo(); } }");
        assert_eq!(f.named("Foo"), Vec::new());
    }

    #[test]
    fn identifier_spans_finds_every_position_including_selected_members() {
        let f = fixture("class A { int go; void go() { this.go = 1; go(); } }");
        let spans = identifier_spans(&f.document, &f.tree, "go", &CancellationToken::new())
            .expect("not cancelled");
        let starts: Vec<u32> = spans.iter().map(|span| span.start_byte).collect();
        assert_eq!(
            starts,
            vec![
                f.nth("go", 0),
                f.nth("go", 1),
                f.nth("go", 2),
                f.nth("go", 3)
            ]
        );
    }

    #[test]
    fn identifier_spans_in_a_range_skips_everything_outside_it() {
        let f = fixture("class A { void m() { int x = 1; use(x); } void n() { int x = 2; } }");
        let start = f.nth("void m", 0);
        let end = f.nth("void n", 0);
        let spans = identifier_spans_in(
            &f.document,
            &f.tree,
            "x",
            start..=end,
            &CancellationToken::new(),
        )
        .expect("not cancelled");
        let starts: Vec<u32> = spans.iter().map(|span| span.start_byte).collect();
        assert_eq!(starts, vec![f.nth("x", 0), f.nth("x)", 0)]);
    }

    #[test]
    fn a_spelling_inside_a_longer_name_a_comment_or_a_string_is_not_an_identifier() {
        let f = fixture("class A { /* go */ int ago; String s = \"go\"; int go; int gone; }");
        let spans = identifier_spans(&f.document, &f.tree, "go", &CancellationToken::new())
            .expect("not cancelled");
        let starts: Vec<u32> = spans.iter().map(|span| span.start_byte).collect();
        assert_eq!(starts, vec![f.nth("int go;", 0) + "int ".len() as u32]);
    }

    #[test]
    fn a_constructors_a_methods_and_a_packages_own_names_are_not_type_occurrences() {
        // Definition answers each of these with the declaration itself, not
        // with a type, so none of them is a reference to one.
        let f = fixture("package p;\nclass Greeter { Greeter() {} void run() {} Greeter g; }");
        assert_eq!(
            f.named("Greeter"),
            vec![
                (f.nth("Greeter", 0), Some("p.Greeter".to_owned())),
                (f.nth("Greeter g", 0), Some("p.Greeter".to_owned())),
            ]
        );
        assert_eq!(f.named("run"), Vec::new());
        assert_eq!(f.named("p"), Vec::new());
    }

    #[test]
    fn a_range_ending_before_a_multibyte_character_still_finds_its_names() {
        let f = fixture("class A { void m(int p) { use(p); }Élan e; }");
        let start = f.nth("p)", 0);
        // Where the method's scope ends: on the `É`, whose second byte is past it.
        let end = f.nth("Élan", 0);
        let spans = identifier_spans_in(
            &f.document,
            &f.tree,
            "p",
            start..=end,
            &CancellationToken::new(),
        )
        .expect("not cancelled");
        assert_eq!(spans.len(), 2, "the parameter and its use: {spans:?}");
    }

    #[test]
    fn member_name_spans_are_calls_fields_and_method_reference_names() {
        let f = fixture(
            "class A { int count; void m(A other) { go(); other.run(1); this.count = 2; \
             Runnable r = other::stop; int local = count; switch (d) { case MON, TUE: } \
             } @Ann(key = count) int x; }",
        );
        let spans = member_name_spans(&f.document, &f.tree, &CancellationToken::new())
            .expect("not cancelled");
        let names: Vec<&str> = spans
            .iter()
            .map(|span| &f.source[span.start_byte as usize..span.end_byte as usize])
            .collect();
        assert_eq!(
            names,
            vec!["go", "run", "count", "stop", "MON", "TUE", "key"]
        );
    }

    #[test]
    fn creation_name_spans_are_each_news_class_name() {
        let f = fixture(
            "class A { Object m(B b) { new a.Box<String>(1); new Plain() { }; b.new Inner(); \
             return new int[1]; } }",
        );
        let spans = creation_name_spans(&f.document, &f.tree, &CancellationToken::new())
            .expect("not cancelled");
        let names: Vec<&str> = spans
            .iter()
            .map(|span| &f.source[span.start_byte as usize..span.end_byte as usize])
            .collect();
        assert_eq!(names, vec!["Box", "Plain", "Inner"]);
    }

    #[test]
    fn a_cancelled_walk_stops() {
        let f = fixture("class A { A a; }");
        let token = CancellationToken::new();
        token.cancel();
        let scopes = ScopeTree::build(&f.document, &f.tree);
        let syntax = FileSyntax::build(&f.document, &f.tree);
        assert_eq!(
            type_occurrences(&f.document, &f.tree, &scopes, &syntax, &token),
            Err(Cancelled)
        );
        assert_eq!(
            identifier_spans(&f.document, &f.tree, "a", &token),
            Err(Cancelled)
        );
    }
}
