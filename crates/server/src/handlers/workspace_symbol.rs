//! `workspace/symbol` — the types and members the workspace's own sources
//! declare.
//!
//! The first thing on the request path to read the workspace source index,
//! which until now was built, published and asked nothing. It reads it and
//! nothing else: no document is opened, no name is resolved and the classpath
//! is not consulted, so the answer is available the moment the background walk
//! lands and is exactly as old as that walk.
//!
//! Three things follow from that, and they are the whole shape of the feature.
//!
//! Types are listed and then members — a method, a field, an enum constant —
//! each under its declaring type as the container, so `Ctrl-T` on a method
//! name finds it. A member is listed where it is declared and not where it is
//! inherited, and a `private` one is listed too: no other file can select
//! it, but a reader navigates to private helpers more than to anything else.
//!
//! The answer is as fresh as the index, which follows the workspace: a save,
//! a watched change on disk and an open buffer's unsaved text are each
//! re-indexed after a quiet period, so a type typed and not saved is listed
//! and one that has moved is listed where it is now. See the `ponytail:`
//! below for the windows in which it is not.
//!
//! And a match is a substring of the *simple* name, ignoring ASCII case.
//! Clients re-rank whatever they are handed against their own fuzzy matcher,
//! so [`javelin_index::WorkspaceIndex::matching`] ranks only enough for the
//! cap to fall on the right side of the list.

use javelin_core::{CancellationToken, Cancelled};
use lsp_types::{Location, SymbolInformation, WorkspaceSymbolParams, WorkspaceSymbolResponse};

use crate::ServerState;
use crate::handlers::document_symbol::symbol_kind;
use crate::handlers::{file_uri, to_lsp_range};

/// How many symbols one answer may carry.
///
/// The same number the completion path caps candidates at, for the same
/// reason: a client that asked for `a` in a monorepo wants a list it can
/// render, and every client with a symbol picker filters what it is given
/// again. LSP has no `isIncomplete` for this request, so a truncated list is
/// indistinguishable from a whole one — which is why the cap is generous
/// rather than tight.
const MAX_RESULTS: usize = 500;

/// Every indexed type whose simple name contains `params.query`.
///
/// `Some` with an empty list rather than `None` for a query that matches
/// nothing: the two are the same to every client, and an empty list says "I
/// looked" where `null` says "I do not do this".
///
/// ponytail: the index lags the workspace by the worker's quiet period — a
/// query in the quarter-second after a keystroke answers from before it — and
/// a change on disk reaches it only through a client that watches files for
/// this server. A client that cannot register a watcher leaves a `git
/// checkout` made outside it unseen until the files are saved or the server
/// restarts. The upgrade for the second is a periodic re-walk, which the
/// incremental walk makes cheap; nothing here needs it with Zed or VS Code.
pub fn handle(
    state: &mut ServerState,
    params: &WorkspaceSymbolParams,
    token: &CancellationToken,
) -> Result<Option<WorkspaceSymbolResponse>, Cancelled> {
    token.check()?;
    let workspace = state.workspace();
    let mut out = Vec::new();
    for ty in workspace.matching(&params.query, MAX_RESULTS) {
        token.check()?;
        // A path that is not a URI and a `DeclKind` an outline would not show
        // are both dropped rather than rendered: a row a client cannot open,
        // or cannot name, is worse than a row that is not there. Neither is
        // reachable from the walk, which takes UTF-8 paths and records only
        // the five named type kinds.
        let (Some(uri), Some(kind)) = (file_uri(&ty.file), symbol_kind(ty.kind)) else {
            continue;
        };
        out.push(symbol(ty, uri, kind));
    }
    // Members after the types, under what is left of the cap: a client
    // re-ranks the list, so the order only decides which side of the cap a
    // row lands on, and a type is the likelier thing to have been asked for.
    let left = MAX_RESULTS.saturating_sub(out.len());
    for member in workspace.matching_members(&params.query, left) {
        token.check()?;
        let file = workspace.file(member.file);
        let (Some(uri), Some(kind)) = (file_uri(&file.path), symbol_kind(member.kind)) else {
            continue;
        };
        out.push(member_symbol(member, uri, kind));
    }
    Ok(Some(WorkspaceSymbolResponse::Flat(out)))
}

// `deprecated` is deprecated in LSP itself — `tags` replaced it — but the
// struct still has the field, so it has to be given a value to build one.
#[expect(
    deprecated,
    reason = "SymbolInformation::deprecated has no non-deprecated spelling"
)]
fn symbol(
    ty: &javelin_index::WorkspaceType,
    uri: lsp_types::Uri,
    kind: lsp_types::SymbolKind,
) -> SymbolInformation {
    SymbolInformation {
        name: ty.simple.clone(),
        kind,
        tags: None,
        deprecated: None,
        location: Location::new(uri, to_lsp_range(ty.name_range)),
        container_name: container(&ty.fqn, &ty.simple),
    }
}

#[expect(
    deprecated,
    reason = "SymbolInformation::deprecated has no non-deprecated spelling"
)]
fn member_symbol(
    member: &javelin_index::WorkspaceMember,
    uri: lsp_types::Uri,
    kind: lsp_types::SymbolKind,
) -> SymbolInformation {
    SymbolInformation {
        name: member.name.clone(),
        kind,
        tags: None,
        deprecated: None,
        location: Location::new(uri, to_lsp_range(member.name_range)),
        // The declaring type, fully qualified: two `greet`s in two classes
        // are told apart by it in the picker.
        container_name: Some(member.owner.clone()),
    }
}

/// What encloses a type, as a client shows it beside the name: the package for
/// a top-level type, the outer type for a nested one.
///
/// Taken off the fully-qualified name rather than stored, because the index
/// already spells the two the same way — `com.example.Outer.Inner` — so the
/// container is whatever precedes the simple name. `None` for a top-level type
/// in the default package, which has nothing in front of it.
fn container(fqn: &str, simple: &str) -> Option<String> {
    let enclosing = fqn.strip_suffix(simple)?.strip_suffix('.')?;
    (!enclosing.is_empty()).then(|| enclosing.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use javelin_core::Config;
    use javelin_document::{Position, Range};
    use javelin_index::{WorkspaceIndex, WorkspaceType};
    use javelin_parser::DeclKind;
    use lsp_types::SymbolKind;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn ty(fqn: &str, simple: &str, file: &str, line: u32) -> WorkspaceType {
        WorkspaceType {
            is_public: true,
            doc_span: None,
            fqn: fqn.to_owned(),
            simple: simple.to_owned(),
            kind: DeclKind::Class,
            file: PathBuf::from(file),
            name_start: 0,
            name_end: 0,
            decl_start: 0,
            decl_end: 0,
            name_range: Range::new(Position::new(line, 6), Position::new(line, 6 + 5)),
            supertypes: Vec::new(),
            synthetic: false,
            lombok: Default::default(),
            type_parameters: Vec::new(),
        }
    }

    fn with(types: Vec<WorkspaceType>) -> ServerState {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        state.install_workspace(crate::workspace::Workspace {
            index: Arc::new(WorkspaceIndex::new(types)),
            elapsed: std::time::Duration::ZERO,
            refreshed: false,
            survey: None,
            absorbed: 0,
        });
        state
    }

    fn symbols(state: &mut ServerState, query: &str) -> Vec<lsp_types::SymbolInformation> {
        let params = serde_json::from_value(serde_json::json!({ "query": query }))
            .expect("well-formed params");
        match handle(state, &params, &CancellationToken::new()).expect("not cancelled") {
            Some(WorkspaceSymbolResponse::Flat(flat)) => flat,
            other => panic!("expected a flat list, got {other:?}"),
        }
    }

    #[test]
    fn a_type_the_index_holds_is_found_by_a_substring_of_its_name() {
        let mut state = with(vec![ty(
            "com.example.Greeter",
            "Greeter",
            "/w/src/com/example/Greeter.java",
            3,
        )]);
        let found = symbols(&mut state, "reet");
        assert_eq!(found.len(), 1, "one match, got {found:?}");
        assert_eq!(found[0].name, "Greeter");
        assert_eq!(found[0].kind, SymbolKind::CLASS);
        assert_eq!(
            found[0].location.uri.as_str(),
            "file:///w/src/com/example/Greeter.java"
        );
        assert_eq!(found[0].location.range.start.line, 3);
        assert_eq!(found[0].location.range.start.character, 6);
        assert_eq!(found[0].container_name.as_deref(), Some("com.example"));
    }

    #[test]
    fn a_nested_types_container_is_the_type_around_it() {
        let mut state = with(vec![ty(
            "com.example.Outer.Inner",
            "Inner",
            "/w/Outer.java",
            5,
        )]);
        let found = symbols(&mut state, "Inner");
        assert_eq!(
            found[0].container_name.as_deref(),
            Some("com.example.Outer")
        );
    }

    #[test]
    fn a_type_in_the_default_package_has_no_container_to_name() {
        let mut state = with(vec![ty("Loose", "Loose", "/w/Loose.java", 0)]);
        assert_eq!(symbols(&mut state, "Loose")[0].container_name, None);
    }

    #[test]
    fn a_query_nothing_matches_answers_an_empty_list_rather_than_null() {
        let mut state = with(vec![ty("p.A", "A", "/w/A.java", 1)]);
        assert!(symbols(&mut state, "zzz").is_empty());
    }

    #[test]
    fn a_session_whose_walk_has_not_landed_answers_an_empty_list() {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        assert!(symbols(&mut state, "A").is_empty());
    }

    #[test]
    fn the_list_is_capped_rather_than_handing_a_client_the_whole_workspace() {
        let types = (0..MAX_RESULTS + 100)
            .map(|n| ty(&format!("p.A{n}"), &format!("A{n}"), "/w/A.java", 0))
            .collect();
        let mut state = with(types);
        assert_eq!(symbols(&mut state, "A").len(), MAX_RESULTS);
    }

    #[test]
    fn a_cancelled_request_stops_rather_than_answering() {
        let mut state = with(vec![ty("p.A", "A", "/w/A.java", 1)]);
        let params =
            serde_json::from_value(serde_json::json!({ "query": "A" })).expect("well-formed");
        let token = CancellationToken::new();
        token.cancel();
        assert!(handle(&mut state, &params, &token).is_err());
    }

    #[test]
    fn a_method_is_found_by_its_name_with_its_type_as_container() {
        let sources = crate::workspace::testing::indexed(
            "symbol-members",
            &[(
                "com/example/Greeter.java",
                "package com.example;\npublic class Greeter {\n    void greetAll() {}\n    int greetings;\n}\n",
            )],
        );
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        state.install_workspace(crate::workspace::Workspace {
            index: Arc::clone(&sources.index),
            elapsed: std::time::Duration::ZERO,
            refreshed: false,
            survey: None,
            absorbed: 0,
        });
        let found = symbols(&mut state, "greet");
        let rows: Vec<(&str, SymbolKind, Option<&str>)> = found
            .iter()
            .map(|symbol| {
                (
                    symbol.name.as_str(),
                    symbol.kind,
                    symbol.container_name.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("Greeter", SymbolKind::CLASS, Some("com.example")),
                ("greetAll", SymbolKind::METHOD, Some("com.example.Greeter")),
                ("greetings", SymbolKind::FIELD, Some("com.example.Greeter")),
            ]
        );
        let method = &found[1];
        assert_eq!(
            Some(method.location.uri.clone()),
            file_uri(&sources.path("com/example/Greeter.java"))
        );
        assert_eq!(method.location.range.start, lsp_types::Position::new(2, 9));
    }

    /// A reviewer navigates to private helpers constantly; a symbol search
    /// lists one even though no other file can select it.
    #[test]
    fn a_private_method_is_found_by_its_name() {
        let sources = crate::workspace::testing::indexed(
            "symbol-private",
            &[(
                "p/A.java",
                "package p;\npublic class A { private void helperOnly() {} }\n",
            )],
        );
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        state.install_workspace(crate::workspace::Workspace {
            index: Arc::clone(&sources.index),
            elapsed: std::time::Duration::ZERO,
            refreshed: false,
            survey: None,
            absorbed: 0,
        });
        let found = symbols(&mut state, "helperOnly");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].kind, SymbolKind::METHOD);
    }
}
