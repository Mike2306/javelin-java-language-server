//! Completion, asked the way an editor asks it.
//!
//! Over a real handshake and against the Java fixtures, so what is checked here
//! is the whole path: an LSP position in, a `CompletionList` out, with the
//! engine's answer surviving serialisation on the way.

use javelin_integration_tests::{Client, fixture};
use lsp_types::notification::Notification as _;
use lsp_types::request::Request as _;
use serde_json::{Value, json};

const SHADOWING: &str = "file:///Shadowing.java";

fn complete(client: &Client, id: i32, uri: &str, line: u32, character: u32) -> Value {
    client.ask(
        id,
        lsp_types::request::Completion::METHOD,
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
        }),
    )
}

/// The labels offered, in the order the server put them in.
fn labels(list: &Value) -> Vec<&str> {
    list["items"]
        .as_array()
        .expect("a completion list has items")
        .iter()
        .map(|item| item["label"].as_str().expect("a label"))
        .collect()
}

#[test]
fn completion_inside_a_method_offers_its_locals_its_parameters_and_the_types_methods() {
    let client = Client::ready();
    client.did_open(SHADOWING, &fixture("java/Shadowing.java"));

    // `        return fromParameter;` — the indent, so nothing is typed yet.
    let list = complete(&client, 1, SHADOWING, 15, 8);
    assert_eq!(
        labels(&list),
        vec![
            "fromParameter",
            "value",
            "Shadowing",
            "inMethod",
            "inLambda",
            "inAnotherMethod",
        ]
    );
    assert_eq!(
        list["isIncomplete"], true,
        "one file is never the whole answer"
    );

    client.shutdown();
}

#[test]
fn nothing_from_outside_the_file_is_offered() {
    let client = Client::ready();
    client.did_open(SHADOWING, &fixture("java/Shadowing.java"));

    let list = complete(&client, 1, SHADOWING, 15, 8);
    let offered = labels(&list);
    // `import java.util.function.IntUnaryOperator;` — named in the file, but
    // declared somewhere this phase cannot see.
    assert!(!offered.contains(&"IntUnaryOperator"), "{offered:?}");
    // A block-local whose scope closed two lines above the cursor.
    assert!(!offered.contains(&"fromBlock"), "{offered:?}");
    // Nothing from the JDK, which is not read at all.
    assert!(!offered.contains(&"String"), "{offered:?}");

    client.shutdown();
}

#[test]
fn a_shadowed_name_is_offered_once_with_the_declaration_that_wins_there() {
    let client = Client::ready();
    client.did_open(SHADOWING, &fixture("java/Shadowing.java"));

    // Inside the block on line 13, where the local `value` shadows both the
    // parameter and the field of that name.
    let items = complete(&client, 2, SHADOWING, 13, 28)["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter(|item| item["label"] == "value")
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["kind"], 6, "CompletionItemKind.Variable");

    client.shutdown();
}

#[test]
fn a_partial_name_narrows_the_offer_to_what_starts_with_it() {
    let client = Client::ready();
    client.did_open(SHADOWING, &fixture("java/Shadowing.java"));

    // `        return fromParameter;` — the cursor four characters into the name.
    assert_eq!(
        labels(&complete(&client, 1, SHADOWING, 15, 19)),
        vec!["fromParameter"]
    );

    client.shutdown();
}

#[test]
fn completion_after_a_dot_offers_nothing_rather_than_everything_in_scope() {
    let client = Client::ready();
    client.did_open(
        "file:///Member.java",
        "class Member {\n  int count;\n  void go(Member other) {\n    int x = other.;\n  }\n}\n",
    );

    let list = complete(&client, 1, "file:///Member.java", 3, 18);
    assert_eq!(labels(&list), Vec::<&str>::new());
    assert_eq!(list["isIncomplete"], true);

    client.shutdown();
}

#[test]
fn completion_answers_the_buffer_as_it_is_now_rather_than_as_it_was_opened() {
    let client = Client::ready();
    client.did_open(
        "file:///Live.java",
        "class A {\n  void go() {\n    r\n  }\n}\n",
    );
    let empty = complete(&client, 1, "file:///Live.java", 2, 5);
    assert_eq!(labels(&empty), Vec::<&str>::new());

    client.notify(
        lsp_types::notification::DidChangeTextDocument::METHOD,
        json!({
            "textDocument": { "uri": "file:///Live.java", "version": 2 },
            "contentChanges": [{
                "range": {
                    "start": { "line": 1, "character": 2 },
                    "end": { "line": 1, "character": 2 }
                },
                "text": "int running = 1;\n  "
            }]
        }),
    );

    // The reparse runs on a worker, so the first ask after the edit may still be
    // answered from the previous view. What is not allowed is never catching up.
    let caught_up = client.eventually(
        lsp_types::request::Completion::METHOD,
        json!({
            "textDocument": { "uri": "file:///Live.java" },
            "position": { "line": 3, "character": 5 },
        }),
        |answer| labels(answer) == vec!["running"],
    );
    assert_eq!(labels(&caught_up), vec!["running"]);

    client.shutdown();
}

#[test]
fn a_file_the_editor_never_opened_completes_to_an_empty_list() {
    let client = Client::ready();
    let list = complete(&client, 1, "file:///Never.java", 0, 0);
    assert_eq!(labels(&list), Vec::<&str>::new());
    client.shutdown();
}

#[test]
fn a_position_past_the_end_of_the_file_is_answered_rather_than_erroring() {
    let client = Client::ready();
    client.did_open(SHADOWING, &fixture("java/Shadowing.java"));
    assert_eq!(
        complete(&client, 1, SHADOWING, 9_000, 0)["isIncomplete"],
        true
    );
    client.shutdown();
}
