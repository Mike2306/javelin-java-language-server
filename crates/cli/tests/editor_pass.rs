//! The manual Zed pass, performed by a client that obeys the handshake.
//!
//! Each test here is one row of `docs/manual-testing-zed.md`, named after the
//! behaviour that breaks. The refusals are load-bearing: they are the positions
//! where a wrong answer is a jump rather than a blank, so a harness that
//! silently passed everything would fail them.

mod harness;

use std::time::Duration;

use harness::editor::{Editor, demo_project, file_uri};
use harness::jdk::SyntheticJdk;
use harness::no_classpath;
use harness::text::{offset_of, position_of};

fn greeter() -> std::path::PathBuf {
    demo_project().join("src/main/java/com/example/Greeter.java")
}

fn broken() -> std::path::PathBuf {
    demo_project().join("src/main/java/com/example/Broken.java")
}

fn reception() -> std::path::PathBuf {
    demo_project().join("src/main/java/com/example/Reception.java")
}

/// The end of `timesSeen`, which is where a scratch *declaration* goes: inside
/// the class body, and below every member another check locates by its text.
fn after_times_seen(editor: &Editor, uri: &str) -> (u32, u32) {
    editor.find(
        uri,
        "    public int timesSeen() {\n        return seen.getOrDefault(name, 0);\n    }",
    )
}

/// The end of `main`'s `var greeter` line, which is where a scratch *statement*
/// goes: `greeter` is in scope there, and nothing below it is a check's needle.
fn in_main(editor: &Editor, uri: &str) -> (u32, u32) {
    editor.find(
        uri,
        "var greeter = new Greeter(DEFAULT, Map.of(DEFAULT, 1));",
    )
}

/// The zero-based line a declaration is written on, for a check whose target
/// was typed in rather than checked in and so has no fixed line number.
fn line_of(editor: &Editor, uri: &str, declaration: &str) -> u32 {
    let text = editor.text(uri);
    let at = text
        .find(declaration)
        .unwrap_or_else(|| panic!("{declaration:?} was never typed"));
    text[..at].matches('\n').count() as u32
}

#[test]
fn opening_a_well_formed_file_parses_it_without_error_recovery() {
    let mut editor = Editor::launch_verbose(&no_classpath());
    let uri = editor.open(&greeter());
    assert!(
        editor
            .logs()
            .wait_for("parsed document", Duration::from_secs(5))
            .is_some(),
        "no parse was logged for {uri}"
    );
    let parsed = editor
        .logs()
        .texts()
        .into_iter()
        .find(|line| line.contains("parsed document"))
        .expect("the line just arrived")
        .to_string();
    assert!(
        parsed.contains("recovered=false"),
        "the demo file is well formed; error recovery means the grammar changed: {parsed}"
    );
}

#[test]
fn a_burst_of_keystrokes_reparses_the_version_the_buffer_ended_at() {
    let mut editor = Editor::launch_verbose(&no_classpath());
    let uri = editor.open(&greeter());
    editor
        .logs()
        .wait_for("parsed document", Duration::from_secs(5));

    let (line, character) = in_main(&editor, &uri);
    editor.type_at(&uri, line, character, "\n        greeter.greet();");

    // The last version the editor sent must be reparsed; the pool may coalesce
    // earlier ones, which is the supersede path working rather than an edit lost.
    let last = editor.version(&uri);
    assert!(
        editor
            .logs()
            .wait_for(&format!("version={last}"), Duration::from_secs(10))
            .is_some(),
        "the buffer reached version {last} and no reparse mentioned it"
    );
    assert!(
        editor
            .logs()
            .texts()
            .iter()
            .any(|line| line.contains("reparsed document")),
        "an edit produced no reparse at all; didChange never reached the document store"
    );
}

/// The manual pass types one character and expects `incremental=true`, which
/// only holds when the previous reparse has landed: `did_change` reparses from
/// the last *installed* tree, so a burst faster than the pool falls back to a
/// full parse by design. This editor types faster than any human, so the wait
/// is what makes the check about incremental sync rather than about the race.
#[test]
fn an_edit_arriving_after_the_previous_reparse_landed_is_parsed_incrementally() {
    let mut editor = Editor::launch_verbose(&no_classpath());
    let uri = editor.open(&greeter());
    editor
        .logs()
        .wait_for("parsed document", Duration::from_secs(5));

    let (line, character) = editor.find(&uri, "public int timesSeen() {");
    editor.type_at(&uri, line, character, " ");

    assert!(
        editor
            .logs()
            .wait_for("reparsed document", Duration::from_secs(10))
            .is_some(),
        "an edit produced no reparse at all; didChange never reached the document store"
    );
    let reparsed = editor
        .logs()
        .texts()
        .into_iter()
        .find(|line| line.contains("reparsed document"))
        .expect("the line just arrived")
        .to_string();
    assert!(
        reparsed.contains("version=1"),
        "one keystroke must reparse the version it produced: {reparsed}"
    );
    assert!(
        reparsed.contains("incremental=true"),
        "the only edit in flight reparsed from scratch; incremental sync is not \
         reaching the parser: {reparsed}"
    );
}

#[test]
fn a_broken_file_degrades_alone_and_leaves_the_session_up() {
    let mut editor = Editor::launch_verbose(&no_classpath());
    let good = editor.open(&greeter());
    let bad = editor.open(&broken());
    assert!(
        editor
            .logs()
            .wait_for("recovered=true", Duration::from_secs(5))
            .is_some(),
        "Broken.java is unparseable on purpose and must be recovered, not rejected"
    );
    // The whole point: the other file still answers.
    let outline = editor.outline(&good);
    assert!(
        outline
            .as_array()
            .is_some_and(|symbols| !symbols.is_empty()),
        "a broken file took the session's other document with it"
    );
    assert!(
        editor.outline(&bad).as_array().is_some(),
        "the broken file must answer an outline, empty or not, rather than error"
    );
}

#[test]
fn a_shutdown_leaves_no_process_behind() {
    let mut editor = Editor::launch(&no_classpath());
    editor.open(&greeter());
    assert_eq!(
        editor.shutdown(),
        Some(0),
        "the server did not exit cleanly; a lingering process is the shutdown deadlock \
         the in-memory tests structurally cannot see"
    );
}

// ---- Phase 1: interactive Java within one file -----------------------------

#[test]
fn the_outline_nests_the_record_and_the_nested_interface() {
    let mut editor = Editor::launch(&no_classpath());
    let uri = editor.open(&greeter());
    let outline = editor.outline(&uri);
    let names = flatten(&outline);
    for expected in [
        "Greeter",
        "greet",
        "greetAll",
        "timesSeen",
        "main",
        "Visit",
        "Listener",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "the outline is missing {expected}: {names:?}"
        );
    }
    let class = outline
        .as_array()
        .expect("an array of symbols")
        .iter()
        .find(|symbol| symbol["name"] == "Greeter")
        .expect("the class");
    let children = flatten(&class["children"]);
    assert!(
        children.contains(&"Visit".to_string()) && children.contains(&"Listener".to_string()),
        "the nested types are not nested under Greeter: {children:?}"
    );
}

/// Every symbol name in a `DocumentSymbol` tree, depth first.
fn flatten(symbols: &serde_json::Value) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(list) = symbols.as_array() {
        for symbol in list {
            if let Some(name) = symbol["name"].as_str() {
                names.push(name.to_string());
            }
            names.extend(flatten(&symbol["children"]));
        }
    }
    names
}

#[test]
fn definition_on_a_shadowing_lambda_parameter_lands_on_the_parameter_not_the_field() {
    let mut editor = Editor::launch(&no_classpath());
    let uri = editor.open(&greeter());
    // `.map(name -> "Hallo, " + name)` — the second `name`, inside the lambda body.
    let (line, character) = editor.find(&uri, "\"Hallo, \" + nam");
    let (target, at) = editor
        .definition_at(&uri, line, character)
        .expect("the lambda parameter");
    // `Definition` carries its own file now, so a jump can legitimately leave
    // the requested buffer. A local one that does has rendered a foreign span.
    assert_eq!(target, uri, "the declaration is in this file");
    assert_eq!(
        at, 31,
        "line 32 declares the lambda parameter; line 16 declares the field"
    );
}

#[test]
fn completion_in_a_method_body_offers_locals_and_members_and_nothing_from_java_util() {
    let mut editor = Editor::launch(&no_classpath());
    let uri = editor.open(&greeter());
    let (line, character) = editor.find(&uri, "return greetin");
    let labels = editor.completions_at(&uri, line, character);
    assert!(
        labels.contains(&"greeting".to_string()),
        "the local is missing: {labels:?}"
    );
    assert!(
        !labels
            .iter()
            .any(|label| label == "ArrayList" || label == "HashMap"),
        "nothing outside the open file may be offered before Phase 3: {labels:?}"
    );
}

// ---- Phase 2a: member resolution -------------------------------------------

/// Type a scratch line and wait until the version it ended at has been reparsed.
///
/// Without the wait these checks read a buffer nobody asked about. `settle`
/// re-asks until an answer is not empty and returns the first one that is not,
/// and the intermediate versions of a typed line answer plenty: the bare `\n`
/// leaves a blank line, where the position the check computed clamps to column
/// zero and completes to every name in scope. That list is not empty, so it
/// wins the poll — which is how `Greeter.;` came back as the three names
/// starting with `g` on the *previous* line, from the pre-edit view.
///
/// **Any check that types before it asks must go through here and launch
/// verbose**, because `reparsed document` is `DEBUG` and the wait is what makes
/// the position mean the buffer the check wrote down.
//
// ponytail: this belongs on `Editor` — as `type_and_settle`, or as `type_at`
// waiting on its own reparse — so that no later test can re-enter the trap by
// simply not knowing about it, and so that the identical wait spelled out in
// `a_burst_of_keystrokes_reparses_the_version_the_buffer_ended_at` has one
// home. It is here because this task's scope was one file, and moving it would
// put every editor launch at debug level to keep the log line visible, which is
// a shape change the plan declined.
//
// ponytail: waiting on the log line is not waiting on the install. `pool.rs`
// logs before it sends the analysis back, and the loop selects over the client
// and the pool with `crossbeam_channel::select!`, which picks randomly among
// ready arms — so a loop descheduled long enough for both to be ready can
// answer a request from the analysis this wait has already seen logged. It
// needs the loop off-CPU across a whole stderr-pipe → reader-thread → mpsc →
// stdin-pipe round trip, and it fails loudly rather than silently. The upgrade
// is for the harness to wait on the install: a request whose answer names the
// version, or a log line emitted where the view is replaced.
//
// ponytail: the needle is a `version=` field and not the `reparsed document`
// line that carries it, because `Logs::wait_for` takes one substring. The
// trailing space is what keeps that honest: `tracing` writes the message and
// then the fields, so `state.rs`'s discarded-parse line and `classpath.rs`'s
// `version=21.0.0` both end at the version and are excluded. Without it the 3b
// rows would match the JDK's version string on a buffer at version 2 or 21 and
// read a pre-edit view.
//
// The trailing space does *not* single out the reparse line, and it is worth
// saying so rather than being one counterexample short twice: `state.rs`'s
// `parsed document` — the `didOpen` parse — logs `recovered=` after its
// `version=`, so it matches the needle too. That is harmless only because
// `Editor::open` always sends version 0 while every `scratch` waits on a
// version of 1 or more. A new line that put a field after a `version=` at a
// version a check can reach would break this quietly; the upgrade is a
// `wait_for` taking more than one needle.
fn scratch(editor: &mut Editor, uri: &str, line: u32, character: u32, text: &str) {
    editor.type_at(uri, line, character, text);
    let last = editor.version(uri);
    assert!(
        editor
            .logs()
            .wait_for(&format!("version={last} "), Duration::from_secs(10))
            .is_some(),
        "the buffer reached version {last} and no reparse mentioned it"
    );
}

#[test]
fn completing_after_this_offers_instance_members_and_no_statics() {
    let mut editor = Editor::launch_verbose(&no_classpath());
    let uri = editor.open(&greeter());
    let (line, character) = editor.find(&uri, "String greeting = \"Grüße, \" + name + \" 🚀\";");
    scratch(&mut editor, &uri, line, character, "\n        this.;");
    // `this.` alone would find the constructor's `this.name = name;` instead,
    // which is the wrong line and not the text this check just typed.
    let (line, character) = editor.find(&uri, "this.;");
    let labels = editor.completions_at(&uri, line, character - 1);
    for member in ["name", "seen", "greet", "greetAll", "timesSeen"] {
        assert!(
            labels.contains(&member.to_string()),
            "instance member {member} is missing: {labels:?}"
        );
    }
    for static_member in ["DEFAULT", "main"] {
        assert!(
            !labels.contains(&static_member.to_string()),
            "{static_member} is static and must not be offered after `this.`: {labels:?}"
        );
    }
}

#[test]
fn completing_after_the_class_name_offers_statics_and_no_instance_members() {
    let mut editor = Editor::launch_verbose(&no_classpath());
    let uri = editor.open(&greeter());
    let (line, character) = editor.find(&uri, "String greeting = \"Grüße, \" + name + \" 🚀\";");
    scratch(&mut editor, &uri, line, character, "\n        Greeter.;");
    let (line, character) = editor.find(&uri, "Greeter.;");
    let labels = editor.completions_at(&uri, line, character - 1);
    for member in ["DEFAULT", "main", "Visit", "Listener"] {
        assert!(
            labels.contains(&member.to_string()),
            "static member {member} is missing: {labels:?}"
        );
    }
    assert!(
        !labels.contains(&"greet".to_string()),
        "instance members must not be offered: {labels:?}"
    );
}

#[test]
fn a_receiver_whose_type_is_outside_the_file_offers_nothing_rather_than_locals() {
    let mut editor = Editor::launch_verbose(&no_classpath());
    let uri = editor.open(&greeter());
    let (line, character) = editor.find(&uri, "String greeting = \"Grüße, \" + name + \" 🚀\";");
    scratch(&mut editor, &uri, line, character, "\n        seen.;");
    let (line, character) = editor.find(&uri, "seen.;");
    let labels = editor.completions_at(&uri, line, character - 1);
    assert!(
        labels.is_empty(),
        "`seen` is a java.util.Map and no classpath is installed; offering locals here \
         would be a fallback that guesses: {labels:?}"
    );
}

#[test]
fn definition_on_a_field_assignment_lands_on_the_field_not_the_parameter() {
    let mut editor = Editor::launch(&no_classpath());
    let uri = editor.open(&greeter());
    let (line, character) = editor.find(&uri, "this.nam");
    let (target, at) = editor
        .definition_at(&uri, line, character)
        .expect("the field");
    assert_eq!(target, uri, "the declaration is in this file");
    assert_eq!(
        at, 15,
        "line 16 declares the field; line 19 declares the constructor parameter"
    );
}

#[test]
fn hover_on_a_field_renders_its_declaration_source_rather_than_a_resolved_type() {
    let mut editor = Editor::launch(&no_classpath());
    let uri = editor.open(&greeter());
    let (line, character) = editor.find(&uri, "this.nam");
    let rendered = editor
        .hover_at(&uri, line, character)
        .expect("hover on the field");
    assert!(
        rendered.contains("private final String name"),
        "hover must render the declaration; rendering just `String` means hover started \
         rendering types and this check is stale: {rendered}"
    );
}

// ---- Phase 2b: `var` and the unqualified call -------------------------------

#[test]
fn a_var_local_has_its_initialisers_type_for_member_completion() {
    let mut editor = Editor::launch_verbose(&no_classpath());
    let uri = editor.open(&greeter());
    let (line, character) = in_main(&editor, &uri);
    scratch(&mut editor, &uri, line, character, "\n        greeter.;");
    let (line, character) = editor.find(&uri, "greeter.;");
    let labels = editor.completions_at(&uri, line, character - 1);
    for member in ["name", "seen", "greet", "greetAll", "timesSeen"] {
        assert!(
            labels.contains(&member.to_string()),
            "`greeter` is declared with `var`; its type is Greeter: {labels:?}"
        );
    }
}

#[test]
fn definition_through_a_var_local_reaches_the_method() {
    let mut editor = Editor::launch_verbose(&no_classpath());
    let uri = editor.open(&greeter());
    let (line, character) = editor.find(&uri, "greeter.gree");
    let (_, at) = editor
        .definition_at(&uri, line, character)
        .expect("greet()");
    assert_eq!(at, 24, "line 25 declares greet()");
}

#[test]
fn definition_on_an_unqualified_call_lands_on_the_method() {
    let mut editor = Editor::launch_verbose(&no_classpath());
    let uri = editor.open(&greeter());
    let (line, character) = editor.find(&uri, "\" (\" + timesSee");
    let (_, at) = editor
        .definition_at(&uri, line, character)
        .expect("timesSeen()");
    assert_eq!(at, 34, "line 35 declares timesSeen()");
}

#[test]
fn hover_on_an_unqualified_call_renders_its_signature() {
    let mut editor = Editor::launch_verbose(&no_classpath());
    let uri = editor.open(&greeter());
    let (line, character) = editor.find(&uri, "\" (\" + timesSee");
    let rendered = editor
        .hover_at(&uri, line, character)
        .expect("hover on the call");
    assert!(
        rendered.contains("public int timesSeen()"),
        "unexpected hover: {rendered}"
    );
}

#[test]
fn a_jdk_name_answers_nothing_at_every_position_before_the_classpath() {
    let mut editor = Editor::launch_verbose(&no_classpath());
    let uri = editor.open(&greeter());
    // Each needle stops inside the JDK name it is about, because a position one
    // past a name still resolves it: `identifier_at` retries at `offset - 1`, so
    // a needle ending at `System.out.println(greeter` would be asking about the
    // local `greeter` and would be answered.
    for needle in ["Syste", "System.ou", "System.out.printl"] {
        let (line, character) = editor.find(&uri, needle);
        assert_eq!(
            editor.definition_at(&uri, line, character),
            None,
            "{needle} comes from the JDK; with no classpath installed there is nothing to answer"
        );
    }
}

// ---- Phase 2c: overload resolution ------------------------------------------

/// 2c's scratch edit, in the doc's order — two checks turn on which declaration
/// comes first.
fn with_overloads(editor: &mut Editor, uri: &str) {
    let (line, character) = after_times_seen(editor, uri);
    scratch(
        editor,
        uri,
        line,
        character,
        concat!(
            "\n\n    public int tally;",
            "\n    public Greeter note(int count) { return this; }",
            "\n    public Visit note(Visit visit) { return visit; }",
            "\n    public int mark(String label) { return 0; }",
            "\n    public int mark(int count) { return count; }",
            "\n    public int tally(int extra) { return extra; }",
        ),
    );
    let (line, character) = in_main(editor, uri);
    scratch(
        editor,
        uri,
        line,
        character,
        concat!(
            "\n        greeter.note(1);",
            "\n        greeter.note(new Visit(\"Welt\", 1));",
            "\n        greeter.mark(1);",
            "\n        greeter.tally(1);",
            "\n        int t = greeter.tally;",
            "\n        greeter.note(1).;",
        ),
    );
}

/// The second `note` call, named by the line above it: both calls begin
/// `greeter.not`, and nothing inside the second one's own text comes before its
/// name.
const SECOND_NOTE: &str = "greeter.note(1);\n        greeter.not";

#[test]
fn an_overloaded_call_picks_the_candidate_its_argument_type_allows() {
    let mut editor = Editor::launch_verbose(&no_classpath());
    let uri = editor.open(&greeter());
    with_overloads(&mut editor, &uri);

    let (line, character) = editor.find(&uri, "greeter.not");
    let (_, int_overload) = editor
        .definition_at(&uri, line, character)
        .expect("note(int)");
    let (line, character) = editor.find(&uri, SECOND_NOTE);
    let (_, visit_overload) = editor
        .definition_at(&uri, line, character)
        .expect("note(Visit)");
    assert_ne!(
        int_overload, visit_overload,
        "both calls landed on the same declaration; before 2c both went to the first declared"
    );
    assert!(
        int_overload < visit_overload,
        "note(int) is declared first, so it is the earlier line"
    );
}

#[test]
fn an_overload_pair_a_jdk_type_cannot_separate_answers_nothing() {
    let mut editor = Editor::launch_verbose(&no_classpath());
    let uri = editor.open(&greeter());
    with_overloads(&mut editor, &uri);
    let (line, character) = editor.find(&uri, "greeter.mar");
    assert_eq!(
        editor.definition_at(&uri, line, character),
        None,
        "mark(String) cannot be ruled out without a classpath; jumping to either is a guess, \
         and before 2c this jumped to mark(String), the wrong method"
    );
}

#[test]
fn a_field_and_a_method_of_one_name_are_told_apart_by_the_namespace_asked_for() {
    let mut editor = Editor::launch_verbose(&no_classpath());
    let uri = editor.open(&greeter());
    with_overloads(&mut editor, &uri);

    let (line, character) = editor.find(&uri, "greeter.tall");
    let (_, method) = editor
        .definition_at(&uri, line, character)
        .expect("tally(int)");
    let (line, character) = editor.find(&uri, "int t = greeter.tall");
    let (_, field) = editor
        .definition_at(&uri, line, character)
        .expect("the tally field");
    assert!(
        field < method,
        "the field is declared first; before 2c both positions landed on it, because \
         declaration order decided instead of the namespace the access asks for"
    );
}

#[test]
fn hover_agrees_with_definition_at_every_overloaded_position() {
    let mut editor = Editor::launch_verbose(&no_classpath());
    let uri = editor.open(&greeter());
    with_overloads(&mut editor, &uri);

    for (needle, expected) in [
        ("greeter.not", "public Greeter note(int count)"),
        (SECOND_NOTE, "public Visit note(Visit visit)"),
        ("greeter.tall", "public int tally(int extra)"),
        ("int t = greeter.tall", "public int tally;"),
    ] {
        let (line, character) = editor.find(&uri, needle);
        let rendered = editor
            .hover_at(&uri, line, character)
            .unwrap_or_else(|| panic!("hover answered nothing at {needle}"));
        assert!(
            rendered.contains(expected),
            "at {needle}: expected {expected}, got {rendered}"
        );
    }

    let (line, character) = editor.find(&uri, "greeter.mar");
    assert_eq!(
        editor.hover_at(&uri, line, character),
        None,
        "hover must refuse exactly where definition refuses"
    );
}

#[test]
fn completion_through_an_overloaded_call_lists_one_entry_per_member() {
    let mut editor = Editor::launch_verbose(&no_classpath());
    let uri = editor.open(&greeter());
    with_overloads(&mut editor, &uri);
    let (line, character) = editor.find(&uri, "greeter.note(1).;");
    let labels = editor.completions_at(&uri, line, character - 1);
    assert_eq!(
        labels,
        vec![
            "greet",
            "greetAll",
            "mark",
            "mark",
            "name",
            "note",
            "note",
            "seen",
            "tally",
            "tally",
            "timesSeen"
        ],
        "one entry per member rather than per name, and no statics"
    );
}

// ---- Phase 3a: the classpath floor ------------------------------------------

/// Wait for the background read to finish before asking anything of it.
fn classpath_ready(editor: &mut Editor) {
    assert!(
        editor
            .logs()
            .wait_for("classpath ready", Duration::from_secs(30))
            .is_some(),
        "the classpath never arrived"
    );
}

/// The padding that makes the claim below observable.
///
/// The unpadded fixture reads in single-digit milliseconds, which is inside the
/// round trip this check measures — so `classpath ready` would already have
/// landed and the check would pass whether or not the outline had waited on it.
/// A read of roughly a second is long enough that an outline arriving first is
/// evidence rather than a coincidence.
const FILLER_TYPES_FOR_A_SLOW_READ: usize = 30_000;

#[test]
fn the_outline_comes_back_before_the_classpath_read_finishes() {
    let jdk = SyntheticJdk::build_padded("still-loading", FILLER_TYPES_FOR_A_SLOW_READ);
    let mut editor = Editor::launch(&jdk.options());
    let uri = editor.open(&greeter());
    let outline = editor.outline(&uri);
    assert!(
        outline
            .as_array()
            .is_some_and(|symbols| !symbols.is_empty()),
        "the outline answered nothing at all"
    );
    // The claim, and the reason this check is not a duplicate of the outline
    // rows: the answer was on the wire before the background read finished, so
    // it cannot have waited on it.
    assert!(
        !editor.logs().saw("classpath ready"),
        "the classpath read finished before the outline came back, so this check \
         proves nothing about whether the outline waited on it — raise \
         FILLER_TYPES_FOR_A_SLOW_READ"
    );
    editor.shutdown();
}

#[test]
fn an_overload_pair_a_jdk_type_can_separate_resolves_once_the_classpath_arrives() {
    let jdk = SyntheticJdk::build("separates-overloads");
    let mut editor = Editor::launch_verbose(&jdk.options());
    let uri = editor.open(&greeter());
    with_overloads(&mut editor, &uri);
    classpath_ready(&mut editor);

    let (line, character) = editor.find(&uri, "greeter.mar");
    let (_, at) = editor
        .definition_at(&uri, line, character)
        .expect("mark(int) — an Integer is not a String, so mark(String) is excluded");
    assert_eq!(
        at,
        line_of(&editor, &uri, "public int mark(int count)"),
        "the jump landed on mark(String), the wrong method"
    );
    editor.shutdown();
}

#[test]
fn of_two_applicable_overloads_the_most_specific_is_picked() {
    let jdk = SyntheticJdk::build("widening");
    let mut editor = Editor::launch_verbose(&jdk.options());
    let uri = editor.open(&greeter());
    let (line, character) = after_times_seen(&editor, &uri);
    scratch(
        &mut editor,
        &uri,
        line,
        character,
        concat!(
            "\n\n    public int widen(long amount) { return 1; }",
            "\n    public int widen(int amount) { return 2; }",
        ),
    );
    let (line, character) = in_main(&editor, &uri);
    scratch(
        &mut editor,
        &uri,
        line,
        character,
        "\n        greeter.widen(1);",
    );
    classpath_ready(&mut editor);

    let (line, character) = editor.find(&uri, "greeter.wide");
    let (declared, _) = editor.find(&uri, "public int widen(int");
    assert_eq!(
        editor.definition_at(&uri, line, character),
        Some((uri.clone(), declared)),
        "int widens to long, both candidates are applicable, and widen(int) is the most \
         specific (JLS 15.12.2.5)"
    );
    editor.shutdown();
}

#[test]
fn a_supertype_outside_the_file_no_longer_aborts_the_walk() {
    let jdk = SyntheticJdk::build("external-supertype");
    let mut editor = Editor::launch_verbose(&jdk.options());
    let uri = editor.open(&greeter());
    let (line, character) = after_times_seen(&editor, &uri);
    scratch(
        &mut editor,
        &uri,
        line,
        character,
        concat!(
            "\n\n    public static class Tally implements java.io.Serializable {}",
            "\n    public int accept(Tally tally) { return 1; }",
            "\n    public int accept(Visit visit) { return 2; }",
        ),
    );
    let (line, character) = in_main(&editor, &uri);
    scratch(
        &mut editor,
        &uri,
        line,
        character,
        "\n        greeter.accept(new Tally());",
    );
    classpath_ready(&mut editor);

    let (line, character) = editor.find(&uri, "greeter.accep");
    let (_, at) = editor
        .definition_at(&uri, line, character)
        .expect("accept(Tally)");
    assert_eq!(
        at,
        line_of(&editor, &uri, "public int accept(Tally tally)"),
        "under 2c this answered nothing; Tally's supertype left the file"
    );
    editor.shutdown();
}

// ---- Phase 3b: external members ---------------------------------------------

/// A `String` local and a dangling `.` on it. Not the last token in the file:
/// tree-sitter drops the enclosing class there and the answer is empty by
/// design, pinned by
/// `a_dot_as_the_last_token_in_the_file_answers_empty_rather_than_wrongly`.
fn with_a_string_receiver(editor: &mut Editor, uri: &str) {
    let (line, character) = in_main(editor, uri);
    scratch(
        editor,
        uri,
        line,
        character,
        concat!(
            "\n        var greeting = greeter.greet();",
            "\n        greeting.;",
        ),
    );
}

#[test]
fn completing_on_an_external_receiver_offers_its_members() {
    let jdk = SyntheticJdk::build("external-members");
    let mut editor = Editor::launch_verbose(&jdk.options());
    let uri = editor.open(&greeter());
    with_a_string_receiver(&mut editor, &uri);
    classpath_ready(&mut editor);

    let (line, character) = editor.find(&uri, "greeting.;");
    let labels = editor.completions_at(&uri, line, character - 1);
    assert!(
        labels.contains(&"length".to_string()),
        "this list is required to be EMPTY in the 3a row; `length` is how the two builds \
         are told apart: {labels:?}"
    );
    editor.shutdown();
}

#[test]
fn hover_on_an_external_member_renders_facts_with_no_prose() {
    let jdk = SyntheticJdk::build("external-hover");
    let mut editor = Editor::launch_verbose(&jdk.options());
    let uri = editor.open(&greeter());
    with_a_string_receiver(&mut editor, &uri);
    let (line, character) = editor.find(&uri, "greeting.");
    scratch(&mut editor, &uri, line, character, "length()");
    classpath_ready(&mut editor);

    let (line, character) = editor.find(&uri, "greeting.lengt");
    let rendered = editor
        .hover_at(&uri, line, character)
        .expect("hover on length");
    // The whole signature, not just the name: hover on a *local* renders the
    // declaration's source text, so `greeting.length();` would satisfy a check
    // for `length` alone and prove nothing about the fact-rendered path.
    assert!(
        rendered.contains("int length()"),
        "unexpected hover: {rendered}"
    );
    // `public` and `return 0` are the rest of the fixture's own `length()`
    // line: a hover that had sliced it out of `src.zip` would carry `int
    // length()` and neither `/**` nor `beginIndex`, and would pass on those two
    // alone. `ExternalMember::signature` renders the declared type, the name and
    // the parameter types and nothing else.
    assert!(
        !rendered.contains("/**")
            && !rendered.contains("beginIndex")
            && !rendered.contains("public")
            && !rendered.contains("return 0"),
        "a type fact stores no prose, no modifiers and no body, and `length` takes no parameter; \
         source text here means something started reading the zip on the request \
         path: {rendered}"
    );
    editor.shutdown();
}

#[test]
fn definition_on_an_external_member_lands_in_the_jdks_own_sources() {
    let jdk = SyntheticJdk::build("external-definition");
    let mut editor = Editor::launch_verbose(&jdk.options());
    let uri = editor.open(&greeter());
    with_a_string_receiver(&mut editor, &uri);
    let (line, character) = editor.find(&uri, "greeting.");
    scratch(&mut editor, &uri, line, character, "length()");
    classpath_ready(&mut editor);

    let (line, character) = editor.find(&uri, "greeting.lengt");
    let (target, at) = editor
        .definition_at(&uri, line, character)
        .expect("3b's third slice extracts the src.zip entry and jumps into it");
    assert!(
        target.contains("String.java") && target != uri,
        "the jump must leave the open buffer and land in the JDK's own source: {target}"
    );
    // Reaching the file is half the claim; the other half is the member's own
    // name offsets, which a jump that landed at the top of the entry would
    // satisfy just as well. The line is the fixture's, not a real JDK's.
    assert_eq!(
        at, 3,
        "`SOURCES` in the fixture declares length() on line 4 of java/lang/String.java; \
         landing elsewhere means the offsets followed were not the member's"
    );
    editor.shutdown();
}

#[test]
fn a_local_class_extending_an_external_one_answers_with_both_halves_in_one_list() {
    let jdk = SyntheticJdk::build("inheritance-merge");
    let mut editor = Editor::launch_verbose(&jdk.options());
    let uri = editor.open(&greeter());
    let (line, character) = after_times_seen(&editor, &uri);
    scratch(
        &mut editor,
        &uri,
        line,
        character,
        "\n\n    public static class Loud extends java.util.ArrayList<String> { int shouts; }",
    );
    let (line, character) = in_main(&editor, &uri);
    scratch(
        &mut editor,
        &uri,
        line,
        character,
        "\n        var loud = new Loud();\n        loud.;",
    );
    classpath_ready(&mut editor);

    let (line, character) = editor.find(&uri, "loud.;");
    let labels = editor.completions_at(&uri, line, character - 1);
    assert!(
        labels.contains(&"shouts".to_string()),
        "the file-local half was lost: {labels:?}"
    );
    // `toString` is the one that proves a *closure* rather than one hop: the
    // other three are declared on `ArrayList` itself, while `toString` comes
    // from the `java.lang.Object` nobody wrote down, two hops out.
    for inherited in ["size", "isEmpty", "add", "toString"] {
        assert!(
            labels.contains(&inherited.to_string()),
            "members_of never reached the external supertype; {inherited} is missing: {labels:?}"
        );
    }
    editor.shutdown();
}

// ---- First release, slice 1: type arguments on JDK and JAR members --------

/// A JDK member's return type is its receiver's type argument, so a chain
/// through `List<String>.get`, `Map<String, Integer>.get` and
/// `Optional<String>.get` keeps going where it used to stop at `E`.
fn with_generic_receivers(editor: &mut Editor, uri: &str) {
    let (line, character) = in_main(editor, uri);
    scratch(
        editor,
        uri,
        line,
        character,
        concat!(
            "\n        List<String> names = null;",
            "\n        names.get(0).length();",
            "\n        Map<String, Integer> counts = null;",
            "\n        counts.get(DEFAULT).intValue();",
            "\n        java.util.Optional<String> maybe = null;",
            "\n        maybe.get().length();",
            "\n        class Names extends java.util.ArrayList<String> {}",
            "\n        new Names().get(0).length();",
        ),
    );
}

#[test]
fn a_type_argument_carries_a_chain_through_a_jdk_member() {
    let jdk = SyntheticJdk::build("generic-chain");
    let mut editor = Editor::launch_verbose(&jdk.options());
    let uri = editor.open(&greeter());
    with_generic_receivers(&mut editor, &uri);
    classpath_ready(&mut editor);

    for (needle, signature) in [
        ("names.get(0).lengt", "int length()"),
        ("counts.get(DEFAULT).intVal", "int intValue()"),
        ("maybe.get().lengt", "int length()"),
        ("new Names().get(0).lengt", "int length()"),
    ] {
        let (line, character) = editor.find(&uri, needle);
        let rendered = editor
            .hover_at(&uri, line, character)
            .unwrap_or_else(|| panic!("no hover at {needle}: the chain stopped at a variable"));
        assert!(rendered.contains(signature), "{needle}: {rendered}");
    }
    let (line, character) = editor.find(&uri, "names.get(0).lengt");
    let (target, _) = editor
        .definition_at(&uri, line, character)
        .expect("a jump through List<String>.get");
    assert!(target.contains("String.java"), "{target}");
    editor.shutdown();
}

#[test]
fn completing_after_a_generic_jdk_members_call_offers_the_type_arguments_members() {
    let jdk = SyntheticJdk::build("generic-completion");
    let mut editor = Editor::launch_verbose(&jdk.options());
    let uri = editor.open(&greeter());
    let (line, character) = in_main(&editor, &uri);
    scratch(
        &mut editor,
        &uri,
        line,
        character,
        "\n        List<String> names = null;\n        names.get(0).;",
    );
    classpath_ready(&mut editor);
    let (line, character) = editor.find(&uri, "names.get(0).;");
    let labels = editor.completions_at(&uri, line, character - 1);
    assert!(
        labels.contains(&"length".to_string()) && labels.contains(&"substring".to_string()),
        "String's members, not nothing: {labels:?}"
    );
    editor.shutdown();
}

#[test]
fn a_hallucinated_method_on_a_type_argument_is_flagged() {
    let jdk = SyntheticJdk::build("generic-diagnostics");
    let root = std::env::temp_dir().join(format!(
        "javelin-generic-diagnostics-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/p");
    std::fs::create_dir_all(&package).expect("package");
    let reception = package.join("Reception.java");
    std::fs::write(
        &reception,
        "package p;\n\nimport java.util.List;\n\npublic class Reception {\n    \
         int measure(List<String> names, List raw) {\n        \
         return names.get(0).lenth() + names.get(0).length() + raw.get(0).lenth();\n    \
         }\n}\n",
    )
    .expect("Reception");

    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let uri = editor.open(&reception);
    let flagged = editor.diagnostics_until(&uri, |found| !found.is_empty());
    assert_eq!(
        flagged.len(),
        1,
        "the typed call only — a raw list's element is unknown, never Object: {flagged:?}"
    );
    let (line, character) = editor.find(&uri, "return names.get(0).");
    assert_eq!(flagged[0]["range"]["start"]["line"], line);
    assert_eq!(flagged[0]["range"]["start"]["character"], character);
    assert_eq!(flagged[0]["code"], "unresolved-method");
    assert!(
        flagged[0]["message"]
            .as_str()
            .is_some_and(|message| message.contains("java.lang.String")),
        "{flagged:?}"
    );
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

// ---- First release, slice 2: implicit lambda parameters -------------------

#[test]
fn an_implicit_lambda_parameter_takes_its_type_from_the_functional_interface() {
    let jdk = SyntheticJdk::build("lambda-parameters");
    let mut editor = Editor::launch_verbose(&jdk.options());
    let uri = editor.open(&greeter());
    let (line, character) = in_main(&editor, &uri);
    scratch(
        &mut editor,
        &uri,
        line,
        character,
        concat!(
            "\n        List<String> names = null;",
            "\n        names.forEach(x -> x.length());",
            "\n        names.stream().filter(y -> y.length() > 0).map(z -> z.substring(0, 1));",
            "\n        List<Greeter> all = null;",
            "\n        all.forEach(g -> g.greet());",
        ),
    );
    classpath_ready(&mut editor);

    for (needle, signature) in [
        ("forEach(x -> x.lengt", "int length()"),
        ("filter(y -> y.lengt", "int length()"),
        (
            "map(z -> z.substr",
            "String substring(int beginIndex, int endIndex)",
        ),
    ] {
        let (line, character) = editor.find(&uri, needle);
        let rendered = editor
            .hover_at(&uri, line, character)
            .unwrap_or_else(|| panic!("no hover at {needle}: the lambda parameter is untyped"));
        assert!(rendered.contains(signature), "{needle}: {rendered}");
    }
    let (line, character) = editor.find(&uri, "forEach(x -> x.lengt");
    let (target, _) = editor
        .definition_at(&uri, line, character)
        .expect("a jump through the lambda parameter");
    assert!(target.contains("String.java"), "{target}");

    // A workspace member called through a lambda parameter is a reference to
    // it, by definition's own rule.
    let (line, character) = editor.find(&uri, "g -> g.gree");
    let (_, at) = editor
        .definition_at(&uri, line, character)
        .expect("a jump to greet through the lambda parameter");
    assert_eq!(at, line_of(&editor, &uri, "public String greet()"));
    let (line, character) = editor.find(&uri, "public String gree");
    let references = editor.references_at(&uri, line, character, false);
    let (call, _) = editor.find(&uri, "g -> g.gree");
    assert!(
        references.contains(&(uri.clone(), call)),
        "the call through the lambda is missing: {references:?}"
    );
    editor.shutdown();
}

#[test]
fn a_dot_on_a_string_literal_offers_strings_members() {
    let jdk = SyntheticJdk::build("string-literal");
    let mut editor = Editor::launch_verbose(&jdk.options());
    let uri = editor.open(&greeter());
    let (line, character) = in_main(&editor, &uri);
    scratch(&mut editor, &uri, line, character, "\n        \"x\".;");
    classpath_ready(&mut editor);

    let (line, character) = editor.find(&uri, "\"x\".;");
    let labels = editor.completions_at(&uri, line, character - 1);
    assert!(
        labels.iter().any(|label| label == "length"),
        "a literal is a java.lang.String: {labels:?}"
    );
    editor.shutdown();
}

// ---- The numbers the results table records ----------------------------------

/// The edit both measuring tests type: a statement, at a position `in_main`
/// already names, in a method where `greeter` is in scope.
///
/// Twenty-five characters, which is twenty-five samples — the same order as
/// the 8-to-81 a human tester left in the manual pass's own table.
const MEASURED_EDIT: &str = "\n        greeter.greet();";

/// `scratch`, one character at a time, waiting for each keystroke's reparse.
///
/// `scratch` sends the whole block as one burst and waits once, which is the
/// fast-typist shape the supersede path exists for — and it leaves exactly
/// *one* reparse behind for the block, because `did_change` abandons the
/// pending job on every keystroke and jobs 1..n-1 die at `job.token.check()`
/// before a worker parses them. A median and a p95 over that is a median and
/// a p95 of one number. Latency per keystroke is what the results table
/// records, so this waits between them, as the human tester's hands did.
fn scratch_one_character_at_a_time(
    editor: &mut Editor,
    uri: &str,
    line: u32,
    character: u32,
    text: &str,
) {
    let (mut line, mut character) = (line, character);
    for ch in text.chars() {
        scratch(editor, uri, line, character, &ch.to_string());
        // The protocol counts UTF-16 units; every character typed here is
        // ASCII, and `len_utf16` says so rather than assuming it.
        if ch == '\n' {
            line += 1;
            character = 0;
        } else {
            character += ch.len_utf16() as u32;
        }
    }
}

/// The digits of a `tracing` field, wherever in the line the field was written.
fn number_after(line: &str, key: &str) -> Option<u64> {
    let at = line.find(key)? + key.len();
    line[at..]
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// Every `duration_us=N` the server logged on a line containing `needle`.
fn microseconds(editor: &mut Editor, needle: &str) -> Vec<u64> {
    editor
        .logs()
        .texts()
        .iter()
        .filter(|line| line.contains(needle))
        .filter_map(|line| number_after(line, "duration_us="))
        .collect()
}

/// A duration as an upper bound in whole units, deliberately coarse.
///
/// The recorded file is meant to be dirty only when the numbers *moved*, and
/// timings never repeat: across the runs that produced this file the reparse
/// median ranged 350–512 µs, a spread of 1.46×, so nothing finer than a bucket
/// that wide holds still. Significant figures do not work here — 1 s.f. puts
/// those two in different buckets. A whole unit does, and it puts every
/// boundary far from anything measured: the sub-millisecond figures sit 30%
/// below `1 ms` and the classpath read 20% below `20 s`.
///
/// The unit is the largest that still leaves a small whole number, so the
/// bucket is always at least 2× wide.
fn at_most(micros: u64) -> String {
    for (unit, name) in [(1u64, "µs"), (1_000, "ms"), (1_000_000, "s")] {
        let whole = micros.div_ceil(unit);
        if whole <= 10 {
            return format!("< {whole} {name}");
        }
    }
    format!("< {} s", micros.div_ceil(10_000_000) * 10)
}

/// The `didOpen` parse. `"parsed document"` is a substring of
/// `"reparsed document"`, so this takes the first match rather than the only
/// one — safe because `did_open` logs before any edit can be sent.
fn first_parse(editor: &mut Editor) -> u64 {
    *microseconds(editor, "parsed document")
        .first()
        .expect("didOpen logs a parse")
}

/// Every keystroke's reparse, sorted, with the count the typing owes.
fn reparses(editor: &mut Editor) -> Vec<u64> {
    let mut samples = microseconds(editor, "reparsed document");
    assert_eq!(
        samples.len(),
        MEASURED_EDIT.chars().count(),
        "a waited-for keystroke owes exactly one reparse, so a shortfall means the typing \
         went back to a burst — and a median over the one sample a burst leaves is not a \
         median"
    );
    samples.sort_unstable();
    samples
}

fn percentile(sorted: &[u64], percent: usize) -> u64 {
    sorted[sorted.len() * percent / 100]
}

#[test]
fn the_first_parse_and_every_reparse_stay_inside_their_budgets() {
    let mut editor = Editor::launch_verbose(&no_classpath());
    let uri = editor.open(&greeter());
    editor
        .logs()
        .wait_for("parsed document", Duration::from_secs(5));
    let (line, character) = in_main(&editor, &uri);
    scratch_one_character_at_a_time(&mut editor, &uri, line, character, MEASURED_EDIT);

    let first = first_parse(&mut editor);
    assert!(
        first < 10_000,
        "first parse took {first} µs against a 10 ms budget; the demo file usually parses \
         in about one millisecond, so this is a real regression rather than a slow machine"
    );

    let samples = reparses(&mut editor);
    let p95 = percentile(&samples, 95);
    assert!(
        p95 < 10_000,
        "reparse p95 is {p95} µs over {} samples; incremental reparsing is decoupled from \
         file size and the demo file is small",
        samples.len()
    );
    editor.shutdown();
}

#[test]
#[ignore = "reads the machine's own JDK, which costs seconds and depends on what is \
            installed; scripts/check.sh runs it with --ignored"]
fn the_measured_numbers_are_recorded() {
    let mut editor = Editor::launch_verbose(&no_classpath());
    let uri = editor.open(&greeter());
    editor
        .logs()
        .wait_for("parsed document", Duration::from_secs(5));
    let (line, character) = in_main(&editor, &uri);
    scratch_one_character_at_a_time(&mut editor, &uri, line, character, MEASURED_EDIT);

    let first = first_parse(&mut editor);
    let samples = reparses(&mut editor);
    let median = percentile(&samples, 50);
    let p95 = percentile(&samples, 95);
    editor.shutdown();

    // The classpath numbers come from the real JDK below, because the
    // synthetic seven-entry archive reads in milliseconds and measures nothing.
    let classpath = real_jdk_timings()
        .unwrap_or_else(|| "not measured — no JDK with `lib/src.zip` on this machine".to_owned());

    write_results(&format!(
        "| Measurement | Value | Budget |\n\
         |---|---|---|\n\
         | First parse (`Greeter.java`) | {} | 10 ms |\n\
         | Reparse, median | {} | — |\n\
         | Reparse, p95 ({} samples) | {} | 10 ms |\n\
         | `classpath ready` | {classpath} | none; nothing waits on it |\n",
        at_most(first),
        at_most(median),
        samples.len(),
        at_most(p95),
    ));
}

/// Rewrite `docs/editor-pass-results.md`, whole.
///
/// Only when asked: an ordinary `cargo test` must not dirty the working tree,
/// so `scripts/check.sh` — the gate — is what sets `JAVELIN_RECORD`. A dirty
/// doc after a gate run is then a signal that the numbers moved, not noise.
///
/// Whole rather than spliced into a marked block, and the file says so: a
/// marker pair nothing parses is a promise to the next person that their prose
/// will survive, and `fs::write` does not keep it.
fn write_results(table: &str) {
    let body = format!(
        "# Editor pass results\n\n\
         Generated by `the_measured_numbers_are_recorded` in \
         `crates/cli/tests/editor_pass.rs`. **The whole file is rewritten** every time \
         `scripts/check.sh` runs, so nothing added here by hand survives; a plain \
         `cargo test` leaves the file alone. Why each measurement exists is in \
         [`manual-testing-zed.md`](manual-testing-zed.md).\n\n\
         Measured against a **{profile}** build, because that is what `cargo test` \
         builds. The classpath figures below are therefore not comparable with the ones \
         in [`performance.md`](performance.md), which are release numbers.\n\n\
         The reparse samples are the **always-incremental** shape: each keystroke waits \
         for its own reparse, which is what makes them countable at all, and that \
         excludes by construction the path where an edit arrives before the previous \
         reparse lands and `JavaParser::reparse` falls back to a full parse. The p95 \
         below is the optimistic branch.\n\n\
         Every duration is an **upper bound in whole units**, not a measurement — \
         `< 1 ms` means the run came in under a millisecond and nothing finer. \
         [`performance.md`](performance.md) is where precise figures live. These are \
         rounded because this file is regenerated by the gate and committed: timings \
         never repeat, so a precise one would report \"the numbers moved\" on every \
         single run and teach its reader to ignore the one time they did. The sample \
         count is exact — it is a count, and its being visible is the point.\n\n\
         The cold classpath read carries no duration at all. It is seconds long, so a \
         bound loose enough not to flap on a loaded machine would say nothing, and it \
         is the one figure here that nothing waits on. The **release** figure for it \
         is in [`performance.md`](performance.md) — a different measurement from the \
         debug one withheld here, not the same number written down elsewhere. What \
         this row claims is that the warm read comes from the cache instead, which is \
         the whole point of having one.\n\n{table}",
        profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    let target = if std::env::var_os("JAVELIN_RECORD").is_some() {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/editor-pass-results.md")
    } else {
        std::env::temp_dir().join("javelin-editor-pass-results.md")
    };
    std::fs::write(&target, &body)
        .unwrap_or_else(|err| panic!("write {}: {err}", target.display()));
}

/// The cold and warm classpath reads, against the machine's real JDK.
///
/// `None` means one thing only — this machine has no JDK with sources, which
/// must not fail a gate that has to run everywhere. Once the probe says
/// otherwise, a read that does not arrive is a failure and says so: reporting
/// "no JDK on this machine" for a timeout would put a false statement in a
/// document, which is the defect this whole file exists to keep out of the
/// numbers.
fn real_jdk_timings() -> Option<String> {
    if !a_jdk_with_sources_is_installed() {
        return None;
    }
    let (options, cache) = harness::own_cache("real-jdk-timings");
    let _ = std::fs::remove_dir_all(&cache);
    let cold = classpath_ready_line(&options);
    let warm = classpath_ready_line(&options);
    let _ = std::fs::remove_dir_all(&cache);
    assert!(
        cold.contains("source=\"src.zip\""),
        "the cold read was served from a cache this test had just removed, so it is not a \
         cold read: {cold}"
    );
    assert!(
        warm.contains("source=\"cache\""),
        "the second read did not come from the cache, so the cache is not earning its \
         place: {warm}"
    );
    // Rebuilt from the line's fields rather than quoted whole: a raw
    // `duration_ms=16287` is the precision this file is committed without.
    //
    // The cold read's duration is dropped rather than rounded. Whole seconds put
    // its bound within ordinary machine load of the real value, so a committed
    // file flapped between `< 20 s` and `< 30 s` between gate runs on one
    // machine — and the next unit up is a bound too loose to mean anything. The
    // warm one is three orders of magnitude inside its own bound, and it is the
    // figure that shows the cache earning its place.
    let duration = |line: &str| {
        at_most(number_after(line, "duration_ms=").expect("the line carries a duration") * 1_000)
    };
    let facts = number_after(&cold, "facts=").expect("the line carries a fact count");
    Some(format!(
        "cold from `src.zip`, warm {} from the cache, {facts} facts",
        duration(&warm)
    ))
}

/// Whether discovery would find a JDK whose sources there is anything to read.
///
/// Asked of `javelin doctor` — the same discovery and the same cache path the
/// background job uses — and asked *before* the reads rather than inferred
/// from them. A JDK without sources publishes no classpath at all, so
/// `classpath ready` is never logged and a machine without one would spend the
/// whole deadline discovering that it has nothing to measure.
fn a_jdk_with_sources_is_installed() -> bool {
    let doctor = std::process::Command::new(env!("CARGO_BIN_EXE_javelin"))
        .arg("doctor")
        .output()
        .expect("run javelin doctor");
    String::from_utf8_lossy(&doctor.stdout).lines().any(|line| {
        // The sentinel `main.rs` prints, matched whole: the rest of this line
        // is a path, and a JDK installed somewhere under a directory called
        // `none` would fail a substring test and skip a read it could do.
        line.starts_with("jdk sources:") && !line.ends_with("none (no lib/src.zip)")
    })
}

/// One session's `classpath ready` line, from `classpath ready` onwards — the
/// timestamp and module path in front of it are noise in a results table.
///
/// Panics rather than answering `None`: every caller has already established
/// that a JDK with sources is installed, so a read that never lands is a
/// broken server and not a machine without a JDK.
fn classpath_ready_line(options: &serde_json::Value) -> String {
    let mut editor = Editor::launch(options);
    editor
        .logs()
        .wait_for("classpath ready", Duration::from_secs(120))
        .expect("discovery found a JDK with sources, so a classpath must arrive");
    let line = editor
        .logs()
        .texts()
        .into_iter()
        .find_map(|line| line.find("classpath ready").map(|at| line[at..].to_owned()))
        .expect("the line `wait_for` just returned is still in `seen`");
    editor.shutdown();
    line
}

// ---- Phase 3b: the workspace source index, read --------------------------

/// Every symbol the picker was offered, as `name` and its container.
fn picked(symbols: &serde_json::Value) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = symbols
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|symbol| {
            (
                symbol["name"].as_str().unwrap_or_default().to_owned(),
                symbol["containerName"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

#[test]
fn a_symbol_picker_finds_a_type_in_a_file_the_editor_never_opened() {
    let jdk = SyntheticJdk::build("workspace-symbols");
    // No file is opened at all: the whole point is that the answer comes from
    // the walk rather than from a buffer.
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &demo_project());

    let found = picked(&editor.workspace_symbols("greet"));
    assert_eq!(
        found,
        vec![
            ("Greeter".to_owned(), "com.example".to_owned()),
            ("greet".to_owned(), "com.example.Greeter".to_owned()),
            ("greetAll".to_owned(), "com.example.Greeter".to_owned()),
            ("greeter".to_owned(), "com.example.Reception".to_owned()),
            (
                "onGreet".to_owned(),
                "com.example.Greeter.Listener".to_owned()
            ),
        ],
        "the demo project declares one type and four members whose names \
         contain `greet` — `Reception.greeter` is private, and listed anyway — \
         and the picker must find them without the file being open, each member \
         under the type that declares it"
    );

    let location = editor.workspace_symbols("Greeter")[0]["location"].clone();
    assert!(
        location["uri"]
            .as_str()
            .expect("a symbol names a file")
            .ends_with("/com/example/Greeter.java"),
        "the location must name the file on disk, not the workspace root: {location}"
    );
    assert_eq!(
        location["range"]["start"]["line"],
        serde_json::json!(line_of_declaration_in_greeter("public class Greeter")),
        "the range must be the type's own name, on its own line"
    );
    editor.shutdown();
}

/// The line `needle` starts on in the demo project's `Greeter.java`, read off
/// disk — the editor has not opened it, so it has no mirror to search.
fn line_of_declaration_in_greeter(needle: &str) -> u32 {
    let text = std::fs::read_to_string(greeter()).expect("the demo project is checked in");
    text.lines()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("{needle} is not in Greeter.java")) as u32
}

/// The flip of the check that used to pin this ceiling: a type typed into a
/// buffer and never saved is offered, because the index re-reads an open
/// buffer's text after each pause in typing — and closing the buffer puts the
/// file back to what is on disk.
#[test]
fn a_type_declared_only_in_an_unsaved_buffer_is_offered_until_the_buffer_closes() {
    let jdk = SyntheticJdk::build("unsaved-symbols");
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &demo_project());
    let uri = editor.open(&greeter());
    // Wait for the walk before typing, so the assertion below is about the
    // buffer reaching the index rather than about the walk arriving.
    assert!(!picked(&editor.workspace_symbols("Greeter")).is_empty());

    let (line, character) = after_times_seen(&editor, &uri);
    scratch(&mut editor, &uri, line, character, "\n}\nclass Unsaved {");

    assert_eq!(
        picked(&editor.workspace_symbols("Unsaved")),
        vec![("Unsaved".to_owned(), "com.example".to_owned())],
        "an unsaved type is found once the buffer has been re-indexed"
    );

    editor.close(&uri);
    let after = editor.workspace_symbols_until("Unsaved", <[_]>::is_empty);
    assert_eq!(
        after,
        serde_json::json!([]),
        "unsaved text must not outlive its buffer"
    );
    editor.shutdown();
}

/// A change the editor never saw — a `git checkout`, a code generator — as
/// the client's watcher reports it: a file created is found, and deleted is
/// gone.
#[test]
fn a_file_created_and_deleted_on_disk_follows_the_watcher() {
    let project = harness::editor::scratch_project("watched");
    let jdk = SyntheticJdk::build("watched");
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &project);
    assert!(!picked(&editor.workspace_symbols("Greeter")).is_empty());

    let created = project.join("src/main/java/com/example/Arrived.java");
    std::fs::write(&created, "package com.example;\npublic class Arrived {}\n").expect("write");
    editor.files_changed(&[(&created, 1)]);
    assert_eq!(
        picked(&editor.workspace_symbols("Arrived")),
        vec![("Arrived".to_owned(), "com.example".to_owned())]
    );

    std::fs::remove_file(&created).expect("remove");
    editor.files_changed(&[(&created, 3)]);
    assert_eq!(
        editor.workspace_symbols_until("Arrived", <[_]>::is_empty),
        serde_json::json!([])
    );
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&project);
}

/// The staleness the version 1 decision promoted to a defect: a declaration
/// moved down its file, on disk, is jumped to at the line it is on now.
#[test]
fn a_declaration_moved_on_disk_is_jumped_to_at_its_new_line() {
    let project = harness::editor::scratch_project("moved");
    let jdk = SyntheticJdk::build("moved");
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &project);
    let greeter = project.join("src/main/java/com/example/Greeter.java");
    let reception = project.join("src/main/java/com/example/Reception.java");
    let uri = editor.open(&reception);
    assert!(!picked(&editor.workspace_symbols("Reception")).is_empty());

    let (line, character) = editor.find(&uri, "private final Greet");
    let (_, before) = editor
        .definition_at(&uri, line, character)
        .expect("the declaration, where the walk saw it");

    let text = std::fs::read_to_string(&greeter).expect("read");
    let moved = text.replacen("public class Greeter", "\n\n\npublic class Greeter", 1);
    std::fs::write(&greeter, moved).expect("write");
    editor.files_changed(&[(&greeter, 2)]);

    let settled = editor.server().eventually(
        "textDocument/definition",
        &serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }),
        |answer| answer["result"]["range"]["start"]["line"].as_u64() == Some(u64::from(before) + 3),
    );
    assert_eq!(
        settled["result"]["range"]["start"]["line"].as_u64(),
        Some(u64::from(before) + 3),
        "the jump follows the declaration to its new line"
    );
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&project);
}

// ---- Phase 3b: a name that means the file next door -----------------------

/// The first answer in the project's life that resolves a *name* across files.
///
/// `Greeter` in `Reception.java` is declared nowhere in that buffer and
/// nowhere in the JDK. The workspace source index is what turns it into
/// `com.example.Greeter`, through the file's own package, and what says where
/// that type is written — so the jump lands in a file the editor never
/// opened.
#[test]
fn a_name_that_means_the_file_next_door_jumps_into_it_and_hovers_as_it() {
    let jdk = SyntheticJdk::build("cross-file-name");
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &demo_project());
    let uri = editor.open(&reception());
    // Wait for the walk before asking: until it lands the name means nothing,
    // which is a different answer from the one under test.
    assert!(!picked(&editor.workspace_symbols("Reception")).is_empty());

    // Inside the identifier rather than at its start: the cursor is usually
    // mid-name when a reader ctrl-clicks it.
    let (line, character) = editor.find(&uri, "private final Greet");

    let (target, at) = editor
        .definition_at(&uri, line, character)
        .expect("the declaration in the file next door");
    assert!(
        target.ends_with("/com/example/Greeter.java"),
        "the jump must name the declaring file, not the buffer that asked: {target}"
    );
    assert_eq!(
        at,
        line_of_declaration_in_greeter("public class Greeter"),
        "and land on the type's own name"
    );

    let rendered = editor.hover_at(&uri, line, character).unwrap_or_default();
    assert!(
        rendered.starts_with("```java\nclass com.example.Greeter\n```\n\nThe file to poke at"),
        "hover answers the same position with what the index can say, and the type's \
         Javadoc read from its file: {rendered}"
    );
    editor.shutdown();
}

/// Ctrl-click on a JDK type's own name lands on that name in the extracted
/// `String.java`, and hover says what it is. The fact records the name's span
/// since schema version 17.
#[test]
fn an_external_type_name_jumps_to_its_own_name_in_the_jdks_sources() {
    let jdk = SyntheticJdk::build("external-type-name");
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &demo_project());
    let uri = editor.open(&reception());
    assert!(!picked(&editor.workspace_symbols("Reception")).is_empty());
    classpath_ready(&mut editor);

    let (line, character) = editor.find(&uri, "public Strin");
    let (target, at) = editor
        .definition_at(&uri, line, character)
        .expect("the type's own name in the extracted entry");
    assert!(
        target.contains("String.java") && target != uri,
        "the jump leaves the buffer for the JDK's own source: {target}"
    );
    assert_eq!(
        at, 2,
        "`SOURCES` in the fixture declares `class String` on line 3 of java/lang/String.java"
    );
    assert_eq!(
        editor.hover_at(&uri, line, character).as_deref(),
        Some("```java\nclass java.lang.String\n```")
    );
    editor.shutdown();
}

/// The eighth slice's whole point, over real pipes: `greeter.greet()` in
/// `Reception.java` is a member of a type declared in `Greeter.java`, which
/// the editor never opens. Ctrl-click lands on `greet`'s own name, hover
/// renders its signature from the index, and a `.` offers the type's members.
#[test]
fn a_cross_file_member_completes_hovers_and_jumps() {
    let jdk = SyntheticJdk::build("cross-file-member");
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &demo_project());
    let uri = editor.open(&reception());
    assert!(!picked(&editor.workspace_symbols("Reception")).is_empty());

    let (line, character) = editor.find(&uri, "return greeter.gree");
    let (target, at) = editor
        .definition_at(&uri, line, character)
        .expect("Greeter.greet, in the file next door");
    assert!(
        target.ends_with("/com/example/Greeter.java"),
        "the jump names the declaring file: {target}"
    );
    assert_eq!(at, line_of_declaration_in_greeter("public String greet()"));

    assert_eq!(
        editor.hover_at(&uri, line, character).as_deref(),
        Some(
            "```java\nString greet()\n```\n\n\
             Non-ASCII on purpose: 🚀 is two UTF-16 code units, ä is one."
        ),
        "rendered from the index, like a JDK member from facts, with its Javadoc"
    );

    let (line, character) = editor.find(&uri, "return greeter.");
    let labels = editor.completions_at(&uri, line, character);
    for expected in ["greet", "greetAll", "timesSeen"] {
        assert!(
            labels.contains(&expected.to_owned()),
            "{expected} is a member of Greeter: {labels:?}"
        );
    }
    editor.shutdown();
}

/// The reviewer's most-used verb, across a file boundary: who uses this
/// type? `Reception.java` is never opened — the workspace walk recorded its
/// two uses of `Greeter`, and the query resolved each against that file's own
/// package when it was asked.
#[test]
fn references_to_a_type_find_a_file_the_editor_never_opened() {
    let jdk = SyntheticJdk::build("references");
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &demo_project());
    let uri = editor.open(&greeter());
    // Wait for the walk: until it lands the open buffer is the whole answer,
    // which is a different answer from the one under test.
    assert!(!picked(&editor.workspace_symbols("Reception")).is_empty());

    let declaration = line_of_declaration_in_greeter("public class Greeter");
    let (line, character) = editor.find(&uri, "public class Greet");
    let found = editor.references_at(&uri, line, character, true);

    let in_reception: Vec<u32> = found
        .iter()
        .filter(|(target, _)| target.ends_with("/com/example/Reception.java"))
        .map(|(_, at)| *at)
        .collect();
    assert_eq!(
        in_reception,
        vec![12, 14],
        "the field's type and the constructor parameter's, in a file never opened: {found:?}"
    );
    assert!(
        found
            .iter()
            .any(|(target, at)| target.ends_with("/com/example/Greeter.java") && *at == declaration),
        "the declaration itself, asked for with includeDeclaration: {found:?}"
    );
    assert!(
        !found
            .iter()
            .any(|(target, _)| target.ends_with("/com/example/Broken.java")),
        "Broken.java mentions Greeter only in a comment: {found:?}"
    );
    editor.shutdown();
}

/// A reviewer cannot tell "no other file calls this" from "not resolved yet",
/// so the background work is shown as work-done progress, which Zed puts
/// beside the server's name: each task's token is created before it is used,
/// and the walk's task ends.
#[test]
fn background_work_is_shown_as_progress_the_editor_can_display() {
    let jdk = SyntheticJdk::build("progress");
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &demo_project());
    let uri = editor.open(&greeter());
    let (line, character) = editor.find(&uri, "public String gree");
    editor.references_until(&uri, line, character, |found| {
        found
            .iter()
            .any(|(target, _)| target.ends_with("/com/example/Reception.java"))
    });

    let messages = editor.server().notifications().to_vec();
    let at = |kind: &str, token: &serde_json::Value| {
        messages.iter().position(|message| {
            message["method"] == "$/progress"
                && message["params"]["token"] == *token
                && message["params"]["value"]["kind"] == kind
        })
    };
    let begin = messages
        .iter()
        .find(|message| {
            message["method"] == "$/progress"
                && message["params"]["value"]["title"] == "Indexing the workspace"
        })
        .unwrap_or_else(|| panic!("no progress for the walk: {messages:?}"));
    let token = &begin["params"]["token"];
    let created = messages.iter().position(|message| {
        message["method"] == "window/workDoneProgress/create"
            && message["params"]["token"] == *token
    });
    assert!(
        created.is_some() && created < at("begin", token),
        "the token is created before it is begun: {messages:?}"
    );
    assert!(
        at("end", token) > at("begin", token),
        "the walk's task ends: {messages:?}"
    );
    editor.shutdown();
}

/// The ninth slice's point: who calls this method from another file? The
/// workspace worker resolved `greeter.greet()` in `Reception.java` — a file
/// the editor never opens — against `Greeter.java`'s members and recorded it;
/// the declaring file's own call in `main` is answered from the buffer.
#[test]
fn references_to_a_method_find_a_call_in_a_file_the_editor_never_opened() {
    let jdk = SyntheticJdk::build("member-references");
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &demo_project());
    let uri = editor.open(&greeter());

    let declaration = line_of_declaration_in_greeter("public String greet()");
    let (line, character) = editor.find(&uri, "public String gree");
    // Member references land in the walk's second publish, after every file
    // is resolved, so wait for the cross-file half rather than any answer.
    let found = editor.references_until(&uri, line, character, |found| {
        found
            .iter()
            .any(|(target, _)| target.ends_with("/com/example/Reception.java"))
    });

    let in_reception: Vec<u32> = found
        .iter()
        .filter(|(target, _)| target.ends_with("/com/example/Reception.java"))
        .map(|(_, at)| *at)
        .collect();
    assert_eq!(
        in_reception,
        vec![19],
        "`greeter.greet()` in `welcome`, in a file never opened: {found:?}"
    );
    let in_greeter: Vec<u32> = found
        .iter()
        .filter(|(target, _)| target.ends_with("/com/example/Greeter.java"))
        .map(|(_, at)| *at)
        .collect();
    assert_eq!(
        in_greeter,
        vec![
            declaration,
            line_of_declaration_in_greeter("System.out.println(greeter.greet())")
        ],
        "the declaration and `main`'s call, each once — live, not also from the index: {found:?}"
    );
    editor.shutdown();
}

/// Phase 4's point, over real pipes: a type a dependency JAR declares
/// completes and hovers. The project's `mvnw` is a fake that names the
/// committed `acme.jar`, so the check runs the real process path — spawn,
/// read, cache — without the network.
#[cfg(unix)]
#[test]
fn a_type_from_a_dependency_jar_completes_and_hovers() {
    use std::os::unix::fs::PermissionsExt as _;
    let jdk = SyntheticJdk::build("dependency-jar");
    let root = std::env::temp_dir().join(format!("javelin-dependency-jar-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/main/java/com/example");
    std::fs::create_dir_all(&package).expect("package");
    std::fs::write(root.join("pom.xml"), "<project/>").expect("pom");
    let jar = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/classfiles/acme.jar")
        .canonicalize()
        .expect("the fixture JAR is checked in");
    let mvnw = root.join("mvnw");
    std::fs::write(
        &mvnw,
        format!(
            "#!/bin/sh\necho '[INFO] Dependencies classpath:'\necho '{}'\n",
            jar.display()
        ),
    )
    .expect("mvnw");
    std::fs::set_permissions(&mvnw, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let source = package.join("UsesBox.java");
    std::fs::write(
        &source,
        "package com.example;\n\nimport com.acme.Box;\n\npublic class UsesBox {\n    \
         Object label(Box<String> box) {\n        return box.names();\n    }\n}\n",
    )
    .expect("source");

    let mut editor = Editor::launch_verbose_in(&jdk.options_with_dependencies(), &root);
    let uri = editor.open(&source);
    let (line, character) = editor.find(&uri, "return box.");
    let labels = editor.completions_at(&uri, line, character);
    for expected in ["get", "names", "put", "map"] {
        assert!(
            labels.contains(&expected.to_owned()),
            "{expected}: {labels:?}"
        );
    }
    for private in ["secret", "hidden"] {
        assert!(
            !labels.contains(&private.to_owned()),
            "{private} is private: {labels:?}"
        );
    }
    let (line, character) = editor.find(&uri, "return box.nam");
    let hover = editor.hover_at(&uri, line, character).expect("a hover");
    assert!(hover.contains("names()"), "{hover}");
    assert_eq!(
        editor.definition_at(&uri, line, character),
        None,
        "a class file has no text to jump into"
    );
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// Tier 2's first slice: a JAR the workspace's own build wrote — Gradle hands
/// one module's `build/libs` JAR to the next — is that module's source
/// compiled, so a type the workspace declares and only that JAR also holds
/// keeps a whole member list and a hallucinated call on it is flagged. The
/// same JAR anywhere else is a rival copy, and the call stays silent.
#[cfg(unix)]
#[test]
fn a_type_only_the_workspaces_own_build_output_also_holds_is_judged() {
    use std::os::unix::fs::PermissionsExt as _;
    let judged = |name: &str, jar_at: &str| -> Vec<String> {
        let jdk = SyntheticJdk::build(name);
        let root = std::env::temp_dir().join(format!("javelin-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let shape = root.join("core/src/main/java/com/acme");
        let package = root.join("app/src/main/java/com/example");
        std::fs::create_dir_all(&shape).expect("core");
        std::fs::create_dir_all(&package).expect("app");
        std::fs::write(root.join("pom.xml"), "<project/>").expect("pom");
        std::fs::write(
            shape.join("Shape.java"),
            "package com.acme;\n\npublic interface Shape {\n    int SIDES = 0;\n\n    \
             double area();\n\n    static Shape unit() { return null; }\n\n    \
             default String describe() { return null; }\n}\n",
        )
        .expect("Shape");
        let jar = root.join(jar_at);
        std::fs::create_dir_all(jar.parent().expect("parent")).expect("jar dir");
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/classfiles/acme.jar"),
            &jar,
        )
        .expect("the fixture JAR is checked in");
        let mvnw = root.join("mvnw");
        std::fs::write(
            &mvnw,
            format!(
                "#!/bin/sh\necho '[INFO] Dependencies classpath:'\necho '{}'\n",
                jar.canonicalize().expect("jar").display()
            ),
        )
        .expect("mvnw");
        std::fs::set_permissions(&mvnw, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        std::fs::write(
            package.join("Plain.java"),
            "package com.example;\n\npublic class Plain {\n    int count() { return 0; }\n}\n",
        )
        .expect("Plain");
        let source = package.join("Uses.java");
        std::fs::write(
            &source,
            "package com.example;\n\nimport com.acme.Shape;\n\npublic class Uses {\n    \
             Shape shape;\n    Plain plain;\n\n    double measure() {\n        \
             return plain.cont() + shape.ares();\n    }\n}\n",
        )
        .expect("source");
        let mut editor = Editor::launch_verbose_in(&jdk.options_with_dependencies(), &root);
        let uri = editor.open(&source);
        // `plain.cont()` is flagged whenever the member half is judged at all.
        let flagged = editor.diagnostics_until(&uri, |found| {
            found.iter().any(|found| {
                found["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("cont"))
            })
        });
        editor.shutdown();
        let _ = std::fs::remove_dir_all(&root);
        flagged
            .iter()
            .filter_map(|found| found["message"].as_str().map(str::to_owned))
            .collect()
    };
    let own = judged("own-output", "core/build/libs/core.jar");
    assert!(
        own.iter().any(|message| message.contains("ares")),
        "{own:?}"
    );
    let rival = judged("rival-copy", "libs/acme.jar");
    assert!(
        !rival.iter().any(|message| message.contains("ares")),
        "{rival:?}"
    );
}

/// Tier 2's second slice: Lombok no longer withholds the member diagnostic
/// from the whole session. A receiver everything Lombok writes into is
/// generated for is judged — `getName()` on a `@Data` class is there,
/// `getNmae()` is not — and one with a `@SuperBuilder` renaming its
/// `builder()`, which nothing here generates for, is not. The annotations are workspace sources, so their
/// names resolve without a JAR.
#[test]
fn a_call_on_a_lombok_class_is_judged_where_what_lombok_writes_is_known() {
    let jdk = SyntheticJdk::build("lombok-members");
    let root = std::env::temp_dir().join(format!("javelin-lombok-members-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for (path, text) in [
        (
            "src/lombok/Data.java",
            "package lombok;\n\npublic @interface Data {}\n",
        ),
        (
            "src/lombok/experimental/SuperBuilder.java",
            "package lombok.experimental;\n\npublic @interface SuperBuilder {}\n",
        ),
        (
            "src/p/Car.java",
            "package p;\n\nimport lombok.Data;\n\n@Data\npublic class Car {\n    \
             private String name;\n}\n",
        ),
        (
            "src/p/Fleet.java",
            "package p;\n\nimport lombok.experimental.SuperBuilder;\n\n\
             @SuperBuilder(builderMethodName = \"make\")\npublic class Fleet {\n    int size;\n}\n",
        ),
    ] {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("package");
        std::fs::write(file, text).expect("source");
    }
    let reception = root.join("src/p/Reception.java");
    std::fs::write(
        &reception,
        "package p;\n\npublic class Reception {\n    Object check(Car car, Fleet fleet) {\n        \
         car.getName();\n        car.setName(\"x\");\n        car.getNmae();\n        \
         return fleet.anything();\n    }\n}\n",
    )
    .expect("Reception");

    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let uri = editor.open(&reception);
    let flagged = editor.diagnostics_until(&uri, |found| !found.is_empty());
    let messages: Vec<&str> = flagged
        .iter()
        .filter_map(|found| found["message"].as_str())
        .collect();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(
        messages[0].contains("p.Car") && messages[0].contains("getNmae"),
        "{messages:?}"
    );
    assert_eq!(flagged[0]["code"], "unresolved-method");
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// Tier 2's third slice: a record's accessor is a member nobody writes, so
/// `entry.date()` jumps to the component it reads and a call the record has
/// no member for is flagged — a record's list was never whole before.
#[test]
fn a_records_accessor_jumps_and_a_call_it_lacks_is_flagged() {
    let jdk = SyntheticJdk::build("record-members");
    let root = std::env::temp_dir().join(format!("javelin-record-members-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src/p")).expect("package");
    std::fs::write(
        root.join("src/p/Entry.java"),
        "package p;\n\npublic record Entry(String date) {}\n",
    )
    .expect("Entry");
    let reception = root.join("src/p/Reception.java");
    std::fs::write(
        &reception,
        "package p;\n\npublic class Reception {\n    Object check(Entry entry) {\n        \
         entry.date().length();\n        return entry.nope();\n    }\n}\n",
    )
    .expect("Reception");

    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let uri = editor.open(&reception);
    let flagged = editor.diagnostics_until(&uri, |found| !found.is_empty());
    let messages: Vec<&str> = flagged
        .iter()
        .filter_map(|found| found["message"].as_str())
        .collect();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(
        messages[0].contains("p.Entry") && messages[0].contains("nope"),
        "{messages:?}"
    );
    let (line, character) = editor.find(&uri, "entry.dat");
    let position = serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character }
    });
    let jump = editor
        .server()
        .eventually("textDocument/definition", &position, |answer| {
            !answer["result"].is_null()
        });
    assert!(
        jump["result"]["uri"]
            .as_str()
            .is_some_and(|target| target.ends_with("/src/p/Entry.java")),
        "{jump}"
    );
    assert_eq!(jump["result"]["range"]["start"]["line"], 2, "{jump}");
    assert_eq!(
        jump["result"]["range"]["start"]["character"], 27,
        "the component"
    );
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// Tier 2's fourth slice: a statically imported constant and method jump
/// to their declarations in another file, and a chain through the constant
/// is typed.
#[test]
fn a_statically_imported_constant_and_method_jump_and_type_their_chain() {
    let jdk = SyntheticJdk::build("static-imports");
    let root = std::env::temp_dir().join(format!("javelin-static-imports-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src/p")).expect("package");
    std::fs::create_dir_all(root.join("src/q")).expect("package");
    std::fs::write(
        root.join("src/p/Limits.java"),
        "package p;\n\npublic class Limits {\n    public static final String MAX = \"m\";\n    \
         public static int twice(int x) { return x; }\n}\n",
    )
    .expect("Limits");
    let user = root.join("src/q/User.java");
    std::fs::write(
        &user,
        "package q;\n\nimport static p.Limits.MAX;\nimport static p.Limits.twice;\n\n\
         public class User {\n    int check() {\n        twice(1);\n        \
         return MAX.length();\n    }\n}\n",
    )
    .expect("User");

    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let uri = editor.open(&user);
    let jump_at = |editor: &mut Editor, needle: &str| {
        let (line, character) = editor.find(&uri, needle);
        let position = serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        });
        editor
            .server()
            .eventually("textDocument/definition", &position, |answer| {
                !answer["result"].is_null()
            })
    };
    let constant = jump_at(&mut editor, "return MA");
    assert!(
        constant["result"]["uri"]
            .as_str()
            .is_some_and(|target| target.ends_with("/src/p/Limits.java")),
        "{constant}"
    );
    assert_eq!(
        constant["result"]["range"]["start"]["line"], 3,
        "{constant}"
    );
    assert_eq!(
        constant["result"]["range"]["start"]["character"], 31,
        "the constant"
    );
    let method = jump_at(&mut editor, "        twi");
    assert_eq!(method["result"]["range"]["start"]["line"], 4, "{method}");
    assert_eq!(
        method["result"]["range"]["start"]["character"], 22,
        "the method"
    );
    let (line, character) = editor.find(&uri, "MAX.len");
    let hover = editor.server().eventually(
        "textDocument/hover",
        &serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }),
        |answer| !answer["result"].is_null(),
    );
    assert!(hover.to_string().contains("length"), "{hover}");
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// Tier 2's sixth slice: the logger `@Slf4j` writes is a field, so a bare
/// `log` jumps to the annotations that generate it and its calls are
/// followed into the logger's type.
#[test]
fn a_lombok_logger_jumps_to_its_annotation_and_types_its_calls() {
    let jdk = SyntheticJdk::build("lombok-log");
    let root = std::env::temp_dir().join(format!("javelin-lombok-log-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src/org/slf4j")).expect("package");
    std::fs::create_dir_all(root.join("src/q")).expect("package");
    std::fs::write(
        root.join("src/org/slf4j/Logger.java"),
        "package org.slf4j;\n\npublic interface Logger {\n    void info(String message);\n}\n",
    )
    .expect("Logger");
    let service = root.join("src/q/Service.java");
    std::fs::write(
        &service,
        "package q;\n\nimport lombok.extern.slf4j.Slf4j;\n\n@Slf4j\npublic class Service {\n    \
         void run() {\n        log.info(\"x\");\n    }\n}\n",
    )
    .expect("Service");

    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let uri = editor.open(&service);
    let jump_at = |editor: &mut Editor, needle: &str| {
        let (line, character) = editor.find(&uri, needle);
        let position = serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        });
        editor
            .server()
            .eventually("textDocument/definition", &position, |answer| {
                !answer["result"].is_null()
            })
    };
    let field = jump_at(&mut editor, "        lo");
    assert!(
        field["result"]["uri"]
            .as_str()
            .is_some_and(|target| target.ends_with("/src/q/Service.java")),
        "{field}"
    );
    assert_eq!(field["result"]["range"]["start"]["line"], 4, "{field}");
    assert_eq!(
        field["result"]["range"]["start"]["character"], 0,
        "the annotation"
    );
    let call = jump_at(&mut editor, "log.in");
    assert!(
        call["result"]["uri"]
            .as_str()
            .is_some_and(|target| target.ends_with("/src/org/slf4j/Logger.java")),
        "{call}"
    );
    assert_eq!(call["result"]["range"]["start"]["line"], 3, "{call}");
    let (line, character) = editor.find(&uri, "        lo");
    let hover = editor.server().eventually(
        "textDocument/hover",
        &serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }),
        |answer| !answer["result"].is_null(),
    );
    assert!(hover.to_string().contains("Logger"), "{hover}");
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// Tier 2's fifth slice: a workspace type's type arguments survive a member
/// and a supertype edge, so a chain through a generic repository keeps its
/// entity.
#[test]
fn a_workspace_generic_keeps_its_type_argument_through_a_chain() {
    let jdk = SyntheticJdk::build("workspace-generics");
    let root =
        std::env::temp_dir().join(format!("javelin-workspace-generics-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src/p")).expect("package");
    std::fs::create_dir_all(root.join("src/q")).expect("package");
    std::fs::write(
        root.join("src/p/Entity.java"),
        "package p;\n\npublic class Entity {\n    public String name() { return null; }\n}\n",
    )
    .expect("Entity");
    std::fs::write(
        root.join("src/p/Repo.java"),
        "package p;\n\nimport java.util.Optional;\n\npublic interface Repo<T> {\n    \
         Optional<T> find();\n}\n",
    )
    .expect("Repo");
    std::fs::write(
        root.join("src/p/EntityRepo.java"),
        "package p;\n\npublic interface EntityRepo extends Repo<Entity> {}\n",
    )
    .expect("EntityRepo");
    std::fs::write(
        root.join("src/p/Util.java"),
        "package p;\n\npublic class Util {\n    public static <T> T same(T t) { return t; }\n}\n",
    )
    .expect("Util");
    let user = root.join("src/q/User.java");
    std::fs::write(
        &user,
        "package q;\n\nimport p.Entity;\nimport p.EntityRepo;\nimport p.Repo;\nimport p.Util;\n\n\
         public class User {\n    void check(Repo<Entity> repo, EntityRepo entities) {\n        \
         repo.find().get().name();\n        entities.find().get().name();\n        \
         Util.same(new Entity()).name();\n    }\n}\n",
    )
    .expect("User");

    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let uri = editor.open(&user);
    for needle in [
        "repo.find().get().na",
        "entities.find().get().na",
        "same(new Entity()).na",
    ] {
        let (line, character) = editor.find(&uri, needle);
        let position = serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        });
        let jump = editor
            .server()
            .eventually("textDocument/definition", &position, |answer| {
                !answer["result"].is_null()
            });
        assert!(
            jump["result"]["uri"]
                .as_str()
                .is_some_and(|target| target.ends_with("/src/p/Entity.java")),
            "{needle}: {jump}"
        );
        assert_eq!(
            jump["result"]["range"]["start"]["line"], 3,
            "{needle}: {jump}"
        );
        assert_eq!(
            jump["result"]["range"]["start"]["character"], 18,
            "{needle}"
        );
    }
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// Tier 1's ninth slice: each module is judged against its own classpath.
/// The fake `mvnw` writes each reactor module's classpath file where
/// `dependency:build-classpath -Dmdep.outputFile=…` does — `acme.jar` for
/// `core`, nothing for `web` — so `com.acme.Box` is known in one module and
/// flagged in the other, where the union of the two called it known.
#[cfg(unix)]
#[test]
fn a_type_only_another_modules_jar_holds_is_flagged() {
    use std::os::unix::fs::PermissionsExt as _;
    let jdk = SyntheticJdk::build("module-classpath");
    let root =
        std::env::temp_dir().join(format!("javelin-module-classpath-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(
        root.join("pom.xml"),
        "<project><packaging>pom</packaging></project>",
    )
    .expect("pom");
    let jar = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/classfiles/acme.jar")
        .canonicalize()
        .expect("the fixture JAR is checked in");
    let text = "package com.example;\n\nimport com.acme.Box;\n\npublic class Uses {\n    \
                Box<String> box;\n}\n";
    let mut sources = Vec::new();
    for module in ["core", "web"] {
        let package = root.join(module).join("src/main/java/com/example");
        std::fs::create_dir_all(&package).expect("package");
        std::fs::write(root.join(module).join("pom.xml"), "<project/>").expect("pom");
        let source = package.join("Uses.java");
        std::fs::write(&source, text).expect("source");
        sources.push(source);
    }
    let mvnw = root.join("mvnw");
    std::fs::write(
        &mvnw,
        format!(
            "#!/bin/sh\nfor arg; do case \"$arg\" in \
             -Dmdep.outputFile=*) out=\"${{arg#-Dmdep.outputFile=}}\";; esac; done\n\
             mkdir -p target core/target web/target\n\
             printf '' > \"$out\"\n\
             printf '{}' > \"core/$out\"\n\
             printf '' > \"web/$out\"\n",
            jar.display()
        ),
    )
    .expect("mvnw");
    std::fs::set_permissions(&mvnw, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let mut editor = Editor::launch_verbose_in(&jdk.options_with_dependencies(), &root);
    let core = editor.open(&sources[0]);
    editor.diagnostics_until(&core, <[serde_json::Value]>::is_empty);
    let web = editor.open(&sources[1]);
    let flagged = editor.diagnostics_until(&web, |found| !found.is_empty());
    assert!(
        flagged.iter().all(|found| found["message"]
            .as_str()
            .is_some_and(|message| message.contains("Box"))),
        "{flagged:?}"
    );
    let (line, _) = editor.find(&web, "import com.acme.Box");
    assert_eq!(flagged[0]["range"]["start"]["line"], line, "{flagged:?}");
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// Tier 1's fourth slice: with the JAR's `-sources.jar` beside it, a JAR
/// member and a JAR type jump into the extracted source.
#[cfg(unix)]
#[test]
fn a_jar_member_jumps_into_its_sources_jar() {
    jumps_into_acme_sources("sources-jar", false);
}

/// Tier 5's first slice: with none on disk, the build tool is asked for the
/// `-sources.jar`, and once it has fetched it the same jumps land.
#[cfg(unix)]
#[test]
fn a_jar_member_jumps_into_the_sources_jar_the_build_tool_fetched() {
    jumps_into_acme_sources("fetched-sources", true);
}

/// A Maven project whose classpath is `acme-1.0.jar`, with its sources beside
/// it, or, when `fetched`, with an `mvnw` that puts them there when asked.
#[cfg(unix)]
fn jumps_into_acme_sources(name: &str, fetched: bool) {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    let jdk = SyntheticJdk::build(name);
    let root = std::env::temp_dir().join(format!("javelin-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/main/java/com/example");
    // Beside the root: no fetch writes into the checkout.
    let repository = root.with_extension("repository");
    let _ = std::fs::remove_dir_all(&repository);
    std::fs::create_dir_all(&package).expect("package");
    std::fs::create_dir_all(&repository).expect("repository");
    std::fs::write(root.join("pom.xml"), "<project/>").expect("pom");
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/classfiles")
        .canonicalize()
        .expect("the fixtures are checked in");
    let jar = repository.join("acme-1.0.jar");
    std::fs::copy(fixtures.join("acme.jar"), &jar).expect("jar");
    let stash = root.join("stash");
    std::fs::create_dir_all(&stash).expect("stash");
    let written = if fetched { &stash } else { &repository };
    let mut sources = zip::ZipWriter::new(
        std::fs::File::create(written.join("acme-1.0-sources.jar")).expect("sources"),
    );
    for name in ["Box", "Color", "Point", "Shape"] {
        sources
            .start_file(
                format!("com/acme/{name}.java"),
                zip::write::SimpleFileOptions::default(),
            )
            .expect("entry");
        sources
            .write_all(
                &std::fs::read(fixtures.join(format!("src/com/acme/{name}.java"))).expect("read"),
            )
            .expect("write");
    }
    sources.finish().expect("finish");
    let mvnw = root.join("mvnw");
    std::fs::write(
        &mvnw,
        format!(
            "#!/bin/sh\ncase \"$*\" in\n*dependency:sources*) cp '{}' '{}';;\n\
             *) echo '[INFO] Dependencies classpath:'; echo '{}';;\nesac\n",
            stash.join("acme-1.0-sources.jar").display(),
            repository.display(),
            jar.display()
        ),
    )
    .expect("mvnw");
    std::fs::set_permissions(&mvnw, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let source = package.join("UsesBox.java");
    std::fs::write(
        &source,
        "package com.example;\n\nimport com.acme.Box;\n\npublic class UsesBox {\n    \
         Object label(Box<String> box) {\n        return box.names();\n    }\n}\n",
    )
    .expect("source");

    let mut editor = Editor::launch_verbose_in(&jdk.options_with_dependencies(), &root);
    let uri = editor.open(&source);
    let (line, character) = editor.find(&uri, "return box.nam");
    let position = serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character }
    });
    let jump = editor
        .server()
        .eventually("textDocument/definition", &position, |answer| {
            !answer["result"].is_null()
        });
    assert!(
        jump["result"]["uri"]
            .as_str()
            .is_some_and(|uri| uri.contains("/jar-sources/") && uri.ends_with("/com/acme/Box.java")),
        "{jump}"
    );
    let (line, character) = editor.find(&uri, "label(Bo");
    let (target, _) = editor
        .definition_at(&uri, line, character)
        .expect("the type's own name");
    assert!(target.ends_with("/com/acme/Box.java"), "{target}");
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&repository);
    let _ = std::fs::remove_dir_all(&root);
}

/// The other half of the row: the build tool can take minutes, and nothing
/// the JDK answers waits for it. The fake `mvnw` never finishes on its own;
/// a JDK member completes while it runs, and shutdown kills it rather than
/// waiting.
#[cfg(unix)]
#[test]
fn the_jdk_answers_while_the_build_tool_is_still_running() {
    use std::os::unix::fs::PermissionsExt as _;
    let jdk = SyntheticJdk::build("slow-build-tool");
    let root = std::env::temp_dir().join(format!("javelin-slow-build-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(root.join("pom.xml"), "<project/>").expect("pom");
    let mvnw = root.join("mvnw");
    let pid = root.join("mvnw.pid");
    std::fs::write(
        &mvnw,
        format!("#!/bin/sh\necho $$ > '{}'\nexec sleep 600\n", pid.display()),
    )
    .expect("mvnw");
    std::fs::set_permissions(&mvnw, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let source = root.join("A.java");
    std::fs::write(
        &source,
        "class A {\n    int m(String s) {\n        return s.length();\n    }\n}\n",
    )
    .expect("source");

    let mut editor = Editor::launch_verbose_in(&jdk.options_with_dependencies(), &root);
    let uri = editor.open(&source);
    let (line, character) = editor.find(&uri, "return s.");
    let labels = editor.completions_at(&uri, line, character);
    assert!(labels.contains(&"length".to_owned()), "{labels:?}");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let pid = loop {
        if let Some(pid) = std::fs::read_to_string(&pid)
            .ok()
            .filter(|text| !text.trim().is_empty())
        {
            break pid.trim().to_owned();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the build tool never started"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let started = std::time::Instant::now();
    assert_eq!(editor.shutdown(), Some(0));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    // The server has exited; the tool it started must not outlive it.
    let alive = || {
        std::process::Command::new("kill")
            .args(["-0", &pid])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while alive() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(), "the build tool outlived the server");
    let _ = std::fs::remove_dir_all(&root);
}

/// Phase 3d's inverse edges, over real pipes: from the interface, its
/// implementation in a file the editor never opened; from its method, the
/// caller that invokes it, grouped under the method it is written inside.
#[test]
fn implementations_and_callers_are_found_in_files_the_editor_never_opened() {
    let jdk = SyntheticJdk::build("hierarchies");
    let root = std::env::temp_dir().join(format!("javelin-hierarchies-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/main/java/p");
    std::fs::create_dir_all(&package).expect("package");
    let shape = package.join("Shape.java");
    std::fs::write(
        &shape,
        "package p;\n\npublic interface Shape {\n    double area();\n}\n",
    )
    .expect("Shape");
    std::fs::write(
        package.join("Circle.java"),
        "package p;\n\npublic class Circle implements Shape {\n    \
         public double area() {\n        return 0;\n    }\n}\n",
    )
    .expect("Circle");
    std::fs::write(
        package.join("Report.java"),
        "package p;\n\npublic class Report {\n    \
         double sum(Shape a, Shape b) {\n        return a.area() + b.area();\n    }\n}\n",
    )
    .expect("Report");

    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let uri = editor.open(&shape);
    let (line, character) = editor.find(&uri, "public interface Sha");
    let position = serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character }
    });
    let answer = editor
        .server()
        .eventually("textDocument/implementation", &position, |answer| {
            answer["result"]
                .as_array()
                .is_some_and(|found| !found.is_empty())
        });
    let found = answer["result"].as_array().expect("locations");
    assert_eq!(found.len(), 1, "{answer}");
    assert!(
        found[0]["uri"]
            .as_str()
            .is_some_and(|uri| uri.ends_with("/p/Circle.java")),
        "{answer}"
    );
    assert_eq!(found[0]["range"]["start"]["line"], 2, "Circle's own name");

    let (line, character) = editor.find(&uri, "double are");
    let position = serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character }
    });
    let prepared =
        editor
            .server()
            .eventually("textDocument/prepareCallHierarchy", &position, |answer| {
                answer["result"]
                    .as_array()
                    .is_some_and(|items| !items.is_empty())
            });
    let item = prepared["result"][0].clone();
    assert_eq!(item["name"], "area", "{prepared}");
    // The callers land in the walk's second publish, once member selections
    // are resolved.
    let incoming = editor.server().eventually(
        "callHierarchy/incomingCalls",
        &serde_json::json!({ "item": item }),
        |answer| {
            answer["result"]
                .as_array()
                .is_some_and(|calls| !calls.is_empty())
        },
    );
    let calls = incoming["result"].as_array().expect("calls");
    assert_eq!(calls.len(), 1, "{incoming}");
    assert_eq!(calls[0]["from"]["name"], "sum");
    assert_eq!(calls[0]["fromRanges"].as_array().map(Vec::len), Some(2));
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

// ---- Tier 1, slice 2: an annotation processor's output ---------------------

/// MapStruct's `*MapperImpl` lives in Gradle's processor output, below a
/// `build` directory the walk otherwise prunes: implementation on the mapper
/// lists it, and a name meaning it jumps into it.
#[test]
fn a_mapper_interface_is_implemented_by_the_processors_output() {
    let jdk = SyntheticJdk::build("processor-output");
    let root =
        std::env::temp_dir().join(format!("javelin-processor-output-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/main/java/p");
    let output = root.join("build/generated/sources/annotationProcessor/java/main/p");
    std::fs::create_dir_all(&package).expect("package");
    std::fs::create_dir_all(&output).expect("output");
    std::fs::write(
        package.join("Car.java"),
        "package p;\n\npublic class Car {}\n",
    )
    .expect("Car");
    let mapper = package.join("CarMapper.java");
    std::fs::write(
        &mapper,
        "package p;\n\npublic interface CarMapper {\n    String toDto(Car car);\n}\n",
    )
    .expect("CarMapper");
    std::fs::write(
        output.join("CarMapperImpl.java"),
        "package p;\n\npublic class CarMapperImpl implements CarMapper {\n    \
         public String toDto(Car car) {\n        return null;\n    }\n}\n",
    )
    .expect("CarMapperImpl");
    let user = package.join("Use.java");
    std::fs::write(
        &user,
        "package p;\n\nclass Use {\n    CarMapperImpl impl;\n}\n",
    )
    .expect("Use");

    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let uri = editor.open(&mapper);
    let (line, character) = editor.find(&uri, "public interface CarMa");
    let position = serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character }
    });
    let answer = editor
        .server()
        .eventually("textDocument/implementation", &position, |answer| {
            answer["result"]
                .as_array()
                .is_some_and(|found| !found.is_empty())
        });
    let found = answer["result"].as_array().expect("locations");
    assert_eq!(found.len(), 1, "{answer}");
    assert!(
        found[0]["uri"].as_str().is_some_and(|uri| uri.ends_with(
            "/build/generated/sources/annotationProcessor/java/main/p/CarMapperImpl.java"
        )),
        "{answer}"
    );
    assert_eq!(found[0]["range"]["start"]["line"], 2, "the impl's own name");

    let use_uri = editor.open(&user);
    let (line, character) = editor.find(&use_uri, "CarMapperIm");
    let position = serde_json::json!({
        "textDocument": { "uri": use_uri },
        "position": { "line": line, "character": character }
    });
    let jump = editor
        .server()
        .eventually("textDocument/definition", &position, |answer| {
            !answer["result"].is_null()
        });
    assert!(
        jump["result"]["uri"]
            .as_str()
            .is_some_and(|uri| uri.ends_with("/annotationProcessor/java/main/p/CarMapperImpl.java")),
        "{jump}"
    );
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

// ---- Tier 1, slice 3: Lombok members ----------------------------------------

/// `@Data` and `@Builder` write members no file declares: from another file,
/// a getter hovers as its signature and jumps to its field, and a builder
/// chain is followed into the type.
#[test]
fn a_lombok_getter_and_builder_are_followed_into_the_type_that_generates_them() {
    let jdk = SyntheticJdk::build("lombok");
    let root = std::env::temp_dir().join(format!("javelin-lombok-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/main/java/p");
    std::fs::create_dir_all(&package).expect("package");
    std::fs::write(
        package.join("Car.java"),
        "package p;\n\nimport lombok.Builder;\nimport lombok.Data;\n\n@Data\n@Builder\n\
         public class Car {\n    private String name;\n}\n",
    )
    .expect("Car");
    let garage = package.join("Garage.java");
    std::fs::write(
        &garage,
        "package p;\n\nclass Garage {\n    String park(Car car) {\n        \
         return car.getName() + Car.builder().name(\"x\").build();\n    }\n}\n",
    )
    .expect("Garage");

    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let uri = editor.open(&garage);
    let (line, character) = editor.find(&uri, "car.getNa");
    let position = serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character }
    });
    let jump = editor
        .server()
        .eventually("textDocument/definition", &position, |answer| {
            !answer["result"].is_null()
        });
    assert!(
        jump["result"]["uri"]
            .as_str()
            .is_some_and(|uri| uri.ends_with("/p/Car.java")),
        "{jump}"
    );
    assert_eq!(
        jump["result"]["range"]["start"]["line"], 8,
        "the field's name"
    );
    assert_eq!(
        editor.hover_at(&uri, line, character).as_deref(),
        Some("```java\nString getName()\n```")
    );
    let (line, character) = editor.find(&uri, ".buil");
    let (target, at) = editor
        .definition_at(&uri, line, character)
        .expect("build() is sited at the type");
    assert!(target.ends_with("/p/Car.java"), "{target}");
    assert_eq!(at, 7, "the type's own name");
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

// ---- Tier 1, slice 5: parameter names and inlay hints ----------------------

/// A call's arguments carry their parameters' names, over real pipes: a JDK
/// method's from its sources and a method of the open file's own.
#[test]
fn a_calls_arguments_are_hinted_with_their_parameters_names() {
    let jdk = SyntheticJdk::build("inlay-hints");
    let root = std::env::temp_dir().join(format!("javelin-inlay-hints-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/main/java/p");
    std::fs::create_dir_all(&package).expect("package");
    let source = package.join("Moves.java");
    std::fs::write(
        &source,
        "package p;\n\nclass Moves {\n    void move(int x, int y) {}\n\n    \
         String m(String s, int x) {\n        move(x, 2);\n        return s.substring(0, 3);\n    }\n}\n",
    )
    .expect("source");
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let uri = editor.open(&source);
    classpath_ready(&mut editor);
    let params = serde_json::json!({
        "textDocument": { "uri": uri },
        "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 20, "character": 0 } }
    });
    let answer = editor
        .server()
        .eventually("textDocument/inlayHint", &params, |answer| {
            answer["result"]
                .as_array()
                .is_some_and(|hints| hints.len() >= 3)
        });
    let labels: Vec<(u64, String)> = answer["result"]
        .as_array()
        .expect("hints")
        .iter()
        .map(|hint| {
            (
                hint["position"]["line"].as_u64().unwrap_or_default(),
                hint["label"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        labels,
        vec![
            (6, "y:".to_owned()),
            (7, "beginIndex:".to_owned()),
            (7, "endIndex:".to_owned()),
        ],
        "`x` already says `x`: {answer}"
    );
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

// ---- Tier 3, slice 1: type inlay hints -------------------------------------

/// A `var` and an implicit lambda parameter carry the type nobody wrote,
/// over real pipes, once the workspace's first walk has said which names the
/// resolver may read.
#[test]
fn a_var_and_an_implicit_lambda_parameter_are_hinted_with_their_types() {
    let jdk = SyntheticJdk::build("type-hints");
    let root = std::env::temp_dir().join(format!("javelin-type-hints-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/main/java/p");
    std::fs::create_dir_all(&package).expect("package");
    let source = package.join("Types.java");
    std::fs::write(
        &source,
        "package p;\n\nimport java.util.List;\n\nclass Types {\n    void m(String s, List<String> l) {\n        \
         var t = s.substring(0, 1);\n        l.stream().filter(x -> x.length() > 0);\n    }\n}\n",
    )
    .expect("source");
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let uri = editor.open(&source);
    classpath_ready(&mut editor);
    let params = serde_json::json!({
        "textDocument": { "uri": uri },
        "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 20, "character": 0 } }
    });
    let types = |answer: &serde_json::Value| -> Vec<(u64, u64, String)> {
        answer["result"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|hint| hint["kind"] == 1)
            .map(|hint| {
                (
                    hint["position"]["line"].as_u64().unwrap_or_default(),
                    hint["position"]["character"].as_u64().unwrap_or_default(),
                    hint["label"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect()
    };
    let answer = editor
        .server()
        .eventually("textDocument/inlayHint", &params, |answer| {
            types(answer).len() >= 2
        });
    assert_eq!(
        types(&answer),
        vec![
            (6, 13, ": String".to_owned()),
            (7, 27, ": String".to_owned())
        ],
        "{answer}"
    );
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

// ---- Tier 3, slice 2: go to type definition ----------------------------------

/// `textDocument/typeDefinition` on a `var` lands on its type's name in the
/// JDK's extracted sources, and on a parameter in the sibling file that
/// declares its type — over real pipes, once the walk has vouched for both.
#[test]
fn a_var_and_a_parameter_jump_to_the_declarations_of_their_types() {
    let jdk = SyntheticJdk::build("type-definition");
    let root = std::env::temp_dir().join(format!("javelin-type-definition-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/main/java/p");
    std::fs::create_dir_all(&package).expect("package");
    std::fs::write(
        package.join("Greeter.java"),
        "package p;\n\nclass Greeter {}\n",
    )
    .expect("sibling");
    let source = package.join("Types.java");
    std::fs::write(
        &source,
        "package p;\n\nclass Types {\n    void m(String s, Greeter g) {\n        \
         var t = s.substring(0, 1);\n        g.hashCode();\n    }\n}\n",
    )
    .expect("source");
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let uri = editor.open(&source);
    classpath_ready(&mut editor);
    let at = |line: u32, character: u32| {
        serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        })
    };
    let target = |answer: &serde_json::Value| -> Option<(String, u64, u64)> {
        let result = &answer["result"];
        Some((
            result["uri"].as_str()?.to_owned(),
            result["range"]["start"]["line"].as_u64()?,
            result["range"]["start"]["character"].as_u64()?,
        ))
    };
    let var = editor
        .server()
        .eventually("textDocument/typeDefinition", &at(4, 13), |answer| {
            target(answer).is_some()
        });
    let (file, line, _) = target(&var).expect("String's declaration");
    assert!(file.ends_with("java/lang/String.java"), "{var}");
    assert_eq!(
        line, 2,
        "`SOURCES` in the fixture declares `class String` on line 3 of java/lang/String.java"
    );
    let parameter =
        editor
            .server()
            .eventually("textDocument/typeDefinition", &at(5, 9), |answer| {
                target(answer).is_some()
            });
    let (file, line, character) = target(&parameter).expect("Greeter's declaration");
    assert!(file.ends_with("p/Greeter.java"), "{parameter}");
    assert_eq!((line, character), (2, 6), "{parameter}");
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

// ---- Tier 3, slice 3: constructors as targets --------------------------------

/// `new Greeter("a", 1)` jumps to the constructor its arguments select, in a
/// file the editor never opened, and its arguments are hinted with that
/// constructor's parameter names — over real pipes, once the walk has
/// vouched for the names it read.
#[test]
fn a_new_jumps_to_its_constructor_and_is_hinted_with_its_names() {
    let jdk = SyntheticJdk::build("constructors");
    let root = std::env::temp_dir().join(format!("javelin-constructors-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/main/java/p");
    std::fs::create_dir_all(&package).expect("package");
    std::fs::write(
        package.join("Greeter.java"),
        "package p;\n\nclass Greeter {\n    Greeter(String name) {}\n    \
         Greeter(String name, int times) {}\n}\n",
    )
    .expect("sibling");
    let source = package.join("Reception.java");
    std::fs::write(
        &source,
        "package p;\n\nclass Reception {\n    Object m() {\n        \
         return new Greeter(\"a\", 1);\n    }\n}\n",
    )
    .expect("source");
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let uri = editor.open(&source);
    classpath_ready(&mut editor);
    let at = serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": 4, "character": 20 }
    });
    let target = |answer: &serde_json::Value| -> Option<(String, u64, u64)> {
        let result = &answer["result"];
        Some((
            result["uri"].as_str()?.to_owned(),
            result["range"]["start"]["line"].as_u64()?,
            result["range"]["start"]["character"].as_u64()?,
        ))
    };
    let jump = editor
        .server()
        .eventually("textDocument/definition", &at, |answer| {
            target(answer).is_some_and(|(file, _, _)| file.ends_with("p/Greeter.java"))
        });
    let (_, line, character) = target(&jump).expect("Greeter(String, int)");
    assert_eq!((line, character), (4, 4), "{jump}");

    let params = serde_json::json!({
        "textDocument": { "uri": uri },
        "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 20, "character": 0 } }
    });
    let names = |answer: &serde_json::Value| -> Vec<(u64, u64, String)> {
        answer["result"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|hint| hint["kind"] == 2)
            .map(|hint| {
                (
                    hint["position"]["line"].as_u64().unwrap_or_default(),
                    hint["position"]["character"].as_u64().unwrap_or_default(),
                    hint["label"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect()
    };
    let answer = editor
        .server()
        .eventually("textDocument/inlayHint", &params, |answer| {
            names(answer).len() >= 2
        });
    assert_eq!(
        names(&answer),
        vec![(4, 27, "name:".to_owned()), (4, 32, "times:".to_owned())],
        "{answer}"
    );
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

// ---- Tier 3, slice 4: generator output beyond processors --------------------

/// protobuf's Gradle plugin writes its messages below `build`, which the
/// walk otherwise prunes: a message's name and its accessor jump into them.
#[test]
fn a_protobuf_message_and_its_accessor_jump_into_the_generators_output() {
    let jdk = SyntheticJdk::build("generator-output");
    let root =
        std::env::temp_dir().join(format!("javelin-generator-output-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/main/java/com/acme");
    let output = root.join("build/generated/source/proto/main/java/com/acme");
    std::fs::create_dir_all(&package).expect("package");
    std::fs::create_dir_all(&output).expect("output");
    std::fs::write(
        output.join("Order.java"),
        "package com.acme;\n\npublic final class Order {\n    \
         public String getId() {\n        return null;\n    }\n}\n",
    )
    .expect("Order");
    let user = package.join("Use.java");
    std::fs::write(
        &user,
        "package com.acme;\n\nclass Use {\n    Object m(Order order) {\n        \
         return order.getId();\n    }\n}\n",
    )
    .expect("Use");

    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let uri = editor.open(&user);
    for (needle, line) in [("m(Orde", 2), ("order.getI", 3)] {
        let (at_line, character) = editor.find(&uri, needle);
        let position = serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": at_line, "character": character }
        });
        let jump = editor
            .server()
            .eventually("textDocument/definition", &position, |answer| {
                !answer["result"].is_null()
            });
        assert!(
            jump["result"]["uri"]
                .as_str()
                .is_some_and(|uri| uri
                    .ends_with("/build/generated/source/proto/main/java/com/acme/Order.java")),
            "{needle}: {jump}"
        );
        assert_eq!(
            jump["result"]["range"]["start"]["line"], line,
            "{needle}: {jump}"
        );
    }
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

// ---- Tier 3, slice 5: semantic tokens for a range ----------------------------

/// A file past the 640 KiB guard answers `full` with nothing, as before, and
/// a range of it with the tokens of its lines.
#[test]
fn a_file_past_the_guard_is_coloured_for_a_range_and_not_whole() {
    let jdk = SyntheticJdk::build("semantic-tokens-range");
    let root = std::env::temp_dir().join(format!("javelin-tokens-range-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/main/java/com/acme");
    std::fs::create_dir_all(&package).expect("package");
    let mut text = String::from("package com.acme;\n\nclass Big {\n");
    for n in 0..20_000 {
        text.push_str(&format!("    int m{n}() {{ return {n}; }}\n"));
    }
    text.push_str("}\n");
    assert!(
        text.len() > 640 * 1024,
        "{} bytes is under the guard",
        text.len()
    );
    let big = package.join("Big.java");
    std::fs::write(&big, &text).expect("Big");

    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    assert_eq!(
        editor.capabilities()["semanticTokensProvider"]["range"],
        serde_json::json!(true)
    );
    let uri = editor.open(&big);
    let range = editor.server().eventually(
        "textDocument/semanticTokens/range",
        &serde_json::json!({
            "textDocument": { "uri": uri },
            "range": { "start": { "line": 3, "character": 0 },
                       "end": { "line": 5, "character": 0 } }
        }),
        |answer| {
            answer["result"]["data"]
                .as_array()
                .is_some_and(|d| !d.is_empty())
        },
    );
    let data = range["result"]["data"].as_array().expect("data");
    assert_eq!(data.len() % 5, 0, "{range}");
    assert_eq!(data[0], 3, "the first token is on line 3: {range}");
    assert!(
        data.chunks(5).skip(1).all(|t| t[0].as_u64() <= Some(1)),
        "every token is on lines 3 and 4: {range}"
    );
    let full = editor.semantic_tokens_full(&uri);
    assert_eq!(full["data"], serde_json::json!([]), "{full}");
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

// ---- Tier 1, slice 6: Javadoc in hover --------------------------------------

/// A JDK member hovers with its Javadoc, read out of `src.zip` at the span
/// the fact records and rendered as Markdown.
#[test]
fn a_jdk_member_hovers_with_its_javadoc() {
    let jdk = SyntheticJdk::build("javadoc-hover");
    let root = std::env::temp_dir().join(format!("javelin-javadoc-hover-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/main/java/p");
    std::fs::create_dir_all(&package).expect("package");
    let source = package.join("Cuts.java");
    std::fs::write(
        &source,
        "package p;\n\nclass Cuts {\n    String m(String s) {\n        return s.substring(0, 3);\n    }\n}\n",
    )
    .expect("source");
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let uri = editor.open(&source);
    classpath_ready(&mut editor);
    let (line, character) = editor.find(&uri, "s.substr");
    assert_eq!(
        editor.hover_at(&uri, line, character).as_deref(),
        Some(
            "```java\nString substring(int beginIndex, int endIndex)\n```\n\n\
             Returns a substring, `beginIndex` to `endIndex`."
        )
    );
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

// ---- Tier 1, slice 7: generic method inference -----------------------------

/// What `map` returns is inferred from its lambda's body, so the next
/// lambda's parameter is typed through it: `w` is a `String`.
#[test]
fn a_lambdas_result_types_the_generic_call_it_is_passed_to() {
    let jdk = SyntheticJdk::build("generic-inference");
    let mut editor = Editor::launch_verbose(&jdk.options());
    let uri = editor.open(&greeter());
    let (line, character) = in_main(&editor, &uri);
    scratch(
        &mut editor,
        &uri,
        line,
        character,
        concat!(
            "\n        List<String> names = null;",
            "\n        names.stream().map(z -> z.substring(0, 1)).map(w -> w.length());",
        ),
    );
    classpath_ready(&mut editor);
    let (line, character) = editor.find(&uri, "w -> w.lengt");
    assert_eq!(
        editor.hover_at(&uri, line, character).as_deref(),
        Some("```java\nint length()\n```")
    );
    editor.shutdown();
}

// ---- Tier 1, slice 8: most-specific selection, literals, qualified names ----

/// A qualified name jumps and is referenced through its last segment, and is
/// flagged when a package qualifies a type nothing holds; a call two
/// overloads both fit goes to the more specific, a string literal included.
#[test]
fn a_qualified_name_jumps_and_the_most_specific_overload_is_picked() {
    let jdk = SyntheticJdk::build("qualified-names");
    let root = std::env::temp_dir().join(format!("javelin-qualified-names-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src/p")).expect("p");
    std::fs::create_dir_all(root.join("src/q")).expect("q");
    let greeter = root.join("src/p/Greeter.java");
    std::fs::write(&greeter, "package p;\n\npublic class Greeter {\n}\n").expect("Greeter");
    let use_ = root.join("src/q/Use.java");
    std::fs::write(
        &use_,
        "package q;\n\nclass Use {\n    p.Greeter g;\n    q.Missinng m;\n    \
         void log(String s) {}\n    void log(Object o) {}\n    \
         void m() {\n        log(\"x\");\n        /*n*/ log(null);\n    }\n}\n",
    )
    .expect("Use");

    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let uri = editor.open(&use_);
    let flagged = editor.diagnostics_until(&uri, |found| !found.is_empty());
    assert_eq!(flagged.len(), 1, "{flagged:?}");
    assert_eq!(flagged[0]["code"], "unresolved-type");
    let (line, character) = editor.find(&uri, "    q.");
    assert_eq!(flagged[0]["range"]["start"]["line"], line);
    assert_eq!(flagged[0]["range"]["start"]["character"], character);

    let (line, character) = editor.find(&uri, "p.Greet");
    let (target, _) = editor
        .definition_at(&uri, line, character)
        .expect("Greeter");
    assert!(target.ends_with("/p/Greeter.java"), "{target}");
    for call in ["        lo", "/*n*/ lo"] {
        let (line, character) = editor.find(&uri, call);
        let (target, at) = editor.definition_at(&uri, line, character).expect(call);
        assert_eq!(
            (target.as_str(), at),
            (uri.as_str(), 5),
            "{call}: log(String)"
        );
    }

    let in_use = |found: &[(String, u32)]| {
        found
            .iter()
            .any(|(uri, line)| uri.ends_with("/q/Use.java") && *line == 3)
    };
    let declared = editor.open(&greeter);
    let (line, character) = editor.find(&declared, "class Greet");
    let found = editor.references_until(&declared, line, character, in_use);
    assert!(in_use(&found), "{found:?}");
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

// ---- First release, slice 3: override-aware references ---------------------

#[test]
fn references_follow_an_override_both_ways_and_skip_its_siblings() {
    let jdk = SyntheticJdk::build("override-references");
    let root = std::env::temp_dir().join(format!(
        "javelin-override-references-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/main/java/p");
    std::fs::create_dir_all(&package).expect("package");
    let runner = package.join("Runner.java");
    std::fs::write(
        &runner,
        "package p;\n\npublic interface Runner {\n    void run();\n}\n",
    )
    .expect("Runner");
    let fast = package.join("Fast.java");
    std::fs::write(
        &fast,
        "package p;\n\npublic class Fast implements Runner {\n    public void run() {}\n}\n",
    )
    .expect("Fast");
    std::fs::write(
        package.join("Slow.java"),
        "package p;\n\npublic class Slow implements Runner {\n    public void run() {}\n}\n",
    )
    .expect("Slow");
    std::fs::write(
        package.join("Use.java"),
        "package p;\n\nclass Use {\n    void m(Runner r, Fast f, Slow s) {\n        \
         r.run();\n        f.run();\n        s.run();\n    }\n}\n",
    )
    .expect("Use");

    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let in_use = |found: &[(String, u32)]| -> Vec<u32> {
        found
            .iter()
            .filter(|(uri, _)| uri.ends_with("/p/Use.java"))
            .map(|(_, line)| *line)
            .collect()
    };
    // On the interface: the call through it and the calls through both
    // overrides, once the walk has resolved them.
    let uri = editor.open(&runner);
    let (line, character) = editor.find(&uri, "void ru");
    let found = editor.references_until(&uri, line, character, |found| in_use(found).len() == 3);
    assert_eq!(in_use(&found), vec![4, 5, 6], "{found:?}");
    // On one override: the call through the interface and its own, and not
    // its sibling's.
    let uri = editor.open(&fast);
    let (line, character) = editor.find(&uri, "public void ru");
    let found = editor.references_until(&uri, line, character, |found| in_use(found).len() >= 2);
    assert_eq!(in_use(&found), vec![4, 5], "{found:?}");
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn folding_ranges_are_offered_for_a_class_and_the_methods_inside_it() {
    let jdk = SyntheticJdk::build("folding-ranges");
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &demo_project());
    let uri = editor.open(&greeter());

    let ranges = editor.folding_ranges(&uri);
    assert!(ranges.is_array(), "folding ranges are an array: {ranges}");
    let ranges = ranges.as_array().expect("checked above");

    // Every claim is anchored to a line the fixture actually writes, so a
    // fold at the wrong place fails here rather than passing a count.
    let fold_at = |line: u32| -> &serde_json::Value {
        ranges
            .iter()
            .find(|r| r["startLine"].as_u64() == Some(line as u64))
            .unwrap_or_else(|| panic!("no fold starts on line {line}: {ranges:?}"))
    };

    // The class's own body, from `public class Greeter {` to the line above
    // the file's last brace.
    let class_line = line_of(&editor, &uri, "public class Greeter {");
    let class_fold = fold_at(class_line);
    assert!(
        class_fold.get("kind").is_none(),
        "a body fold carries no kind: {class_fold}"
    );

    // A method inside it — the half of this test's own name that a single
    // class-body fold used to satisfy on its own.
    let greet_line = line_of(&editor, &uri, "public String greet() {");
    let greet_fold = fold_at(greet_line);
    assert!(
        greet_fold.get("kind").is_none(),
        "a body fold carries no kind: {greet_fold}"
    );
    assert_eq!(
        greet_fold["endLine"].as_u64(),
        Some((greet_line + 2) as u64),
        "`greet()` has two statements, so its body folds to the line above its \
         closing brace: {greet_fold}"
    );

    // Every fold hides at least one line; a fold arrow that collapses nothing
    // is what `emit_fold`'s own guard exists to refuse.
    for range in ranges {
        assert!(
            range["endLine"].as_u64() > range["startLine"].as_u64(),
            "a fold must hide at least one line: {range}"
        );
    }

    // The two imports, folded as one run of kind `imports`.
    let imports_line = line_of(&editor, &uri, "import java.util.List;");
    let imports_fold = fold_at(imports_line);
    assert_eq!(imports_fold["kind"].as_str(), Some("imports"));

    editor.shutdown();
}

#[test]
fn selection_ranges_expand_outward_from_an_identifier() {
    let jdk = SyntheticJdk::build("selection-ranges");
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &demo_project());
    let uri = editor.open(&greeter());

    let text = editor.text(&uri).to_owned();
    // The local `greeting`'s own declaration, in `greet()`. A cursor on its
    // first character, so the innermost step must be the identifier itself.
    let greeting_at = text.find("greeting").expect("the local is declared");
    let (line, character) = position_of(&text, greeting_at);

    let ranges = editor.selection_ranges(&uri, vec![(line, character)]);
    assert!(ranges.is_array(), "selection ranges are an array: {ranges}");
    let ranges = ranges.as_array().expect("checked above");
    assert_eq!(
        ranges.len(),
        1,
        "exactly one chain per requested position: {ranges:?}"
    );

    // Flatten the chain to (start offset, end offset) pairs in the fixture's
    // own bytes, so every step is checked against the text it covers.
    let mut chain: Vec<(usize, usize)> = Vec::new();
    let mut step = Some(&ranges[0]);
    while let Some(current) = step {
        let at = |end: &str| -> usize {
            let point = &current["range"][end];
            let line = point["line"].as_u64().expect("a line") as u32;
            let character = point["character"].as_u64().expect("a character") as u32;
            offset_of(&text, line, character)
        };
        chain.push((at("start"), at("end")));
        step = current.get("parent");
    }

    assert_eq!(
        &text[chain[0].0..chain[0].1],
        "greeting",
        "the innermost step is the identifier under the cursor: {chain:?}"
    );
    assert!(
        chain.len() >= 4,
        "identifier, declaration, method body, method, class — several steps, \
         not one: {chain:?}"
    );
    // Strictly nesting outward: each step contains the one before it and is
    // wider than it, which is the whole promise of the feature.
    for pair in chain.windows(2) {
        let (inner, outer) = (pair[0], pair[1]);
        assert!(
            outer.0 <= inner.0 && inner.1 <= outer.1,
            "each step must contain the one inside it: {inner:?} then {outer:?}"
        );
        assert!(
            outer.1 - outer.0 > inner.1 - inner.0,
            "each step must be strictly wider: {inner:?} then {outer:?}"
        );
    }
    // The outermost step covers the whole class declaration, which is as far
    // out as expansion goes in a one-type file.
    let outermost = chain[chain.len() - 1];
    assert!(
        text[outermost.0..outermost.1].starts_with("package com.example;"),
        "the chain expands out to the whole compilation unit: {:?}",
        &text[outermost.0..outermost.1.min(outermost.0 + 40)]
    );

    editor.shutdown();
}

#[test]
fn clicking_a_local_highlights_every_use_of_it_and_nothing_else() {
    let jdk = SyntheticJdk::build("document-highlight");
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &demo_project());
    let uri = editor.open(&greeter());

    // `greeting` is declared once in `greet()` and used once, in the `return`
    // right below it — nowhere else in the file spells the same name.
    let (line, character) = editor.find(&uri, "greeting");
    let highlights = editor.document_highlights(&uri, line, character);
    assert!(
        highlights.is_array(),
        "document highlights are an array: {highlights}"
    );
    let highlights = highlights.as_array().expect("checked above");
    assert_eq!(
        highlights.len(),
        2,
        "the declaration and its one use, nothing else in the file: {highlights:?}"
    );

    // Each highlight is checked against the text it actually points at, so a
    // stray range — the field `name` on the same line as the declaration,
    // say — would fail this rather than only the count above.
    let text = editor.text(&uri).to_owned();
    for highlight in highlights {
        assert_eq!(
            highlight["kind"].as_u64(),
            Some(1),
            "every highlight is DocumentHighlightKind.Text (Ruling 4): {highlight:?}"
        );
        let start = &highlight["range"]["start"];
        let start_line = start["line"].as_u64().expect("a line") as u32;
        let start_character = start["character"].as_u64().expect("a character") as u32;
        let offset = offset_of(&text, start_line, start_character);
        assert_eq!(
            &text[offset..offset + "greeting".len()],
            "greeting",
            "each highlight starts exactly on an occurrence of `greeting`: {highlight:?}"
        );
    }

    editor.shutdown();
}

/// The byte offset of `ident` inside the first occurrence of `context`,
/// global to `text` — for a name that is not unique on its own (`name`,
/// `seen`) but is unique inside a surrounding phrase.
fn find_within(text: &str, context: &str, ident: &str) -> usize {
    let context_at = text
        .find(context)
        .unwrap_or_else(|| panic!("{context:?} is not in the file"));
    let ident_at = text[context_at..]
        .find(ident)
        .unwrap_or_else(|| panic!("{ident:?} is not inside {context:?}"));
    context_at + ident_at
}

#[test]
fn semantic_tokens_are_offered_for_the_open_file_and_decode_to_the_declared_legend() {
    let jdk = SyntheticJdk::build("semantic-tokens");
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &demo_project());
    let uri = editor.open(&greeter());
    let text = editor.text(&uri).to_owned();

    let legend = editor.semantic_tokens_legend();
    let result = editor.semantic_tokens_full(&uri);
    let raw: Vec<u32> = result["data"]
        .as_array()
        .expect("semantic tokens are a flat integer array")
        .iter()
        .map(|value| value.as_u64().expect("an integer") as u32)
        .collect();
    assert!(!raw.is_empty(), "a real file offers tokens: {result}");
    assert_eq!(raw.len() % 5, 0, "five integers per token: {raw:?}");

    // Decode LSP's relative stream into absolute (line, character, length,
    // type name) — the same math `handlers::semantic_tokens::encode` runs to
    // build it, run backwards, so a wrong delta here is caught by an editor
    // rather than only by the handler's own unit tests.
    let mut line = 0u32;
    let mut character = 0u32;
    let mut decoded: Vec<(u32, u32, u32, &str)> = Vec::new();
    for chunk in raw.chunks(5) {
        let [delta_line, delta_start, length, token_type, modifiers] = chunk else {
            unreachable!("checked divisible by 5 above")
        };
        line += delta_line;
        character = if *delta_line == 0 {
            character + delta_start
        } else {
            *delta_start
        };
        assert_eq!(*modifiers, 0, "no modifiers are advertised in this slice");
        let name = legend
            .get(*token_type as usize)
            .unwrap_or_else(|| panic!("token type {token_type} is outside the declared legend"));
        decoded.push((line, character, *length, name.as_str()));
    }

    // Each position is checked against the text it actually points at, and
    // against the legend name it decodes to — not only against a count — so
    // a token at the wrong offset or holding the wrong type index fails here
    // rather than only looking plausible.
    let token_at = |at: usize| -> (u32, u32, u32, &str) {
        let (want_line, want_character) = position_of(&text, at);
        *decoded
            .iter()
            .find(|(l, c, _, _)| *l == want_line && *c == want_character)
            .unwrap_or_else(|| {
                panic!("no token decoded at {want_line}:{want_character} among {decoded:?}")
            })
    };
    let assert_decoded = |at: usize, expected_length: usize, expected_type: &str| {
        let found = token_at(at);
        assert_eq!(found.2, expected_length as u32, "token at {at}: {found:?}");
        assert_eq!(found.3, expected_type, "token at {at}: {found:?}");
    };

    // `package com.example;` — a name in a package declaration is namespace,
    // unambiguous without resolving anything.
    assert_decoded(text.find("com").unwrap(), "com".len(), "namespace");
    // `public class Greeter {` — the class's own keyword and its own name.
    assert_decoded(text.find("class").unwrap(), "class".len(), "keyword");
    assert_decoded(text.find("Greeter").unwrap(), "Greeter".len(), "class");
    // `interface Listener {` — the nested interface's own name.
    assert_decoded(
        find_within(&text, "interface Listener", "Listener"),
        "Listener".len(),
        "interface",
    );
    // `private final String name;` — the field's own declaration.
    assert_decoded(text.find("name;").unwrap(), "name".len(), "property");
    // `public Greeter(String name, Map<String, Integer> seen) {` — the
    // constructor's own parameter, not the field of the same spelling.
    assert_decoded(
        find_within(&text, "(String name,", "name"),
        "name".len(),
        "parameter",
    );
    // `String greeting = "Grüße, " + name + " 🚀";` — the local's own
    // declaration.
    assert_decoded(text.find("greeting").unwrap(), "greeting".len(), "variable");
    // The string literal `"Welt"`, quotes included — the token covers the
    // whole literal, not just its content.
    assert_decoded(text.find("\"Welt\"").unwrap(), "\"Welt\"".len(), "string");
    // `seen.getOrDefault(name, 0)` — the integer literal argument.
    assert_decoded(find_within(&text, ", 0)", "0"), 1, "number");
    // The doc comment right above `greet()`. Only its type is asserted here;
    // splitting a multi-line comment into one token per line is pinned by
    // `a_block_comment_spanning_three_lines_becomes_three_tokens` in the
    // parser crate.
    let comment = token_at(text.find("/** Non-ASCII on purpose").unwrap());
    assert_eq!(comment.3, "comment", "{comment:?}");

    editor.shutdown();
}

#[test]
fn a_hallucinated_type_is_flagged_in_a_loose_project_and_fixing_it_clears_the_flag() {
    let jdk = SyntheticJdk::build("diagnostics");
    // No build file: nothing on the classpath but the JDK can be missing, so
    // the world is whole once the JDK and the walk are in.
    let root = std::env::temp_dir().join(format!("javelin-diagnostics-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/p");
    std::fs::create_dir_all(&package).expect("package");
    std::fs::write(
        package.join("Greeter.java"),
        "package p;\n\npublic class Greeter {\n}\n",
    )
    .expect("Greeter");
    let reception = package.join("Reception.java");
    std::fs::write(
        &reception,
        "package p;\n\nimport java.util.List;\n\npublic class Reception {\n    \
         Greeter greeter;\n    String name;\n    List<Greter> queue;\n}\n",
    )
    .expect("Reception");

    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    // A valid file is judged and found clean: an empty list is published,
    // which is the pass condition rather than the absence of one.
    let greeter = editor.open(&package.join("Greeter.java"));
    editor.diagnostics_until(&greeter, <[serde_json::Value]>::is_empty);
    let uri = editor.open(&reception);
    let flagged = editor.diagnostics_until(&uri, |found| !found.is_empty());
    assert_eq!(flagged.len(), 1, "only the hallucinated name: {flagged:?}");
    let (line, character) = editor.find(&uri, "List<");
    assert_eq!(flagged[0]["range"]["start"]["line"], line);
    assert_eq!(flagged[0]["range"]["start"]["character"], character);
    assert_eq!(
        flagged[0]["range"]["end"]["character"],
        character + "Greter".len() as u32
    );
    assert_eq!(flagged[0]["severity"], 2, "a warning, never an error");
    assert!(
        flagged[0]["message"]
            .as_str()
            .is_some_and(|message| message.contains("Greter")),
        "{flagged:?}"
    );

    let (line, character) = editor.find(&uri, "List<Gre");
    editor.type_at(&uri, line, character, "e");
    editor.diagnostics_until(&uri, <[serde_json::Value]>::is_empty);
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_hallucinated_field_and_unqualified_call_are_flagged_and_a_nested_call_reaches_out() {
    let jdk = SyntheticJdk::build("field-diagnostics");
    let root =
        std::env::temp_dir().join(format!("javelin-field-diagnostics-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/p");
    std::fs::create_dir_all(&package).expect("package");
    std::fs::write(
        package.join("Order.java"),
        "package p;\n\npublic class Order {\n    int total;\n}\n",
    )
    .expect("Order");
    let reception = package.join("Reception.java");
    std::fs::write(
        &reception,
        "package p;\n\npublic class Reception {\n    int count() { return 1; }\n\n    \
         int measure(Order order) {\n        return order.totl + order.total + helpr();\n    }\n\n    \
         class Desk {\n        int ask() { return count(); }\n    }\n}\n",
    )
    .expect("Reception");

    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let uri = editor.open(&reception);
    let flagged = editor.diagnostics_until(&uri, |found| found.len() == 2);
    let (line, field) = editor.find(&uri, "return order.");
    let (_, call) = editor.find(&uri, "order.total + ");
    let codes: Vec<(u64, u64, &str)> = flagged
        .iter()
        .map(|found| {
            (
                found["range"]["start"]["line"].as_u64().expect("line"),
                found["range"]["start"]["character"]
                    .as_u64()
                    .expect("character"),
                found["code"].as_str().expect("code"),
            )
        })
        .collect();
    assert_eq!(
        codes,
        vec![
            (line.into(), field.into(), "unresolved-field"),
            (line.into(), call.into(), "unresolved-method"),
        ],
        "{flagged:?}"
    );
    assert!(
        flagged[0]["message"]
            .as_str()
            .is_some_and(|message| message.contains("p.Order") && message.contains("totl")),
        "{flagged:?}"
    );

    let (line, character) = editor.find(&uri, "return count");
    let (target, at) = editor
        .definition_at(&uri, line, character - 1)
        .expect("the nested class's call reaches the outer class's method");
    assert_eq!(target, uri);
    assert_eq!(at, 3, "`int count()` in Reception");

    let (line, character) = editor.find(&uri, "return order.tot");
    editor.type_at(&uri, line, character, "a");
    let (line, character) = editor.find(&uri, "+ help");
    editor.type_at(&uri, line, character, "e");
    editor.diagnostics_until(&uri, |found| {
        found.len() == 1 && found[0]["code"] == "unresolved-method"
    });
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_hallucinated_method_is_flagged_in_a_loose_project_and_fixing_it_clears_the_flag() {
    let jdk = SyntheticJdk::build("member-diagnostics");
    let root =
        std::env::temp_dir().join(format!("javelin-member-diagnostics-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/p");
    std::fs::create_dir_all(&package).expect("package");
    let reception = package.join("Reception.java");
    std::fs::write(
        &reception,
        "package p;\n\npublic class Reception {\n    int measure(String name) {\n        \
         return name.lenth() + name.length();\n    }\n}\n",
    )
    .expect("Reception");

    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let uri = editor.open(&reception);
    let flagged = editor.diagnostics_until(&uri, |found| !found.is_empty());
    assert_eq!(flagged.len(), 1, "only the hallucinated call: {flagged:?}");
    let (line, character) = editor.find(&uri, "return name.");
    assert_eq!(flagged[0]["range"]["start"]["line"], line);
    assert_eq!(flagged[0]["range"]["start"]["character"], character);
    assert_eq!(
        flagged[0]["range"]["end"]["character"],
        character + "lenth".len() as u32
    );
    assert_eq!(flagged[0]["code"], "unresolved-method");
    assert_eq!(flagged[0]["severity"], 2, "a warning, never an error");
    assert!(
        flagged[0]["message"]
            .as_str()
            .is_some_and(|message| message.contains("java.lang.String")),
        "{flagged:?}"
    );

    let (line, character) = editor.find(&uri, "return name.len");
    editor.type_at(&uri, line, character, "g");
    editor.diagnostics_until(&uri, <[serde_json::Value]>::is_empty);
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_hallucinated_method_after_a_workspace_getter_is_flagged() {
    let jdk = SyntheticJdk::build("workspace-chains");
    let root =
        std::env::temp_dir().join(format!("javelin-workspace-chains-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/p");
    std::fs::create_dir_all(&package).expect("package");
    std::fs::write(
        package.join("Entity.java"),
        "package p;\n\npublic class Entity {\n    public int size() { return 1; }\n}\n",
    )
    .expect("Entity");
    std::fs::write(
        package.join("Repo.java"),
        "package p;\n\npublic class Repo {\n    public Entity find() { return null; }\n}\n",
    )
    .expect("Repo");
    let reception = package.join("Reception.java");
    std::fs::write(
        &reception,
        "package p;\n\npublic class Reception {\n    int measure(Repo repo) {\n        \
         return repo.find().nope() + repo.find().size();\n    }\n}\n",
    )
    .expect("Reception");

    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let uri = editor.open(&reception);
    let flagged = editor.diagnostics_until(&uri, |found| !found.is_empty());
    assert_eq!(flagged.len(), 1, "only the hallucinated call: {flagged:?}");
    let (line, character) = editor.find(&uri, "return repo.find().");
    assert_eq!(flagged[0]["range"]["start"]["line"], line);
    assert_eq!(flagged[0]["range"]["start"]["character"], character);
    assert_eq!(flagged[0]["code"], "unresolved-method");
    assert!(
        flagged[0]["message"]
            .as_str()
            .is_some_and(|message| message.contains("p.Entity") && message.contains("nope")),
        "{flagged:?}"
    );
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_for_each_var_and_a_class_literal_are_typed() {
    let jdk = SyntheticJdk::build("for-each-var");
    let root = std::env::temp_dir().join(format!("javelin-for-each-var-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/p");
    std::fs::create_dir_all(&package).expect("package");
    let entity = package.join("Entity.java");
    std::fs::write(
        &entity,
        "package p;\n\npublic class Entity {\n    public int size() { return 1; }\n}\n",
    )
    .expect("Entity");
    let reception = package.join("Reception.java");
    std::fs::write(
        &reception,
        "package p;\n\nimport java.util.List;\n\npublic class Reception {\n    \
         static <T> T attach(Class<T> type) { return null; }\n\n    \
         int measure(List<Entity> all, Entity[] some) {\n        \
         for (var each : all) { each.size(); }\n        \
         for (var one : some) { one.size(); }\n        \
         return Entity.class.cast(null).size() + Entity.class.getName().length();\n    }\n}\n",
    )
    .expect("Reception");

    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let uri = editor.open(&reception);
    classpath_ready(&mut editor);
    assert!(!picked(&editor.workspace_symbols("Entity")).is_empty());
    let entity_uri = file_uri(&entity);
    for context in ["each.si", "one.si", "cast(null).si"] {
        let (line, character) = editor.find(&uri, context);
        let (target, at) = editor
            .definition_at(&uri, line, character)
            .unwrap_or_else(|| panic!("{context} jumps"));
        assert_eq!(target, entity_uri, "{context}");
        assert_eq!(at, 3, "{context}: `size` in Entity");
    }
    let (line, character) = editor.find(&uri, "getName().len");
    let (target, _) = editor
        .definition_at(&uri, line, character)
        .expect("`Entity.class.getName()` is a `String`");
    assert!(target.ends_with("java/lang/String.java"), "{target}");
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// Tier 2, slice 9: an annotation's element key names the annotation type's element, in another
/// file, and never a field the annotated class declares by the same name.
#[test]
fn an_annotation_elements_key_jumps_to_the_element() {
    let jdk = SyntheticJdk::build("annotation-element");
    let root =
        std::env::temp_dir().join(format!("javelin-annotation-element-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/p");
    std::fs::create_dir_all(&package).expect("package");
    let column = package.join("Column.java");
    std::fs::write(
        &column,
        "package p;\n\npublic @interface Column {\n    String name();\n    int length();\n}\n",
    )
    .expect("Column");
    let entity = package.join("Entity.java");
    std::fs::write(
        &entity,
        "package p;\n\npublic class Entity {\n    \
         @Column(name = \"n\", length = 3)\n    String name;\n}\n",
    )
    .expect("Entity");

    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let uri = editor.open(&entity);
    classpath_ready(&mut editor);
    assert!(!picked(&editor.workspace_symbols("Column")).is_empty());
    let column_uri = file_uri(&column);
    for (context, line) in [("(na", 3), (", len", 4)] {
        let (at_line, character) = editor.find(&uri, context);
        let (target, at) = editor
            .definition_at(&uri, at_line, character)
            .unwrap_or_else(|| panic!("{context} jumps"));
        assert_eq!(target, column_uri, "{context}: the element, not the field");
        assert_eq!(at, line, "{context}");
    }

    let at_entity = |found: &[(String, u32)], line: u32| {
        found
            .iter()
            .any(|(uri, at)| uri.ends_with("/p/Entity.java") && *at == line)
    };
    let declared = editor.open(&column);
    let (line, character) = editor.find(&declared, "String na");
    let found = editor.references_until(&declared, line, character, |found| at_entity(found, 3));
    assert!(at_entity(&found, 3), "the key is a use: {found:?}");
    assert!(!at_entity(&found, 4), "the field is not: {found:?}");
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

// ---- Tier 3, slice 6: JDK and JAR supertypes in the type hierarchy ----------

/// `class Names extends ArrayList<String>` used to have no supertype: the
/// JDK's `ArrayList` is one now, at its name in the extracted entry, and it
/// expands to the JDK's own supertypes and back down to `Names`.
#[test]
fn a_workspace_class_expands_up_into_the_jdk_and_back_down() {
    let jdk = SyntheticJdk::build("hierarchy-jdk");
    let root = std::env::temp_dir().join(format!("javelin-hierarchy-jdk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/main/java/p");
    std::fs::create_dir_all(&package).expect("package");
    let names = package.join("Names.java");
    std::fs::write(
        &names,
        "package p;\n\nimport java.util.ArrayList;\n\npublic class Names extends ArrayList<String> {}\n",
    )
    .expect("Names");

    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let uri = editor.open(&names);
    let (line, character) = editor.find(&uri, "class Na");
    let position = serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character }
    });
    let prepared =
        editor
            .server()
            .eventually("textDocument/prepareTypeHierarchy", &position, |answer| {
                answer["result"]
                    .as_array()
                    .is_some_and(|items| !items.is_empty())
            });
    let item = prepared["result"][0].clone();
    assert_eq!(item["name"], "Names", "{prepared}");

    // The JDK's world lands after the index: ask until it has.
    let above = editor.server().eventually(
        "typeHierarchy/supertypes",
        &serde_json::json!({ "item": item }),
        |answer| {
            answer["result"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
        },
    );
    let items = above["result"].as_array().expect("supertypes");
    assert_eq!(items.len(), 1, "{above}");
    assert_eq!(items[0]["name"], "ArrayList");
    assert!(
        items[0]["uri"]
            .as_str()
            .is_some_and(|uri| uri.ends_with("java/util/ArrayList.java")),
        "the extracted entry: {above}"
    );
    assert_eq!(items[0]["selectionRange"]["start"]["line"], 2, "its name");

    let list = editor.server().eventually(
        "typeHierarchy/supertypes",
        &serde_json::json!({ "item": items[0] }),
        |answer| answer["result"].is_array(),
    );
    let mut names: Vec<&str> = list["result"]
        .as_array()
        .expect("the JDK's own supertypes")
        .iter()
        .filter_map(|item| item["name"].as_str())
        .collect();
    names.sort();
    assert_eq!(names, vec!["List", "Object"], "{list}");

    let below = editor.server().eventually(
        "typeHierarchy/subtypes",
        &serde_json::json!({ "item": items[0] }),
        |answer| answer["result"].is_array(),
    );
    assert_eq!(below["result"][0]["name"], "Names", "{below}");
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

// ---- Tier 4, slice 1: method references in inference ----------------------

/// A method reference types what `map` returns, as a lambda's body does:
/// unbound on a raw generic type through the stream's element, and static
/// with a primitive result boxed — so the next lambda's parameter is typed.
#[test]
fn a_method_reference_types_the_generic_call_it_is_passed_to() {
    let jdk = SyntheticJdk::build("method-references");
    let mut editor = Editor::launch_verbose(&jdk.options());
    let uri = editor.open(&greeter());
    let (line, character) = in_main(&editor, &uri);
    scratch(
        &mut editor,
        &uri,
        line,
        character,
        concat!(
            "\n        List<Map.Entry<String, Integer>> entries = null;",
            "\n        entries.stream().map(Map.Entry::getKey).map(k -> k.length());",
            "\n        List<String> names = null;",
            "\n        names.stream().map(Integer::parseInt).map(i -> i.intValue());",
        ),
    );
    classpath_ready(&mut editor);
    let (line, character) = editor.find(&uri, "k -> k.lengt");
    assert_eq!(
        editor.hover_at(&uri, line, character).as_deref(),
        Some("```java\nint length()\n```")
    );
    let (line, character) = editor.find(&uri, "i -> i.intVal");
    assert_eq!(
        editor.hover_at(&uri, line, character).as_deref(),
        Some("```java\nint intValue()\n```")
    );
    editor.shutdown();
}

// ---- Tier 4, slice 7: `@SuperBuilder` and `@Singular` ----------------------

/// A subclass's `@SuperBuilder` chain sets the superclass's field, and the
/// jump lands on it in the superclass's file; a `@Singular` field's singular
/// adder lands on the field it adds to.
#[test]
fn a_super_builder_chain_reaches_the_parents_field_and_a_singular_adder_its_field() {
    let jdk = SyntheticJdk::build("super-builder");
    let root = std::env::temp_dir().join(format!("javelin-super-builder-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/main/java/p");
    std::fs::create_dir_all(&package).expect("package");
    std::fs::write(
        package.join("Vehicle.java"),
        "package p;\n\nimport lombok.experimental.SuperBuilder;\n\n@SuperBuilder\n\
         public abstract class Vehicle {\n    private int wheels;\n}\n",
    )
    .expect("Vehicle");
    std::fs::write(
        package.join("Truck.java"),
        "package p;\n\nimport java.util.List;\nimport lombok.Singular;\n\
         import lombok.experimental.SuperBuilder;\n\n@SuperBuilder\n\
         public class Truck extends Vehicle {\n    @Singular private List<String> axles;\n}\n",
    )
    .expect("Truck");
    let depot = package.join("Depot.java");
    std::fs::write(
        &depot,
        "package p;\n\nclass Depot {\n    Truck ship() {\n        \
         return Truck.builder().axle(\"a\").wheels(4).build();\n    }\n}\n",
    )
    .expect("Depot");

    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let uri = editor.open(&depot);
    let (line, character) = editor.find(&uri, ".wheel");
    let position = serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character }
    });
    let jump = editor
        .server()
        .eventually("textDocument/definition", &position, |answer| {
            !answer["result"].is_null()
        });
    assert!(
        jump["result"]["uri"]
            .as_str()
            .is_some_and(|uri| uri.ends_with("/p/Vehicle.java")),
        "{jump}"
    );
    assert_eq!(
        jump["result"]["range"]["start"]["line"], 6,
        "the field's name"
    );
    let (line, character) = editor.find(&uri, ".axl");
    let (target, at) = editor
        .definition_at(&uri, line, character)
        .expect("the singular adder is sited at its field");
    assert!(target.ends_with("/p/Truck.java"), "{target}");
    assert_eq!(at, 8, "the field's name");
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

// ---- Tier 4, slice 6: explicit constructor calls and enum constants ---------

/// `super("a", 1)` jumps from its keyword to the superclass's constructor its
/// arguments select, in a file the editor never opened; references on that
/// constructor list the `super` call; and an enum constant's arguments are
/// hinted with its constructor's parameter names.
#[test]
fn an_explicit_constructor_call_jumps_is_listed_and_a_constant_is_hinted() {
    let jdk = SyntheticJdk::build("explicit-constructors");
    let root = std::env::temp_dir().join(format!(
        "javelin-explicit-constructors-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/main/java/p");
    std::fs::create_dir_all(&package).expect("package");
    let greeter = package.join("Greeter.java");
    std::fs::write(
        &greeter,
        "package p;\n\nclass Greeter {\n    Greeter(String name) {}\n    \
         Greeter(String name, int times) {}\n}\n",
    )
    .expect("sibling");
    let loud = package.join("Loud.java");
    std::fs::write(
        &loud,
        "package p;\n\nclass Loud extends Greeter {\n    Loud() {\n        \
         super(\"a\", 1);\n    }\n}\n",
    )
    .expect("subclass");
    let day = package.join("Day.java");
    std::fs::write(
        &day,
        "package p;\n\nenum Day {\n    MON(8);\n\n    Day(int hours) {}\n}\n",
    )
    .expect("enum");
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &root);
    let loud_uri = editor.open(&loud);
    classpath_ready(&mut editor);
    let at = serde_json::json!({
        "textDocument": { "uri": loud_uri },
        "position": { "line": 4, "character": 10 }
    });
    let jump = editor
        .server()
        .eventually("textDocument/definition", &at, |answer| {
            answer["result"]["uri"]
                .as_str()
                .is_some_and(|file| file.ends_with("p/Greeter.java"))
        });
    assert_eq!(
        (
            jump["result"]["range"]["start"]["line"].as_u64(),
            jump["result"]["range"]["start"]["character"].as_u64()
        ),
        (Some(4), Some(4)),
        "{jump}"
    );

    let greeter_uri = editor.open(&greeter);
    let found = editor.references_until(&greeter_uri, 4, 6, |found| {
        found
            .iter()
            .any(|(target, _)| target.ends_with("p/Loud.java"))
    });
    assert!(
        found
            .iter()
            .any(|(target, at)| target.ends_with("p/Loud.java") && *at == 4),
        "the super call in Loud.java: {found:?}"
    );

    let day_uri = editor.open(&day);
    let params = serde_json::json!({
        "textDocument": { "uri": day_uri },
        "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 20, "character": 0 } }
    });
    let answer = editor
        .server()
        .eventually("textDocument/inlayHint", &params, |answer| {
            answer["result"]
                .as_array()
                .is_some_and(|hints| hints.iter().any(|hint| hint["label"] == "hours:"))
        });
    let hint = answer["result"]
        .as_array()
        .and_then(|hints| hints.iter().find(|hint| hint["label"] == "hours:"))
        .expect("the constant's hint");
    assert_eq!(
        (
            hint["position"]["line"].as_u64(),
            hint["position"]["character"].as_u64()
        ),
        (Some(3), Some(8)),
        "{answer}"
    );
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

// ---- Tier 4, slice 5: references on library methods -------------------------

/// Who calls a JAR method: references on `box.names()` list the call in a
/// file the editor never opened, recorded by the worker under the member's
/// key, since a class file read without sources has no site to key it by.
#[cfg(unix)]
#[test]
fn a_jar_methods_references_include_a_call_in_a_file_never_opened() {
    use std::os::unix::fs::PermissionsExt as _;
    let jdk = SyntheticJdk::build("library-refs");
    let root = std::env::temp_dir().join(format!("javelin-library-refs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let package = root.join("src/main/java/com/example");
    std::fs::create_dir_all(&package).expect("package");
    std::fs::write(root.join("pom.xml"), "<project/>").expect("pom");
    let jar = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/classfiles/acme.jar")
        .canonicalize()
        .expect("the fixture JAR is checked in");
    let mvnw = root.join("mvnw");
    std::fs::write(
        &mvnw,
        format!(
            "#!/bin/sh\necho '[INFO] Dependencies classpath:'\necho '{}'\n",
            jar.display()
        ),
    )
    .expect("mvnw");
    std::fs::set_permissions(&mvnw, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let user = |class: &str| {
        format!(
            "package com.example;\n\nimport com.acme.Box;\n\npublic class {class} {{\n    \
             Object label(Box<String> box) {{\n        return box.names();\n    }}\n}}\n"
        )
    };
    let open = package.join("UsesBox.java");
    std::fs::write(&open, user("UsesBox")).expect("source");
    std::fs::write(package.join("AlsoBox.java"), user("AlsoBox")).expect("source");

    let mut editor = Editor::launch_verbose_in(&jdk.options_with_dependencies(), &root);
    let uri = editor.open(&open);
    let (line, character) = editor.find(&uri, "return box.nam");
    let found = editor.references_until(&uri, line, character, |found| {
        found
            .iter()
            .any(|(target, _)| target.ends_with("/com/example/AlsoBox.java"))
    });
    assert!(
        found
            .iter()
            .any(|(target, at)| target.ends_with("/com/example/AlsoBox.java") && *at == 6),
        "the call in AlsoBox.java: {found:?}"
    );
    assert!(
        found.contains(&(uri.clone(), 6)),
        "the open file's own: {found:?}"
    );
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

// ---- Tier 4, slice 4: bare inherited fields --------------------------------

/// A bare `count` in a subclass is the field its supertype declares in
/// another file: it jumps there, and references on that field list the use
/// in a file the editor never opened.
#[test]
fn a_bare_inherited_field_jumps_to_the_supertype_and_is_referenced_from_it() {
    let project = harness::editor::scratch_project("bare-fields");
    let package = project.join("src/main/java/com/example");
    let base = package.join("Counted.java");
    let sub = package.join("Tally.java");
    std::fs::write(
        &base,
        "package com.example;\n\npublic class Counted {\n    protected int count;\n}\n",
    )
    .expect("write");
    std::fs::write(
        &sub,
        "package com.example;\n\npublic class Tally extends Counted {\n    \
         int next() {\n        return count + 1;\n    }\n}\n",
    )
    .expect("write");
    let jdk = SyntheticJdk::build("bare-fields");
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &project);
    let uri = editor.open(&sub);
    let (line, character) = editor.find(&uri, "return coun");
    let (target, at) = editor
        .definition_at(&uri, line, character)
        .expect("a jump to the inherited field");
    assert!(target.ends_with("/com/example/Counted.java"), "{target}");
    assert_eq!(at, 3);

    let base_uri = editor.open(&base);
    let (line, character) = editor.find(&base_uri, "int coun");
    let found = editor.references_until(&base_uri, line, character, |found| {
        found
            .iter()
            .any(|(target, _)| target.ends_with("/com/example/Tally.java"))
    });
    assert!(
        found.contains(&(uri.clone(), 4)),
        "the bare use in Tally.java: {found:?}"
    );
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&project);
}

// ---- Tier 4, slice 3: inference toward a parameter -------------------------

/// Tier 4, slice 3: `Collectors.toList()` is inferred toward the `Collector`
/// `collect` takes, so the list it returns is a list of the stream's element.
#[test]
fn a_collector_is_inferred_toward_the_stream_it_collects() {
    let jdk = SyntheticJdk::build("collectors");
    let mut editor = Editor::launch_verbose(&jdk.options());
    let uri = editor.open(&greeter());
    let (line, character) = in_main(&editor, &uri);
    scratch(
        &mut editor,
        &uri,
        line,
        character,
        concat!(
            "\n        List<String> names = null;",
            "\n        names.stream().collect(Collectors.toList()).get(0).length();",
        ),
    );
    scratch(
        &mut editor,
        &uri,
        3,
        0,
        "import java.util.stream.Collectors;\n",
    );
    classpath_ready(&mut editor);
    let (line, character) = editor.find(&uri, "get(0).lengt");
    assert_eq!(
        editor.hover_at(&uri, line, character).as_deref(),
        Some("```java\nint length()\n```")
    );
    editor.shutdown();
}

// ---- Tier 4, slice 2: block lambda return types ----------------------------

/// A block lambda's own `return`s type what `map` returns, through a local
/// the block declares — so the next lambda's parameter is typed.
#[test]
fn a_block_lambdas_returns_type_the_generic_call_it_is_passed_to() {
    let jdk = SyntheticJdk::build("block-lambdas");
    let mut editor = Editor::launch_verbose(&jdk.options());
    let uri = editor.open(&greeter());
    let (line, character) = in_main(&editor, &uri);
    scratch(
        &mut editor,
        &uri,
        line,
        character,
        concat!(
            "\n        List<String> names = null;",
            "\n        names.stream().map(n -> {",
            "\n            String part = n.substring(1);",
            "\n            if (part == null) return null;",
            "\n            return part;",
            "\n        }).map(p -> p.length());",
        ),
    );
    classpath_ready(&mut editor);
    let (line, character) = editor.find(&uri, "p -> p.lengt");
    assert_eq!(
        editor.hover_at(&uri, line, character).as_deref(),
        Some("```java\nint length()\n```")
    );
    editor.shutdown();
}

// ---- Tier 5, slice 2: `case` constants -------------------------------------

/// `case MON` under a `Day` selector is `Day.MON` in another file, not the
/// `MON` field the switch's own class declares: it jumps there, and
/// references on the constant list the label in a file the editor never
/// opened.
#[test]
fn a_case_label_jumps_to_the_enum_constant_and_is_referenced_from_it() {
    let project = harness::editor::scratch_project("case-constants");
    let package = project.join("src/main/java/com/example");
    let day = package.join("Day.java");
    let planner = package.join("Planner.java");
    std::fs::write(
        &day,
        "package com.example;\n\npublic enum Day {\n    MON,\n    TUE\n}\n",
    )
    .expect("write");
    std::fs::write(
        &planner,
        "package com.example;\n\npublic class Planner {\n    static final int MON = 9;\n\n    \
         int hours(Day day) {\n        switch (day) {\n            case MON:\n                \
         return 8;\n            default:\n                return 0;\n        }\n    }\n}\n",
    )
    .expect("write");
    let jdk = SyntheticJdk::build("case-constants");
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &project);
    let uri = editor.open(&planner);
    let (line, character) = editor.find(&uri, "case MO");
    // Before the index holds `Day`, the selector may still be an enum.
    if let Some((target, _)) = editor.definition_at(&uri, line, character) {
        assert!(target.ends_with("/com/example/Day.java"), "{target}");
    }
    assert!(
        editor
            .logs()
            .wait_for("workspace ready", Duration::from_secs(30))
            .is_some()
    );
    let (target, at) = editor
        .definition_at(&uri, line, character)
        .expect("a jump to the enum constant");
    assert!(target.ends_with("/com/example/Day.java"), "{target}");
    assert_eq!(at, 3);

    let day_uri = editor.open(&day);
    let (line, character) = editor.find(&day_uri, "    MO");
    let found = editor.references_until(&day_uri, line, character, |found| {
        found
            .iter()
            .any(|(target, _)| target.ends_with("/com/example/Planner.java"))
    });
    assert!(
        found.contains(&(uri.clone(), 7)),
        "the label in Planner.java: {found:?}"
    );
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&project);
}

// ---- Tier 5, slice 3: a JDK argument against a dependency's parameter -------

/// slf4j's overloads, with `Marker` declared in another file as a JAR would
/// declare it: a `String` is no `Marker`, so `info("…", a, b)` jumps to
/// `info(String, Object, Object)` and `info("…", a, b, c)` to the variadic
/// one, not to the `Marker` overload of its arity.
#[test]
fn a_string_argument_rules_out_a_marker_overload() {
    let project = harness::editor::scratch_project("marker-overloads");
    let package = project.join("src/main/java/com/example");
    std::fs::write(
        package.join("Marker.java"),
        "package com.example;\n\npublic interface Marker {}\n",
    )
    .expect("write");
    std::fs::write(
        package.join("Logger.java"),
        "package com.example;\n\npublic interface Logger {\n    \
         void info(String format, Object a, Object b);\n    \
         void info(Marker marker, String format, Object a);\n    \
         void info(String format, Object... arguments);\n    \
         void info(Marker marker, String format, Object a, Object b);\n}\n",
    )
    .expect("write");
    let app = package.join("App.java");
    std::fs::write(
        &app,
        "package com.example;\n\npublic class App {\n    void run(Logger log, Logger out, Object a) {\n        \
         log.info(\"{} {}\", \"s\", a);\n        out.info(\"{} {} {}\", a, a, a);\n    }\n}\n",
    )
    .expect("write");
    let jdk = SyntheticJdk::build("marker-overloads");
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &project);
    let uri = editor.open(&app);
    for needle in ["workspace ready", "classpath ready"] {
        assert!(
            editor
                .logs()
                .wait_for(needle, Duration::from_secs(30))
                .is_some(),
            "{needle}"
        );
    }
    for (call, line) in [("log.inf", 3), ("out.inf", 5)] {
        let (at, character) = editor.find(&uri, call);
        let (target, landed) = editor
            .definition_at(&uri, at, character)
            .expect("a jump to one overload");
        assert!(target.ends_with("/com/example/Logger.java"), "{target}");
        assert_eq!(landed, line, "{call} on line {at}");
    }
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&project);
}

// ---- Tier 5, slice 4: explicit type arguments and generic builders ---------

/// `Map.<String, String>of(null, null)` binds `V` from what the call writes,
/// where `null` arguments bind nothing, and a generic type's `@Builder` is
/// generated: `Box.<String>builder()`'s chain keeps `T` as `String` to
/// `length()`.
#[test]
fn written_type_arguments_type_a_generic_call_and_a_generic_builder() {
    let project = harness::editor::scratch_project("type-arguments");
    let package = project.join("src/main/java/p");
    std::fs::create_dir_all(&package).expect("package");
    std::fs::write(
        package.join("Box.java"),
        "package p;\n\nimport lombok.Builder;\nimport lombok.Getter;\n\n@Builder\n@Getter\n\
         public class Box<T> {\n    private T item;\n}\n",
    )
    .expect("write");
    let app = package.join("App.java");
    std::fs::write(
        &app,
        "package p;\n\nimport java.util.Map;\n\nclass App {\n    int run() {\n        \
         int a = Map.<String, String>of(null, null).get(null).length();\n        \
         return a + Box.<String>builder().item(\"x\").build().getItem().length();\n    \
         }\n}\n",
    )
    .expect("write");
    let jdk = SyntheticJdk::build("type-arguments");
    let mut editor = Editor::launch_verbose_in(&jdk.options(), &project);
    let uri = editor.open(&app);
    for needle in ["workspace ready", "classpath ready"] {
        assert!(
            editor
                .logs()
                .wait_for(needle, Duration::from_secs(30))
                .is_some(),
            "{needle}"
        );
    }
    let (line, character) = editor.find(&uri, ".ite");
    let (target, at) = editor
        .definition_at(&uri, line, character)
        .expect("the builder's setter");
    assert!(target.ends_with("/p/Box.java"), "{target}");
    assert_eq!(at, 8, "the field the setter sets");
    for (needle, line) in [("null).lengt", 6), ("getItem().lengt", 7)] {
        let (at, character) = editor.find(&uri, needle);
        assert_eq!(at, line);
        let (target, _) = editor
            .definition_at(&uri, at, character)
            .unwrap_or_else(|| panic!("{needle} answers"));
        assert!(target.contains("String.java"), "{target}");
    }
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&project);
}
