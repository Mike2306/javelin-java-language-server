//! `textDocument/documentSymbol` — the outline of the open file.
//!
//! This is the first feature a user sees, and it needs nothing but a parse:
//! [`Feature::DocumentSymbols`] is available from [`Readiness::Syntax`], which
//! the server reaches on `didOpen`. There is no index to consult and no build
//! system to wait for, so the answer is always about the buffer in front of the
//! user and always complete for it.
//!
//! [`Feature::DocumentSymbols`]: javelin_protocol::Feature::DocumentSymbols
//! [`Readiness::Syntax`]: javelin_protocol::Readiness::Syntax

use std::fmt::Write as _;

use javelin_core::{CancellationToken, Cancelled};
use javelin_document::DocumentId;
use javelin_parser::{Decl, DeclKind};
use lsp_types::{DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, SymbolKind};

use crate::ServerState;
use crate::handlers::to_lsp_range;

/// Outline one document.
///
/// A document that is not open, or whose latest version never parsed, outlines
/// as empty. Both are ordinary states — the editor asks for the outline while
/// the file is still being opened — and an error would only make the client
/// show a failure for something it will ask about again in a moment.
pub fn handle(
    state: &mut ServerState,
    params: &DocumentSymbolParams,
    token: &CancellationToken,
) -> Result<DocumentSymbolResponse, Cancelled> {
    // Before `syntax`, which builds the view on demand and is the expensive
    // part of this request on a large file.
    token.check()?;
    let id = DocumentId::new(params.text_document.uri.as_str());
    let Some(view) = state.syntax(&id) else {
        return Ok(DocumentSymbolResponse::Nested(Vec::new()));
    };
    symbols(&view.declarations, token).map(DocumentSymbolResponse::Nested)
}

/// Project declarations onto symbols, dropping the ones an outline should not
/// show and keeping whatever they contain.
///
/// The token is polled once per declaration rather than per node: a keystroke
/// invalidates this request, and a file large enough for the walk to be worth
/// abandoning has plenty of declarations to notice at.
fn symbols(decls: &[Decl], token: &CancellationToken) -> Result<Vec<DocumentSymbol>, Cancelled> {
    let mut out = Vec::with_capacity(decls.len());
    for decl in decls {
        token.check()?;
        let children = symbols(&decl.children, token)?;
        // An empty name means the grammar recovered mid-declaration; clients
        // render it as a blank row. Its members are still worth showing.
        match symbol_kind(decl.kind).filter(|_| !decl.name.is_empty()) {
            Some(kind) => out.push(symbol(decl, kind, children)),
            None => out.extend(children),
        }
    }
    Ok(out)
}

// `deprecated` is deprecated in LSP itself — `tags` replaced it — but the struct
// still has the field, so it has to be given a value to build one.
#[expect(
    deprecated,
    reason = "DocumentSymbol::deprecated has no non-deprecated spelling"
)]
fn symbol(decl: &Decl, kind: SymbolKind, children: Vec<DocumentSymbol>) -> DocumentSymbol {
    DocumentSymbol {
        name: decl.name.clone(),
        detail: detail(decl),
        kind,
        tags: None,
        deprecated: None,
        range: to_lsp_range(decl.span.range),
        selection_range: to_lsp_range(decl.name_span.range),
        children: (!children.is_empty()).then_some(children),
    }
}

/// `None` for the declarations an outline is better off without: parameters and
/// locals are noise at this scale, and an initialiser block has no name to show.
/// What they contain — a local class, an anonymous class — is kept.
pub(crate) fn symbol_kind(kind: DeclKind) -> Option<SymbolKind> {
    Some(match kind {
        DeclKind::Class | DeclKind::AnonymousClass => SymbolKind::CLASS,
        // `@interface Foo {}` is an interface declaration, however it is spelled.
        DeclKind::Interface | DeclKind::AnnotationType => SymbolKind::INTERFACE,
        DeclKind::Enum => SymbolKind::ENUM,
        DeclKind::EnumConstant => SymbolKind::ENUM_MEMBER,
        // A record is a data aggregate rather than a general class, and STRUCT
        // is the one kind clients give a distinct icon for exactly that.
        DeclKind::Record => SymbolKind::STRUCT,
        DeclKind::Method | DeclKind::AnnotationElement => SymbolKind::METHOD,
        DeclKind::Constructor => SymbolKind::CONSTRUCTOR,
        DeclKind::Field | DeclKind::RecordComponent => SymbolKind::FIELD,
        DeclKind::StaticInitializer
        | DeclKind::InstanceInitializer
        | DeclKind::Parameter
        | DeclKind::LocalVariable => return None,
    })
}

/// The signature as the author wrote it. Nothing here is resolved: a type is
/// whatever text stands where a type goes.
fn detail(decl: &Decl) -> Option<String> {
    match decl.kind {
        DeclKind::Method | DeclKind::Constructor | DeclKind::AnnotationElement => {
            let mut out = decl.type_parameters.clone().unwrap_or_default();
            let parameters: Vec<_> = decl.parameters.iter().map(parameter).collect();
            let _ = write!(out, "({})", parameters.join(", "));
            if let Some(returns) = &decl.declared_type {
                let _ = write!(out, ": {returns}");
            }
            Some(out)
        }
        DeclKind::Field | DeclKind::RecordComponent => decl.declared_type.clone(),
        _ => decl.type_parameters.clone(),
    }
}

fn parameter(decl: &Decl) -> String {
    let ellipsis = if decl.varargs { "..." } else { "" };
    match &decl.declared_type {
        Some(declared) => format!("{declared}{ellipsis} {}", decl.name),
        None => decl.name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use javelin_core::Config;

    fn outline(source: &str) -> Vec<DocumentSymbol> {
        let mut state = opened(source);
        let params = params("file:///A.java");
        nested(handle(&mut state, &params, &CancellationToken::new()))
    }

    fn opened(source: &str) -> ServerState {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        state.did_open(DocumentId::new("file:///A.java"), 1, source.to_owned());
        state
    }

    fn params(uri: &str) -> DocumentSymbolParams {
        serde_json::from_value(serde_json::json!({ "textDocument": { "uri": uri } }))
            .expect("well-formed params")
    }

    fn nested(answer: Result<DocumentSymbolResponse, Cancelled>) -> Vec<DocumentSymbol> {
        match answer.expect("not cancelled") {
            DocumentSymbolResponse::Nested(symbols) => symbols,
            DocumentSymbolResponse::Flat(_) => panic!("the outline must be hierarchical"),
        }
    }

    /// One `kind name` line per symbol, nested by indentation, so a test can
    /// state the shape of a file rather than a page of struct literals. Details
    /// and ranges have their own tests.
    fn shape(symbols: &[DocumentSymbol]) -> String {
        fn render(symbols: &[DocumentSymbol], depth: usize, out: &mut String) {
            for symbol in symbols {
                let _ = writeln!(
                    out,
                    "{}{:?} {}",
                    "  ".repeat(depth),
                    symbol.kind,
                    symbol.name
                );
                if let Some(children) = &symbol.children {
                    render(children, depth + 1, out);
                }
            }
        }
        let mut out = String::new();
        render(symbols, 0, &mut out);
        out
    }

    /// The first symbol reachable by walking the given names downward.
    fn find<'a>(symbols: &'a [DocumentSymbol], path: &[&str]) -> &'a DocumentSymbol {
        let (head, rest) = path.split_first().expect("a non-empty path");
        let found = symbols
            .iter()
            .find(|symbol| symbol.name == *head)
            .unwrap_or_else(|| panic!("no symbol named {head} in\n{}", shape(symbols)));
        match rest.is_empty() {
            true => found,
            false => find(found.children.as_deref().unwrap_or_default(), rest),
        }
    }

    #[test]
    fn members_are_nested_under_the_type_that_declares_them() {
        let symbols = outline(
            "class Outer {\n\
               int field;\n\
               static class Inner { void go() {} }\n\
             }\n",
        );
        assert_eq!(
            shape(&symbols),
            "Class Outer\n  Field field\n  Class Inner\n    Method go\n"
        );
    }

    #[test]
    fn the_range_covers_the_declaration_and_the_selection_range_only_the_name() {
        let symbols = outline("public class Box {}");
        let class = &symbols[0];
        assert_eq!(class.range.start, lsp_types::Position::new(0, 0));
        assert_eq!(class.range.end, lsp_types::Position::new(0, 19));
        assert_eq!(class.selection_range.start, lsp_types::Position::new(0, 13));
        assert_eq!(class.selection_range.end, lsp_types::Position::new(0, 16));
    }

    #[test]
    fn each_java_declaration_reaches_the_outline_as_its_own_kind() {
        let symbols = outline(
            "enum Colour { RED; }\n\
             interface Face { int VALUE = 1; }\n\
             @interface Marker { String value(); }\n\
             record Point(int x) { Point(int x, int y) { this.x = x; } }\n",
        );
        assert_eq!(
            shape(&symbols),
            "Enum Colour\n  \
               EnumMember RED\n\
             Interface Face\n  \
               Field VALUE\n\
             Interface Marker\n  \
               Method value\n\
             Struct Point\n  \
               Field x\n  \
               Constructor Point\n"
        );
    }

    #[test]
    fn a_method_signature_reads_as_it_was_written() {
        let symbols = outline("class A { <T> T pick(T first, String... rest) { return first; } }");
        let method = find(&symbols, &["A", "pick"]);
        assert_eq!(
            method.detail.as_deref(),
            Some("<T>(T first, String... rest): T")
        );
    }

    #[test]
    fn a_field_shows_the_type_it_was_declared_with() {
        let symbols = outline("class A { private Map<String, Integer> counts; }");
        assert_eq!(
            find(&symbols, &["A", "counts"]).detail.as_deref(),
            Some("Map<String, Integer>")
        );
    }

    #[test]
    fn locals_stay_out_of_the_outline_but_the_classes_inside_them_do_not() {
        let symbols = outline(
            "class A {\n\
               void go() {\n\
                 int counter = 0;\n\
                 Runnable r = new Runnable() { public void run() {} };\n\
               }\n\
             }\n",
        );
        assert_eq!(
            shape(&symbols),
            "Class A\n  Method go\n    Class Runnable\n      Method run\n"
        );
    }

    #[test]
    fn a_declaration_the_parser_could_not_name_is_replaced_by_its_members() {
        let symbols = outline("class { void stillHere() {} }");
        assert_eq!(shape(&symbols), "Method stillHere\n");
    }

    #[test]
    fn siblings_do_not_overlap_so_a_client_can_nest_them() {
        let symbols = outline("class A { int a, b, c; }");
        let members = symbols[0].children.clone().expect("three fields");
        assert_eq!(members.len(), 3);
        for pair in members.windows(2) {
            assert!(pair[0].range.end <= pair[1].range.start, "{pair:?}");
        }
    }

    #[test]
    fn a_document_that_was_never_opened_outlines_as_empty_rather_than_failing() {
        let mut state = opened("class A {}");
        let params = params("file:///Absent.java");
        assert!(nested(handle(&mut state, &params, &CancellationToken::new())).is_empty());
    }

    #[test]
    fn a_file_that_has_not_parsed_yet_outlines_as_empty() {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        let params = params("file:///A.java");
        assert!(nested(handle(&mut state, &params, &CancellationToken::new())).is_empty());
    }

    #[test]
    fn a_cancelled_outline_stops_instead_of_finishing() {
        let mut state = opened("class A { void go() {} }");
        let token = CancellationToken::new();
        token.cancel();
        let answer = handle(&mut state, &params("file:///A.java"), &token);
        assert_eq!(answer.err(), Some(Cancelled));
    }
}
