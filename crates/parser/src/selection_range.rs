//! Compute selection ranges from a syntax tree.
//!
//! Selection ranges allow a user to expand or contract their selection outward
//! from a starting position, progressing through syntactic ancestors. Each step
//! in the chain is a distinct syntactic unit that is meaningful for selection.

use javelin_core::{CancellationToken, Cancelled};
use tree_sitter::Node;

/// A selection range: the span of a syntactic unit and the next, outer unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionRange {
    /// Start byte offset.
    pub start_byte: u32,
    /// End byte offset (exclusive).
    pub end_byte: u32,
    /// The span of the next, outer syntactic unit, if one exists.
    pub parent: Option<Box<SelectionRange>>,
}

impl SelectionRange {
    /// Create a new selection range.
    fn new(start_byte: u32, end_byte: u32) -> Self {
        SelectionRange {
            start_byte,
            end_byte,
            parent: None,
        }
    }
}

/// Find the smallest named node containing the given byte offset.
/// If the smallest node is unnamed, walk up to find the first named ancestor.
/// If no descendant contains the offset, returns the root (which covers the whole file).
fn smallest_named_node_at(root: Node<'_>, offset: u32) -> Node<'_> {
    // Use tree-sitter's primitive to find the smallest node at this offset.
    let mut node = root
        .descendant_for_byte_range(offset as usize, offset as usize)
        .unwrap_or(root);

    // If it's unnamed, walk up to the first named ancestor.
    while !node.is_named() {
        if let Some(parent) = node.parent() {
            node = parent;
        } else {
            // Reached root and it's unnamed (shouldn't happen; root is always named).
            break;
        }
    }

    node
}

/// Build the parent chain from a node up to the root, collecting all named
/// ancestors and collapsing consecutive ancestors with identical ranges.
fn build_parent_chain(
    node: Node<'_>,
    token: &CancellationToken,
) -> Result<SelectionRange, Cancelled> {
    token.check()?;

    // Collect all named ancestors into a vector, bottom-up from the node.
    let mut ancestors = vec![SelectionRange::new(
        node.start_byte() as u32,
        node.end_byte() as u32,
    )];
    let mut parent = node.parent();

    while let Some(p) = parent {
        token.check()?;
        // Only collect named nodes.
        if p.is_named() {
            let ancestor = SelectionRange::new(p.start_byte() as u32, p.end_byte() as u32);
            // Deduplicate: skip if identical to the last collected ancestor.
            if let Some(last) = ancestors.last() {
                if ancestor.start_byte != last.start_byte || ancestor.end_byte != last.end_byte {
                    ancestors.push(ancestor);
                }
            } else {
                ancestors.push(ancestor);
            }
        }
        parent = p.parent();
    }

    // Fold the vector into nested SelectionRanges from outermost inward.
    // ancestors[0] is the innermost (the starting node),
    // ancestors[n-1] is the outermost (closest to root).
    // Each node's parent pointer should point to the next outer level.
    let mut result = ancestors
        .pop()
        .expect("ancestors is seeded with the starting node and only pushed to");
    while let Some(mut inner) = ancestors.pop() {
        inner.parent = Some(Box::new(result));
        result = inner;
    }

    Ok(result)
}

/// Compute selection ranges for one or more requested positions.
///
/// For each position, finds the smallest named syntactic node containing it,
/// then builds outward through named ancestors, collapsing consecutive
/// ancestors with identical byte ranges. Returns one `SelectionRange` per
/// requested position, in the same order.
pub fn selection_ranges(
    root: Node<'_>,
    positions: &[u32],
    token: &CancellationToken,
) -> Result<Vec<SelectionRange>, Cancelled> {
    token.check()?;
    let mut ranges = Vec::new();

    for &offset in positions {
        token.check()?;
        let node = smallest_named_node_at(root, offset);
        let range = build_parent_chain(node, token)?;
        ranges.push(range);
    }

    Ok(ranges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JavaParser;
    use javelin_document::{DocumentId, TextDocument};

    fn get_ranges(source: &str, offsets: &[u32]) -> Result<Vec<SelectionRange>, Cancelled> {
        let doc = TextDocument::new(DocumentId::new("file:///test.java"), 1, source);
        let mut parser = JavaParser::new().expect("parser created");
        let tree = parser.parse(&doc).expect("parse succeeded");
        let token = CancellationToken::new();
        selection_ranges(tree.root(), offsets, &token)
    }

    #[test]
    fn expanding_from_an_identifier_reaches_its_statement_then_its_method_then_its_class() {
        let source = "class Foo {\n  void m() {\n    int x = 1;\n  }\n}";
        let x_offset = source.find('x').expect("found x") as u32;
        let ranges = get_ranges(source, &[x_offset]).expect("ranges computed");

        // Verify we have at least one range and it has a parent chain extending to
        // multiple levels: identifier → statement → method → class are expected.
        assert_eq!(
            ranges.len(),
            1,
            "should have exactly one range for one position"
        );
        let first = &ranges[0];

        // Count levels in the chain.
        let mut count = 0;
        let mut current = Some(first);
        while let Some(r) = current {
            count += 1;
            current = r.parent.as_ref().map(|p| p.as_ref());
        }

        // Should have multiple distinct levels: at minimum identifier, its statement
        // context, method context, and class context.
        assert!(
            count >= 4,
            "should have at least 4 levels (identifier, statement, method, class), got {}",
            count
        );

        // Verify each level's range is distinct and non-empty.
        let mut current = first;
        let mut last_span = (current.start_byte, current.end_byte);
        while let Some(parent) = &current.parent {
            let parent_span = (parent.start_byte, parent.end_byte);
            assert_ne!(
                parent_span, last_span,
                "parent span should differ from child (no identical consecutive ranges)"
            );
            assert!(
                parent.end_byte > parent.start_byte,
                "parent range must be non-empty"
            );
            assert!(
                parent.start_byte <= current.start_byte && current.end_byte <= parent.end_byte,
                "parent must contain child"
            );
            last_span = parent_span;
            current = parent;
        }
    }

    /// Every span in a chain, innermost first.
    fn spans(range: &SelectionRange) -> Vec<(u32, u32)> {
        let mut spans = Vec::new();
        let mut current = Some(range);
        while let Some(r) = current {
            spans.push((r.start_byte, r.end_byte));
            current = r.parent.as_ref().map(|p| p.as_ref());
        }
        spans
    }

    #[test]
    fn two_ancestors_with_the_same_span_collapse_to_one_step() {
        // With no trailing newline, `program` and the sole `class_declaration`
        // under it cover exactly the same bytes — a real duplicated-span
        // wrapper pair in this grammar, rather than a source where no two
        // ancestors could have coincided in the first place. Removing the
        // deduplication in `build_parent_chain` makes the whole-file span
        // appear twice and fails this.
        let source = "class Foo { int x; }";
        let x_offset = source.find('x').expect("found x") as u32;
        let ranges = get_ranges(source, &[x_offset]).expect("ranges computed");
        let spans = spans(&ranges[0]);

        let whole_file = (0u32, source.len() as u32);
        assert_eq!(
            spans.iter().filter(|span| **span == whole_file).count(),
            1,
            "`program` and `class_declaration` share {whole_file:?} and must be one step: {spans:?}"
        );
        assert!(
            spans.windows(2).all(|pair| pair[0] != pair[1]),
            "no two consecutive steps may share a span: {spans:?}"
        );
    }

    #[test]
    fn a_cursor_on_an_anonymous_token_starts_at_its_nearest_named_ancestor() {
        let source = "class Foo { int x; }";
        // The opening brace: an anonymous token, and the only node that
        // starts at this offset besides the `class_body` that owns it.
        let brace_offset = source.find('{').expect("found {") as u32;
        let ranges = get_ranges(source, &[brace_offset]).expect("ranges computed");

        assert_eq!(
            ranges.len(),
            1,
            "every position yields exactly one range, even on an anonymous token"
        );
        // `class_body` — from the brace through the matching close, which is
        // the last byte of the file. A chain that admitted the brace itself
        // would start at (11, 12) instead.
        assert_eq!(
            (ranges[0].start_byte, ranges[0].end_byte),
            (brace_offset, source.len() as u32),
            "the innermost step is the brace's nearest named ancestor, `class_body`"
        );
    }

    #[test]
    fn a_position_past_the_end_of_the_file_clamps_instead_of_panicking() {
        let source = "class Foo {}";
        let past_end = (source.len() as u32) + 100;
        let ranges = get_ranges(source, &[past_end]).expect("ranges computed");

        // A clamped past-end position should still yield exactly one range
        // (the root or nearest named ancestor).
        assert_eq!(
            ranges.len(),
            1,
            "past-end position should yield exactly one range (not empty)"
        );

        // The range should be valid and non-empty.
        let first = &ranges[0];
        assert!(
            first.end_byte > first.start_byte,
            "past-end range must be non-empty"
        );
    }

    #[test]
    fn every_requested_position_gets_exactly_one_range_in_order() {
        let source = "class Foo {\n  int x;\n  int y;\n}";
        let x_offset = source.find('x').expect("found x") as u32;
        let y_offset = source.find('y').expect("found y") as u32;

        let ranges = get_ranges(source, &[x_offset, y_offset]).expect("ranges computed");
        assert_eq!(
            ranges.len(),
            2,
            "should have exactly 2 ranges for 2 positions"
        );

        // Verify they are different (covering different variables).
        assert_ne!(
            ranges[0].start_byte, ranges[1].start_byte,
            "ranges for different positions should differ"
        );
    }

    #[test]
    fn a_cancelled_token_stops_the_walk() {
        let source = "class Foo {}";
        let doc = TextDocument::new(DocumentId::new("file:///test.java"), 1, source);
        let mut parser = JavaParser::new().expect("parser created");
        let tree = parser.parse(&doc).expect("parse succeeded");
        let token = CancellationToken::new();
        token.cancel();
        let offset = 5u32;
        let result = selection_ranges(tree.root(), &[offset], &token);
        assert!(
            result.is_err(),
            "cancelled token should return Err(Cancelled)"
        );
    }
}
