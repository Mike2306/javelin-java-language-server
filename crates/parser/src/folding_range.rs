//! Compute folding ranges from a syntax tree.
//!
//! A folding range is a pair of *line numbers* and nothing else, so this is
//! the one reading aid that reads `Node::start_position().row` straight off
//! tree-sitter rather than converting a byte offset through a
//! `TextDocument`. The two line models are the same model: `LineIndex::new`
//! splits on `\n` alone (`crates/document/src/line_index.rs`) and tree-sitter
//! counts a row per `\n` too, so a row here is the `line` an LSP client
//! reads. Nothing on this path needs a *character*, which is where the two
//! would part company — a character is UTF-16 and a byte offset is not, and
//! that conversion still belongs to `TextDocument`.

use javelin_core::{CancellationToken, Cancelled};
use tree_sitter::Node;

/// A folding range in the source code.
#[derive(Debug, Clone)]
pub struct FoldingRange {
    pub start_line: usize,
    pub end_line: usize,
    /// None for bodies, Some(kind) for comments/imports.
    pub kind: Option<FoldingRangeKind>,
}

/// The kind of a folding range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldingRangeKind {
    Comment,
    Imports,
}

/// Find all foldable regions in a syntax tree.
pub fn folding_ranges(
    root: Node<'_>,
    token: &CancellationToken,
) -> Result<Vec<FoldingRange>, Cancelled> {
    token.check()?;
    let mut ranges = Vec::new();
    walk_folds(root, &mut ranges, token)?;
    fold_imports(root, &mut ranges, token)?;
    ranges.sort_by_key(|r| (r.start_line, r.end_line));
    Ok(ranges)
}

/// Recursively visit all nodes and emit folding ranges for ones that qualify.
fn walk_folds(
    node: Node<'_>,
    ranges: &mut Vec<FoldingRange>,
    token: &CancellationToken,
) -> Result<(), Cancelled> {
    token.check()?;
    match node.kind() {
        // Foldable bodies: class, interface, enum, annotation, constructor,
        // block, switch, array initializer.
        "class_body"
        | "interface_body"
        | "enum_body"
        | "annotation_type_body"
        | "constructor_body"
        | "block"
        | "switch_block"
        | "array_initializer" => {
            emit_fold(node, ranges, None);
        }
        // Multi-line block comments. Same shape as a body — the closing `*/`
        // stays visible — so the same emitter, and the same guard with it.
        "block_comment" => {
            emit_fold(node, ranges, Some(FoldingRangeKind::Comment));
        }
        _ => {}
    }

    // After processing this node, walk its children.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_folds(child, ranges, token)?;
    }
    Ok(())
}

/// Fold a node's import declarations, first to last, as one range per run.
///
/// Adjacency is a question about the AST and not about line numbers: the
/// blank line between `import java.util.List;` and `import com.example.Thing;`
/// is the most common import layout there is, and the reader who wants those
/// two collapsed wants them collapsed together. But `program` is
/// `repeat($._toplevel_statement)` and `_toplevel_statement` reaches
/// `class_declaration` as well as `import_declaration` — legal Java never
/// interleaves them, but a half-typed or machine-generated file can — so a
/// run only extends while direct children stay `import_declaration` (or a
/// comment, see below); anything else breaks it before the loop looks at
/// another import. `imports.len() >= 2` first-to-last spanning would instead
/// fold whatever sits between the two.
///
/// A `line_comment` or `block_comment` between two imports does *not* break
/// the run: it is an ordinary interspersed thing to write there (a license
/// header, a note on why the next import is deprecated), and folding it away
/// with the imports it sits beside is what the reader who folds imports at
/// all wants.
fn fold_imports(
    node: Node<'_>,
    ranges: &mut Vec<FoldingRange>,
    token: &CancellationToken,
) -> Result<(), Cancelled> {
    token.check()?;
    let mut run = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "import_declaration" => run.push(child),
            "line_comment" | "block_comment" => {}
            _ => {
                emit_import_run(&run, ranges);
                run.clear();
            }
        }
    }
    emit_import_run(&run, ranges);

    // Defensive against error recovery rather than reachable in a well-formed
    // file: `import_declaration` appears only as a direct child of `program`
    // in the grammar, so the recursion finds nothing below it — except where
    // recovery has parked one inside an `ERROR` node, which is exactly the
    // half-typed file this walk still has to answer for.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        fold_imports(child, ranges, token)?;
    }
    Ok(())
}

/// Emit one run of consecutive imports as a fold, if it holds two or more.
fn emit_import_run(run: &[Node<'_>], ranges: &mut Vec<FoldingRange>) {
    if run.len() >= 2 {
        let start_line = run[0].start_position().row;
        let end_line = run[run.len() - 1].end_position().row;
        // Two imports written on one line are two imports and no lines to
        // hide; a fold arrow that collapses nothing is worse than none.
        if end_line > start_line {
            ranges.push(FoldingRange {
                start_line,
                end_line,
                kind: Some(FoldingRangeKind::Imports),
            });
        }
    }
}

/// Emit a folding range for a body node.
///
/// Body nodes like `class_body` contain the opening and closing braces.
/// The fold spans from the line of the opening brace to the line just
/// before the closing brace (so the brace stays visible when collapsed).
///
/// The guard is on the range that would be *emitted*, not on the node: a
/// `void m() {\n}` body spans two rows, so decrementing leaves
/// `start_line == end_line` and a fold arrow that collapses nothing. Empty
/// two-line bodies are everywhere in generated Java, which is the source this
/// product exists to read.
fn emit_fold(node: Node<'_>, ranges: &mut Vec<FoldingRange>, kind: Option<FoldingRangeKind>) {
    let start_line = node.start_position().row;
    let end_line = node.end_position().row.saturating_sub(1);

    if end_line > start_line {
        ranges.push(FoldingRange {
            start_line,
            end_line,
            kind,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JavaParser;
    use javelin_document::{DocumentId, TextDocument};

    fn get_folds(source: &str) -> Result<Vec<super::FoldingRange>, Cancelled> {
        let doc = TextDocument::new(DocumentId::new("file:///test.java"), 1, source);
        let mut parser = JavaParser::new().expect("parser created");
        let tree = parser.parse(&doc).expect("parse succeeded");
        let token = CancellationToken::new();
        folding_ranges(tree.root(), &token)
    }

    #[test]
    fn a_class_body_folds_from_its_brace_to_the_line_above_its_close() {
        let source = "class Foo {\n  void m() {}\n}";
        let folds = get_folds(source).expect("folding succeeded");
        // Class body: from line 0 to line 1 (line above closing brace).
        let class_body_fold = folds
            .iter()
            .find(|f| f.start_line == 0 && f.end_line == 1)
            .expect("found class body fold");
        assert_eq!(class_body_fold.kind, None);
    }

    #[test]
    fn a_method_body_folds_inside_the_class_body_that_contains_it() {
        let source = "class Foo {\n  void m() {\n    int x = 1;\n  }\n}";
        let folds = get_folds(source).expect("folding succeeded");
        // Method body should exist.
        let method_fold = folds
            .iter()
            .find(|f| f.kind.is_none() && f.start_line == 1)
            .expect("found method body fold");
        assert!(method_fold.end_line > method_fold.start_line);
    }

    #[test]
    fn a_body_that_would_hide_no_line_is_not_offered_as_a_fold() {
        // Two rejection shapes and one acceptance, so the assertions below
        // are not vacuous: `one`'s body is all on one line, `two`'s spans two
        // rows and hides nothing once the closing brace is left visible, and
        // the class body around both genuinely folds.
        let source = "class Foo {\n  void one() {}\n  void two() {\n  }\n}";
        let folds = get_folds(source).expect("folding succeeded");

        let bodies: Vec<_> = folds.iter().filter(|f| f.kind.is_none()).collect();
        assert_eq!(
            bodies.len(),
            1,
            "only the class body hides a line; the two method bodies do not: {folds:?}"
        );
        assert_eq!((bodies[0].start_line, bodies[0].end_line), (0, 3));
        for fold in &folds {
            assert!(
                fold.end_line > fold.start_line,
                "a fold that collapses nothing must not be offered: {fold:?}"
            );
        }
    }

    #[test]
    fn a_run_of_imports_folds_as_one_range_of_kind_imports() {
        let source = "import java.util.List;\nimport java.util.Set;\n\npublic class Foo {}";
        let folds = get_folds(source).expect("folding succeeded");
        let import_fold = folds
            .iter()
            .find(|f| f.kind == Some(super::FoldingRangeKind::Imports))
            .expect("found import fold");
        assert_eq!(import_fold.start_line, 0);
        assert_eq!(import_fold.end_line, 1);
    }

    #[test]
    fn a_single_import_is_not_a_fold() {
        let source = "import java.util.List;\n\npublic class Foo {}";
        let folds = get_folds(source).expect("folding succeeded");
        let import_fold = folds
            .iter()
            .find(|f| f.kind == Some(super::FoldingRangeKind::Imports));
        assert!(import_fold.is_none(), "single import should not fold");
    }

    #[test]
    fn a_blank_line_between_two_imports_still_folds_them_as_one_range() {
        // The layout of nearly every real Java file: the JDK's imports, a
        // blank line, then the project's. A line-adjacency rule would split
        // this into two runs of one and offer no import fold at all.
        let source = "import java.util.List;\n\nimport com.example.Thing;\n\npublic class Foo {}";
        let folds = get_folds(source).expect("folding succeeded");
        let import_fold = folds
            .iter()
            .find(|f| f.kind == Some(super::FoldingRangeKind::Imports))
            .expect("found import fold");
        assert_eq!((import_fold.start_line, import_fold.end_line), (0, 2));
    }

    #[test]
    fn two_imports_written_on_one_line_are_not_offered_as_a_fold() {
        let source = "import a.B; import c.D;\n\npublic class Foo {}";
        let folds = get_folds(source).expect("folding succeeded");
        let import_fold = folds
            .iter()
            .find(|f| f.kind == Some(super::FoldingRangeKind::Imports));
        assert!(
            import_fold.is_none(),
            "an import run that hides no line must not be offered: {folds:?}"
        );
    }

    #[test]
    fn a_declaration_between_two_imports_does_not_get_folded_into_the_import_range() {
        // Legal Java never does this, but `_toplevel_statement` reaches both
        // `import_declaration` and `class_declaration`, so a half-typed or
        // machine-generated file can put one between two imports. Spanning
        // first import to last would fold the class declaration away too.
        let source = "import a.B;\nclass C {}\nimport d.E;\n";
        let folds = get_folds(source).expect("folding succeeded");
        let import_fold = folds
            .iter()
            .find(|f| f.kind == Some(super::FoldingRangeKind::Imports));
        assert!(
            import_fold.is_none(),
            "a declaration splits this into two one-import runs, neither foldable: {folds:?}"
        );
    }

    #[test]
    fn a_block_comment_on_one_line_is_not_a_fold() {
        let source = "public class Foo { /* comment */ }";
        let folds = get_folds(source).expect("folding succeeded");
        let comment_fold = folds
            .iter()
            .find(|f| f.kind == Some(super::FoldingRangeKind::Comment));
        assert!(
            comment_fold.is_none(),
            "single-line comment should not fold"
        );
    }

    #[test]
    fn a_multi_line_block_comment_folds_with_kind_comment() {
        let source = "/* comment\n   line 2\n */\npublic class Foo {}";
        let folds = get_folds(source).expect("folding succeeded");
        let comment_fold = folds
            .iter()
            .find(|f| f.kind == Some(super::FoldingRangeKind::Comment))
            .expect("found comment fold");
        assert_eq!(comment_fold.start_line, 0);
        assert_eq!(comment_fold.end_line, 1);
    }

    #[test]
    fn a_cancelled_token_stops_the_walk() {
        let source = "class Foo {\n  void m() {}\n}";
        let doc = TextDocument::new(DocumentId::new("file:///test.java"), 1, source);
        let mut parser = JavaParser::new().expect("parser created");
        let tree = parser.parse(&doc).expect("parse succeeded");
        let token = CancellationToken::new();
        token.cancel();
        let result = folding_ranges(tree.root(), &token);
        assert!(
            result.is_err(),
            "cancelled token should return Err(Cancelled)"
        );
    }
}
