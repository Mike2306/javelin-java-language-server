//! `textDocument/selectionRange` — nested selection expansion.
//!
//! Selection ranges allow a user to expand or contract their selection outward
//! from a starting position, progressing through syntactic ancestors. Each step
//! in the chain is a distinct syntactic unit. Computed from the syntax tree alone,
//! available from [`Readiness::Syntax`].
//!
//! [`Readiness::Syntax`]: javelin_protocol::Readiness::Syntax

use javelin_core::{CancellationToken, Cancelled};
use javelin_document::DocumentId;
use javelin_parser::selection_ranges as compute_selection_ranges;
use lsp_types::{SelectionRange as LspSelectionRange, SelectionRangeParams};

use crate::ServerState;
use crate::handlers::{to_lsp_position, to_position};

/// Selection ranges for one or more requested positions.
///
/// A document that is not open, or whose latest version never parsed, offers
/// no ranges. Both are ordinary states and an error would only make the client
/// show a failure for something it will ask about again in a moment. Returns
/// an empty vector rather than an error.
pub fn handle(
    state: &mut ServerState,
    params: &SelectionRangeParams,
    token: &CancellationToken,
) -> Result<Vec<LspSelectionRange>, Cancelled> {
    token.check()?;
    let id = DocumentId::new(params.text_document.uri.as_str());
    let Some(view) = state.view(&id) else {
        return Ok(Vec::new());
    };

    // ponytail: this handler reads `view.tree` and `view.document` alone and
    // still pays for the `FileSyntax` and `ScopeTree` the warm view builds —
    // the module doc on `crates/server/src/handlers.rs` states the ceiling and
    // the upgrade, which is the same one `folding_range` is waiting on.
    //
    // Convert requested positions to byte offsets, clamping past-end positions.
    let doc_len = view.document.text().len() as u32;
    let mut offsets = Vec::new();
    for pos in &params.positions {
        let javelin_pos = to_position(*pos);
        let offset = view.document.offset_at(javelin_pos);
        // Clamp to document bounds.
        offsets.push(offset.min(doc_len));
    }

    // Compute selection ranges from the parser.
    let parser_ranges = compute_selection_ranges(view.tree.root(), &offsets, token)?;

    // Convert from parser type to LSP type.
    let lsp_ranges = parser_ranges
        .iter()
        .map(|r| range_to_lsp(r, view.document.as_ref()))
        .collect();

    Ok(lsp_ranges)
}

/// Convert a parser SelectionRange to an LSP SelectionRange.
fn range_to_lsp(
    range: &javelin_parser::SelectionRange,
    document: &javelin_document::TextDocument,
) -> LspSelectionRange {
    let start = to_lsp_position(document.position_at(range.start_byte));
    let end = to_lsp_position(document.position_at(range.end_byte));
    LspSelectionRange {
        range: lsp_types::Range { start, end },
        parent: range
            .parent
            .as_ref()
            .map(|p| Box::new(range_to_lsp(p, document))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServerState;
    use javelin_core::Config;
    use javelin_document::DocumentId;

    fn opened(source: &str) -> ServerState {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        state.did_open(DocumentId::new("file:///A.java"), 1, source.to_owned());
        state
    }

    fn params(uri: &str, positions: Vec<(u32, u32)>) -> SelectionRangeParams {
        let positions: Vec<_> = positions
            .into_iter()
            .map(|(line, character)| serde_json::json!({ "line": line, "character": character }))
            .collect();
        serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": uri },
            "positions": positions
        }))
        .expect("well-formed params")
    }

    #[test]
    fn a_position_on_an_anonymous_token_yields_a_range() {
        let source = "class Foo { int x; }";
        let mut state = opened(source);
        let token = CancellationToken::new();
        // Position at the opening brace (an anonymous token).
        let result = handle(&mut state, &params("file:///A.java", vec![(0, 11)]), &token);
        let ranges = result.expect("ranges computed");

        // The handler's responsibility: every position yields exactly one range,
        // even on anonymous tokens. The algorithm tests in the parser ensure
        // the chain structure and named-node resolution.
        assert_eq!(
            ranges.len(),
            1,
            "handler must return exactly one range per requested position, even on anonymous tokens"
        );
        assert!(
            ranges[0].range.start < ranges[0].range.end,
            "range must be non-empty"
        );
    }

    #[test]
    fn a_position_past_the_end_of_the_file_clamps_instead_of_panicking() {
        let source = "class Foo {}";
        let mut state = opened(source);
        let token = CancellationToken::new();
        // Position way past the end should clamp to document length and find
        // the root or nearest named ancestor.
        let result = handle(
            &mut state,
            &params("file:///A.java", vec![(0, 100)]),
            &token,
        );
        let ranges = result.expect("ranges computed");
        // Clamped position should still yield exactly one range.
        assert_eq!(
            ranges.len(),
            1,
            "position past end should yield exactly one range (for the clamped position)"
        );
        assert!(
            ranges[0].range.start <= ranges[0].range.end,
            "range should be valid"
        );
    }

    #[test]
    fn a_document_that_is_not_open_answers_an_empty_list() {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        let token = CancellationToken::new();
        let result = handle(
            &mut state,
            &params("file:///Absent.java", vec![(0, 0)]),
            &token,
        );
        assert!(
            result.is_ok(),
            "a nonexistent document should return Ok(empty) not an error"
        );
        assert_eq!(
            result.unwrap(),
            Vec::new(),
            "a nonexistent document should return empty list"
        );
    }

    #[test]
    fn every_requested_position_gets_exactly_one_range_in_order() {
        let source = "class Foo {\n  int x;\n  int y;\n}";
        let mut state = opened(source);
        let token = CancellationToken::new();
        // Request ranges for both x and y.
        let result = handle(
            &mut state,
            &params("file:///A.java", vec![(1, 7), (2, 7)]),
            &token,
        );
        let ranges = result.expect("ranges computed");
        assert_eq!(
            ranges.len(),
            2,
            "should have exactly 2 ranges for 2 positions"
        );

        // Verify they are different (covering different variables).
        assert_ne!(
            ranges[0].range, ranges[1].range,
            "ranges for different positions should differ"
        );
    }

    #[test]
    fn a_requested_position_after_a_non_ascii_character_is_read_as_utf16_not_as_bytes() {
        // This is the one reading aid whose *input* positions come off the
        // wire, so the character-to-offset direction is exercised nowhere
        // else. `café` is 5 UTF-8 bytes and 4 UTF-16 units, so every column
        // after it on this line sits one unit below its byte offset; reading
        // the incoming character as a byte offset lands the cursor on the
        // space before `x`, whose nearest named ancestor is the whole field
        // declaration rather than the one-character identifier.
        let source = "class A { int café = 1; int x = 2; }";
        let mut state = opened(source);
        let token = CancellationToken::new();

        let x_byte = source.find("x =").expect("the second field's own name") as u32;
        let x_character = x_byte - 1;

        let ranges = handle(
            &mut state,
            &params("file:///A.java", vec![(0, x_character)]),
            &token,
        )
        .expect("not cancelled");

        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].range.start.character, x_character);
        assert_eq!(
            ranges[0].range.end.character,
            x_character + 1,
            "the innermost step is the identifier `x` itself: {:?}",
            ranges[0]
        );
    }

    #[test]
    fn a_cancelled_token_returns_request_cancelled() {
        let mut state = opened("class Foo {}");
        let token = CancellationToken::new();
        token.cancel();
        let result = handle(&mut state, &params("file:///A.java", vec![(0, 0)]), &token);
        assert_eq!(result.err(), Some(Cancelled));
    }
}
