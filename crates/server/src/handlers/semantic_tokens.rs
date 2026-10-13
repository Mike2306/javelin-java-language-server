//! `textDocument/semanticTokens/full` — every classified token in the file,
//! encoded as LSP's relative five-integer stream.
//!
//! Available from [`Readiness::Syntax`] like folding, selection and highlight
//! ranges: `javelin_parser::semantic_tokens` reads the syntax tree and the
//! scope tree alone, no classpath and no workspace index. `full` only —
//! neither `delta` nor `range` is implemented (Ruling 2) — so `result_id` is
//! always `None`: there is nothing to key a delta request to.
//!
//! `benchmarks/benches/semantic_tokens.rs` measures the walk beneath this
//! handler at p95 30.5 ms on `Tier::Large` and 357 ms on `Tier::Huge`, against
//! the 50 ms interactive budget. Both are linear in the file now: until
//! `ScopeTree` indexed its bindings by name, `find` scanned a scope's bindings
//! one at a time, and the fixture's one flat class-body scope of 20 000 fields
//! made the walk quadratic — 78.29 ms and 5.47 s. `semanticTokens/full/delta`
//! would not have moved either number, because LSP's delta variant computes
//! the full current token list before diffing it against the previous
//! `resultId`. What is left over budget is a walk proportional to a file of
//! several megabytes, and `MAX_DOCUMENT_BYTES` is what keeps that cost off the
//! request loop; its own `ponytail:` says what a file above it loses.
//!
//! [`LEGEND`] is the one place a [`SemanticTokenKind`] becomes the integer a
//! client reads back out of `token_type`. [`legend`] builds the capability
//! from the very same array, and `type_index` (private — it is this module's
//! own lookup, not part of the crate's public API) looks a token's wire index
//! up in it rather than casting a discriminant — so the index this handler
//! emits and the legend `capabilities.rs` advertises are the same lookup and
//! cannot drift apart the way two hand-written copies could.
//!
//! [`Readiness::Syntax`]: javelin_protocol::Readiness::Syntax

use javelin_core::{CancellationToken, Cancelled};
use javelin_document::DocumentId;
use javelin_parser::{SemanticToken as ClassifiedToken, SemanticTokenKind, semantic_tokens_in};
use lsp_types::{
    SemanticToken, SemanticTokenType, SemanticTokens, SemanticTokensParams,
    SemanticTokensRangeParams, SemanticTokensRangeResult, SemanticTokensResult,
};

use crate::ServerState;

/// The wire legend, in the order `type_index` reads it back at. Declared
/// once, beside the handler that indexes into it — see the module doc.
pub const LEGEND: &[(SemanticTokenKind, SemanticTokenType)] = &[
    (SemanticTokenKind::Namespace, SemanticTokenType::NAMESPACE),
    (SemanticTokenKind::Class, SemanticTokenType::CLASS),
    (SemanticTokenKind::Interface, SemanticTokenType::INTERFACE),
    (SemanticTokenKind::Enum, SemanticTokenType::ENUM),
    (
        SemanticTokenKind::TypeParameter,
        SemanticTokenType::TYPE_PARAMETER,
    ),
    (SemanticTokenKind::Parameter, SemanticTokenType::PARAMETER),
    (SemanticTokenKind::Variable, SemanticTokenType::VARIABLE),
    (SemanticTokenKind::Property, SemanticTokenType::PROPERTY),
    (SemanticTokenKind::Keyword, SemanticTokenType::KEYWORD),
    (SemanticTokenKind::Comment, SemanticTokenType::COMMENT),
    (SemanticTokenKind::String, SemanticTokenType::STRING),
    (SemanticTokenKind::Number, SemanticTokenType::NUMBER),
];

/// The legend `capabilities.rs` advertises, built from [`LEGEND`] and nothing
/// else. No modifiers in this slice: `static`, `readonly` and `declaration`
/// are all derivable later and none changes what a reviewer can read.
pub fn legend() -> lsp_types::SemanticTokensLegend {
    lsp_types::SemanticTokensLegend {
        token_types: LEGEND.iter().map(|(_, ty)| ty.clone()).collect(),
        token_modifiers: Vec::new(),
    }
}

/// Where a [`SemanticTokenKind`] sits in [`LEGEND`] — the index this handler
/// writes into `token_type` on the wire.
///
/// Exhaustive on purpose rather than a search of [`LEGEND`]: a search has to
/// answer *something* when a new variant arrives without a legend row, and the
/// only honest something is a panic — on the request path, for a request an
/// editor re-sends after every edit. A `match` makes the compiler refuse the
/// new variant instead, and the test below keeps the two in step by asserting
/// `LEGEND[type_index(kind)].0 == kind` for every row.
fn type_index(kind: SemanticTokenKind) -> u32 {
    match kind {
        SemanticTokenKind::Namespace => 0,
        SemanticTokenKind::Class => 1,
        SemanticTokenKind::Interface => 2,
        SemanticTokenKind::Enum => 3,
        SemanticTokenKind::TypeParameter => 4,
        SemanticTokenKind::Parameter => 5,
        SemanticTokenKind::Variable => 6,
        SemanticTokenKind::Property => 7,
        SemanticTokenKind::Keyword => 8,
        SemanticTokenKind::Comment => 9,
        SemanticTokenKind::String => 10,
        SemanticTokenKind::Number => 11,
    }
}

/// The largest document this handler will walk, in bytes of source.
///
/// Read off the benchmark's own tiers rather than picked: `Tier::Large` is
/// 539 601 bytes and walks in 30.5 ms at p95, `Tier::Huge` is ten times that
/// and walks in 357 ms. The walk is linear, so interpolating from the first
/// puts 640 KiB near 37 ms — inside the 50 ms budget with room for a slower
/// machine than the one that measured it.
///
/// Public so the benchmark can assert which side of it each fixture tier
/// falls on, rather than a generator quietly moving one across it.
pub const MAX_DOCUMENT_BYTES: usize = 640 * 1024;

/// Every token in the file, LSP-encoded.
///
/// A document that is not open, or whose latest version never parsed, offers
/// an empty token list rather than an error — the editor asks while the file
/// is still being opened, and that is an ordinary state, not a failure. So
/// does one past [`MAX_DOCUMENT_BYTES`]; see the guard below.
pub fn handle(
    state: &mut ServerState,
    params: &SemanticTokensParams,
    token: &CancellationToken,
) -> Result<Option<SemanticTokensResult>, Cancelled> {
    let data = walked(state, params.text_document.uri.as_str(), None, token)?;
    Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
        result_id: None,
        data,
    })))
}

/// The tokens of the nodes meeting `params.range`, LSP-encoded relative to
/// the document's start as `full`'s are: one walk, bounded by the range, so
/// a screen of a file past [`MAX_DOCUMENT_BYTES`] is coloured where `full`
/// answers nothing.
pub fn handle_range(
    state: &mut ServerState,
    params: &SemanticTokensRangeParams,
    token: &CancellationToken,
) -> Result<Option<SemanticTokensRangeResult>, Cancelled> {
    let range = (
        crate::handlers::to_position(params.range.start),
        crate::handlers::to_position(params.range.end),
    );
    let data = walked(state, params.text_document.uri.as_str(), Some(range), token)?;
    Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
        result_id: None,
        data,
    })))
}

/// The encoded tokens of the open document at `uri` — of the whole of it,
/// or of the nodes meeting `range` — or none past the guard.
fn walked(
    state: &mut ServerState,
    uri: &str,
    range: Option<(javelin_document::Position, javelin_document::Position)>,
    token: &CancellationToken,
) -> Result<Vec<SemanticToken>, Cancelled> {
    token.check()?;
    let id = DocumentId::new(uri);
    let Some(view) = state.view(&id) else {
        return Ok(Vec::new());
    };
    let within = match range {
        // A line past the last clamps to the last line, not to its end, so
        // an end past the document is the document's end.
        Some((start, end)) if end.line >= view.document.line_index().line_count() => {
            view.document.offset_at(start)..view.document.text().len() as u32
        }
        Some((start, end)) => view.document.offset_at(start)..view.document.offset_at(end),
        None => 0..view.document.text().len() as u32,
    };

    // ponytail: a file past the threshold gets no semantic colouring at all,
    // and the client falls back to its own TextMate grammar — a plainer
    // rendering rather than a wrong one, which is the fallback Ruling 5 already
    // blesses for anything this engine cannot justify. The guard exists because
    // this walk is the one request whose cost is bounded by the file rather
    // than by the answer, and the loop it runs on is single-threaded: the
    // per-node `token.check()` inside the walk is correct discipline for a
    // future that dispatches requests off the loop, and cannot fire today,
    // because `$/cancelRequest` arrives on the very receiver the loop is
    // blocked on while this handler runs (see `lib.rs`'s own note on that). So
    // 357 ms at `Tier::Huge` would be the whole server stopped for that long,
    // again after every edit. The upgrade is running the walk off the loop —
    // on the parse pool, beside the view it reads. A client asking for
    // `range` is answered for what it shows, which the guard reads instead:
    // the walk is bounded by the range, bar a scan of its siblings.
    let length = within.len();
    if length > MAX_DOCUMENT_BYTES {
        tracing::debug!(
            uri = id.as_str(),
            bytes = length,
            threshold = MAX_DOCUMENT_BYTES,
            "too much for semantic tokens; answering an empty list"
        );
        return Ok(Vec::new());
    }

    let classified = semantic_tokens_in(
        &view.document,
        view.tree,
        view.scopes,
        view.syntax,
        within,
        token,
    )?;
    encode(classified, token)
}

/// LSP's relative encoding: `delta_line` relative to the previous token's
/// line, `delta_start` relative to the previous token's *start* character —
/// only when the two share a line, since a character offset means nothing
/// once the line has changed, in which case `delta_start` is absolute.
/// `classified` is already sorted by position (the parser module's own
/// invariant — see its doc), which is what makes a single running
/// `(prev_line, prev_char)` enough.
fn encode(
    classified: Vec<ClassifiedToken>,
    token: &CancellationToken,
) -> Result<Vec<SemanticToken>, Cancelled> {
    let mut data = Vec::with_capacity(classified.len());
    let mut previous_line = 0u32;
    let mut previous_start = 0u32;
    for classified_token in classified {
        token.check()?;
        let delta_line = classified_token.line - previous_line;
        let delta_start = if delta_line == 0 {
            classified_token.start_character - previous_start
        } else {
            classified_token.start_character
        };
        data.push(SemanticToken {
            delta_line,
            delta_start,
            length: classified_token.length,
            token_type: type_index(classified_token.kind),
            token_modifiers_bitset: 0,
        });
        previous_line = classified_token.line;
        previous_start = classified_token.start_character;
    }
    Ok(data)
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

    fn params(uri: &str) -> SemanticTokensParams {
        serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": uri },
        }))
        .expect("well-formed params")
    }

    fn data(state: &mut ServerState, uri: &str) -> Vec<SemanticToken> {
        let token = CancellationToken::new();
        match handle(state, &params(uri), &token)
            .expect("not cancelled")
            .expect("always Some — see the module doc")
        {
            SemanticTokensResult::Tokens(tokens) => tokens.data,
            SemanticTokensResult::Partial(_) => panic!("this handler never returns a partial"),
        }
    }

    #[test]
    fn the_first_token_on_a_line_encodes_its_absolute_character() {
        // Indented, so the first token is not already at column 0 — the one
        // shape that would let a delta-from-zero bug hide behind a correct
        // absolute-looking number.
        let mut state = opened("  class Foo {}\n");
        let data = data(&mut state, "file:///A.java");
        let class_keyword = data[0];
        assert_eq!(class_keyword.delta_line, 0);
        assert_eq!(class_keyword.delta_start, 2, "two leading spaces");
        assert_eq!(class_keyword.length, "class".len() as u32);
        assert_eq!(
            class_keyword.token_type,
            type_index(SemanticTokenKind::Keyword)
        );
    }

    #[test]
    fn a_second_token_on_the_same_line_encodes_a_delta_from_the_first() {
        let mut state = opened("  class Foo {}\n");
        let data = data(&mut state, "file:///A.java");
        assert_eq!(data.len(), 2, "the keyword and the type name: {data:?}");
        let type_name = data[1];
        // "Foo" sits at absolute column 8; a correct delta from the previous
        // token's own start (column 2) is 6. An implementation that emitted
        // the absolute column here instead would report 8, not 6.
        assert_eq!(type_name.delta_line, 0);
        assert_eq!(type_name.delta_start, 6);
        assert_eq!(type_name.length, "Foo".len() as u32);
        assert_eq!(type_name.token_type, type_index(SemanticTokenKind::Class));
    }

    #[test]
    fn a_token_after_a_non_ascii_character_is_encoded_in_utf16_units() {
        // "café" is 4 bytes of ASCII plus a 2-byte 'é': 5 UTF-8 bytes but 4
        // UTF-16 units. A byte-counting implementation would place `int` one
        // column later than it actually reads.
        let mut state = opened("class café { int x = 1; }\n");
        let data = data(&mut state, "file:///A.java");
        // class(0), café(1), int(2) — the order the tokens appear in source.
        assert_eq!(data.len(), 5, "class, café, int, x, 1: {data:?}");
        let int_keyword = data[2];
        assert_eq!(
            int_keyword.token_type,
            type_index(SemanticTokenKind::Keyword)
        );
        // "café" starts at UTF-16 column 6; "int" starts at column 13 (after
        // "café" (4) + " { " (3)). The delta from "café"'s own start is 7.
        assert_eq!(int_keyword.delta_line, 0);
        assert_eq!(int_keyword.delta_start, 7);
    }

    #[test]
    fn the_legend_the_capability_advertises_is_the_one_the_handler_indexes_into() {
        let advertised = match crate::capabilities::server_capabilities().semantic_tokens_provider {
            Some(lsp_types::SemanticTokensServerCapabilities::SemanticTokensOptions(options)) => {
                options.legend
            }
            other => panic!("semantic tokens capability missing or an unexpected shape: {other:?}"),
        };
        let expected_types: Vec<SemanticTokenType> =
            LEGEND.iter().map(|(_, ty)| ty.clone()).collect();
        assert_eq!(
            advertised.token_types, expected_types,
            "the capability must be built from LEGEND, not a second copy of it"
        );
        for (index, (kind, _)) in LEGEND.iter().enumerate() {
            assert_eq!(
                type_index(*kind),
                index as u32,
                "the index the handler emits for {kind:?} must be its own position in LEGEND"
            );
        }
    }

    #[test]
    fn a_document_that_is_not_open_answers_an_empty_token_list() {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        let token = CancellationToken::new();
        let result = handle(&mut state, &params("file:///Absent.java"), &token)
            .expect("a nonexistent document should return Ok, not an error");
        match result.expect("always Some") {
            SemanticTokensResult::Tokens(tokens) => assert_eq!(tokens.data, Vec::new()),
            other => panic!("expected an empty token list, got {other:?}"),
        }
    }

    /// Valid Java: one field and one method per repetition, the shape
    /// `benchmarks/src/lib.rs` generates and the shape that made this walk
    /// quadratic before `ScopeTree` indexed its bindings. The byte length is what the guard reads, so each caller
    /// asserts which side of it its own fixture lands on.
    fn generated(repetitions: usize) -> String {
        let mut source = String::from("class Big {\n");
        for i in 0..repetitions {
            source.push_str(&format!(
                "    private int field{i} = {i};\n    int method{i}() {{ return field{i}; }}\n"
            ));
        }
        source.push_str("}\n");
        source
    }

    #[test]
    fn a_file_past_the_size_guard_answers_an_empty_list_instead_of_freezing_the_loop() {
        // Past the guard by comment padding rather than by members: the guard
        // reads the byte length alone, and twenty thousand members would make
        // a debug build spend seconds building a view this test never walks.
        let mut source = generated(200);
        while source.len() <= MAX_DOCUMENT_BYTES {
            source.push_str("// padding, to put the file past the guard by its bytes alone\n");
        }
        assert!(
            source.len() > MAX_DOCUMENT_BYTES,
            "the fixture must actually exceed the guard: {} bytes",
            source.len()
        );
        let mut state = opened(&source);
        assert_eq!(
            data(&mut state, "file:///A.java"),
            Vec::new(),
            "a file past the guard offers no tokens at all, not a truncated list"
        );
    }

    fn ranged(state: &mut ServerState, lines: std::ops::Range<u32>) -> Vec<SemanticToken> {
        let params: SemanticTokensRangeParams = serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": "file:///A.java" },
            "range": {
                "start": { "line": lines.start, "character": 0 },
                "end": { "line": lines.end, "character": 0 }
            }
        }))
        .expect("well-formed params");
        match handle_range(state, &params, &CancellationToken::new())
            .expect("not cancelled")
            .expect("always Some")
        {
            SemanticTokensRangeResult::Tokens(tokens) => tokens.data,
            SemanticTokensRangeResult::Partial(_) => panic!("never a partial"),
        }
    }

    /// The guard is on what is walked: a screen of a file past it is
    /// coloured, with lines relative to the document's start as `full`'s are,
    /// and a range past it is not.
    #[test]
    fn a_range_of_a_file_past_the_guard_is_coloured_and_a_range_past_it_is_not() {
        let mut source = generated(200);
        while source.len() <= MAX_DOCUMENT_BYTES {
            source.push_str("// padding, to put the file past the guard by its bytes alone\n");
        }
        let mut state = opened(&source);
        let screen = ranged(&mut state, 2..4);
        // `int method0() { return field0; }` on line 2, `field1`'s line on 3.
        assert_eq!(screen[0].delta_line, 2, "{screen:?}");
        assert!(
            screen.iter().all(|t| t.delta_line <= 2) && screen.len() > 6,
            "{screen:?}"
        );
        let mut small = opened(&generated(200));
        assert_eq!(
            ranged(&mut small, 2..4),
            screen,
            "the same tokens as a small file's"
        );
        let lines = source.lines().count() as u32;
        assert_eq!(ranged(&mut state, 0..lines), Vec::new());
    }

    /// A client asks for the lines it shows, and the last one shown may be
    /// past the end of a file with no final newline.
    #[test]
    fn a_range_ending_past_the_last_line_reaches_the_documents_end() {
        let mut state = opened("class A {}\n// tail");
        let tail = ranged(&mut state, 1..2);
        assert_eq!(tail.len(), 1, "the comment on the last line: {tail:?}");
        assert_eq!(tail[0].token_type, type_index(SemanticTokenKind::Comment));
    }

    #[test]
    fn a_file_below_the_size_guard_is_still_coloured() {
        // Same shape, an order of magnitude smaller — so the emptiness above
        // is the guard and not the fixture.
        let source = generated(2_000);
        assert!(
            source.len() < MAX_DOCUMENT_BYTES,
            "the fixture must sit below the guard: {} bytes",
            source.len()
        );
        let mut state = opened(&source);
        assert!(
            !data(&mut state, "file:///A.java").is_empty(),
            "a file below the guard is walked as usual"
        );
    }

    #[test]
    fn a_cancelled_token_returns_request_cancelled() {
        let mut state = opened("class A { void go() { int x = 1; } }");
        let token = CancellationToken::new();
        token.cancel();
        let result = handle(&mut state, &params("file:///A.java"), &token);
        assert_eq!(result.err(), Some(Cancelled));
    }
}
