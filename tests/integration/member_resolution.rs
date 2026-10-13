//! Member resolution at the LSP level: the receiver's type comes from the open
//! file, and everything else answers nothing rather than guessing.
//!
//! Over a real handshake, so what is checked here is the whole path: an LSP
//! position in, a real response out, with the engine's answer surviving
//! serialisation on the way.

use javelin_integration_tests::Client;
use lsp_types::request::Request as _;
use serde_json::{Value, json};

fn at(uri: &str, line: u32, character: u32) -> Value {
    json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character },
    })
}

fn complete(client: &Client, id: i32, uri: &str, line: u32, character: u32) -> Value {
    client.ask(
        id,
        lsp_types::request::Completion::METHOD,
        at(uri, line, character),
    )
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
fn completing_after_a_dot_offers_the_receivers_members() {
    let client = Client::ready();
    // A trailing `;` right after the dot, the shape a real editor buffer
    // almost always has: a dangling `.` with nothing at all after it sends
    // tree-sitter's error recovery into an ERROR node covering the class.
    client.did_open(
        "file:///Box.java",
        "class Box { int width; String label() { return null; } }\n\
         class A { void m() { Box b; b.; } }\n",
    );

    let list = complete(&client, 1, "file:///Box.java", 1, 30);
    let offered = labels(&list);
    assert!(offered.contains(&"width"), "{offered:?}");
    assert!(offered.contains(&"label"), "{offered:?}");
    assert_eq!(list["isIncomplete"], true);

    client.shutdown();
}

#[test]
fn completing_after_a_dot_on_a_jdk_type_offers_nothing() {
    let client = Client::ready();
    client.did_open(
        "file:///Str.java",
        "class A { void m() { String s; s.; } }\n",
    );

    // It IS a member selection — `s` receives a completion request after its
    // dot — but the JDK's `String` is not something this file declares, so
    // the answer is the empty list rather than every local falling back in.
    let list = complete(&client, 1, "file:///Str.java", 0, 33);
    assert_eq!(labels(&list), Vec::<&str>::new());
    assert_eq!(list["isIncomplete"], true);

    client.shutdown();
}

#[test]
fn going_to_definition_on_a_member_lands_on_its_declaration() {
    let client = Client::ready();
    client.did_open(
        "file:///Box.java",
        "class Box { int width; }\n\
         class A { void m() { Box b; b.width; } }\n",
    );

    // The `d` inside `width` of `b.width;` on line 1.
    let location = definition(&client, 1, "file:///Box.java", 1, 32);
    assert_eq!(location["uri"], "file:///Box.java");
    // The `width` of `int width;` on line 0.
    assert_eq!(
        location["range"]["start"],
        json!({ "line": 0, "character": 16 })
    );
    assert_eq!(
        location["range"]["end"],
        json!({ "line": 0, "character": 21 })
    );

    client.shutdown();
}

#[test]
fn hovering_a_member_renders_its_declaration() {
    let client = Client::ready();
    client.did_open(
        "file:///Box.java",
        "class Box { /** How wide. */ int width; }\n\
         class A { void m() { Box b; b.width; } }\n",
    );

    let found = hover(&client, 1, "file:///Box.java", 1, 32);
    let markdown = found["contents"]["value"].as_str().expect("markdown");
    assert!(markdown.contains("int width"), "{markdown}");
    assert!(markdown.contains("How wide."), "{markdown}");

    client.shutdown();
}

#[test]
fn completing_after_a_var_local_offers_the_inferred_types_members() {
    let client = Client::ready();
    // The trailing `;` for the same reason as above. `b` has no written type:
    // everything offered here comes from typing `new Box()` and giving the
    // local that type.
    client.did_open(
        "file:///Var.java",
        "class Box { int width; String label() { return null; } }\n\
         class A { void m() { var b = new Box(); b.; } }\n",
    );

    let list = complete(&client, 1, "file:///Var.java", 1, 42);
    let offered = labels(&list);
    assert!(offered.contains(&"width"), "{offered:?}");
    assert!(offered.contains(&"label"), "{offered:?}");

    client.shutdown();
}

#[test]
fn definition_through_a_var_local_lands_on_the_member() {
    let client = Client::ready();
    client.did_open(
        "file:///Var.java",
        "class Box { int width; }\n\
         class A { void m() { var b = new Box(); b.width; } }\n",
    );

    // The `d` inside `width` of `b.width;` on line 1.
    let location = definition(&client, 1, "file:///Var.java", 1, 44);
    assert_eq!(location["uri"], "file:///Var.java");
    // The `width` of `int width;` on line 0.
    assert_eq!(
        location["range"]["start"],
        json!({ "line": 0, "character": 16 })
    );
    assert_eq!(
        location["range"]["end"],
        json!({ "line": 0, "character": 21 })
    );

    client.shutdown();
}

#[test]
fn hover_through_a_var_local_renders_the_members_declaration() {
    let client = Client::ready();
    client.did_open(
        "file:///Var.java",
        "class Box { /** How wide. */ int width; }\n\
         class A { void m() { var b = new Box(); b.width; } }\n",
    );

    let found = hover(&client, 1, "file:///Var.java", 1, 44);
    let markdown = found["contents"]["value"].as_str().expect("markdown");
    assert!(markdown.contains("int width"), "{markdown}");
    assert!(markdown.contains("How wide."), "{markdown}");

    client.shutdown();
}

#[test]
fn an_inherited_member_resolves_through_a_file_local_supertype() {
    let client = Client::ready();
    client.did_open(
        "file:///Box.java",
        "class Base { int id; }\n\
         class Box extends Base {}\n\
         class A { void m() { Box b; b.; } }\n",
    );

    let list = complete(&client, 1, "file:///Box.java", 2, 30);
    let offered = labels(&list);
    assert!(offered.contains(&"id"), "{offered:?}");

    client.shutdown();
}

#[test]
fn definition_on_an_unqualified_call_jumps_to_the_method() {
    let client = Client::ready();
    client.did_open(
        "file:///Call.java",
        "class A {\n\
         \x20   int count() { return 1; }\n\
         \x20   void m() { count(); }\n\
         }\n",
    );

    // The `u` inside `count` of `count();` on line 2.
    let location = definition(&client, 1, "file:///Call.java", 2, 17);
    assert_eq!(location["uri"], "file:///Call.java");
    // The `count` of `int count()` on line 1.
    assert_eq!(
        location["range"]["start"],
        json!({ "line": 1, "character": 8 })
    );
    assert_eq!(
        location["range"]["end"],
        json!({ "line": 1, "character": 13 })
    );

    client.shutdown();
}

#[test]
fn hover_on_an_unqualified_call_renders_its_declaration() {
    let client = Client::ready();
    client.did_open(
        "file:///Call.java",
        "class A {\n\
         \x20   /** How many. */\n\
         \x20   int count() { return 1; }\n\
         \x20   void m() { count(); }\n\
         }\n",
    );

    let found = hover(&client, 1, "file:///Call.java", 3, 17);
    let markdown = found["contents"]["value"].as_str().expect("markdown");
    assert!(markdown.contains("int count()"), "{markdown}");
    assert!(markdown.contains("How many."), "{markdown}");

    client.shutdown();
}

#[test]
fn definition_on_an_ambiguous_unqualified_call_answers_nothing() {
    let client = Client::ready();
    // Both overloads take one argument, so the call fits both and nothing
    // in the open file can separate them. Null is the honest answer; jumping
    // to whichever came first would be a guess dressed up as a result.
    client.did_open(
        "file:///Ambiguous.java",
        "class A {\n\
         \x20   int g(int i) { return i; }\n\
         \x20   int g(String s) { return 0; }\n\
         \x20   void m() { g(1); }\n\
         }\n",
    );

    assert_eq!(
        definition(&client, 1, "file:///Ambiguous.java", 3, 15),
        Value::Null
    );

    client.shutdown();
}

#[test]
fn definition_on_an_ambiguous_qualified_call_answers_nothing() {
    let client = Client::ready();
    // The qualified twin of `definition_on_an_ambiguous_unqualified_call_
    // answers_nothing` above: before Task 2, `o.g(1)` landed on whichever `g`
    // was declared first, because `member_definition_at` took the first match
    // rather than routing through `sole_candidate`. Both overloads fit the
    // call's one argument, and the argument cannot separate them either —
    // `g(String)`'s parameter is the JDK's, so no exclusion is demonstrable
    // until Phase 3 resolves it. Nothing is the honest answer, receiver or not.
    client.did_open(
        "file:///AmbiguousQualified.java",
        "class O {\n\
         \x20   int g(int i) { return i; }\n\
         \x20   int g(String s) { return 0; }\n\
         }\n\
         class A {\n\
         \x20   O o;\n\
         \x20   void m() { o.g(1); }\n\
         }\n",
    );

    assert_eq!(
        definition(&client, 1, "file:///AmbiguousQualified.java", 6, 17),
        Value::Null
    );

    client.shutdown();
}

#[test]
fn definition_on_a_qualified_call_lands_on_the_overload_the_argument_types_choose() {
    let client = Client::ready();
    // Both halves of the invariant over a real handshake. `f`'s two parameter
    // types are declared in this file, so the argument says which overload the
    // call means and definition lands on that one — on a different line for
    // each, which is the whole assertion. `g`'s do not: `g(String)`'s parameter
    // is the JDK's, nothing rules either out, and the answer stays null.
    client.did_open(
        "file:///Overloads.java",
        "class Foo {}\n\
         class Bar {}\n\
         class O {\n\
         \x20   int f(Foo p) { return 1; }\n\
         \x20   int f(Bar p) { return 2; }\n\
         \x20   int g(int i) { return i; }\n\
         \x20   int g(String s) { return 0; }\n\
         }\n\
         class A {\n\
         \x20   O o;\n\
         \x20   void m() { o.f(new Foo()); o.f(new Bar()); o.g(1); }\n\
         }\n",
    );

    // The `f` of `o.f(new Foo())` on line 10.
    let foo = definition(&client, 1, "file:///Overloads.java", 10, 17);
    assert_eq!(foo["uri"], "file:///Overloads.java");
    // The `f` of `int f(Foo p)` on line 3.
    assert_eq!(foo["range"]["start"], json!({ "line": 3, "character": 8 }));
    assert_eq!(foo["range"]["end"], json!({ "line": 3, "character": 9 }));

    // The `f` of `o.f(new Bar())`, which must land one line further down.
    let bar = definition(&client, 2, "file:///Overloads.java", 10, 33);
    assert_eq!(bar["range"]["start"], json!({ "line": 4, "character": 8 }));
    assert_eq!(bar["range"]["end"], json!({ "line": 4, "character": 9 }));

    // The `g` of `o.g(1)`, whose overloads no argument can separate.
    assert_eq!(
        definition(&client, 3, "file:///Overloads.java", 10, 49),
        Value::Null
    );

    client.shutdown();
}

#[test]
fn hover_on_an_ambiguous_qualified_call_renders_nothing() {
    let client = Client::ready();
    // Same offset, same ambiguity, the other feature that shares
    // `member_definition_at`'s answer: hover must refuse exactly where
    // definition does, not disagree with it.
    client.did_open(
        "file:///AmbiguousQualifiedHover.java",
        "class O {\n\
         \x20   int g(int i) { return i; }\n\
         \x20   int g(String s) { return 0; }\n\
         }\n\
         class A {\n\
         \x20   O o;\n\
         \x20   void m() { o.g(1); }\n\
         }\n",
    );

    assert_eq!(
        hover(&client, 1, "file:///AmbiguousQualifiedHover.java", 6, 17),
        Value::Null
    );

    client.shutdown();
}

#[test]
fn unqualified_completion_still_answers_from_locals() {
    let client = Client::ready();
    client.did_open(
        "file:///Local.java",
        "class A { void m() { int width = 1; wid } }\n",
    );

    // No dot at all: the member path declines and the file-local query, which
    // this change does not touch, answers as it always has.
    let list = complete(&client, 1, "file:///Local.java", 0, 39);
    assert_eq!(labels(&list), vec!["width"]);

    client.shutdown();
}

#[test]
fn a_dot_as_the_last_token_in_the_file_answers_empty_rather_than_wrongly() {
    let client = Client::ready();
    // `this.` is the very last token before the file's closing braces — no
    // statement, not even a stray `;`, follows it. Tree-sitter's error
    // recovery responds by dropping the enclosing `class_declaration`
    // entirely, so `FileSyntax::build` hoists `width` and `m` to top level.
    // `enclosing_type` then walks the syntax tree looking for a declaration
    // that contains the cursor and finds none, so `this.` has no type to
    // list members from. The same file with anything at all after the dot —
    // another statement, or merely another method — keeps its enclosing
    // class and resolves normally; see
    // `completing_after_a_dot_offers_the_receivers_members` above for that
    // shape.
    //
    // The empty list here is the honest answer, not a wrong one: Javelin does
    // not know `this`'s type in this position, and says so rather than
    // guessing. If a future tree-sitter grammar upgrade recovers the class
    // declaration in this position too, this test starts failing — that is
    // the signal to update it, not to leave it red.
    client.did_open(
        "file:///Terminal.java",
        "class A {\n    int width;\n    void m() {\n        this.\n    }\n}\n",
    );

    let list = complete(&client, 1, "file:///Terminal.java", 3, 13);
    assert_eq!(labels(&list), Vec::<&str>::new());
    assert_eq!(list["isIncomplete"], true);

    client.shutdown();
}

#[test]
fn a_var_local_as_the_last_token_in_the_file_still_offers_its_members() {
    let client = Client::ready();
    // The `var` counterpart to
    // `a_dot_as_the_last_token_in_the_file_answers_empty_rather_than_wrongly`,
    // and it lands on the other side of that ceiling: `b.` is the very last
    // token before the closing braces, and the members still come back.
    //
    // The ceiling needs both conditions at once — the terminal position *and*
    // a receiver tree-sitter cannot close. Neither alone does it. Position
    // alone is not enough: `infer`'s `this_is_the_enclosing_type` resolves the
    // same `this.` receiver to `A` with only a `;` after it, and the `get(). ;`
    // sibling in the very next test below resolves for the same reason. (Its
    // name would run past 100 columns quoted in full, so it is not.) Receiver
    // shape alone is not enough either: a bare name followed by a `.` recovers
    // even here. A `var` local is always referenced by a bare name, so the
    // `var` path never meets both conditions and never reaches the ceiling.
    client.did_open(
        "file:///TerminalVar.java",
        "class Box {\n\
         \x20   int width;\n\
         }\n\
         class A {\n\
         \x20   void m() {\n\
         \x20       var b = new Box();\n\
         \x20       b.\n\
         \x20   }\n\
         }\n",
    );

    let list = complete(&client, 1, "file:///TerminalVar.java", 6, 10);
    let offered = labels(&list);
    assert!(offered.contains(&"width"), "{offered:?}");

    client.shutdown();
}

#[test]
fn an_unqualified_call_receiver_as_the_last_token_in_the_file_answers_empty_rather_than_wrongly() {
    let client = Client::ready();
    // The shape that does reach the ceiling, through the other new path:
    // `g().` needs the enclosing type to know which `g` it means, and with
    // nothing after the dot, recovery has dropped `class A` and hoisted `get`
    // and `m` to top level. There is no enclosing type to ask, so the
    // receiver types as unknown and the list is empty — offering `Box`'s
    // members would be a guess about which method the call reached.
    //
    // If a future grammar upgrade recovers the class declaration in this
    // position, this test starts failing — that is the signal to update it,
    // not to leave it red.
    client.did_open(
        "file:///TerminalCall.java",
        "class Box {\n\
         \x20   int width;\n\
         }\n\
         class A {\n\
         \x20   Box get() { return null; }\n\
         \x20   void m() {\n\
         \x20       get().\n\
         \x20   }\n\
         }\n",
    );

    let list = complete(&client, 1, "file:///TerminalCall.java", 6, 14);
    assert_eq!(labels(&list), Vec::<&str>::new());
    assert_eq!(list["isIncomplete"], true);

    // The positive control, and the evidence that the terminal position is
    // half of the condition rather than incidental: the identical receiver
    // with a `;` after it keeps its enclosing class and resolves. A separate
    // document, because putting both in one file would drop the class for
    // both — recovery discards the whole `class_declaration`, not one
    // statement.
    client.did_open(
        "file:///AnchoredCall.java",
        "class Box {\n\
         \x20   int width;\n\
         }\n\
         class A {\n\
         \x20   Box get() { return null; }\n\
         \x20   void m() {\n\
         \x20       get(). ;\n\
         \x20   }\n\
         }\n",
    );

    let anchored = complete(&client, 2, "file:///AnchoredCall.java", 6, 14);
    let offered = labels(&anchored);
    assert!(offered.contains(&"width"), "{offered:?}");

    client.shutdown();
}

#[test]
fn whitespace_before_the_dot_still_offers_the_receivers_members() {
    let client = Client::ready();
    // Two shapes that put something between the receiver and the dot: a space,
    // and the leading-dot chain IntelliJ formats a long call chain as. Both
    // are ordinary Java, and both used to answer the empty list.
    client.did_open(
        "file:///Chain.java",
        "class Box { int width; Box child() { return null; } }\n\
         class A { void m() { Box b; b . ; } }\n\
         class B { void m(Box b) { b\n\
         \x20 .child()\n\
         \x20 . ; } }\n",
    );

    let spaced_list = complete(&client, 1, "file:///Chain.java", 1, 31);
    let spaced = labels(&spaced_list);
    assert!(spaced.contains(&"width"), "{spaced:?}");
    assert!(spaced.contains(&"child"), "{spaced:?}");

    let chained_list = complete(&client, 2, "file:///Chain.java", 4, 3);
    let chained = labels(&chained_list);
    assert!(chained.contains(&"width"), "{chained:?}");
    assert!(chained.contains(&"child"), "{chained:?}");

    client.shutdown();
}
