//! A feature that is implemented and not advertised is a feature no editor can
//! reach. That shipped once — Phase 2a's member completion, behind a handshake
//! that named no trigger characters — and every in-process test passed against
//! it, because they call the handler directly.

mod harness;

use harness::editor::{Editor, demo_project};
use harness::no_classpath;

/// Every method Javelin might plausibly route, and the capability key a client
/// reads to decide whether to send it.
///
/// Extending this when a phase implements a new request is the point: forgetting
/// to is what this test catches — for the methods listed here.
//
// ponytail: this list is maintained by hand, not derived from `route()`'s match
// arms, so a wholly new method that nobody added to `SURFACE` is invisible to
// this test in both directions — routed-but-unadvertised or
// advertised-but-unrouted, either one escapes unnoticed. The upgrade is to
// derive the method set from the router itself (e.g. have `route()` expose its
// arms, or generate `SURFACE` from the same list `capabilities.rs` builds)
// rather than restating it here.
const SURFACE: &[(&str, &str)] = &[
    ("textDocument/documentSymbol", "documentSymbolProvider"),
    ("textDocument/definition", "definitionProvider"),
    ("textDocument/hover", "hoverProvider"),
    ("textDocument/completion", "completionProvider"),
    ("textDocument/references", "referencesProvider"),
    ("textDocument/rename", "renameProvider"),
    (
        "textDocument/prepareRename",
        "renameProvider/prepareProvider",
    ),
    ("workspace/symbol", "workspaceSymbolProvider"),
    (
        "textDocument/documentHighlight",
        "documentHighlightProvider",
    ),
    ("textDocument/codeAction", "codeActionProvider"),
    ("textDocument/semanticTokens/full", "semanticTokensProvider"),
    (
        "textDocument/semanticTokens/range",
        "semanticTokensProvider/range",
    ),
    ("textDocument/formatting", "documentFormattingProvider"),
    ("textDocument/signatureHelp", "signatureHelpProvider"),
    ("textDocument/implementation", "implementationProvider"),
    ("textDocument/typeDefinition", "typeDefinitionProvider"),
    ("textDocument/foldingRange", "foldingRangeProvider"),
    ("textDocument/selectionRange", "selectionRangeProvider"),
    ("textDocument/prepareTypeHierarchy", "typeHierarchyProvider"),
    ("typeHierarchy/supertypes", "typeHierarchyProvider"),
    ("typeHierarchy/subtypes", "typeHierarchyProvider"),
    ("textDocument/prepareCallHierarchy", "callHierarchyProvider"),
    ("callHierarchy/incomingCalls", "callHierarchyProvider"),
    ("callHierarchy/outgoingCalls", "callHierarchyProvider"),
    ("textDocument/inlayHint", "inlayHintProvider"),
];

const METHOD_NOT_FOUND: i64 = -32601;

#[test]
fn every_method_the_router_answers_is_advertised_and_the_reverse() {
    let capabilities = agree(&no_classpath());
    assert!(capabilities["documentFormattingProvider"].is_null());
}

/// Formatting is the one method answered only as configured, so it must be
/// advertised only as configured too.
#[test]
fn formatting_is_reachable_exactly_when_a_formatter_is_named() {
    let mut options = no_classpath();
    options["format"] = serde_json::json!({ "command": ["cat"] });
    let capabilities = agree(&options);
    assert_eq!(
        capabilities["documentFormattingProvider"],
        serde_json::json!(true)
    );
}

/// The answer comes from the formatter's thread, over the same pipe, as one
/// edit replacing the whole buffer — and the loop answers in the meantime.
#[test]
fn a_formatting_answer_is_the_formatters_output_over_the_whole_buffer() {
    let mut options = no_classpath();
    options["format"] = serde_json::json!({ "command": ["sh", "-c", "sleep 1; tr a-z A-Z"] });
    let mut editor = Editor::launch(&options);
    let path = demo_project().join("src/main/java/com/example/Greeter.java");
    let uri = editor.open(&path);
    let text = std::fs::read_to_string(&path).expect("fixture");
    let params = serde_json::json!({
        "textDocument": { "uri": uri },
        "options": { "tabSize": 4, "insertSpaces": true }
    });
    let answer = editor
        .server()
        .eventually("textDocument/formatting", &params, |answer| {
            answer["result"].is_array()
        });
    let edits = answer["result"].as_array().expect("edits");
    assert_eq!(edits.len(), 1, "{answer}");
    assert_eq!(
        edits[0]["range"]["start"],
        serde_json::json!({ "line": 0, "character": 0 })
    );
    assert_eq!(
        edits[0]["newText"],
        serde_json::json!(text.to_ascii_uppercase())
    );
}

/// Asserts the router and the handshake agree on every `SURFACE` method, and
/// returns what the handshake advertised.
fn agree(options: &serde_json::Value) -> serde_json::Value {
    let mut editor = Editor::launch(options);
    let uri = editor.open(&demo_project().join("src/main/java/com/example/Greeter.java"));
    let (line, character) = editor.find(&uri, "public String greet");

    for (method, capability) in SURFACE {
        // Well-formed params, so a rejection is about the method and not about
        // the shape of the request.
        let params = match *method {
            "workspace/symbol" => serde_json::json!({ "query": "" }),
            "textDocument/documentSymbol"
            | "textDocument/semanticTokens/full"
            | "textDocument/foldingRange" => {
                serde_json::json!({ "textDocument": { "uri": uri } })
            }
            "textDocument/selectionRange" => serde_json::json!({
                "textDocument": { "uri": uri },
                "positions": [{ "line": line, "character": character }]
            }),
            "textDocument/inlayHint" | "textDocument/semanticTokens/range" => serde_json::json!({
                "textDocument": { "uri": uri },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": line, "character": character }
                }
            }),
            "textDocument/formatting" => serde_json::json!({
                "textDocument": { "uri": uri },
                "options": { "tabSize": 4, "insertSpaces": true }
            }),
            "textDocument/rename" => serde_json::json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
                "newName": "renamed"
            }),
            "textDocument/references" => serde_json::json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
                "context": { "includeDeclaration": true }
            }),
            "typeHierarchy/supertypes" | "typeHierarchy/subtypes" => serde_json::json!({
                "item": {
                    "name": "Greeter", "kind": 5, "uri": uri,
                    "range": { "start": { "line": line, "character": 0 },
                               "end": { "line": line, "character": 1 } },
                    "selectionRange": { "start": { "line": line, "character": 0 },
                                        "end": { "line": line, "character": 1 } }
                }
            }),
            "callHierarchy/incomingCalls" | "callHierarchy/outgoingCalls" => serde_json::json!({
                "item": {
                    "name": "greet", "kind": 6, "uri": uri,
                    "range": { "start": { "line": line, "character": 0 },
                               "end": { "line": line, "character": 1 } },
                    "selectionRange": { "start": { "line": line, "character": 0 },
                                        "end": { "line": line, "character": 1 } }
                }
            }),
            "textDocument/codeAction" => serde_json::json!({
                "textDocument": { "uri": uri },
                "range": {
                    "start": { "line": line, "character": character },
                    "end": { "line": line, "character": character }
                },
                "context": { "diagnostics": [] }
            }),
            _ => serde_json::json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }),
        };

        let answer = editor.server().eventually(method, &params, |_| true);
        // A proxy for "the router has a handler for this method", not for "the
        // handler succeeded": InvalidParams, InternalError and RequestCancelled
        // all read as routed too, which only matters the day a `SURFACE` entry's
        // well-formed params hit a rejection unrelated to routing.
        let routed = answer["error"]["code"].as_i64() != Some(METHOD_NOT_FOUND);
        // A path below a capability — `semanticTokensProvider/range` — is
        // advertised when it is there and not `false`.
        let advertised = editor
            .capabilities()
            .pointer(&format!("/{capability}"))
            .is_some_and(|value| !value.is_null() && *value != serde_json::json!(false));

        assert_eq!(
            routed,
            advertised,
            "{method} is {} but {capability} is {}; a handler without its capability is \
             unreachable from an editor, and a capability without a handler is a lie. \
             Answer was: {answer}",
            if routed { "routed" } else { "not routed" },
            if advertised { "advertised" } else { "absent" },
        );
    }
    editor.capabilities().clone()
}

#[test]
fn the_negotiated_sync_kind_is_the_one_the_editor_can_speak() {
    let editor = Editor::launch(&no_classpath());
    assert_eq!(
        editor.capabilities()["textDocumentSync"]["change"],
        serde_json::json!(2),
        "the harness only sends incremental changes; a server that wants full \
         documents would be sent changes it did not ask for"
    );
    assert_eq!(
        editor.capabilities()["positionEncoding"],
        serde_json::json!("utf-16"),
        "every position in this suite is counted in UTF-16 code units"
    );
}
