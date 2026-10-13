//! Go-to-definition and hover, asked the way an editor asks them.
//!
//! Over a real handshake and against the Java fixtures, so what is checked here
//! is the whole path: LSP position in, UTF-16 range out, with the engine's
//! answer surviving serialisation on the way.

use javelin_integration_tests::{Client, fixture};
use lsp_server::Message;
use lsp_types::notification::Notification as _;
use lsp_types::request::Request as _;
use serde_json::{Value, json};

const SHADOWING: &str = "file:///Shadowing.java";
const JAVADOC: &str = "file:///Javadoc.java";

fn at(uri: &str, line: u32, character: u32) -> Value {
    json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character },
    })
}

fn definition(client: &Client, id: i32, uri: &str, line: u32, character: u32) -> Value {
    client.ask(
        id,
        lsp_types::request::GotoDefinition::METHOD,
        at(uri, line, character),
    )
}

fn hover(client: &Client, id: i32, uri: &str, line: u32, character: u32) -> Value {
    client.ask(
        id,
        lsp_types::request::HoverRequest::METHOD,
        at(uri, line, character),
    )
}

/// `line:character` of where a `Location` points, for a one-line assertion.
fn points_at(location: &Value) -> String {
    let start = &location["range"]["start"];
    format!("{}:{}", start["line"], start["character"])
}

#[test]
fn a_use_of_a_shadowed_name_jumps_to_the_declaration_that_wins_there() {
    let client = Client::ready();
    client.did_open(SHADOWING, &fixture("java/Shadowing.java"));

    // `int fromParameter = value;` — the parameter on line 9.
    assert_eq!(
        points_at(&definition(&client, 1, SHADOWING, 10, 28)),
        "9:21"
    );
    // `int fromBlock = value;` — the block local on line 12 shadows it.
    assert_eq!(
        points_at(&definition(&client, 2, SHADOWING, 13, 28)),
        "12:16"
    );
    // `return value;` in another method — the field declared last, on line 28.
    assert_eq!(
        points_at(&definition(&client, 3, SHADOWING, 23, 15)),
        "28:16"
    );
    // The lambda parameter, visible only inside the lambda.
    assert_eq!(
        points_at(&definition(&client, 4, SHADOWING, 19, 25)),
        "19:15"
    );

    client.shutdown();
}

#[test]
fn a_definition_points_at_the_name_and_not_at_the_whole_declaration() {
    let client = Client::ready();
    client.did_open(SHADOWING, &fixture("java/Shadowing.java"));

    let location = definition(&client, 1, SHADOWING, 13, 28);
    assert_eq!(location["uri"], SHADOWING);
    assert_eq!(
        location["range"]["start"],
        json!({ "line": 12, "character": 16 })
    );
    assert_eq!(
        location["range"]["end"],
        json!({ "line": 12, "character": 21 })
    );

    client.shutdown();
}

#[test]
fn a_type_this_file_only_imports_has_no_definition() {
    let client = Client::ready();
    client.did_open(SHADOWING, &fixture("java/Shadowing.java"));

    // `IntUnaryOperator inLambda()` — the return type comes from an import, and
    // nothing in this file declares it.
    assert_eq!(definition(&client, 1, SHADOWING, 18, 8), Value::Null);
    assert_eq!(hover(&client, 2, SHADOWING, 18, 8), Value::Null);

    client.shutdown();
}

#[test]
fn hover_on_a_documented_declaration_shows_its_signature_and_its_doc() {
    let client = Client::ready();
    client.did_open(JAVADOC, &fixture("java/Javadoc.java"));

    let documented = hover(&client, 1, JAVADOC, 18, 9);
    assert_eq!(documented["contents"]["kind"], "markdown");
    assert_eq!(
        documented["contents"]["value"],
        "```java\nvoid add(int by)\n```\n\nA documented method.\n\n**Parameters**\n- `by` — how much"
    );
    // The range is the identifier, which is what the editor highlights.
    assert_eq!(
        documented["range"],
        json!({
            "start": { "line": 18, "character": 9 },
            "end": { "line": 18, "character": 12 }
        })
    );

    let undocumented = hover(&client, 2, JAVADOC, 11, 16);
    assert_eq!(
        undocumented["contents"]["value"],
        "```java\nprivate int ignored;\n```"
    );

    client.shutdown();
}

#[test]
fn hover_on_a_type_stops_at_the_brace_rather_than_printing_the_file() {
    let client = Client::ready();
    client.did_open(JAVADOC, &fixture("java/Javadoc.java"));

    assert_eq!(
        hover(&client, 1, JAVADOC, 6, 13)["contents"]["value"],
        "```java\npublic class Javadoc\n```\n\nA documented type."
    );

    client.shutdown();
}

#[test]
fn a_position_on_nothing_answers_null_rather_than_an_error() {
    let client = Client::ready();
    client.did_open(SHADOWING, &fixture("java/Shadowing.java"));

    for (line, character, what) in [
        (8, 0, "the `public` keyword"),
        (10, 7, "the space after the indent"),
        (5, 0, "a blank line"),
        (9_000, 0, "past the end of the file"),
    ] {
        assert_eq!(
            definition(&client, 1, SHADOWING, line, character),
            Value::Null,
            "definition on {what}"
        );
        assert_eq!(
            hover(&client, 2, SHADOWING, line, character),
            Value::Null,
            "hover on {what}"
        );
    }

    client.shutdown();
}

#[test]
fn a_file_the_editor_never_opened_answers_null() {
    let client = Client::ready();
    assert_eq!(
        definition(&client, 1, "file:///Never.java", 0, 0),
        Value::Null
    );
    assert_eq!(hover(&client, 2, "file:///Never.java", 0, 0), Value::Null);
    client.shutdown();
}

#[test]
fn definition_and_hover_answer_the_buffer_as_it_is_now_rather_than_as_it_was_opened() {
    let client = Client::ready();
    client.did_open(
        "file:///Live.java",
        "class A {\n  int x = 1;\n  int y = x;\n}\n",
    );
    client.notify(
        lsp_types::notification::DidChangeTextDocument::METHOD,
        json!({
            "textDocument": { "uri": "file:///Live.java", "version": 2 },
            "contentChanges": [{
                "range": {
                    "start": { "line": 1, "character": 2 },
                    "end": { "line": 1, "character": 5 }
                },
                "text": "long"
            }]
        }),
    );

    // The reparse runs on a worker, so the first ask after the edit may still be
    // answered from the previous view. What is not allowed is never catching up.
    const CAUGHT_UP: &str = "```java\nlong x = 1;\n```";
    let answer = client.eventually(
        lsp_types::request::HoverRequest::METHOD,
        at("file:///Live.java", 2, 10),
        |answer| answer["contents"]["value"] == CAUGHT_UP,
    );
    assert_eq!(answer["contents"]["value"], CAUGHT_UP);

    client.shutdown();
}

#[test]
fn a_malformed_position_is_rejected_without_taking_the_server_down() {
    let client = Client::ready();
    client.did_open("file:///Live.java", "class A {}");

    let response = client.request(
        1,
        lsp_types::request::HoverRequest::METHOD,
        json!({ "textDocument": { "uri": "file:///Live.java" }, "position": "middle" }),
    );
    match response.response_result {
        Err(error) => assert_eq!(error.code, lsp_server::ErrorCode::InvalidParams as i32),
        Ok(value) => panic!("expected InvalidParams, got {value}"),
    }
    assert!(matches!(
        client.request(
            2,
            lsp_types::request::HoverRequest::METHOD,
            at("file:///Live.java", 0, 6)
        ),
        lsp_server::Response {
            response_result: Ok(_),
            ..
        }
    ));

    client.shutdown();
}

/// A guard on the harness itself: every assertion above reads a response, so a
/// server that answered twice would leave a message behind and the next test
/// would read the wrong one.
#[test]
fn one_request_produces_exactly_one_response() {
    let client = Client::ready();
    client.did_open("file:///Live.java", "class A {\n  int x = 1;\n}\n");
    definition(&client, 1, "file:///Live.java", 1, 6);
    client.send(
        2,
        lsp_types::request::DocumentSymbolRequest::METHOD,
        json!({ "textDocument": { "uri": "file:///Live.java" } }),
    );
    match client.receive() {
        Message::Response(response) => assert_eq!(response.id, lsp_server::RequestId::from(2)),
        other => panic!("expected the outline response, got {other:?}"),
    }
    client.shutdown();
}
