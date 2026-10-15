//! `textDocument/foldingRange` — code regions that can be collapsed.
//!
//! Folding ranges are computed from the syntax tree alone and are available from
//! [`Readiness::Syntax`], which the server reaches on `didOpen`. There is no
//! semantic work or indexing involved.
//!
//! [`Readiness::Syntax`]: javelin_protocol::Readiness::Syntax

use javelin_core::{CancellationToken, Cancelled};
use javelin_document::DocumentId;
use javelin_parser::{FoldingRangeKind, folding_ranges as compute_folding_ranges};
use lsp_types::{FoldingRange, FoldingRangeKind as LspFoldingRangeKind, FoldingRangeParams};

use crate::ServerState;

/// All foldable regions in one document.
///
/// A document that is not open, or whose latest version never parsed, offers
/// no folds. Both are ordinary states — the editor asks for folding ranges
/// while the file is still being opened — and an error would only make the
/// client show a failure for something it will ask about again in a moment.
pub fn handle(
    state: &mut ServerState,
    params: &FoldingRangeParams,
    token: &CancellationToken,
) -> Result<Vec<FoldingRange>, Cancelled> {
    token.check()?;
    let id = DocumentId::new(params.text_document.uri.as_str());
    // The tree alone, so a file never asked for its view is not built one.
    let Some(tree) = state.tree(&id) else {
        return Ok(Vec::new());
    };
    let parser_ranges = compute_folding_ranges(tree.root(), token)?;

    // One poll per emitted range, the way `document_highlight` polls per
    // emitted highlight: the range count grows with the file, so this loop is
    // file-sized work and not answer-sized.
    let mut lsp_ranges = Vec::with_capacity(parser_ranges.len());
    for r in parser_ranges {
        token.check()?;
        lsp_ranges.push(FoldingRange {
            start_line: r.start_line as u32,
            end_line: r.end_line as u32,
            start_character: None,
            end_character: None,
            kind: r.kind.map(|k| match k {
                FoldingRangeKind::Comment => LspFoldingRangeKind::Comment,
                FoldingRangeKind::Imports => LspFoldingRangeKind::Imports,
            }),
            collapsed_text: None,
        });
    }

    Ok(lsp_ranges)
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

    fn params(uri: &str) -> FoldingRangeParams {
        serde_json::from_value(serde_json::json!({ "textDocument": { "uri": uri } }))
            .expect("well-formed params")
    }

    /// A document whose latest version never parsed has no separate check:
    /// `ServerState::view` answers `None` for it on the same line it answers
    /// `None` for a document nobody opened, and in-process there is no way to
    /// reach the second state — `did_open` parses inline and tree-sitter
    /// recovers from anything. A second test over the same `None` would name a
    /// behaviour it could not tell from this one.
    #[test]
    fn a_document_that_is_not_open_folds_as_empty_rather_than_erroring() {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        let token = CancellationToken::new();
        let result = handle(&mut state, &params("file:///Absent.java"), &token);
        assert!(
            result.is_ok(),
            "a nonexistent document should return Ok(empty) not an error"
        );
        assert_eq!(
            result.unwrap(),
            Vec::new(),
            "a nonexistent document should fold as empty"
        );
    }

    /// Folding reads the tree without the view, and so without the store's
    /// say on what is open: a closed file's tree has to be gone with it.
    #[test]
    fn a_closed_document_folds_as_empty() {
        let mut state = opened("class A {\n    void m() {\n        int x;\n    }\n}\n");
        let token = CancellationToken::new();
        assert!(
            !handle(&mut state, &params("file:///A.java"), &token)
                .unwrap()
                .is_empty()
        );
        state.did_close(&DocumentId::new("file:///A.java"));
        assert_eq!(
            handle(&mut state, &params("file:///A.java"), &token),
            Ok(Vec::new())
        );
    }

    #[test]
    fn a_blank_line_between_two_imports_does_not_cost_the_import_fold() {
        // The layout of nearly every real Java file, and the fold a reader
        // uses most. The parser crate pins the range; this pins that it
        // survives the projection onto `lsp_types`, kind included.
        let mut state = opened("import java.util.List;\n\nimport com.example.Thing;\n\nclass A {}");
        let token = CancellationToken::new();
        let ranges = handle(&mut state, &params("file:///A.java"), &token).expect("not cancelled");
        let imports = ranges
            .iter()
            .find(|r| r.kind == Some(LspFoldingRangeKind::Imports))
            .unwrap_or_else(|| panic!("no import fold: {ranges:?}"));
        assert_eq!((imports.start_line, imports.end_line), (0, 2));
    }

    #[test]
    fn a_cancelled_token_returns_request_cancelled() {
        let mut state = opened("class Foo { void m() {} }");
        let token = CancellationToken::new();
        token.cancel();
        let result = handle(&mut state, &params("file:///A.java"), &token);
        assert_eq!(result.err(), Some(Cancelled));
    }
}
