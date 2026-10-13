//! A real LSP conversation, over an in-memory connection.
//!
//! This is the test that would catch a broken handshake, a capability that does
//! not serialise, or a server that fails to exit — the failures that make an
//! editor sit at "starting language server" forever.

use javelin_integration_tests::{Client, fixture};
use lsp_types::notification::Notification as _;
use lsp_types::request::Request as _;

#[test]
fn the_handshake_advertises_incremental_sync_and_utf16_positions() {
    let (client, result) = Client::started();

    let capabilities = &result["capabilities"];
    assert_eq!(capabilities["positionEncoding"], "utf-16");
    // TextDocumentSyncKind::INCREMENTAL == 2
    assert_eq!(capabilities["textDocumentSync"]["change"], 2);
    assert_eq!(
        capabilities["textDocumentSync"]["save"]["includeText"],
        false
    );

    client.shutdown();
}

#[test]
fn the_handshake_advertises_exactly_the_features_that_are_implemented() {
    let (client, result) = Client::started();

    let capabilities = &result["capabilities"];
    assert_eq!(capabilities["documentSymbolProvider"], true);
    assert_eq!(capabilities["definitionProvider"], true);
    assert_eq!(capabilities["hoverProvider"], true);
    assert!(capabilities["completionProvider"].is_object());
    assert_eq!(capabilities["referencesProvider"], true);
    assert!(capabilities["renameProvider"].is_null());

    client.shutdown();
}

#[test]
fn document_notifications_are_accepted_without_a_reply() {
    let client = Client::ready();

    client.did_open("file:///Simple.java", "class Simple {}");
    client.notify(
        lsp_types::notification::DidChangeTextDocument::METHOD,
        serde_json::json!({
            "textDocument": { "uri": "file:///Simple.java", "version": 2 },
            "contentChanges": [{
                "range": {
                    "start": { "line": 0, "character": 6 },
                    "end": { "line": 0, "character": 12 }
                },
                "text": "Renamed"
            }]
        }),
    );
    client.notify(
        lsp_types::notification::DidCloseTextDocument::METHOD,
        serde_json::json!({ "textDocument": { "uri": "file:///Simple.java" } }),
    );

    client.shutdown();
}

#[test]
fn a_malformed_notification_does_not_kill_the_server() {
    let client = Client::ready();

    client.notify(
        lsp_types::notification::DidOpenTextDocument::METHOD,
        serde_json::json!({ "textDocument": "this is not an object" }),
    );

    // The server must still be answering afterwards.
    let reply = client.request(
        5,
        lsp_types::request::DocumentSymbolRequest::METHOD,
        serde_json::json!({ "textDocument": { "uri": "file:///Simple.java" } }),
    );
    assert!(reply.response_result.is_ok());

    client.shutdown();
}

#[test]
fn unimplemented_requests_are_answered_rather_than_dropped() {
    let client = Client::ready();

    let response = client.request(
        1,
        lsp_types::request::Rename::METHOD,
        serde_json::json!({
            "textDocument": { "uri": "file:///Simple.java" },
            "position": { "line": 0, "character": 0 },
            "newName": "Other"
        }),
    );
    match response.response_result {
        Err(error) => assert_eq!(error.code, lsp_server::ErrorCode::MethodNotFound as i32),
        Ok(value) => panic!("expected MethodNotFound, got {value}"),
    }

    client.shutdown();
}

#[test]
fn cancelling_an_unknown_request_is_ignored() {
    let client = Client::ready();
    client.notify(
        lsp_types::notification::Cancel::METHOD,
        serde_json::json!({ "id": 12345 }),
    );
    client.shutdown();
}

#[test]
fn the_outline_of_an_open_file_comes_back_nested() {
    let client = Client::ready();
    client.did_open("file:///Nested.java", &fixture("java/Nested.java"));

    let symbols = client.ask(
        2,
        lsp_types::request::DocumentSymbolRequest::METHOD,
        serde_json::json!({ "textDocument": { "uri": "file:///Nested.java" } }),
    );

    let outer = &symbols[0];
    assert_eq!(outer["name"], "Nested");
    // The hierarchical shape, not the flat `SymbolInformation` one: children
    // rather than a `location`, and the two ranges an outline navigates by.
    assert!(outer["location"].is_null());
    assert_eq!(outer["range"]["start"]["line"], 3);
    assert_eq!(outer["selectionRange"]["start"]["character"], 13);

    let members: Vec<&str> = outer["children"]
        .as_array()
        .expect("members")
        .iter()
        .map(|child| child["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(members, vec!["Static", "Inner", "go"]);

    // A local class and an anonymous class are both declarations inside `go`,
    // and both are worth navigating to; the locals beside them are not.
    let inside_go: Vec<&str> = outer["children"][2]["children"]
        .as_array()
        .expect("declarations inside go()")
        .iter()
        .map(|child| child["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(inside_go, vec!["Local", "Runnable"]);
    assert_eq!(
        outer["children"][2]["children"][1]["children"][0]["name"],
        "run"
    );

    client.shutdown();
}

#[test]
fn the_outline_of_a_file_the_editor_never_opened_is_empty() {
    let client = Client::ready();
    let symbols = client.ask(
        3,
        lsp_types::request::DocumentSymbolRequest::METHOD,
        serde_json::json!({ "textDocument": { "uri": "file:///Never.java" } }),
    );
    assert_eq!(symbols, serde_json::json!([]));
    client.shutdown();
}
