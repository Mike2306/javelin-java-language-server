//! What Lombok generates, answered by every reader verb: a getter, a setter
//! and a builder are completed, hovered and followed into the field or type
//! that generated them, from another file and from the declaring one.

use javelin_core::{CancellationToken, Config};
use javelin_document::DocumentId;
use lsp_types::{
    CompletionResponse, GotoDefinitionParams, GotoDefinitionResponse, HoverContents, Location,
    TextDocumentPositionParams,
};

use crate::ServerState;
use crate::handlers::file_uri;
use crate::workspace::testing::{Sources, indexed_with_member_refs};

const CAR: &str = "package p;\n\
                   import lombok.Builder;\n\
                   import lombok.Data;\n\
                   @Data @Builder\n\
                   public class Car implements Named {\n    \
                   private String name;\n    \
                   private final int wheels;\n    \
                   void tune() { setName(getName()); Car.builder().wheels(4); }\n\
                   }\n";
const NAMED: &str = "package p;\npublic interface Named {\n    String getName();\n}\n";
const GARAGE: &str = "package p;\n\
                      class Garage {\n    \
                      String park(Car car) {\n        \
                      car.setName(\"x\");\n        \
                      return Car.builder().name(\"y\").build().getName();\n    \
                      }\n    \
                      String label(Named named) { return named.getName(); }\n\
                      }\n";

fn sources() -> Sources {
    indexed_with_member_refs(
        "lombok-handlers",
        &[
            ("p/Car.java", CAR),
            ("p/Named.java", NAMED),
            ("p/Garage.java", GARAGE),
        ],
    )
}

/// A session with `relative` open at its real path and the index installed.
fn open(sources: &Sources, relative: &str, text: &str) -> (ServerState, String) {
    let uri = file_uri(&sources.path(relative)).expect("a uri");
    let mut state = ServerState::new(Config::default()).expect("grammar loads");
    state.did_open(DocumentId::new(uri.as_str()), 1, text.to_owned());
    state.install_workspace(crate::workspace::Workspace {
        index: std::sync::Arc::clone(&sources.index),
        elapsed: std::time::Duration::ZERO,
        refreshed: false,
        survey: Some(std::sync::Arc::default()),
        absorbed: 0,
    });
    (state, uri.as_str().to_owned())
}

/// The position just inside the `nth` occurrence of `needle`.
fn at(uri: &str, text: &str, needle: &str, nth: usize) -> TextDocumentPositionParams {
    let offset = text
        .match_indices(needle)
        .nth(nth)
        .map(|(offset, _)| offset + 1)
        .expect("in the text");
    let before = &text[..offset];
    let line = before.matches('\n').count() as u32;
    let character = (offset - before.rfind('\n').map_or(0, |n| n + 1)) as u32;
    serde_json::from_value(serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character }
    }))
    .expect("well-formed")
}

fn definition(state: &mut ServerState, at: TextDocumentPositionParams) -> Option<Location> {
    let params = GotoDefinitionParams {
        text_document_position_params: at,
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    match crate::handlers::definition::handle(state, &params, &CancellationToken::new())
        .expect("not cancelled")?
    {
        GotoDefinitionResponse::Scalar(location) => Some(location),
        other => panic!("one location, got {other:?}"),
    }
}

/// Where `location` is, as the file's name and the text it starts at.
fn landed(sources: &Sources, location: &Location) -> (String, String) {
    let name = location
        .uri
        .as_str()
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_owned();
    let text = std::fs::read_to_string(sources.path(&format!("p/{name}"))).expect("on disk");
    let line = text
        .lines()
        .nth(location.range.start.line as usize)
        .unwrap_or_default();
    (
        name,
        line[location.range.start.character as usize..].to_owned(),
    )
}

#[test]
fn a_lombok_getter_from_another_file_completes_hovers_and_jumps_to_its_field() {
    let sources = sources();
    let (mut state, uri) = open(&sources, "p/Garage.java", GARAGE);
    let jump = definition(&mut state, at(&uri, GARAGE, "setName", 0)).expect("the setter");
    assert_eq!(
        landed(&sources, &jump),
        ("Car.java".to_owned(), "name;".to_owned())
    );

    let hover = crate::handlers::hover::handle(
        &mut state,
        &lsp_types::HoverParams {
            text_document_position_params: at(&uri, GARAGE, "setName", 0),
            work_done_progress_params: Default::default(),
        },
        &CancellationToken::new(),
    )
    .expect("not cancelled")
    .expect("a hover");
    let HoverContents::Markup(markup) = hover.contents else {
        panic!("markup");
    };
    assert!(
        markup.value.contains("void setName(String name)"),
        "{}",
        markup.value
    );

    let mut position = at(&uri, GARAGE, "car.setName", 0);
    position.position.character += 3;
    let listed = crate::handlers::completion::handle(
        &mut state,
        &lsp_types::CompletionParams {
            text_document_position: position,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        },
        &CancellationToken::new(),
    )
    .expect("not cancelled");
    let CompletionResponse::List(list) = listed else {
        panic!("a list");
    };
    let labels: Vec<&str> = list.items.iter().map(|item| item.label.as_str()).collect();
    for expected in ["getName", "setName", "getWheels"] {
        assert!(labels.contains(&expected), "{expected} in {labels:?}");
    }
    assert!(
        !labels.contains(&"setWheels"),
        "a final field has no setter"
    );
}

/// `Car.builder().name("y").build().getName()`: each hop is a generated
/// member, and each lands on what generated it.
#[test]
fn a_lombok_builder_chain_is_followed_hop_by_hop() {
    let sources = sources();
    let (mut state, uri) = open(&sources, "p/Garage.java", GARAGE);
    let line = |needle: &str, nth: usize, state: &mut ServerState| {
        let jump = definition(state, at(&uri, GARAGE, needle, nth)).expect(needle);
        landed(&sources, &jump)
    };
    // `builder()` on the type's name and `build()` just after it: two
    // methods, two sites.
    let car = ("Car.java".to_owned(), "Car implements Named {".to_owned());
    assert_eq!(line("builder", 0, &mut state), car);
    assert_eq!(
        line("name(\"y\")", 0, &mut state),
        ("Car.java".to_owned(), "name;".to_owned())
    );
    assert_eq!(
        line("build()", 0, &mut state),
        ("Car.java".to_owned(), " implements Named {".to_owned())
    );
    assert_eq!(
        line("getName", 0, &mut state),
        ("Car.java".to_owned(), "name;".to_owned())
    );
}

/// Inside the declaring file the walk reads what is written and asks the
/// index for what Lombok adds.
#[test]
fn a_lombok_member_is_answered_inside_the_file_that_generates_it() {
    let sources = sources();
    let (mut state, uri) = open(&sources, "p/Car.java", CAR);
    for (needle, expected) in [
        ("setName", "name;"),
        ("getName", "name;"),
        ("builder", "Car implements Named {"),
        ("wheels(4)", "wheels;"),
    ] {
        let jump = definition(&mut state, at(&uri, CAR, needle, 0)).expect(needle);
        assert_eq!(
            landed(&sources, &jump),
            ("Car.java".to_owned(), expected.to_owned()),
            "{needle}"
        );
    }
}

fn references(state: &mut ServerState, at: TextDocumentPositionParams) -> Vec<(String, u32)> {
    let params = lsp_types::ReferenceParams {
        text_document_position: at,
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: lsp_types::ReferenceContext {
            include_declaration: false,
        },
    };
    let mut found: Vec<(String, u32)> =
        crate::handlers::references::handle(state, &params, &CancellationToken::new())
            .expect("not cancelled")
            .into_iter()
            .map(|location| {
                (
                    location
                        .uri
                        .as_str()
                        .rsplit('/')
                        .next()
                        .unwrap_or_default()
                        .to_owned(),
                    location.range.start.line,
                )
            })
            .collect();
    found.sort();
    found
}

/// A getter is navigated as its property: the field's references are its
/// accessors' uses, in every file.
#[test]
fn references_on_a_lombok_field_list_its_accessors_uses() {
    let sources = sources();
    let (mut state, uri) = open(&sources, "p/Car.java", CAR);
    assert_eq!(
        references(&mut state, at(&uri, CAR, "name;", 0)),
        vec![
            ("Car.java".to_owned(), 7),
            ("Car.java".to_owned(), 7),
            ("Garage.java".to_owned(), 3),
            ("Garage.java".to_owned(), 4),
            ("Garage.java".to_owned(), 4),
        ]
    );
}

/// An interface method a Lombok getter implements: implementation lists the
/// class, and references stay the interface's own — the getter's site is
/// its field's, whose setter and builder uses are not calls of `getName`.
#[test]
fn an_interface_method_a_lombok_getter_implements() {
    let sources = sources();
    let (mut state, uri) = open(&sources, "p/Named.java", NAMED);
    let params = GotoDefinitionParams {
        text_document_position_params: at(&uri, NAMED, "getName", 0),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    let Some(GotoDefinitionResponse::Array(found)) =
        crate::handlers::hierarchy::implementation(&mut state, &params, &CancellationToken::new())
            .expect("not cancelled")
    else {
        panic!("an array");
    };
    assert_eq!(found.len(), 1);
    assert_eq!(
        landed(&sources, &found[0]),
        ("Car.java".to_owned(), "name;".to_owned())
    );
    assert_eq!(
        references(&mut state, at(&uri, NAMED, "getName", 0)),
        vec![("Garage.java".to_owned(), 6)]
    );
}

/// Asked from a use in another file, the property's uses include the
/// declaring file's own calls, which the worker resolved through what Lombok
/// generates there.
#[test]
fn references_on_a_lombok_accessor_reach_the_declaring_files_own_calls() {
    let sources = sources();
    let (mut state, uri) = open(&sources, "p/Garage.java", GARAGE);
    let found = references(&mut state, at(&uri, GARAGE, "setName", 0));
    for expected in [("Car.java".to_owned(), 7), ("Garage.java".to_owned(), 3)] {
        assert!(found.contains(&expected), "{expected:?} in {found:?}");
    }
}

/// A hand-written override of a Lombok getter: what it overrides is sited at
/// a field, whose setter use is no call of the getter.
#[test]
fn references_on_an_override_of_a_lombok_getter_leave_the_field_alone() {
    const BASE: &str = "package p;\nimport lombok.Getter;\nimport lombok.Setter;\n\
                        @Getter @Setter\npublic class Base { String name; }\n";
    const SUB: &str = "package p;\npublic class Sub extends Base {\n    \
                       @Override public String getName() { return \"\"; }\n}\n";
    const USE: &str = "package p;\nclass Use {\n    \
                       void m(Base b) { b.setName(\"x\"); b.getName(); }\n}\n";
    let sources = indexed_with_member_refs(
        "lombok-override",
        &[
            ("p/Base.java", BASE),
            ("p/Sub.java", SUB),
            ("p/Use.java", USE),
        ],
    );
    let (mut state, uri) = open(&sources, "p/Sub.java", SUB);
    assert_eq!(
        references(&mut state, at(&uri, SUB, "getName", 0)),
        Vec::<(String, u32)>::new()
    );
}

/// A buffer the index does not hold as this path — another worktree's
/// `p/Car.java` — is not the workspace's `p.Car`, and borrows none of what
/// Lombok generated there.
#[test]
fn a_buffer_the_index_does_not_hold_borrows_no_generated_members() {
    let sources = sources();
    let (mut state, uri) = open(&sources, "elsewhere/p/Car.java", CAR);
    assert_eq!(definition(&mut state, at(&uri, CAR, "setName", 0)), None);
}

/// A quarter-second after the line above a field is deleted, the index's
/// row at the field's live offset is another field's: its accessors are not
/// this field's uses.
#[test]
fn references_on_a_field_ignore_accessors_the_stale_index_puts_at_its_offset() {
    const INDEXED: &str = "package p;\nimport lombok.Getter;\n@Getter\npublic class Pair {\n    \
                           private String title;\n    private String name;\n}\n";
    const BUFFER: &str = "package p;\nimport lombok.Getter;\n@Getter\npublic class Pair {\n    \
                          private String name;\n    void m() { getTitle(); }\n}\n";
    let sources = indexed_with_member_refs("lombok-stale", &[("p/Pair.java", INDEXED)]);
    let (mut state, uri) = open(&sources, "p/Pair.java", BUFFER);
    assert_eq!(
        references(&mut state, at(&uri, BUFFER, "name;", 0)),
        Vec::<(String, u32)>::new()
    );
}

/// A call hierarchy item is a written method's: a generated one shares a
/// field's site and is not prepared as the field.
#[test]
fn a_generated_accessor_is_not_a_call_hierarchy_item() {
    let sources = sources();
    let (mut state, uri) = open(&sources, "p/Garage.java", GARAGE);
    let prepared = crate::handlers::hierarchy::prepare_call_hierarchy(
        &mut state,
        &lsp_types::CallHierarchyPrepareParams {
            text_document_position_params: at(&uri, GARAGE, "setName", 0),
            work_done_progress_params: Default::default(),
        },
        &CancellationToken::new(),
    )
    .expect("not cancelled");
    assert!(
        prepared.as_ref().is_none_or(|items| items.is_empty()),
        "{prepared:?}"
    );
}

// ---- Tier 4, slice 7: `@SuperBuilder` and `@Singular`.

const VEHICLE: &str = "package p;\n\
                       import lombok.experimental.SuperBuilder;\n\
                       @SuperBuilder\n\
                       public abstract class Vehicle {\n    \
                       private int wheels;\n\
                       }\n";
const TRUCK: &str = "package p;\n\
                     import java.util.List;\n\
                     import lombok.Singular;\n\
                     import lombok.experimental.SuperBuilder;\n\
                     @SuperBuilder\n\
                     public class Truck extends Vehicle {\n    \
                     private String load;\n    \
                     @Singular private List<String> axles;\n    \
                     public String getLoad() { return load; }\n\
                     }\n";
const DEPOT: &str = "package p;\n\
                     class Depot {\n    \
                     String ship() {\n        \
                     return Truck.builder().load(\"x\").wheels(4).axle(\"a\")\n            \
                     .axles(List.of()).clearAxles().build().getLoad();\n    \
                     }\n\
                     }\n";

fn super_sources() -> Sources {
    indexed_with_member_refs(
        "lombok-super-builder",
        &[
            ("p/Vehicle.java", VEHICLE),
            ("p/Truck.java", TRUCK),
            ("p/Depot.java", DEPOT),
        ],
    )
}

/// A subclass's builder sets the superclass's fields after its own, and
/// `build()` is the subclass: every hop of the chain lands on what generated
/// it, the parent's setter in the parent's file.
#[test]
fn a_super_builder_chain_reaches_the_parents_setter_and_builds_the_subclass() {
    let sources = super_sources();
    let (mut state, uri) = open(&sources, "p/Depot.java", DEPOT);
    state.install_classpath(crate::classpath::Classpath::holding(vec![
        crate::classpath::fact("java.lang.Object"),
        crate::classpath::fact("java.lang.String"),
        crate::classpath::fact("java.util.List"),
    ]));
    let mut line = |needle: &str| {
        let jump = definition(&mut state, at(&uri, DEPOT, needle, 0)).expect(needle);
        landed(&sources, &jump)
    };
    let truck = |text: &str| ("Truck.java".to_owned(), text.to_owned());
    assert_eq!(line("builder"), truck("Truck extends Vehicle {"));
    assert_eq!(line("load(\"x\")"), truck("load;"));
    assert_eq!(
        line("wheels(4)"),
        ("Vehicle.java".to_owned(), "wheels;".to_owned())
    );
    assert_eq!(line("axle("), truck("axles;"));
    assert_eq!(line("axles(List"), truck("axles;"));
    assert_eq!(line("clearAxles"), truck("axles;"));
    assert_eq!(line("build()"), truck(" extends Vehicle {"));
    assert_eq!(line("getLoad"), truck("getLoad() { return load; }"));
}

/// The singular adder is sited at the field, so references on the field list
/// it beside the plural adder and the clear.
#[test]
fn references_on_a_singular_field_list_its_adders() {
    let sources = super_sources();
    let (mut state, uri) = open(&sources, "p/Truck.java", TRUCK);
    assert_eq!(
        references(&mut state, at(&uri, TRUCK, "axles;", 0)),
        vec![
            ("Depot.java".to_owned(), 3),
            ("Depot.java".to_owned(), 4),
            ("Depot.java".to_owned(), 4),
        ]
    );
}

/// `builder()` is typed as the private `TruckBuilderImpl` it returns, so a
/// chain is followed; a hint would name a class Java gives no expression,
/// whose type there is `TruckBuilder<?, ?>`, so none is shown.
#[test]
fn a_super_builders_private_implementation_is_never_a_type_hint() {
    let sources = super_sources();
    let yard = "package p;\n\
                class Yard {\n    \
                void m() {\n        \
                var built = Truck.builder().build();\n        \
                var bare = Truck.builder();\n        \
                var set = Truck.builder().load(\"x\");\n    \
                }\n\
                }\n";
    let (mut state, uri) = open(&sources, "p/Yard.java", yard);
    state.install_classpath(crate::classpath::Classpath::holding(vec![
        crate::classpath::fact("java.lang.Object"),
        crate::classpath::fact("java.lang.String"),
    ]));
    let params: lsp_types::InlayHintParams = serde_json::from_value(serde_json::json!({
        "textDocument": { "uri": uri },
        "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 9, "character": 0 } }
    }))
    .expect("well-formed");
    let labels: Vec<(u32, String)> =
        crate::handlers::inlay_hint::handle(&mut state, &params, &CancellationToken::new())
            .expect("not cancelled")
            .into_iter()
            .map(|hint| match hint.label {
                lsp_types::InlayHintLabel::String(label) => (hint.position.line, label),
                other => panic!("a plain label, got {other:?}"),
            })
            .collect();
    assert_eq!(
        labels,
        vec![(3, ": Truck".to_owned()), (5, "load:".to_owned())]
    );
}
