//! The method calls a stretch of a file makes, and where each argument
//! starts — what an inlay hint is placed by.
//!
//! Byte offsets only: which method a call selects is overload resolution,
//! which this crate does not do, and converting an offset to a position is
//! `TextDocument`'s.

use std::ops::Range;

use javelin_core::{CancellationToken, Cancelled};
use tree_sitter::Node;

/// One `receiver.name(…)` or `name(…)` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    /// The byte offset of the method's name, where a definition query on the
    /// call is asked.
    pub name: u32,
    /// Each argument's byte span, in order; comments between them are not
    /// arguments.
    pub arguments: Vec<Range<u32>>,
}

/// The first `limit` calls with at least one argument whose span meets
/// `within`, in source order — stopping there, so a range as large as a
/// generated file costs what `limit` calls do and not what the file does.
///
/// A `new T(…)` is one, named at its class name, where the constructor it
/// selects is asked for; `outer.new Inner(…)` is not, since its name alone
/// does not say its type.
pub fn call_sites(
    root: Node<'_>,
    within: Range<u32>,
    limit: usize,
    token: &CancellationToken,
) -> Result<Vec<CallSite>, Cancelled> {
    let mut out = Vec::new();
    walk(root, &within, limit, &mut out, token)?;
    Ok(out)
}

/// Iterative, through one cursor: a generated file nests a call chain or a
/// concatenation deep enough that recursing per node could exhaust the stack.
fn walk(
    root: Node<'_>,
    within: &Range<u32>,
    limit: usize,
    out: &mut Vec<CallSite>,
    token: &CancellationToken,
) -> Result<(), Cancelled> {
    let mut cursor = root.walk();
    loop {
        token.check()?;
        if out.len() >= limit {
            return Ok(());
        }
        let node = cursor.node();
        let outside =
            (node.end_byte() as u32) < within.start || (node.start_byte() as u32) > within.end;
        if !outside {
            visit(node, out);
            if cursor.goto_first_child() {
                continue;
            }
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return Ok(());
            }
        }
    }
}

fn visit(node: Node<'_>, out: &mut Vec<CallSite>) {
    let name = match node.kind() {
        "method_invocation" => node.child_by_field_name("name"),
        "object_creation_expression" => class_name(node),
        "explicit_constructor_invocation" => node.child_by_field_name("constructor"),
        "enum_constant" => node.child_by_field_name("name"),
        _ => return,
    };
    let (Some(name), Some(arguments)) = (name, node.child_by_field_name("arguments")) else {
        return;
    };
    let mut cursor = arguments.walk();
    let spans: Vec<Range<u32>> = arguments
        .named_children(&mut cursor)
        .filter(|argument| !argument.kind().ends_with("comment"))
        .map(|argument| argument.start_byte() as u32..argument.end_byte() as u32)
        .collect();
    if !spans.is_empty() {
        out.push(CallSite {
            name: name.start_byte() as u32,
            arguments: spans,
        });
    }
}

/// The class name of a `new T(…)`, where `crate::creation_at` answers.
fn class_name(creation: Node<'_>) -> Option<Node<'_>> {
    let mut ty = creation.child_by_field_name("type")?;
    loop {
        match ty.kind() {
            "generic_type" => ty = ty.named_child(0)?,
            "scoped_type_identifier" => {
                ty = ty.named_child(u32::try_from(ty.named_child_count()).ok()?.checked_sub(1)?)?;
            }
            _ => break,
        }
    }
    (crate::expr::creation_named(ty) == Some(creation)).then_some(ty)
}

/// One call a multi-line chain selects from on the next line:
/// `orders.stream()` in `orders.stream()⏎    .map(…)⏎    .toList()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainLink {
    /// The call, projected far enough to type it.
    pub call: crate::Expr,
    /// The byte offset its text ends at, where a hint for it goes.
    pub end: u32,
    /// The byte offset the whole chain starts at, the same for every link of
    /// one chain.
    pub chain: u32,
}

/// Every link of every chain meeting `within` that breaks onto a new line at
/// two or more calls, innermost first — those outside `within` included, for
/// a caller comparing a link with the one before it — stopping after `limit`
/// links. The
/// last selection of a chain is not a link: its value is the statement's.
pub fn chain_links(
    document: &javelin_document::TextDocument,
    root: Node<'_>,
    within: Range<u32>,
    limit: usize,
    token: &CancellationToken,
) -> Result<Vec<ChainLink>, Cancelled> {
    let mut out = Vec::new();
    let mut cursor = root.walk();
    loop {
        token.check()?;
        if out.len() >= limit {
            return Ok(out);
        }
        let node = cursor.node();
        let outside =
            (node.end_byte() as u32) < within.start || (node.start_byte() as u32) > within.end;
        if !outside {
            if is_selection(node) && !selected(node) {
                links(document, node, &mut out);
            }
            if cursor.goto_first_child() {
                continue;
            }
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return Ok(out);
            }
        }
    }
}

fn is_selection(node: Node<'_>) -> bool {
    matches!(node.kind(), "method_invocation" | "field_access")
}

/// Whether `node` is the receiver of the selection around it.
fn selected(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        is_selection(parent) && parent.child_by_field_name("object") == Some(node)
    })
}

/// The links of the chain whose outermost selection is `outer`.
fn links(document: &javelin_document::TextDocument, outer: Node<'_>, out: &mut Vec<ChainLink>) {
    let mut found = Vec::new();
    let mut current = outer;
    // Iterative, down the receivers, so a generated chain of any length costs
    // a loop rather than a stack frame per link.
    while let Some(receiver) = current.child_by_field_name("object") {
        let breaks = receiver
            .next_sibling()
            .is_some_and(|separator| separator.start_position().row > receiver.end_position().row);
        if breaks && receiver.kind() == "method_invocation" {
            found.push(receiver);
        }
        if !is_selection(receiver) {
            break;
        }
        current = receiver;
    }
    if found.len() < 2 {
        return;
    }
    let chain = current.start_byte() as u32;
    for link in found.into_iter().rev() {
        out.push(ChainLink {
            call: crate::expr::project(document, link, 0),
            end: link.end_byte() as u32,
            chain,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JavaParser;
    use javelin_document::{DocumentId, TextDocument};

    fn sites(source: &str, within: Range<u32>) -> Vec<(String, Vec<String>)> {
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 0, source.to_owned());
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parses");
        let text = |range: Range<u32>| source[range.start as usize..range.end as usize].to_owned();
        call_sites(tree.root(), within, usize::MAX, &CancellationToken::new())
            .expect("not cancelled")
            .into_iter()
            .map(|site| {
                let name_end = source[site.name as usize..]
                    .find(|c: char| !c.is_alphanumeric())
                    .unwrap_or(0) as u32;
                (
                    text(site.name..site.name + name_end),
                    site.arguments.into_iter().map(text).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn a_calls_arguments_are_spanned_whole_and_comments_are_not_arguments() {
        let source = "class A { void m() { f(a.b(), /* x */ \"s\"); g(); new B(1); h(c -> c); } }";
        assert_eq!(
            sites(source, 0..source.len() as u32),
            vec![
                ("f".to_owned(), vec!["a.b()".to_owned(), "\"s\"".to_owned()]),
                ("B".to_owned(), vec!["1".to_owned()]),
                ("h".to_owned(), vec!["c -> c".to_owned()]),
            ]
        );
    }

    /// `this(…)`, `super(…)` and an enum constant's arguments are a
    /// constructor's, hinted like a `new`'s.
    #[test]
    fn explicit_constructor_calls_and_enum_constants_are_call_sites() {
        let source = "enum E { A(1), B; E(int a) { } E() { this(2); } }\n\
                      class K extends P { K() { super(3, 4); } }";
        assert_eq!(
            sites(source, 0..source.len() as u32),
            vec![
                ("A".to_owned(), vec!["1".to_owned()]),
                ("this".to_owned(), vec!["2".to_owned()]),
                ("super".to_owned(), vec!["3".to_owned(), "4".to_owned()]),
            ]
        );
    }

    #[test]
    fn only_calls_meeting_the_range_are_listed() {
        let source = "class A { void m() {\n f(1);\n g(2);\n } }";
        let second = source.find("g(2)").expect("g") as u32;
        assert_eq!(
            sites(source, second..second + 1),
            vec![("g".to_owned(), vec!["2".to_owned()])]
        );
    }

    /// A chain thousands deep is walked without recursing per node.
    #[test]
    fn a_deeply_nested_chain_does_not_exhaust_the_stack() {
        let chain: String = std::iter::repeat_n(".f(1)", 20_000).collect();
        let source = format!("class A {{ void m() {{ a{chain}; }} }}");
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 0, source.clone());
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parses");
        let found = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                call_sites(
                    tree.root(),
                    0..source.len() as u32,
                    usize::MAX,
                    &CancellationToken::new(),
                )
                .map(|sites| sites.len())
            })
            .expect("thread")
            .join()
            .expect("no overflow");
        assert_eq!(found, Ok(20_000));
    }

    #[test]
    fn the_walk_stops_at_its_limit() {
        let source = "class A { void m() { f(1); g(2); h(3); } }";
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 0, source.to_owned());
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parses");
        let found = call_sites(
            tree.root(),
            0..source.len() as u32,
            2,
            &CancellationToken::new(),
        )
        .expect("not cancelled");
        assert_eq!(found.len(), 2);
    }

    /// The text of each link `chain_links` finds, and which chain it is in.
    fn links(source: &str) -> Vec<(String, u32)> {
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 0, source.to_owned());
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parses");
        chain_links(
            &document,
            tree.root(),
            0..source.len() as u32,
            usize::MAX,
            &CancellationToken::new(),
        )
        .expect("not cancelled")
        .into_iter()
        .map(|link| {
            let crate::Expr::Call { name, .. } = &link.call else {
                panic!("a call: {:?}", link.call);
            };
            assert!(
                source[..link.end as usize].ends_with(')'),
                "ends after the call"
            );
            (name.clone(), link.chain)
        })
        .collect()
    }

    #[test]
    fn a_chain_breaking_at_two_calls_lists_them_innermost_first_but_not_its_last() {
        let source = "class A { void m() {\n  a.b()\n   .c(1)\n   .d()\n   .e();\n } }";
        let start = source.find("a.b").expect("a") as u32;
        assert_eq!(
            links(source),
            vec![
                ("b".to_owned(), start),
                ("c".to_owned(), start),
                ("d".to_owned(), start)
            ]
        );
    }

    /// One break is a wrapped line, not a chain; a field access is no call,
    /// and a chain inside an argument is a chain of its own.
    #[test]
    fn a_single_break_and_a_field_access_are_not_links() {
        let one = "class A { void m() {\n  a.b()\n   .c();\n } }";
        assert_eq!(links(one), Vec::new());
        let field = "class A { void m() {\n  a.f\n   .g\n   .h();\n } }";
        assert_eq!(links(field), Vec::new());
        let inline = "class A { void m() {\n  a.b().c()\n   .d()\n   .e();\n } }";
        assert_eq!(
            links(inline)
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            vec!["c", "d"],
            "`b()` is selected from on its own line"
        );
        let nested = "class A { void m() {\n  f(a.b()\n   .c()\n   .d());\n } }";
        assert_eq!(
            links(nested)
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            vec!["b", "c"]
        );
    }
}
