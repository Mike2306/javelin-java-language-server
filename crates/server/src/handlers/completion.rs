//! `textDocument/completion` — the names that can be written at the cursor.
//!
//! Two lists, and they answer different questions. A bare prefix offers what
//! the open file puts in scope: the locals and parameters, and the members of
//! the types enclosing the cursor. After a `.` the list is the receiver's
//! members, and that one reaches past the buffer — a receiver typed from the
//! classpath answers with the members the JDK's facts record, already closed
//! over its supertypes, and a file-local class extending a framework one
//! answers with both halves in one list.
//!
//! Neither list waits for the JDK. A request is answered from whatever has
//! arrived: [`Feature::Completion`] is available from [`Readiness::File`],
//! which the server reaches on `didOpen`, and waiting for a classpath read
//! would make the popup the slowest thing in the editor. What the classpath's
//! absence changes is the flag below, not the answer.
//!
//! `isIncomplete` is that flag, and it is a proof rather than a hedge:
//! `false` entitles a client to filter the list locally as the user types and
//! to stop asking, so it is written only where every one of five things holds.
//! [`handle`] names them. Anything unproven leaves it `true`, which costs a
//! re-ask and never a wrong answer.
//!
//! ponytail: a bare-prefix list is always incomplete, and truthfully so — it
//! offers no classpath names at all, so `Strin` completes to nothing in a file
//! where `java.lang.String` is perfectly available. Offering type names from
//! the world is its own piece of work — a prefix search over the fact set, and
//! an import to insert with the name — and until it exists the flag on that
//! path cannot honestly be anything but `true`.
//!
//! ponytail: a `.` on a class the open file declares is nearly always
//! incomplete too, and for a reason worth knowing before it reads as a bug.
//! `extends java.lang.Object` is never written down and the member walk
//! follows written supertypes only, so `toString` is missing from that list —
//! which is a member missing, not a flag being coy. The upgrade is the walk's
//! (`javelin_semantic::members`), and it changes what the popup shows rather
//! than what it claims, so it is not this seam's to make.
//!
//! [`Feature::Completion`]: javelin_protocol::Feature::Completion
//! [`Readiness::File`]: javelin_protocol::Readiness::File

use javelin_core::{CancellationToken, Cancelled};
use javelin_document::DocumentId;
use javelin_parser::{Candidate, DefinitionKind};
use javelin_protocol::Answer;
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionList, CompletionParams, CompletionResponse,
    Documentation, MarkupContent, MarkupKind,
};

use crate::ServerState;
use crate::handlers::to_position;

/// Complete at one position.
///
/// A document that is not open, a cursor after a `.`, and a prefix nothing in
/// the file starts with all answer with an empty list. None of them is an error:
/// completion is requested mid-edit, so the buffer is rarely valid Java and a
/// miss is the ordinary case.
///
/// `isIncomplete` is the conjunction of five proofs. Four arrive together from
/// the engine, because only the engine can show them: the answer came
/// from the member query rather than the prefix one, the receiver's type
/// resolved, its member walk reached everything the type names, and the list
/// was not cut off at the candidate cap. The fifth is the classpath having
/// arrived, which is checked here because it is the server that knows. A
/// missing proof is not a failure — the list is offered either way, and the
/// client keeps asking.
pub fn handle(
    state: &mut ServerState,
    params: &CompletionParams,
    token: &CancellationToken,
) -> Result<CompletionResponse, Cancelled> {
    let offered = self::answer(state, params, token)?
        .value()
        .unwrap_or_default();
    // Two predicates over the same fact, deliberately not one. `answer` above
    // marks the response `Answer::Complete` on the classpath alone, which is
    // the seam `javelin-protocol` exists for and is read by nothing on the
    // wire. `isIncomplete` is the strictly stronger claim, and the classpath
    // is only its first conjunct — so it is read from the state that owns it
    // rather than off an `Answer` whose meaning belongs to the other seam.
    let complete = state.is_complete() && offered.whole;
    Ok(CompletionResponse::List(CompletionList {
        is_incomplete: !complete,
        items: offered.items,
    }))
}

/// A completion list, and whether the engine could show it was the whole of
/// what the position offers.
///
/// `whole` is everything but the classpath's arrival: [`handle`] adds that and
/// nothing else. `Default` is the honest one — no items, and no proof — which
/// is what an [`Answer`] carrying no value degrades to.
#[derive(Debug, Default)]
pub(crate) struct Offered {
    pub items: Vec<CompletionItem>,
    pub whole: bool,
}

/// The list, and how much of the engine it was answered from.
///
/// `Complete` here means the classpath has arrived, which is *not* the same as
/// the list being everything this position can offer — [`handle`] makes that
/// distinction, from [`Offered::whole`] and this. It stays on this seam
/// because a client-side retry policy has nowhere else to read it from.
pub(crate) fn answer(
    state: &mut ServerState,
    params: &CompletionParams,
    token: &CancellationToken,
) -> Result<Answer<Offered>, Cancelled> {
    token.check()?;
    let at = &params.text_document_position;
    let id = DocumentId::new(at.text_document.uri.as_str());
    let externals = state.externals();
    let (candidates, whole) = match state.view(&id) {
        Some(view) => {
            let offset = view.document.offset_at(to_position(at.position));
            // A member selection answers for itself, empty list included: the
            // cursor is after a `.`, and the locals in scope are not what was
            // asked for. Only a cursor that is not selecting a member falls
            // through to the file-local query.
            let files = javelin_semantic::Files::single(id.clone(), view.syntax, view.scopes);
            match javelin_semantic::member_completions_at(
                &view.document,
                view.tree,
                &files,
                &externals,
                offset,
            ) {
                Some(found) => (found.candidates, found.complete),
                // The prefix query offers no classpath names, so its list is
                // short by every type the world holds however warm the engine
                // is — see the module comment's `ponytail:`.
                None => (
                    javelin_parser::completions_at(
                        &view.document,
                        view.tree,
                        view.scopes,
                        view.syntax,
                        offset,
                    ),
                    false,
                ),
            }
        }
        None => (Vec::new(), false),
    };
    token.check()?;
    let items = candidates.into_iter().map(item).collect();
    Ok(crate::handlers::answer(
        Offered { items, whole },
        state.is_complete(),
    ))
}

fn item(candidate: Candidate) -> CompletionItem {
    CompletionItem {
        label: candidate.name,
        kind: Some(item_kind(candidate.kind)),
        detail: candidate.detail,
        documentation: candidate.doc.as_deref().and_then(documentation),
        ..CompletionItem::default()
    }
}

/// The same rendering hover uses, so the popup and the tooltip do not disagree
/// about what a doc comment says.
fn documentation(doc: &str) -> Option<Documentation> {
    let prose = crate::handlers::javadoc::render(doc);
    if prose.is_empty() {
        return None;
    }
    Some(Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value: prose,
    }))
}

fn item_kind(kind: DefinitionKind) -> CompletionItemKind {
    match kind {
        // LSP has no separate kind for a parameter or a pattern binding, and
        // clients that show one anyway key off VARIABLE.
        DefinitionKind::LocalVariable
        | DefinitionKind::Parameter
        | DefinitionKind::PatternBinding => CompletionItemKind::VARIABLE,
        DefinitionKind::Field => CompletionItemKind::FIELD,
        DefinitionKind::TypeParameter => CompletionItemKind::TYPE_PARAMETER,
        DefinitionKind::Type => CompletionItemKind::CLASS,
        DefinitionKind::Method => CompletionItemKind::METHOD,
        DefinitionKind::Constructor => CompletionItemKind::CONSTRUCTOR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classpath::Classpath;
    use javelin_classpath::{MemberFacts, MemberType, TypeFacts};
    use javelin_core::Config;

    const URI: &str = "file:///Completing.java";

    const COMPLETING: &str = "\
class Completing {
    /** How many. */
    private int count;

    int add(int by) {
        int running = count;
        c
    }
}
";

    fn opened(source: &str) -> ServerState {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        state.did_open(DocumentId::new(URI), 1, source.to_owned());
        state
    }

    fn params(uri: &str, line: u32, character: u32) -> CompletionParams {
        serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
        }))
        .expect("well-formed params")
    }

    fn complete(state: &mut ServerState, line: u32, character: u32) -> CompletionList {
        match handle(
            state,
            &params(URI, line, character),
            &CancellationToken::new(),
        )
        .expect("not cancelled")
        {
            CompletionResponse::List(list) => list,
            CompletionResponse::Array(items) => panic!("expected a list, got {items:?}"),
        }
    }

    /// `label kind detail` per item, so a whole popup reads as one string.
    fn rendered(list: &CompletionList) -> String {
        list.items
            .iter()
            .map(|item| {
                format!(
                    "{} {:?} {}",
                    item.label,
                    item.kind.expect("every item has a kind"),
                    item.detail.as_deref().unwrap_or("?")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_partial_name_completes_to_what_is_in_scope_with_its_kind_and_its_type() {
        let mut state = opened(COMPLETING);
        // `c` on line 6, cursor just past it.
        assert_eq!(rendered(&complete(&mut state, 6, 9)), "count Field int");
    }

    #[test]
    fn an_empty_prefix_offers_the_locals_the_parameters_and_the_enclosing_members() {
        let mut state = opened(COMPLETING);
        assert_eq!(
            rendered(&complete(&mut state, 6, 8)),
            "\
running Variable int
by Variable int
count Field int
Completing Class class Completing
add Method int add(int by)"
        );
    }

    #[test]
    fn a_documented_member_carries_its_doc_into_the_popup() {
        let mut state = opened(COMPLETING);
        let count = complete(&mut state, 6, 9).items.remove(0);
        assert_eq!(
            count.documentation,
            Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: "How many.".to_owned(),
            }))
        );
    }

    #[test]
    fn an_undocumented_candidate_carries_no_documentation() {
        let mut state = opened(COMPLETING);
        assert!(
            complete(&mut state, 6, 8)
                .items
                .iter()
                .filter(|item| item.label != "count")
                .all(|item| item.documentation.is_none())
        );
    }

    /// A statement about the prefix path, not about the classpath being
    /// absent. That list offers no classpath names however warm the engine is,
    /// so its arrival cannot make one whole — and marking a short list
    /// complete would entitle a client to filter it locally and stop asking.
    #[test]
    fn a_bare_prefix_list_is_returned_incomplete_even_with_the_classpath_in() {
        let mut state = opened(COMPLETING);
        assert!(complete(&mut state, 6, 8).is_incomplete);

        state.install_classpath(Classpath::hand_built(&["java.lang.String"]));
        let list = complete(&mut state, 6, 8);
        assert!(list.is_incomplete);
        assert!(!list.items.is_empty(), "and still the same list");
        assert!(
            complete(&mut state, 0, 0).is_incomplete,
            "including where there is nothing to offer"
        );
    }

    /// The list `member_completions_at` answers for a classpath-typed
    /// receiver whose facts carry no members: empty, and incomplete because
    /// the closure behind it is short.
    ///
    /// The fact is an *interface*, which is what makes this the realistic
    /// case rather than a special one: `sources.rs` writes down the
    /// `extends java.lang.Object` a class never writes, and gives an
    /// interface no supertype at all, so `ClasspathExternals::members`
    /// reaches for `Object`'s own fact — which this world does not hold. A
    /// list missing every member of `Object` is not one a client may cache as
    /// "nothing completes here".
    #[test]
    fn a_classpath_fact_carrying_no_members_offers_none() {
        let mut state = opened("import java.util.List;\nclass A { void go(List l) { l.; } }\n");
        state.install_classpath(Classpath::holding(vec![TypeFacts {
            constructors: Vec::new(),
            doc_span: None,
            name_start: 0,
            name_end: 0,
            fqn: "java.util.List".to_owned(),
            simple: "List".to_owned(),
            kind: javelin_classpath::DeclKind::Interface,
            type_parameters: vec!["E".to_owned()],
            supertype_arguments: Vec::new(),
            supertypes: Vec::new(),
            supertypes_complete: true,
            members_complete: true,
            members: Vec::new(),
            source: String::new(),
        }]));
        let list = complete(&mut state, 1, 30);
        assert!(list.items.is_empty(), "{:?}", rendered(&list));
        assert!(list.is_incomplete);
    }

    /// The list at the `|`, which is stripped before the document is opened.
    ///
    /// Marked rather than counted because these tests turn on the cursor being
    /// after a `.`, and a hand-counted column that drifts by one turns a
    /// member list into a prefix list — which is a different flag for a
    /// different reason.
    fn marked(source: &str, classpath: Option<Classpath>) -> CompletionList {
        let offset = source.find('|').expect("mark the cursor with |");
        let before = &source[..offset];
        let line = before.matches('\n').count() as u32;
        let character = (offset - before.rfind('\n').map_or(0, |at| at + 1)) as u32;
        let mut state = opened(&source.replace('|', ""));
        if let Some(classpath) = classpath {
            state.install_classpath(classpath);
        }
        complete(&mut state, line, character)
    }

    /// A receiver whose type only the classpath can settle, so both the
    /// resolution and the members come from the world.
    const EXTERNAL_RECEIVER: &str = "class A {\n  void m(String s) { s.| }\n}\n";

    /// A world holding `java.lang.String` and one method of it.
    ///
    /// Hand-built out of `TypeFacts` rather than read off the machine, and for
    /// the reasons `hover`'s identical fixture gives: a real JDK would depend
    /// on which one is installed and cost eleven seconds, and `classpath`'s own
    /// fixture helpers are private to that module.
    fn string_classpath() -> Classpath {
        let length = MemberFacts {
            type_parameters: Vec::new(),
            doc_span: None,
            parameter_names: Vec::new(),
            name: "length".to_owned(),
            kind: javelin_classpath::DeclKind::Method,
            is_static: false,
            is_abstract: false,
            is_public: true,
            declared: MemberType::Primitive("int".to_owned()),
            parameters: Vec::new(),
            variadic: false,
            name_start: 0,
            name_end: 0,
        };
        // `String` names its superclass and the world holds that fact, which
        // is the shape a real read produces: `sources.rs` synthesises the
        // `extends java.lang.Object` nobody writes, so a closure that stops
        // one type short is a gap this flag has to see.
        let object = TypeFacts {
            constructors: Vec::new(),
            doc_span: None,
            name_start: 0,
            name_end: 0,
            fqn: "java.lang.Object".to_owned(),
            simple: "Object".to_owned(),
            kind: javelin_classpath::DeclKind::Class,
            type_parameters: Vec::new(),
            supertype_arguments: Vec::new(),
            supertypes: Vec::new(),
            supertypes_complete: true,
            members_complete: true,
            members: Vec::new(),
            source: String::new(),
        };
        Classpath::holding(vec![
            TypeFacts {
                constructors: Vec::new(),
                doc_span: None,
                name_start: 0,
                name_end: 0,
                fqn: "java.lang.String".to_owned(),
                simple: "String".to_owned(),
                kind: javelin_classpath::DeclKind::Class,
                type_parameters: Vec::new(),
                supertype_arguments: Vec::new(),
                supertypes: vec!["java.lang.Object".to_owned()],
                supertypes_complete: true,
                members_complete: true,
                members: vec![length],
                source: String::new(),
            },
            object,
        ])
    }

    #[test]
    fn a_list_answered_before_the_classpath_arrives_is_returned_incomplete() {
        let list = marked(EXTERNAL_RECEIVER, None);
        assert!(list.items.is_empty(), "{:?}", rendered(&list));
        assert!(list.is_incomplete);
        // The first conjunct is checked at this seam and asserted here, and
        // with no world it is not the only one failing: nothing the engine can
        // walk is whole without one, because the `extends java.lang.Object`
        // every class has is never written down. Which is why the pair that
        // isolates the flag is this test and the next, on one buffer.
    }

    #[test]
    fn a_member_list_over_a_resolved_receiver_with_the_classpath_in_is_returned_complete() {
        // The same buffer, differing only in the classpath having arrived:
        // `s` is a `java.lang.String`, the world reports its closure closed,
        // and nothing was cut off.
        let list = marked(EXTERNAL_RECEIVER, Some(string_classpath()));
        assert_eq!(rendered(&list), "length Method int length()");
        assert!(!list.is_incomplete);
    }

    #[test]
    fn a_truncated_member_list_is_returned_incomplete() {
        // More members than a popup can show, so the list is cut at the
        // engine's candidate cap. Every item is right and the list is not all
        // of them, which is exactly what the flag is for.
        let mut source = String::from("class Big extends String {\n");
        for index in 0..600 {
            source.push_str(&format!("  int m{index};\n"));
        }
        source.push_str("}\nclass A { void m(Big b) { b.| } }\n");
        let list = marked(&source, Some(string_classpath()));
        assert_eq!(list.items.len(), 500, "the engine's candidate cap");
        assert!(list.is_incomplete);
    }

    #[test]
    fn an_unknown_receivers_empty_list_is_returned_incomplete() {
        // `Mystery` is a name no file here declares and this world does not
        // hold, so the empty list is a refusal rather than an answer. Marking
        // it complete would tell the client nothing completes at a position
        // the server simply could not read.
        let list = marked(
            "class A { void m(Mystery x) { x.| } }\n",
            Some(string_classpath()),
        );
        assert!(list.items.is_empty(), "{:?}", rendered(&list));
        assert!(list.is_incomplete);
    }

    #[test]
    fn completion_after_a_dot_is_empty_rather_than_every_local_in_scope() {
        let mut state = opened("class A {\n  void go(A other) {\n    int x = other.;\n  }\n}\n");
        let list = complete(&mut state, 2, 18);
        assert!(list.items.is_empty(), "{:?}", rendered(&list));
    }

    #[test]
    fn a_document_that_was_never_opened_completes_to_nothing_rather_than_failing() {
        let mut state = opened(COMPLETING);
        let answer = handle(
            &mut state,
            &params("file:///Absent.java", 0, 0),
            &CancellationToken::new(),
        )
        .expect("not cancelled");
        match answer {
            CompletionResponse::List(list) => assert!(list.items.is_empty()),
            CompletionResponse::Array(items) => panic!("expected a list, got {items:?}"),
        }
    }

    #[test]
    fn a_cancelled_completion_stops_instead_of_answering() {
        let mut state = opened(COMPLETING);
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            handle(&mut state, &params(URI, 6, 9), &token).err(),
            Some(Cancelled)
        );
    }

    #[test]
    fn completion_after_a_cross_file_receiver_offers_its_members() {
        let sources = crate::workspace::testing::indexed(
            "completion-cross-file-member",
            &[(
                "com/example/Greeter.java",
                "package com.example;\npublic class Greeter {\n    void greet() {}\n    int count;\n}\n",
            )],
        );
        let mut state = opened("package com.example;\nclass A { void m(Greeter g) { g. } }\n");
        state.install_workspace(crate::workspace::Workspace {
            index: std::sync::Arc::clone(&sources.index),
            elapsed: std::time::Duration::ZERO,
            refreshed: false,
            survey: None,
            absorbed: 0,
        });
        let labels: Vec<String> = complete(&mut state, 1, 32)
            .items
            .into_iter()
            .map(|item| item.label)
            .collect();
        assert!(labels.contains(&"greet".to_owned()), "{labels:?}");
        assert!(labels.contains(&"count".to_owned()), "{labels:?}");
    }

    #[test]
    fn a_local_class_extending_a_workspace_class_sees_the_workspace_half() {
        let sources = crate::workspace::testing::indexed(
            "completion-local-extends-workspace",
            &[(
                "com/example/Greeter.java",
                "package com.example;\npublic class Greeter {\n    void greet() {}\n}\n",
            )],
        );
        // A member after the `.`: a dangling one as the file's last token is
        // a tree-sitter recovery gap pinned in
        // `tests/integration/member_resolution.rs`, and not what this is about.
        let mut state = opened(
            "package com.example;\nclass Loud extends Greeter { void shout() { this. } void x() {} }\n",
        );
        state.install_workspace(crate::workspace::Workspace {
            index: std::sync::Arc::clone(&sources.index),
            elapsed: std::time::Duration::ZERO,
            refreshed: false,
            survey: None,
            absorbed: 0,
        });
        let labels: Vec<String> = complete(&mut state, 1, 49)
            .items
            .into_iter()
            .map(|item| item.label)
            .collect();
        assert!(labels.contains(&"shout".to_owned()), "its own: {labels:?}");
        assert!(
            labels.contains(&"greet".to_owned()),
            "and Greeter's: {labels:?}"
        );
    }
}
