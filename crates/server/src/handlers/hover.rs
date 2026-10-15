//! `textDocument/hover` — the declaration behind the identifier under the
//! cursor.
//!
//! What is shown is the declaration *as written*: the signature source text and
//! the doc comment above it, if there is one. A member selection — `receiver.`
//! followed by a name — resolves against the receiver's type, reaching a
//! file-local supertype's members too, and reaching a type the classpath holds
//! where the open file declares no such type at all. A plain name is not
//! resolved against anything: its type here is the spelling the author used,
//! which is honest and, for a name declared a few lines up, is also the
//! answer.
//!
//! A classpath member is the one thing here that was never text: it is
//! rendered from facts, so its signature carries no parameter names and it has
//! no doc comment to show. That is a different path through this module rather
//! than a different-looking answer. Definition falls through at the same
//! position and to the same facts, and does something else with them entirely;
//! `answer` below says what.

use javelin_core::{CancellationToken, Cancelled};
use javelin_document::DocumentId;
use javelin_parser::Definition;
use javelin_protocol::Answer;
use lsp_types::{Hover, HoverContents, HoverParams, MarkupContent, MarkupKind};

use crate::ServerState;
use crate::handlers::{resolve_at, to_lsp_range, to_position};

pub fn handle(
    state: &mut ServerState,
    params: &HoverParams,
    token: &CancellationToken,
) -> Result<Option<Hover>, Cancelled> {
    Ok(answer(state, params, token)?.value().flatten())
}

/// The same answer, plus whether it is the best one there will be — see
/// [`crate::handlers::definition::answer`] for why that is not on the wire.
pub(crate) fn answer(
    state: &mut ServerState,
    params: &HoverParams,
    token: &CancellationToken,
) -> Result<Answer<Option<Hover>>, Cancelled> {
    token.check()?;
    let at = &params.text_document_position_params;
    let id = DocumentId::new(at.text_document.uri.as_str());
    // `resolve_at` first and unchanged, so every answer that had one keeps it;
    // only a position it cannot address at all falls through. A classpath
    // member is exactly such a position — `resolve_at` answers with a
    // `Definition`, whose spans are offsets into a file it names and whose
    // signature is a slice of that file's text, and an external member has
    // neither.
    //
    // Definition falls through at the same position and to the same
    // `ExternalSummary`; what each does with it is where the two part. Hover
    // is asked what the thing *is*, which the facts on the summary answer as
    // they stand. Definition is asked where to jump, so it takes the
    // summary's *site* instead and extracts that `src.zip` entry, which is how
    // a client is given a `file:` URI it can open. So the asymmetry is what
    // each question needs out of one answer, not one of them having been
    // forgotten.
    // The constructor a `new` selects ahead of the type its name names, as
    // definition asks.
    let found = match constructor(state, &id, at.position) {
        Some(found) => Some(found),
        None => match resolve_at(state, &id, at.position) {
            Some(found) => Some(hovered(markdown(&found), found.reference.range)),
            // The same two fall-throughs, in the same order and for the same
            // reason, as `definition`'s — and the pair below is where the two
            // features finally answer the *same* question rather than two halves
            // of one: definition sends the client to the declaration, hover says
            // what it is.
            None => external(state, &id, at.position)
                .or_else(|| workspace_type(state, &id, at.position))
                .or_else(|| classpath_type(state, &id, at.position)),
        },
    };
    token.check()?;
    let complete = found.is_some() || state.is_complete();
    Ok(crate::handlers::answer(found, complete))
}

/// The classpath or cross-file member the cursor is on, rendered from its
/// facts — and its Javadoc, where its site records one and the text there
/// still holds the member's name where the site says.
fn external(
    state: &mut ServerState,
    id: &DocumentId,
    position: lsp_types::Position,
) -> Option<Hover> {
    // Taken before the view, which borrows the state mutably — the same order,
    // and for the same reason, as `handlers::resolve_at`.
    let externals = state.externals();
    let view = state.view(id)?;
    let offset = view.document.offset_at(to_position(position));
    let files = javelin_semantic::Files::single(id.clone(), view.syntax, view.scopes);
    let found = javelin_semantic::external_member_at(
        &view.document,
        view.tree,
        &files,
        &externals,
        offset,
    )?;
    let mut markdown = format!("```java\n{}\n```", found.signature);
    if let Some(site) = &found.site {
        append_doc(state, site, &found.name, &mut markdown);
    }
    Some(hovered(markdown, found.reference.range))
}

/// The constructor the `new` under the cursor selects: as written where
/// this file declares it, from facts and their Javadoc otherwise.
fn constructor(
    state: &mut ServerState,
    id: &DocumentId,
    position: lsp_types::Position,
) -> Option<Hover> {
    let externals = state.externals();
    let survey = crate::handlers::trusted_survey(state);
    let summary = {
        let view = state.view(id)?;
        let offset = view.document.offset_at(to_position(position));
        let found = crate::handlers::constructor_in(
            &view,
            id,
            &externals,
            survey.as_deref(),
            offset,
            false,
            crate::handlers::inlay_hint::lombok_short(
                view.syntax,
                view.document.text(),
                &externals,
                id,
            ),
        )?;
        match found.target {
            javelin_semantic::ConstructorTarget::Local(definition) => {
                return Some(hovered(markdown(&definition), definition.reference.range));
            }
            javelin_semantic::ConstructorTarget::External(summary) => summary,
        }
    };
    let mut markdown = format!("```java\n{}\n```", summary.signature);
    if let Some(site) = &summary.site {
        append_doc(state, site, &summary.name, &mut markdown);
    }
    Some(hovered(markdown, summary.reference.range))
}

/// The Javadoc `site` records, rendered after `markdown`.
fn append_doc(
    state: &mut ServerState,
    site: &javelin_semantic::ExternalSite,
    name: &str,
    markdown: &mut String,
) {
    let Some(doc) = site.doc_span else {
        return;
    };
    let Some((_, text)) = crate::handlers::site_text(state, site) else {
        return;
    };
    if let Some(comment) =
        crate::handlers::doc_at(&text, name, (site.name_start, site.name_end), doc)
    {
        let prose = crate::handlers::javadoc::render(comment);
        if !prose.is_empty() {
            markdown.push_str("\n\n");
            markdown.push_str(&prose);
        }
    }
}

/// The workspace type the name under the cursor means, as the one line the
/// index can say about it.
///
/// `class com.example.Greeter`, and its Javadoc where the index recorded one:
/// the index records a type's name, its kind, where it is written and where
/// its comment is, and no modifiers and no type parameters.
fn workspace_type(
    state: &mut ServerState,
    id: &DocumentId,
    position: lsp_types::Position,
) -> Option<Hover> {
    let (reference, ty) = crate::handlers::workspace_type_at(state, id, position)?;
    let mut markdown = format!("```java\n{} {}\n```", keyword(ty.kind), ty.fqn);
    let site = javelin_semantic::ExternalSite {
        source: ty.file.to_string_lossy().into_owned(),
        name_start: ty.name_start,
        name_end: ty.name_end,
        name_range: Some(ty.name_range),
        doc_span: ty.doc_span,
    };
    append_doc(state, &site, &ty.simple, &mut markdown);
    Some(hovered(markdown, reference.range))
}

/// The JDK or JAR type the name under the cursor means, rendered the way
/// [`workspace_type`] renders one of the workspace's: the kind and the
/// fully-qualified name, which is all a fact says about the type itself.
///
/// Skipped for a name the workspace declares at all, for the reason
/// `handlers::jdk_type_at` gives: that name is the workspace's to answer, and
/// two copies there are an ambiguity the world does not break.
fn classpath_type(
    state: &mut ServerState,
    id: &DocumentId,
    position: lsp_types::Position,
) -> Option<Hover> {
    let (reference, fqn) = crate::handlers::type_name_at(state, id, position)?;
    let externals = state.externals();
    if externals.workspace().with_fqn(&fqn).next().is_some() {
        return None;
    }
    let facts = externals.world().facts(&fqn)?;
    let mut markdown = format!("```java\n{} {}\n```", keyword(facts.kind), facts.fqn);
    let simple = facts.simple.clone();
    if let Some(site) = externals.type_site(&fqn) {
        append_doc(state, &site, &simple, &mut markdown);
    }
    Some(hovered(markdown, reference.range))
}

/// How a type declaration of this kind begins in Java source.
///
/// The workspace walk records exactly five kinds, and four of them are named
/// here; `class` catches the fifth and, with it, anything a later walk might
/// add. That fallback is the only guess in this hover and it is a small one —
/// a kind this list has not learnt yet renders as the most common word for a
/// type, beside a fully-qualified name that is right either way.
fn keyword(kind: javelin_parser::DeclKind) -> &'static str {
    use javelin_parser::DeclKind;
    match kind {
        DeclKind::Interface => "interface",
        DeclKind::Enum => "enum",
        DeclKind::Record => "record",
        DeclKind::AnnotationType => "@interface",
        _ => "class",
    }
}

/// One hover over a range in the requested document.
///
/// The range is the identifier rather than the declaration: this is what the
/// editor highlights while the popup is open, and it is the one span an
/// external member has to give.
fn hovered(markdown: String, range: javelin_document::Range) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown,
        }),
        range: Some(to_lsp_range(range)),
    }
}

fn markdown(found: &Definition) -> String {
    let mut out = format!("```java\n{}\n```", found.signature);
    if let Some(doc) = &found.doc {
        let prose = crate::handlers::javadoc::render(doc);
        if !prose.is_empty() {
            out.push_str("\n\n");
            out.push_str(&prose);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classpath::Classpath;
    use javelin_classpath::{MemberFacts, MemberType, TypeFacts};
    use javelin_core::Config;
    use javelin_parser::DeclKind;
    use lsp_types::Position;

    const URI: &str = "file:///A.java";

    const DOCUMENTED: &str = "\
class A {
    /** How many. */
    private int count;

    /**
     * Add to the count.
     *
     * @param by how much
     */
    void add(int by) {
        count += by;
    }

    void plain() {
    }
}
";

    fn opened(source: &str) -> ServerState {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        state.did_open(DocumentId::new(URI), 1, source.to_owned());
        state
    }

    fn params(uri: &str, line: u32, character: u32) -> HoverParams {
        serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
        }))
        .expect("well-formed params")
    }

    fn hover(state: &mut ServerState, line: u32, character: u32) -> Option<Hover> {
        handle(
            state,
            &params(URI, line, character),
            &CancellationToken::new(),
        )
        .expect("not cancelled")
    }

    fn markdown_of(hover: &Hover) -> &str {
        match &hover.contents {
            HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }) => value,
            other => panic!("expected markdown, got {other:?}"),
        }
    }

    #[test]
    fn a_documented_method_shows_its_signature_and_its_doc_text() {
        let mut state = opened(DOCUMENTED);
        let hover = hover(&mut state, 9, 9).expect("a hover");
        assert_eq!(
            markdown_of(&hover),
            "```java\nvoid add(int by)\n```\n\nAdd to the count.\n\n**Parameters**\n- `by` — how much"
        );
    }

    #[test]
    fn an_undocumented_method_shows_only_its_signature() {
        let mut state = opened(DOCUMENTED);
        let hover = hover(&mut state, 13, 9).expect("a hover");
        assert_eq!(markdown_of(&hover), "```java\nvoid plain()\n```");
    }

    #[test]
    fn a_field_used_in_a_method_shows_the_declaration_it_was_written_as() {
        let mut state = opened(DOCUMENTED);
        let hover = hover(&mut state, 10, 8).expect("a hover");
        assert_eq!(
            markdown_of(&hover),
            "```java\nprivate int count;\n```\n\nHow many."
        );
    }

    #[test]
    fn the_hover_range_covers_the_identifier_under_the_cursor() {
        let mut state = opened("class A {\n  int x = 1;\n  int y = x;\n}\n");
        let hover = hover(&mut state, 2, 10).expect("a hover");
        assert_eq!(
            hover.range,
            Some(lsp_types::Range::new(
                Position::new(2, 10),
                Position::new(2, 11),
            ))
        );
    }

    #[test]
    fn a_name_this_file_does_not_declare_answers_null() {
        let mut state = opened("import java.util.List;\nclass A { List<String> items; }\n");
        assert!(hover(&mut state, 1, 11).is_none());
    }

    #[test]
    fn whitespace_and_keywords_answer_null() {
        let mut state = opened("class A {\n  int x = 1;\n}\n");
        assert!(hover(&mut state, 1, 2).is_none(), "on `int`");
        assert!(hover(&mut state, 1, 10).is_none(), "on the `1`");
    }

    #[test]
    fn a_document_that_was_never_opened_answers_null_rather_than_failing() {
        let mut state = opened("class A {}");
        let params = params("file:///Absent.java", 0, 6);
        assert_eq!(
            handle(&mut state, &params, &CancellationToken::new()).expect("not cancelled"),
            None
        );
    }

    #[test]
    fn a_cancelled_hover_stops_instead_of_answering() {
        let mut state = opened("class A { int x = 1; }");
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            handle(&mut state, &params(URI, 0, 14), &token).err(),
            Some(Cancelled)
        );
    }

    /// A world holding `java.lang.String` and one method of it.
    ///
    /// Hand-built rather than read off the machine: a test that read a real
    /// JDK would depend on which one is installed and cost eleven seconds.
    /// Built out of `TypeFacts` directly because `classpath`'s own fixture
    /// helpers are private to that module — this is the same shape they build.
    fn string_state(source: &str) -> ServerState {
        let mut state = opened(source);
        let length = MemberFacts {
            type_parameters: Vec::new(),
            doc_span: None,
            parameter_names: Vec::new(),
            name: "length".to_owned(),
            kind: DeclKind::Method,
            is_static: false,
            is_abstract: false,
            is_public: true,
            declared: MemberType::Primitive("int".to_owned()),
            parameters: Vec::new(),
            variadic: false,
            name_start: 0,
            name_end: 0,
        };
        state.install_classpath(Classpath::holding(vec![TypeFacts {
            is_public: true,
            constructors: Vec::new(),
            doc_span: None,
            name_start: 0,
            name_end: 0,
            fqn: "java.lang.String".to_owned(),
            simple: "String".to_owned(),
            kind: DeclKind::Class,
            type_parameters: Vec::new(),
            supertype_arguments: Vec::new(),
            supertypes: Vec::new(),
            supertypes_complete: true,
            members_complete: true,
            members: vec![length],
            source: String::new(),
        }]));
        state
    }

    /// `s.length` with the cursor in the name, on line 1.
    const SELECTING: &str = "class A {\n  void m(String s) { s.length; }\n}\n";

    #[test]
    fn hover_on_an_external_member_renders_its_signature() {
        let mut state = string_state(SELECTING);
        let found = hover(&mut state, 1, 24).expect("a hover");
        assert_eq!(markdown_of(&found), "```java\nint length()\n```");
        // The use site, which is what the editor highlights — and the only
        // span an external member has, since its declaration is in no buffer
        // this server opened.
        assert_eq!(
            found.range,
            Some(lsp_types::Range::new(
                Position::new(1, 23),
                Position::new(1, 29),
            ))
        );
        // Not vacuous: the same buffer with no classpath behind it answers
        // nothing, which is what every phase before this one answered.
        assert!(hover(&mut opened(SELECTING), 1, 24).is_none());
    }

    #[test]
    fn hover_on_an_external_member_carries_no_documentation() {
        // A type fact carries no comment, so the block is the whole answer and
        // no prose is invented to sit under it. The contrast is a declared
        // member, whose hover does append its doc text — so the absence here
        // is the facts having none rather than this path having lost the
        // ability to render one.
        let mut state = string_state(SELECTING);
        let external = markdown_of(&hover(&mut state, 1, 24).expect("a hover")).to_owned();
        assert!(external.ends_with("```"), "{external}");
        assert!(!external.contains("\n\n"), "{external}");
        let mut documented = opened(DOCUMENTED);
        assert!(
            markdown_of(&hover(&mut documented, 9, 9).expect("a hover")).contains("\n\n"),
            "a declared member still shows its doc text"
        );
    }

    /// What each of the two needs out of one `ExternalSummary`, asserted in one
    /// place so neither half can be read without the other.
    ///
    /// The world here holds the member and cannot say where it is written, and
    /// there is no archive behind it either. That is enough for hover, which
    /// wants the facts, and not for definition, which wants a file — so the
    /// asymmetry is visible without a `src.zip` anywhere near it.
    /// `definition.rs` owns the other direction: a site and an archive, and a
    /// location in the extracted entry.
    #[test]
    fn a_member_hover_renders_from_facts_where_definition_needs_a_file() {
        let mut state = string_state(SELECTING);
        assert!(hover(&mut state, 1, 24).is_some(), "hover answers it");
        let at: lsp_types::GotoDefinitionParams = serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": URI },
            "position": { "line": 1, "character": 24 },
        }))
        .expect("well-formed params");
        assert_eq!(
            crate::handlers::definition::handle(&mut state, &at, &CancellationToken::new())
                .expect("not cancelled"),
            None
        );
    }

    /// The type-level half of the cross-file slice, and the one line the
    /// index can honestly say. Definition answers the same position with a
    /// jump; the two are the same question asked twice, which is exactly the
    /// relationship `definition.rs` and this module have everywhere else.
    #[test]
    fn a_name_that_means_a_sibling_source_file_hovers_as_that_type() {
        let mut state = opened("package com.example;\nclass A { Greeter g; }\n");
        assert!(
            hover(&mut state, 1, 12).is_none(),
            "the premise: with no index the name means nothing"
        );

        state.install_workspace(crate::workspace::Workspace {
            index: std::sync::Arc::new(javelin_index::WorkspaceIndex::new(vec![
                javelin_index::WorkspaceType {
                    is_public: true,
                    doc_span: None,
                    fqn: "com.example.Greeter".to_owned(),
                    simple: "Greeter".to_owned(),
                    kind: DeclKind::Interface,
                    file: std::path::PathBuf::from("/w/com/example/Greeter.java"),
                    name_start: 0,
                    name_end: 0,
                    decl_start: 0,
                    decl_end: 0,
                    name_range: javelin_document::Range::new(
                        javelin_document::Position::new(4, 13),
                        javelin_document::Position::new(4, 20),
                    ),
                    supertypes: Vec::new(),
                    synthetic: false,
                    lombok: Default::default(),
                    type_parameters: Vec::new(),
                },
            ])),
            elapsed: std::time::Duration::ZERO,
            refreshed: false,
            survey: Some(std::sync::Arc::default()),
            absorbed: 0,
        });

        let hover = hover(&mut state, 1, 12).expect("a hover");
        // The kind the index recorded, not a guessed `class`, and the
        // fully-qualified name — which is what the reader does not have in
        // front of them.
        assert_eq!(
            markdown_of(&hover),
            "```java\ninterface com.example.Greeter\n```"
        );
        // The range is the identifier in *this* buffer, which is the one span
        // an answer about another file can give.
        assert_eq!(
            hover.range,
            Some(lsp_types::Range::new(
                Position::new(1, 10),
                Position::new(1, 17)
            ))
        );
    }

    /// A JDK or JAR type's own name renders as one, from its fact.
    #[test]
    fn a_name_that_means_a_classpath_type_hovers_as_that_type() {
        let mut state = string_state("class A { String s; }\n");
        state.install_workspace(crate::workspace::Workspace {
            index: std::sync::Arc::default(),
            elapsed: std::time::Duration::ZERO,
            refreshed: false,
            survey: Some(std::sync::Arc::default()),
            absorbed: 0,
        });
        let found = hover(&mut state, 0, 12).expect("a hover");
        assert_eq!(markdown_of(&found), "```java\nclass java.lang.String\n```");
        assert!(
            hover(&mut opened("class A { String s; }\n"), 0, 12).is_none(),
            "the premise: with no classpath the name means nothing"
        );
    }

    #[test]
    fn a_doc_comment_with_nothing_in_it_adds_nothing_to_the_hover() {
        let mut state = opened("class A {\n  /***/\n  int x;\n}\n");
        let hover = hover(&mut state, 2, 6).expect("a hover");
        assert_eq!(markdown_of(&hover), "```java\nint x;\n```");
    }

    #[test]
    fn hover_on_a_cross_file_member_renders_its_signature() {
        let sources = crate::workspace::testing::indexed(
            "hover-cross-file-member",
            &[(
                "com/example/Greeter.java",
                "package com.example;\npublic class Greeter {\n    String greet(int times) { return null; }\n}\n",
            )],
        );
        let mut state =
            opened("package com.example;\nclass A { void m(Greeter g) { g.greet(1); } }\n");
        state.install_workspace(crate::workspace::Workspace {
            index: std::sync::Arc::clone(&sources.index),
            elapsed: std::time::Duration::ZERO,
            refreshed: false,
            survey: Some(std::sync::Arc::default()),
            absorbed: 0,
        });
        let hover = hover(&mut state, 1, 34).expect("a hover");
        assert_eq!(markdown_of(&hover), "```java\nString greet(int times)\n```");
    }

    const DOCUMENTED_GREETER: &str = "package com.example;\n/** Says hello. */\npublic class Greeter {\n    \
                                      /**\n     * Greets.\n     * @param times how often\n     */\n    \
                                      String greet(int times) { return null; }\n}\n";

    fn with_index(sources: &crate::workspace::testing::Sources, text: &str) -> ServerState {
        let mut state = opened(text);
        state.install_workspace(crate::workspace::Workspace {
            index: std::sync::Arc::clone(&sources.index),
            elapsed: std::time::Duration::ZERO,
            refreshed: false,
            survey: Some(std::sync::Arc::default()),
            absorbed: 0,
        });
        state
    }

    /// A cross-file member and a cross-file type hover with their Javadoc,
    /// read from the file where the index saw the comment.
    #[test]
    fn hover_on_a_cross_file_member_and_type_renders_their_javadoc() {
        let sources = crate::workspace::testing::indexed(
            "hover-cross-file-doc",
            &[("com/example/Greeter.java", DOCUMENTED_GREETER)],
        );
        let mut state = with_index(
            &sources,
            "package com.example;\nclass A { void m(Greeter g) { g.greet(1); } }\n",
        );
        assert_eq!(
            markdown_of(&hover(&mut state, 1, 34).expect("a hover")),
            "```java\nString greet(int times)\n```\n\nGreets.\n\n**Parameters**\n- `times` — how often"
        );
        assert_eq!(
            markdown_of(&hover(&mut state, 1, 19).expect("a hover")),
            "```java\nclass com.example.Greeter\n```\n\nSays hello."
        );
    }

    /// The file changed after the walk: the offsets no longer find the
    /// member's name, so no comment is read — rather than whichever comment
    /// now sits there.
    #[test]
    fn a_comment_the_index_saw_before_an_edit_is_not_shown() {
        let sources = crate::workspace::testing::indexed(
            "hover-stale-doc",
            &[("com/example/Greeter.java", DOCUMENTED_GREETER)],
        );
        std::fs::write(
            sources.path("com/example/Greeter.java"),
            format!("// moved\n{DOCUMENTED_GREETER}"),
        )
        .expect("edit on disk");
        let mut state = with_index(
            &sources,
            "package com.example;\nclass A { void m(Greeter g) { g.greet(1); } }\n",
        );
        assert_eq!(
            markdown_of(&hover(&mut state, 1, 34).expect("a hover")),
            "```java\nString greet(int times)\n```"
        );
    }

    /// Lombok copies a field's Javadoc onto its getter, so hover does too.
    #[test]
    fn a_lombok_getter_hovers_with_its_fields_javadoc() {
        let sources = crate::workspace::testing::indexed(
            "hover-lombok-doc",
            &[(
                "com/example/Car.java",
                "package com.example;\nimport lombok.Getter;\npublic class Car {\n    \
                 /** The car's name. */\n    @Getter private String name;\n}\n",
            )],
        );
        let mut state = with_index(
            &sources,
            "package com.example;\nclass A { void m(Car c) { c.getName(); } }\n",
        );
        let rendered = hover(&mut state, 1, 31).expect("a hover");
        assert!(
            markdown_of(&rendered).ends_with("The car's name."),
            "{}",
            markdown_of(&rendered)
        );
    }

    /// An open buffer is what the index's rows for it were read from, so its
    /// comment is read from the buffer and not from disk.
    #[test]
    fn an_open_files_comment_is_read_from_its_buffer() {
        let sources = crate::workspace::testing::indexed(
            "hover-open-doc",
            &[("com/example/Greeter.java", DOCUMENTED_GREETER)],
        );
        let mut state = with_index(
            &sources,
            "package com.example;\nclass A { void m(Greeter g) { g.greet(1); } }\n",
        );
        let uri =
            crate::handlers::file_uri(&sources.path("com/example/Greeter.java")).expect("uri");
        state.did_open(
            DocumentId::new(uri.as_str()),
            1,
            DOCUMENTED_GREETER.replace("Greets.", "Waves.."),
        );
        assert!(
            markdown_of(&hover(&mut state, 1, 34).expect("a hover")).contains("Waves.."),
            "the buffer's comment"
        );
    }
}
