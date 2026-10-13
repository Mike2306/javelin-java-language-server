//! `textDocument/documentHighlight` — every occurrence, in this file, of the
//! binding under the cursor.
//!
//! Computed from the scope tree alone, available from [`Readiness::Syntax`]
//! like folding and selection ranges: no classpath and no workspace index is
//! consulted. Ruling 4 settles the kind on the wire — every highlight is
//! [`DocumentHighlightKind::TEXT`], because telling a read from a write needs
//! assignment analysis nothing in the engine does. Ruling 6 settles the scope
//! — only what [`javelin_parser::ScopeTree`] binds is ever offered; see the
//! `ponytail:` on [`javelin_parser::occurrences_at`] for what that leaves out.
//!
//! [`Readiness::Syntax`]: javelin_protocol::Readiness::Syntax

use javelin_core::{CancellationToken, Cancelled};
use javelin_document::DocumentId;
use javelin_parser::occurrences_at;
use lsp_types::{DocumentHighlight, DocumentHighlightKind, DocumentHighlightParams};

use crate::ServerState;
use crate::handlers::{to_lsp_range, to_position};

/// Every occurrence of the binding at the requested position.
///
/// A document that is not open, or whose latest version never parsed, offers
/// no highlights. Both are ordinary states — the editor asks while the file
/// is still being opened — and an error would only make the client show a
/// failure for something it will ask about again in a moment.
pub fn handle(
    state: &mut ServerState,
    params: &DocumentHighlightParams,
    token: &CancellationToken,
) -> Result<Vec<DocumentHighlight>, Cancelled> {
    token.check()?;
    let at = &params.text_document_position_params;
    let id = DocumentId::new(at.text_document.uri.as_str());
    let Some(view) = state.view(&id) else {
        return Ok(Vec::new());
    };

    let offset = view.document.offset_at(to_position(at.position));
    // The scan inside `occurrences_at` polls `token` itself — see its own
    // doc — so a cancellation lands there rather than waiting for this `?`.
    let occurrences = occurrences_at(&view.document, view.tree, view.scopes, offset, token)?;

    let mut highlights = Vec::with_capacity(occurrences.len());
    for occurrence in occurrences {
        token.check()?;
        highlights.push(DocumentHighlight {
            range: to_lsp_range(occurrence.range),
            kind: Some(DocumentHighlightKind::TEXT),
        });
    }
    Ok(highlights)
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

    fn params(uri: &str, line: u32, character: u32) -> DocumentHighlightParams {
        serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
        }))
        .expect("well-formed params")
    }

    #[test]
    fn a_document_that_is_not_open_answers_an_empty_list() {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        let token = CancellationToken::new();
        let result = handle(&mut state, &params("file:///Absent.java", 0, 0), &token);
        assert!(
            result.is_ok(),
            "a nonexistent document should return Ok(empty) not an error"
        );
        assert_eq!(
            result.unwrap(),
            Vec::new(),
            "a nonexistent document should highlight as empty"
        );
    }

    #[test]
    fn a_cancelled_token_returns_request_cancelled() {
        let mut state = opened("class A { void go() { int x = 1; int y = x; } }");
        let token = CancellationToken::new();
        token.cancel();
        let result = handle(&mut state, &params("file:///A.java", 0, 27), &token);
        assert_eq!(result.err(), Some(Cancelled));
    }

    /// The same shape as `queries::a_multibyte_identifier_reports_utf16_columns`
    /// pins for the engine: a non-ASCII character before the identifier shifts
    /// its UTF-16 column away from its byte offset, and the range on the wire
    /// must reflect that rather than a raw byte count.
    #[test]
    fn ranges_on_the_wire_are_utf16_not_byte_offsets() {
        // "café" is 4 bytes of ASCII plus a 2-byte 'é', so its own occurrence
        // starts 1 UTF-16 unit later than its byte offset would suggest, and
        // `café`'s use sits later still.
        let mut state = opened("class A { int café = 1; int b = café; }");
        let token = CancellationToken::new();

        // Byte offset of the declaration's own `café`, landing the cursor on it.
        let byte_offset = "class A { int ".len() as u32;
        let position = state
            .view(&DocumentId::new("file:///A.java"))
            .expect("the file parsed")
            .document
            .position_at(byte_offset);

        let result = handle(
            &mut state,
            &params("file:///A.java", position.line, position.character),
            &token,
        )
        .expect("not cancelled");

        assert_eq!(result.len(), 2, "the declaration and its one use");
        // Byte offset 14 and UTF-16 column 14 agree here, since nothing
        // non-ASCII precedes the declaration.
        assert_eq!(result[0].range.start.character, 14);
        // The second `café` is preceded by the first one, which is one UTF-16
        // unit narrower than its 5-byte UTF-8 form — the field this test would
        // fail if the handler ever started reporting byte columns instead.
        assert_eq!(result[1].range.start.character, 32);
        for highlight in &result {
            assert_eq!(highlight.kind, Some(DocumentHighlightKind::TEXT));
        }
    }
}
