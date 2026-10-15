//! `textDocument/typeDefinition` — jump to the declaration of the type a
//! value has: `order` in `order.ship()` lands on `class Order`, without the
//! reader finding where `order` was declared and what it was declared as.
//!
//! The type is the one a type hint would show, read as strictly and gated
//! as hard (`handlers/inlay_hint.rs`), because a jump the reader did not see
//! coming is as much a claim as a hint they did not ask for. It is placed as
//! definition places a type name: in this file at its name, in the
//! workspace through the index, and in a JDK or a JAR source through its
//! extracted entry.

use std::str::FromStr as _;

use javelin_core::{CancellationToken, Cancelled};
use javelin_document::DocumentId;
use javelin_semantic::TypeTarget;
use lsp_types::request::{GotoTypeDefinitionParams, GotoTypeDefinitionResponse};
use lsp_types::{Location, Uri};

use crate::ServerState;
use crate::handlers::{file_uri, to_lsp_range, to_position};

pub fn handle(
    state: &mut ServerState,
    params: &GotoTypeDefinitionParams,
    token: &CancellationToken,
) -> Result<Option<GotoTypeDefinitionResponse>, Cancelled> {
    token.check()?;
    let at = &params.text_document_position_params;
    let id = DocumentId::new(at.text_document.uri.as_str());
    Ok(located(state, &id, at.position).map(GotoTypeDefinitionResponse::Scalar))
}

fn located(
    state: &mut ServerState,
    id: &DocumentId,
    position: lsp_types::Position,
) -> Option<Location> {
    // Taken before the view, which borrows the state mutably.
    let externals = state.externals();
    let survey = crate::handlers::trusted_survey(state);
    // The view's borrow ends with this block, and the state is free for the
    // `&mut` that extracting an entry needs.
    let fqn = {
        let view = state.view(id)?;
        let offset = view.document.offset_at(to_position(position));
        let files = javelin_semantic::Files::single(id.clone(), view.syntax, view.scopes);
        let found = javelin_semantic::type_definition_at(
            &view.document,
            view.tree,
            &files,
            &externals,
            offset,
        )?;
        if crate::handlers::unclaimable(
            &view,
            crate::handlers::inlay_hint::lombok_short(
                view.syntax,
                view.document.text(),
                &externals,
                id,
            ),
            &externals,
            survey.as_deref(),
            offset,
            &found.written,
        ) {
            return None;
        }
        match found.target {
            TypeTarget::Declared(site) => {
                // Every `Files` the server builds is this one file.
                if &site.file != id {
                    return None;
                }
                let range = javelin_document::Range::new(
                    view.document.position_at(site.span.start_byte),
                    view.document.position_at(site.span.end_byte),
                );
                return Some(Location::new(
                    Uri::from_str(id.as_str()).ok()?,
                    to_lsp_range(range),
                ));
            }
            TypeTarget::External(fqn) => fqn,
        }
    };
    // The workspace's copy wins, and two of them answer nothing — the rule
    // `handlers::workspace_type_at` and `jdk_type_at` keep together.
    let workspace = state.workspace();
    let mut hits = workspace.with_fqn(&fqn);
    if let Some(ty) = hits.next() {
        return hits
            .next()
            .is_none()
            .then(|| {
                Some(Location::new(
                    file_uri(&ty.file)?,
                    to_lsp_range(ty.name_range),
                ))
            })
            .flatten();
    }
    let site = externals.type_site(&fqn)?;
    let simple = fqn.rsplit('.').next().unwrap_or(&fqn);
    crate::handlers::definition::sited(state, &site, simple)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::definition::tests::{URI, indexed, jdk_string, opened, scratch};
    use lsp_types::{Position, Range};

    fn params(line: u32, character: u32) -> GotoTypeDefinitionParams {
        GotoTypeDefinitionParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams::new(
                lsp_types::TextDocumentIdentifier::new(Uri::from_str(URI).expect("a URI")),
                Position::new(line, character),
            ),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }
    }

    fn go_to(state: &mut ServerState, line: u32, character: u32) -> Option<Location> {
        match handle(state, &params(line, character), &CancellationToken::new()).expect("live") {
            Some(GotoTypeDefinitionResponse::Scalar(location)) => Some(location),
            None => None,
            Some(other) => panic!("one location, not {other:?}"),
        }
    }

    fn walked(state: &mut ServerState, index: javelin_index::WorkspaceIndex, generated: &[&str]) {
        let mut survey = javelin_index::Survey::default();
        survey
            .generated
            .extend(generated.iter().map(|name| (*name).to_owned()));
        state.install_workspace(crate::workspace::Workspace {
            index: std::sync::Arc::new(index),
            elapsed: std::time::Duration::ZERO,
            refreshed: false,
            survey: Some(std::sync::Arc::new(survey)),
            absorbed: 0,
        });
    }

    const LOCAL: &str = "class B {}\nclass A { B b; }\n";

    #[test]
    fn a_type_this_file_declares_is_placed_at_its_name() {
        let mut state = opened(LOCAL);
        walked(&mut state, indexed(&[]), &[]);
        let location = go_to(&mut state, 1, 13).expect("B's name");
        assert_eq!(location.uri.as_str(), URI);
        assert_eq!(
            location.range,
            Range::new(Position::new(0, 6), Position::new(0, 7))
        );
    }

    /// Before the first walk a sibling may outrank what a name resolved to,
    /// and a generated name is one the walk never read.
    #[test]
    fn a_name_the_walk_has_not_vouched_for_answers_nothing() {
        let mut state = opened(LOCAL);
        assert_eq!(go_to(&mut state, 1, 13), None);
        walked(&mut state, indexed(&[]), &["B"]);
        assert_eq!(go_to(&mut state, 1, 13), None);
    }

    #[test]
    fn a_name_a_static_import_may_mean_answers_nothing() {
        let mut state = opened(&format!("import static p.Q.B;\n{LOCAL}"));
        walked(&mut state, indexed(&[]), &[]);
        assert_eq!(go_to(&mut state, 2, 13), None);
    }

    /// A local class's inherited field may shadow what its body names.
    #[test]
    fn nothing_inside_a_local_class_is_placed() {
        let mut state = opened("class B {}\nclass A { void m() { class L { B b; } } }\n");
        walked(&mut state, indexed(&[]), &[]);
        assert_eq!(go_to(&mut state, 1, 33), None);
    }

    /// What Lombok writes past the model may be a member a name means.
    #[test]
    fn nothing_in_a_file_lombok_writes_past_the_model_in_is_placed() {
        let mut state = opened(LOCAL);
        walked(&mut state, indexed(&[]), &[]);
        assert!(go_to(&mut state, 1, 13).is_some(), "the premise");
        let delegated = format!(
            "import lombok.experimental.Delegate;\n{}",
            LOCAL.replace("B b;", "@Delegate B b;")
        );
        let mut state = opened(&delegated);
        walked(&mut state, indexed(&[]), &[]);
        assert_eq!(go_to(&mut state, 2, 23), None);
    }

    const GREETER: &str = "package com.example;\nclass A { Greeter g; }\n";

    #[test]
    fn a_workspace_type_is_placed_where_the_index_saw_it_and_two_copies_answer_nothing() {
        let mut state = opened(GREETER);
        walked(
            &mut state,
            indexed(&[("com.example.Greeter", "/w/com/example/Greeter.java", 4)]),
            &[],
        );
        let location = go_to(&mut state, 1, 18).expect("the sibling's declaration");
        assert_eq!(location.uri.as_str(), "file:///w/com/example/Greeter.java");
        assert_eq!(
            location.range,
            Range::new(Position::new(4, 13), Position::new(4, 20))
        );
        walked(
            &mut state,
            indexed(&[
                ("com.example.Greeter", "/w/a/Greeter.java", 4),
                ("com.example.Greeter", "/w/b/Greeter.java", 9),
            ]),
            &[],
        );
        assert_eq!(go_to(&mut state, 1, 18), None);
    }

    /// `s` in `String s` lands where definition on `String` does.
    #[test]
    fn a_jdk_type_is_placed_in_its_extracted_entry() {
        let mut state = jdk_string(&scratch("type_definition_jdk"), true);
        walked(&mut state, indexed(&[]), &[]);
        let location = go_to(&mut state, 0, 17).expect("String's name");
        let definition = crate::handlers::definition::handle(
            &mut state,
            &lsp_types::GotoDefinitionParams {
                text_document_position_params: params(0, 12).text_document_position_params,
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
            &CancellationToken::new(),
        )
        .expect("live");
        assert_eq!(
            definition,
            Some(lsp_types::GotoDefinitionResponse::Scalar(location))
        );
    }
}
