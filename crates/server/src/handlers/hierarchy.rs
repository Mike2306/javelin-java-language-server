//! `textDocument/implementation`, the type hierarchy and the call hierarchy:
//! the inverse edges, read out of the published workspace index.
//!
//! What the cursor is on is decided the way references decides it — the same
//! `target_at` — so the verbs cannot disagree about what a position names. A
//! type is its fully-qualified name; a field or a method is its site, the
//! declaring file's path and the offset of its name, which is the key the
//! member references are stored under.
//!
//! An item carries that key in `data`, so an expansion asks the index for it
//! rather than resolving a position again. Both of an item's ranges are the
//! name's: the index stores lines and characters for names only.
//!
//! ponytail: every position here is the index's, a quarter-second behind the
//! buffer after an edit — and a dependent that is an open, dirty buffer has
//! its references re-resolved from disk while its members come from the
//! buffer, so a call can be grouped under the wrong member until its next
//! edit. The upgrade is the one references names for its indexed half.
//! Implementation on a JDK or JAR *method* answers nothing: its overrides
//! would need that member's site, which a class-file fact does not have and
//! a JDK one keys by entry rather than by workspace path.

use std::collections::BTreeMap;
use std::path::Path;

use javelin_core::{CancellationToken, Cancelled};
use javelin_document::DocumentId;
use javelin_index::{WorkspaceIndex, WorkspaceMember, WorkspaceType};
use javelin_parser::DefinitionKind;
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    GotoDefinitionParams, GotoDefinitionResponse, Location, Range, SymbolKind,
    TextDocumentPositionParams, TypeHierarchyItem, TypeHierarchyPrepareParams,
    TypeHierarchySubtypesParams, TypeHierarchySupertypesParams,
};

use crate::ServerState;
use crate::classpath::ClasspathExternals;
use crate::handlers::document_symbol::symbol_kind;
use crate::handlers::references::{Target, named_types, target_at};
use crate::handlers::{file_uri, to_lsp_range, to_position};
use crate::hierarchy;

/// What the cursor names, as the index keys it.
enum Subject {
    Type(String),
    Member { source: String, start: u32 },
}

fn subject_at(
    state: &mut ServerState,
    at: &TextDocumentPositionParams,
) -> Option<(Subject, ClasspathExternals)> {
    let uri = at.text_document.uri.as_str();
    let id = DocumentId::new(uri);
    let externals = state.externals();
    let view = state.view(&id)?;
    let offset = view.document.offset_at(to_position(at.position));
    let named = named_types(&view);
    let subject = match target_at(&view, &id, &externals, &named, offset)? {
        Target::Type(fqn) => Subject::Type(fqn),
        Target::Decl {
            name_span,
            kind: DefinitionKind::Field | DefinitionKind::Method,
            ..
        } => Subject::Member {
            source: crate::workspace::path_of(uri)?
                .to_string_lossy()
                .into_owned(),
            start: name_span.start_byte,
        },
        // A generated member shares its field's or its type's site, whose
        // written member is another: not a subject for these verbs.
        Target::Member {
            name,
            source,
            start,
            ..
        } => {
            externals
                .workspace()
                .member_at(std::path::Path::new(&source), start)
                .filter(|written| written.name == name)?;
            Subject::Member { source, start }
        }
        Target::Decl { .. } | Target::Constructor { .. } => return None,
    };
    Some((subject, externals))
}

/// Every workspace type below the type under the cursor, or every override
/// of the method under it.
pub fn implementation(
    state: &mut ServerState,
    params: &GotoDefinitionParams,
    token: &CancellationToken,
) -> Result<Option<GotoDefinitionResponse>, Cancelled> {
    token.check()?;
    let Some((subject, externals)) = subject_at(state, &params.text_document_position_params)
    else {
        return Ok(None);
    };
    let index = externals.workspace();
    let locations: Vec<Location> = match subject {
        Subject::Type(fqn) => hierarchy::all_subtypes(&externals, index, &fqn)
            .into_iter()
            .filter_map(type_location)
            .collect(),
        Subject::Member { source, start } => {
            let Some(owner) = index
                .member_at(Path::new(&source), start)
                .map(|member| member.owner.clone())
            else {
                return Ok(None);
            };
            let Some(method) = hierarchy::member(&externals, &owner, start) else {
                return Ok(None);
            };
            hierarchy::overrides(&externals, index, &owner, &method)
                .into_iter()
                .filter_map(|(_, found)| {
                    let site = found.site?;
                    Some(Location::new(
                        file_uri(Path::new(&site.source))?,
                        to_lsp_range(site.name_range?),
                    ))
                })
                .collect()
        }
    };
    Ok(Some(GotoDefinitionResponse::Array(locations)))
}

fn type_location(ty: &WorkspaceType) -> Option<Location> {
    Some(Location::new(
        file_uri(&ty.file)?,
        to_lsp_range(ty.name_range),
    ))
}

/// The type under the cursor, as the one root of its hierarchy: the
/// workspace's, or a JDK or JAR type the workspace declares nowhere, named
/// through the gates definition on that name passes.
pub fn prepare_type_hierarchy(
    state: &mut ServerState,
    params: &TypeHierarchyPrepareParams,
    token: &CancellationToken,
) -> Result<Option<Vec<TypeHierarchyItem>>, Cancelled> {
    token.check()?;
    let at = &params.text_document_position_params;
    let Some((Subject::Type(fqn), externals)) = subject_at(state, at) else {
        return Ok(None);
    };
    if externals.workspace().with_fqn(&fqn).next().is_none() {
        let id = DocumentId::new(at.text_document.uri.as_str());
        let named = crate::handlers::type_name_at(state, &id, at.position);
        if named.is_none_or(|(_, named)| named != fqn) {
            return Ok(None);
        }
        return Ok(external_item(state, &externals, &fqn).map(|item| vec![item]));
    }
    Ok(one_type(externals.workspace(), &fqn)
        .and_then(type_item)
        .map(|item| vec![item]))
}

pub fn supertypes(
    state: &mut ServerState,
    params: &TypeHierarchySupertypesParams,
    token: &CancellationToken,
) -> Result<Option<Vec<TypeHierarchyItem>>, Cancelled> {
    token.check()?;
    let externals = state.externals();
    let index = externals.workspace();
    let Some(fqn) = type_data(&params.item) else {
        return Ok(None);
    };
    if index.with_fqn(fqn).next().is_none() {
        // A JDK type's supertypes are the JDK's: one the workspace also
        // declares is a vendored copy, and not what this fact means.
        let Some(facts) = externals
            .world()
            .facts(fqn)
            .filter(|_| !externals.world().rivalled(fqn))
        else {
            return Ok(None);
        };
        let above: Vec<String> = facts
            .supertypes
            .iter()
            .filter(|supertype| index.with_fqn(supertype).next().is_none())
            .cloned()
            .collect();
        return Ok(Some(external_items(state, &externals, &above, token)?));
    }
    let Some(ty) = one_type(index, fqn) else {
        return Ok(None);
    };
    let mut items: Vec<TypeHierarchyItem> = hierarchy::direct_supertypes(&externals, index, ty)
        .into_iter()
        .filter_map(type_item)
        .collect();
    if let Some((survey, modules)) = external_reading(state) {
        let above = hierarchy::external_supertypes(
            &externals,
            index,
            ty,
            &survey.generated,
            modules.as_ref(),
        );
        items.extend(external_items(state, &externals, &above, token)?);
    }
    Ok(Some(items))
}

/// What a workspace type's edge into the JDK or a JAR is read under: a
/// trusted survey's generated names and the build's modules, once every
/// dependency is in the world. Before that a JAR in the type's own package
/// can hold the name the JDK answers for now, so `Error` is not yet
/// `java.lang.Error`.
fn external_reading(
    state: &ServerState,
) -> Option<(
    std::sync::Arc<javelin_index::Survey>,
    Option<std::sync::Arc<crate::modules::Modules>>,
)> {
    let modules = state.whole_dependencies()?;
    Some((crate::handlers::trusted_survey(state)?, modules))
}

/// An item for each of `fqns` that can be placed; one that cannot — a JAR
/// type without sources — is left out rather than shown nowhere.
fn external_items(
    state: &mut ServerState,
    externals: &ClasspathExternals,
    fqns: &[String],
    token: &CancellationToken,
) -> Result<Vec<TypeHierarchyItem>, Cancelled> {
    let mut items = Vec::new();
    for fqn in fqns {
        token.check()?;
        items.extend(external_item(state, externals, fqn));
    }
    Ok(items)
}

/// A JDK or JAR type as an item, at its name in its extracted source.
fn external_item(
    state: &mut ServerState,
    externals: &ClasspathExternals,
    fqn: &str,
) -> Option<TypeHierarchyItem> {
    // Two JARs' versions of it: which one's source to open is not the
    // first one's to say.
    if externals.world().rivalled(fqn) {
        return None;
    }
    let facts = externals.world().facts(fqn)?;
    let site = externals.type_site(fqn)?;
    let location = crate::handlers::definition::sited(state, &site, &facts.simple)?;
    Some(TypeHierarchyItem {
        name: facts.simple.clone(),
        kind: symbol_kind(facts.kind).unwrap_or(SymbolKind::CLASS),
        tags: None,
        detail: fqn.rsplit_once('.').map(|(outer, _)| outer.to_owned()),
        uri: location.uri,
        range: location.range,
        selection_range: location.range,
        data: Some(serde_json::json!({ "fqn": fqn })),
    })
}

pub fn subtypes(
    state: &mut ServerState,
    params: &TypeHierarchySubtypesParams,
    token: &CancellationToken,
) -> Result<Option<Vec<TypeHierarchyItem>>, Cancelled> {
    token.check()?;
    let externals = state.externals();
    let index = externals.workspace();
    let Some(fqn) = type_data(&params.item) else {
        return Ok(None);
    };
    if index.with_fqn(fqn).next().is_none() {
        let Some((survey, modules)) = external_reading(state) else {
            return Ok(None);
        };
        return Ok(Some(
            hierarchy::external_subtypes(
                &externals,
                index,
                fqn,
                &survey.generated,
                modules.as_ref(),
            )
            .into_iter()
            .filter_map(type_item)
            .collect(),
        ));
    }
    Ok(Some(
        hierarchy::direct_subtypes(&externals, index, fqn)
            .into_iter()
            .filter_map(type_item)
            .collect(),
    ))
}

/// The one workspace type of this name; two is ambiguity, and answers none.
fn one_type<'a>(index: &'a WorkspaceIndex, fqn: &str) -> Option<&'a WorkspaceType> {
    let mut declared = index.with_fqn(fqn);
    match (declared.next(), declared.next()) {
        (Some(one), None) => Some(one),
        _ => None,
    }
}

fn type_item(ty: &WorkspaceType) -> Option<TypeHierarchyItem> {
    let range = to_lsp_range(ty.name_range);
    Some(TypeHierarchyItem {
        name: ty.simple.clone(),
        kind: symbol_kind(ty.kind).unwrap_or(SymbolKind::CLASS),
        tags: None,
        detail: ty.fqn.rsplit_once('.').map(|(outer, _)| outer.to_owned()),
        uri: file_uri(&ty.file)?,
        range,
        selection_range: range,
        data: Some(serde_json::json!({ "fqn": ty.fqn })),
    })
}

fn type_data(item: &TypeHierarchyItem) -> Option<&str> {
    item.data.as_ref()?.get("fqn")?.as_str()
}

/// The method or field under the cursor, as the one root of its calls.
pub fn prepare_call_hierarchy(
    state: &mut ServerState,
    params: &CallHierarchyPrepareParams,
    token: &CancellationToken,
) -> Result<Option<Vec<CallHierarchyItem>>, Cancelled> {
    token.check()?;
    let Some((Subject::Member { source, start }, externals)) =
        subject_at(state, &params.text_document_position_params)
    else {
        return Ok(None);
    };
    let index = externals.workspace();
    Ok(index
        .member_at(Path::new(&source), start)
        .and_then(|member| member_item(index, member))
        .map(|item| vec![item]))
}

/// Every recorded use of the item's member, grouped by the member each is
/// written inside — or the type, for a use outside every member.
///
/// ponytail: a JDK or JAR method's callers are not listed, because calls into
/// them are not recorded; the upgrade is the one `member_refs.rs` names.
///
/// ponytail: exact, where references follow `hierarchy::override_line` — a
/// call through an override is not an incoming call of the interface method.
/// The upgrade is grouping every site on that line here too.
pub fn incoming_calls(
    state: &mut ServerState,
    params: &CallHierarchyIncomingCallsParams,
    token: &CancellationToken,
) -> Result<Option<Vec<CallHierarchyIncomingCall>>, Cancelled> {
    token.check()?;
    let index = state.workspace();
    let Some((source, start)) = member_data(&params.item) else {
        return Ok(None);
    };
    let mut callers: BTreeMap<(String, u32, u32), (CallHierarchyItem, Vec<Range>)> =
        BTreeMap::new();
    for found in index.member_refs_to(source, start) {
        token.check()?;
        let path = &index.file(found.file).path;
        let Some(caller) = enclosing(&index, path, found.start_byte) else {
            continue;
        };
        let key = (
            caller.uri.as_str().to_owned(),
            caller.selection_range.start.line,
            caller.selection_range.start.character,
        );
        callers
            .entry(key)
            .or_insert_with(|| (caller, Vec::new()))
            .1
            .push(to_lsp_range(found.range));
    }
    Ok(Some(
        callers
            .into_values()
            .map(|(from, from_ranges)| CallHierarchyIncomingCall { from, from_ranges })
            .collect(),
    ))
}

/// Every recorded use inside the item's own declaration, grouped by the
/// member it lands on.
///
/// ponytail: calls into the JDK or a JAR are not recorded, so they are not
/// outgoing calls here.
pub fn outgoing_calls(
    state: &mut ServerState,
    params: &CallHierarchyOutgoingCallsParams,
    token: &CancellationToken,
) -> Result<Option<Vec<CallHierarchyOutgoingCall>>, Cancelled> {
    token.check()?;
    let index = state.workspace();
    let Some((source, start)) = member_data(&params.item) else {
        return Ok(None);
    };
    let path = Path::new(source);
    let Some(member) = index.member_at(path, start) else {
        return Ok(None);
    };
    let mut targets: BTreeMap<(String, u32), (CallHierarchyItem, Vec<Range>)> = BTreeMap::new();
    for found in index.member_refs_between(path, member.decl_start, member.decl_end) {
        token.check()?;
        let key = (found.target_source.clone(), found.target_start);
        if !targets.contains_key(&key) {
            let Some(item) = index
                .member_at(Path::new(&found.target_source), found.target_start)
                .and_then(|target| member_item(&index, target))
            else {
                continue;
            };
            targets.insert(key.clone(), (item, Vec::new()));
        }
        if let Some((_, ranges)) = targets.get_mut(&key) {
            ranges.push(to_lsp_range(found.range));
        }
    }
    Ok(Some(
        targets
            .into_values()
            .map(|(to, from_ranges)| CallHierarchyOutgoingCall { to, from_ranges })
            .collect(),
    ))
}

/// The innermost member of the file at `path` whose declaration holds
/// `offset`, or failing that the innermost type — a use in a field
/// initialiser is its field's, and one in an initialiser block its type's.
fn enclosing(index: &WorkspaceIndex, path: &Path, offset: u32) -> Option<CallHierarchyItem> {
    if let Some(member) = index.member_enclosing(path, offset) {
        return member_item(index, member);
    }
    let ty = index
        .types_in(path)
        .filter(|ty| ty.decl_start <= offset && offset < ty.decl_end)
        .min_by_key(|ty| ty.decl_end - ty.decl_start)?;
    let range = to_lsp_range(ty.name_range);
    Some(CallHierarchyItem {
        name: ty.simple.clone(),
        kind: symbol_kind(ty.kind).unwrap_or(SymbolKind::CLASS),
        tags: None,
        detail: Some(ty.fqn.clone()),
        uri: file_uri(&ty.file)?,
        range,
        selection_range: range,
        data: None,
    })
}

fn member_item(index: &WorkspaceIndex, member: &WorkspaceMember) -> Option<CallHierarchyItem> {
    let path = &index.file(member.file).path;
    let range = to_lsp_range(member.name_range);
    Some(CallHierarchyItem {
        name: member.name.clone(),
        kind: symbol_kind(member.kind).unwrap_or(SymbolKind::METHOD),
        tags: None,
        detail: Some(member.owner.clone()),
        uri: file_uri(path)?,
        range,
        selection_range: range,
        data: Some(serde_json::json!({
            "source": path.to_string_lossy(),
            "start": member.name_start,
        })),
    })
}

fn member_data(item: &CallHierarchyItem) -> Option<(&str, u32)> {
    let data = item.data.as_ref()?;
    let source = data.get("source")?.as_str()?;
    let start = u32::try_from(data.get("start")?.as_u64()?).ok()?;
    Some((source, start))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::testing::{Sources, indexed_with_member_refs};
    use javelin_core::Config;

    const SHAPE: &str = "package p;\npublic interface Shape {\n    double area();\n}\n";
    const CIRCLE: &str = "package p;\npublic class Circle implements Shape {\n    \
                          public double area() { return 0; }\n}\n";
    const REPORT: &str = "package p;\npublic class Report {\n    \
                          double total = new Circle().area();\n    \
                          double sum(Shape a, Shape b) {\n        return a.area() + b.area();\n    }\n}\n";

    fn sources() -> Sources {
        indexed_with_member_refs(
            "hierarchy-handlers",
            &[
                ("p/Shape.java", SHAPE),
                ("p/Circle.java", CIRCLE),
                ("p/Report.java", REPORT),
            ],
        )
    }

    fn open(sources: &Sources, relative: &str, text: &str) -> (ServerState, String) {
        let uri = file_uri(&sources.path(relative)).expect("a uri");
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        state.did_open(DocumentId::new(uri.as_str()), 1, text.to_owned());
        state.install_workspace(crate::workspace::Workspace {
            index: std::sync::Arc::clone(&sources.index),
            elapsed: std::time::Duration::ZERO,
            refreshed: false,
            survey: None,
            absorbed: 0,
        });
        (state, uri.as_str().to_owned())
    }

    fn at(uri: &str, text: &str, needle: &str) -> TextDocumentPositionParams {
        let offset = text.find(needle).expect("in the text");
        let before = &text[..offset];
        let line = before.matches('\n').count() as u32;
        let character = (offset - before.rfind('\n').map_or(0, |n| n + 1)) as u32;
        serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }))
        .expect("well-formed")
    }

    fn file_names(locations: &[Location]) -> Vec<(String, u32)> {
        locations
            .iter()
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
            .collect()
    }

    #[test]
    fn implementation_on_an_interface_and_its_method_lists_the_class_and_its_override() {
        let sources = sources();
        let (mut state, uri) = open(&sources, "p/Shape.java", SHAPE);
        let on = |needle: &str| GotoDefinitionParams {
            text_document_position_params: at(&uri, SHAPE, needle),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        let token = CancellationToken::new();
        let Some(GotoDefinitionResponse::Array(types)) =
            implementation(&mut state, &on("Shape {"), &token).expect("not cancelled")
        else {
            panic!("an array");
        };
        assert_eq!(file_names(&types), vec![("Circle.java".to_owned(), 1)]);
        let Some(GotoDefinitionResponse::Array(methods)) =
            implementation(&mut state, &on("area"), &token).expect("not cancelled")
        else {
            panic!("an array");
        };
        assert_eq!(file_names(&methods), vec![("Circle.java".to_owned(), 2)]);
    }

    #[test]
    fn the_type_hierarchy_expands_both_ways_through_item_data() {
        let sources = sources();
        let (mut state, uri) = open(&sources, "p/Shape.java", SHAPE);
        let token = CancellationToken::new();
        let params = TypeHierarchyPrepareParams {
            text_document_position_params: at(&uri, SHAPE, "Shape {"),
            work_done_progress_params: Default::default(),
        };
        let root = prepare_type_hierarchy(&mut state, &params, &token)
            .expect("not cancelled")
            .expect("Shape")
            .remove(0);
        assert_eq!(root.name, "Shape");
        let below = subtypes(
            &mut state,
            &TypeHierarchySubtypesParams {
                item: root,
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
            &token,
        )
        .expect("not cancelled")
        .expect("subtypes");
        let names: Vec<&str> = below.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, vec!["Circle"]);
        let above = supertypes(
            &mut state,
            &TypeHierarchySupertypesParams {
                item: below[0].clone(),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
            &token,
        )
        .expect("not cancelled")
        .expect("supertypes");
        let names: Vec<&str> = above.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, vec!["Shape"]);
    }

    /// `sum` calls `Shape.area` twice and is one caller with two ranges; the
    /// field initialiser calls `Circle.area` and is grouped under its field.
    #[test]
    fn incoming_calls_are_grouped_by_the_member_each_use_is_inside() {
        let sources = sources();
        let (mut state, uri) = open(&sources, "p/Shape.java", SHAPE);
        let token = CancellationToken::new();
        let prepare = |state: &mut ServerState, uri: &str, text: &str| {
            prepare_call_hierarchy(
                state,
                &CallHierarchyPrepareParams {
                    text_document_position_params: at(uri, text, "area"),
                    work_done_progress_params: Default::default(),
                },
                &token,
            )
            .expect("not cancelled")
            .expect("a method")
            .remove(0)
        };
        let incoming = |state: &mut ServerState, item: CallHierarchyItem| {
            incoming_calls(
                state,
                &CallHierarchyIncomingCallsParams {
                    item,
                    work_done_progress_params: Default::default(),
                    partial_result_params: Default::default(),
                },
                &token,
            )
            .expect("not cancelled")
            .expect("calls")
        };
        let shape = prepare(&mut state, &uri, SHAPE);
        let calls = incoming(&mut state, shape);
        let callers: Vec<(&str, usize)> = calls
            .iter()
            .map(|call| (call.from.name.as_str(), call.from_ranges.len()))
            .collect();
        assert_eq!(callers, vec![("sum", 2)]);

        let (mut state, uri) = open(&sources, "p/Circle.java", CIRCLE);
        let circle = prepare(&mut state, &uri, CIRCLE);
        let calls = incoming(&mut state, circle);
        let callers: Vec<&str> = calls.iter().map(|call| call.from.name.as_str()).collect();
        assert_eq!(callers, vec!["total"]);
    }

    #[test]
    fn outgoing_calls_are_the_uses_inside_the_declaration_grouped_by_target() {
        let sources = sources();
        let (mut state, uri) = open(&sources, "p/Report.java", REPORT);
        let token = CancellationToken::new();
        let sum = prepare_call_hierarchy(
            &mut state,
            &CallHierarchyPrepareParams {
                text_document_position_params: at(&uri, REPORT, "sum"),
                work_done_progress_params: Default::default(),
            },
            &token,
        )
        .expect("not cancelled")
        .expect("sum")
        .remove(0);
        let calls = outgoing_calls(
            &mut state,
            &CallHierarchyOutgoingCallsParams {
                item: sum,
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
            &token,
        )
        .expect("not cancelled")
        .expect("calls");
        let targets: Vec<(&str, &str, usize)> = calls
            .iter()
            .map(|call| {
                (
                    call.to.name.as_str(),
                    call.to.detail.as_deref().unwrap_or_default(),
                    call.from_ranges.len(),
                )
            })
            .collect();
        assert_eq!(targets, vec![("area", "p.Shape", 2)]);
    }

    /// Constructors are not members, so a call inside one is its type's.
    #[test]
    fn a_call_inside_a_constructor_is_grouped_under_its_type() {
        let builder = "package p;\npublic class Builder {\n    Builder(Shape s) {\n        s.area();\n    }\n}\n";
        let sources = indexed_with_member_refs(
            "hierarchy-constructor",
            &[("p/Shape.java", SHAPE), ("p/Builder.java", builder)],
        );
        let (mut state, uri) = open(&sources, "p/Shape.java", SHAPE);
        let token = CancellationToken::new();
        let item = prepare_call_hierarchy(
            &mut state,
            &CallHierarchyPrepareParams {
                text_document_position_params: at(&uri, SHAPE, "area"),
                work_done_progress_params: Default::default(),
            },
            &token,
        )
        .expect("not cancelled")
        .expect("area")
        .remove(0);
        let calls = incoming_calls(
            &mut state,
            &CallHierarchyIncomingCallsParams {
                item,
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
            &token,
        )
        .expect("not cancelled")
        .expect("calls");
        let callers: Vec<&str> = calls.iter().map(|call| call.from.name.as_str()).collect();
        assert_eq!(callers, vec!["Builder"]);
    }

    /// `data` comes back from the client: missing, malformed or out of range
    /// answers nothing and never panics.
    #[test]
    fn an_item_whose_data_is_missing_or_malformed_answers_nothing() {
        let sources = sources();
        let (mut state, uri) = open(&sources, "p/Shape.java", SHAPE);
        let token = CancellationToken::new();
        for data in [
            None,
            Some(serde_json::json!("junk")),
            Some(serde_json::json!({ "source": 7, "start": 0 })),
            Some(serde_json::json!({ "source": "/nowhere", "start": 1_u64 << 40 })),
        ] {
            let item: CallHierarchyItem = serde_json::from_value(serde_json::json!({
                "name": "x", "kind": 6, "uri": uri,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
                "data": data,
            }))
            .expect("an item");
            let incoming = incoming_calls(
                &mut state,
                &CallHierarchyIncomingCallsParams {
                    item: item.clone(),
                    work_done_progress_params: Default::default(),
                    partial_result_params: Default::default(),
                },
                &token,
            )
            .expect("not cancelled");
            assert!(incoming.is_none_or(|calls| calls.is_empty()), "{data:?}");
            let outgoing = outgoing_calls(
                &mut state,
                &CallHierarchyOutgoingCallsParams {
                    item,
                    work_done_progress_params: Default::default(),
                    partial_result_params: Default::default(),
                },
                &token,
            )
            .expect("not cancelled");
            assert!(outgoing.is_none_or(|calls| calls.is_empty()), "{data:?}");
        }
    }

    const MAPPER: &str =
        "package p;\npublic interface CarMapper {\n    String toDto(Car car);\n}\n";
    const MAPPER_IMPL: &str = "package p;\npublic class CarMapperImpl implements CarMapper {\n    \
                               public String toDto(Car car) { return null; }\n}\n";
    const USE: &str = "package p;\nclass Use {\n    CarMapperImpl impl;\n}\n";
    const OUTPUT: &str = "build/generated/sources/annotationProcessor/java/main/";

    /// A mapper interface is implemented by what MapStruct generated, which
    /// the walk reads out of the processor's output: implementation lists it
    /// and a name meaning it jumps into it, since the survey does not report
    /// an indexed name as one the index never read.
    #[test]
    fn a_processors_output_is_an_implementation_and_a_jump_target() {
        let impl_path = format!("{OUTPUT}p/CarMapperImpl.java");
        let sources = indexed_with_member_refs(
            "processor-output",
            &[
                ("p/Car.java", "package p;\npublic class Car {}\n"),
                ("p/CarMapper.java", MAPPER),
                (&impl_path, MAPPER_IMPL),
                ("p/Use.java", USE),
            ],
        );
        let (mut state, uri) = open(&sources, "p/CarMapper.java", MAPPER);
        let token = CancellationToken::new();
        let on = |uri: &str, text: &str, needle: &str| GotoDefinitionParams {
            text_document_position_params: at(uri, text, needle),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        let Some(GotoDefinitionResponse::Array(types)) =
            implementation(&mut state, &on(&uri, MAPPER, "CarMapper {"), &token)
                .expect("not cancelled")
        else {
            panic!("an array");
        };
        assert_eq!(
            file_names(&types),
            vec![("CarMapperImpl.java".to_owned(), 1)]
        );
        let Some(GotoDefinitionResponse::Array(methods)) =
            implementation(&mut state, &on(&uri, MAPPER, "toDto"), &token).expect("not cancelled")
        else {
            panic!("an array");
        };
        assert_eq!(
            file_names(&methods),
            vec![("CarMapperImpl.java".to_owned(), 2)]
        );

        let use_uri = file_uri(&sources.path("p/Use.java")).expect("a uri");
        state.did_open(DocumentId::new(use_uri.as_str()), 1, USE.to_owned());
        let (_, survey) = javelin_index::java_sources_surveyed(
            std::slice::from_ref(&sources.root),
            1 << 20,
            &token,
        );
        assert!(
            !survey.generated.contains("CarMapperImpl"),
            "an indexed name"
        );
        state.install_workspace(crate::workspace::Workspace {
            index: std::sync::Arc::clone(&sources.index),
            elapsed: std::time::Duration::ZERO,
            refreshed: false,
            survey: Some(std::sync::Arc::new(survey)),
            absorbed: 0,
        });
        let jump = crate::handlers::definition::handle(
            &mut state,
            &on(use_uri.as_str(), USE, "CarMapperImpl impl"),
            &token,
        )
        .expect("not cancelled");
        let Some(GotoDefinitionResponse::Scalar(location)) = jump else {
            panic!("one location, got {jump:?}");
        };
        assert_eq!(Some(location.uri), file_uri(&sources.path(&impl_path)));
        assert_eq!(location.range.start.line, 1);
    }

    /// The JDK's entries the world below is sited in: each a type's own
    /// name on its second line, `List` unsited as a JAR type without
    /// sources is.
    const JDK: &[(&str, &str, &str)] = &[
        (
            "java.lang.Runnable",
            "java.base/java/lang/Runnable.java",
            "package java.lang;\npublic interface Runnable { void run(); }\n",
        ),
        (
            "java.util.ArrayList",
            "java.base/java/util/ArrayList.java",
            "package java.util;\npublic class ArrayList<E> extends AbstractList<E> implements List<E> {}\n",
        ),
        (
            "java.util.AbstractList",
            "java.base/java/util/AbstractList.java",
            "package java.util;\npublic abstract class AbstractList<E> {}\n",
        ),
        (
            "java.util.RandomAccess",
            "java.base/java/util/RandomAccess.java",
            "package java.util;\npublic interface RandomAccess {}\n",
        ),
    ];
    /// A workspace copy of a JDK type, which the JDK's own edges do not mean.
    const VENDORED: &str = "package java.util;\npublic interface RandomAccess {}\n";

    const JOBS: &str =
        "package p;\npublic class Jobs implements Runnable {\n    public void run() {}\n}\n";
    const NAMES: &str = "package p;\nimport java.util.ArrayList;\npublic class Names extends ArrayList<String> {}\n";
    /// `Inner`'s `Runnable` is `Base.Runnable`, which `Outer` inherits.
    const BASE: &str = "package p;\npublic class Base {\n    public interface Runnable {}\n}\n";
    const OUTER: &str = "package p;\npublic class Outer extends Base {\n    static class Inner implements Runnable {}\n}\n";
    const STATIC: &str =
        "package p;\nimport static p.Holder.*;\npublic class Static implements Runnable {}\n";
    const BROKEN: &str = "package p;\npublic class Broken extends Missing implements Runnable {}\n";
    const BAD: &str = "package p;\npublic class Bad implements Runnable {\n    void f( }\n";
    const GEN: &str = "package p;\npublic class Gen implements Runnable {}\n";

    /// `rivals` are moved out of the JDK into two JARs that disagree about
    /// them, and `jars` are laid after the JDK too.
    fn jdk_world(
        dir: &Path,
        rivals: &[&str],
        jars: Vec<javelin_classpath::TypeFacts>,
        dependencies: Option<crate::classpath::DependencyReport>,
    ) -> crate::classpath::Classpath {
        use std::io::Write;
        let zip_path = dir.join("src.zip");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&zip_path).expect("zip"));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let mut facts = Vec::new();
        for (fqn, entry, text) in JDK {
            writer.start_file(*entry, options).expect("entry");
            writer.write_all(text.as_bytes()).expect("write");
            let mut fact = crate::classpath::fact(fqn);
            let simple = fact.simple.clone();
            fact.name_start = text.find(&format!(" {simple}")).expect("its name") as u32 + 1;
            fact.name_end = fact.name_start + simple.len() as u32;
            fact.source = (*entry).to_owned();
            facts.push(fact);
        }
        writer.finish().expect("zip");
        facts[0].kind = javelin_parser::DeclKind::Interface;
        facts[1].supertypes = vec![
            "java.util.AbstractList".into(),
            "java.util.List".into(),
            "java.util.RandomAccess".into(),
        ];
        let mut list = crate::classpath::fact("java.util.List");
        list.kind = javelin_parser::DeclKind::Interface;
        facts.push(list);
        let (moved, platform): (Vec<_>, Vec<_>) = facts
            .into_iter()
            .partition(|fact| rivals.contains(&fact.fqn.as_str()));
        let mut more = jars;
        for fact in moved {
            let mut other = fact.clone();
            other.supertypes.push("java.lang.Cloneable".into());
            more.extend([fact, other]);
        }
        let mut classpath = crate::classpath::Classpath::holding(platform);
        if !more.is_empty() {
            classpath.world = std::sync::Arc::new(classpath.world.extended(more));
        }
        classpath.sources = Some((zip_path, "21".to_owned()));
        classpath.dependencies = dependencies;
        classpath
    }

    /// Every dependency the build names is in, over `modules`.
    fn whole(
        modules: Option<std::sync::Arc<crate::modules::Modules>>,
    ) -> crate::classpath::DependencyReport {
        crate::classpath::DependencyReport {
            key: 0,
            complete: true,
            processors: false,
            loose: true,
            codegen: false,
            rewrites: false,
            release: None,
            modules,
        }
    }

    /// A session over the files above and the world, with a survey as
    /// trusted as `survey` says.
    fn session(name: &str, survey: Option<javelin_index::Survey>) -> (Sources, ServerState) {
        session_over(name, survey, |dir, _| {
            jdk_world(dir, &[], Vec::new(), Some(whole(None)))
        })
    }

    /// [`session`] over the world `world` builds in a directory, given the
    /// workspace's sources.
    fn session_over(
        name: &str,
        survey: Option<javelin_index::Survey>,
        world: impl FnOnce(&Path, &Sources) -> crate::classpath::Classpath,
    ) -> (Sources, ServerState) {
        let sources = indexed_with_member_refs(
            name,
            &[
                ("p/Jobs.java", JOBS),
                ("p/Names.java", NAMES),
                ("p/Base.java", BASE),
                ("p/Outer.java", OUTER),
                ("p/Static.java", STATIC),
                ("p/Broken.java", BROKEN),
                ("p/Bad.java", BAD),
                (
                    "build/generated/sources/annotationProcessor/java/main/p/Gen.java",
                    GEN,
                ),
                ("java/util/RandomAccess.java", VENDORED),
            ],
        );
        let dir = sources.path("jdk");
        std::fs::create_dir_all(&dir).expect("dir");
        let mut config = Config::default();
        config.index.cache_dir = Some(dir.join("cache"));
        let mut state = ServerState::new(config).expect("grammar loads");
        state.install_classpath(world(&dir, &sources));
        state.install_workspace(crate::workspace::Workspace {
            index: std::sync::Arc::clone(&sources.index),
            elapsed: std::time::Duration::ZERO,
            refreshed: false,
            survey: survey.map(std::sync::Arc::new),
            absorbed: 0,
        });
        (sources, state)
    }

    fn item(fqn: &str) -> TypeHierarchyItem {
        TypeHierarchyItem {
            name: fqn.rsplit('.').next().unwrap_or(fqn).to_owned(),
            kind: SymbolKind::CLASS,
            tags: None,
            detail: None,
            uri: "file:///nowhere".parse().expect("a uri"),
            range: Range::default(),
            selection_range: Range::default(),
            data: Some(serde_json::json!({ "fqn": fqn })),
        }
    }

    fn above(state: &mut ServerState, fqn: &str) -> Vec<(String, String, u32)> {
        supertypes(
            state,
            &TypeHierarchySupertypesParams {
                item: item(fqn),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
            &CancellationToken::new(),
        )
        .expect("not cancelled")
        .unwrap_or_default()
        .into_iter()
        .map(|item| {
            let file = item
                .uri
                .as_str()
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_owned();
            (item.name, file, item.selection_range.start.line)
        })
        .collect()
    }

    fn below(state: &mut ServerState, fqn: &str) -> Vec<String> {
        subtypes(
            state,
            &TypeHierarchySubtypesParams {
                item: item(fqn),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
            &CancellationToken::new(),
        )
        .expect("not cancelled")
        .unwrap_or_default()
        .into_iter()
        .map(|item| item.name)
        .collect()
    }

    fn placed(name: &str, file: &str) -> (String, String, u32) {
        (name.to_owned(), file.to_owned(), 1)
    }

    #[test]
    fn a_jdk_supertype_is_listed_at_its_name_and_lists_the_workspace_type_below_it() {
        let (_sources, mut state) = session("hierarchy-jdk", Some(Default::default()));
        assert_eq!(
            above(&mut state, "p.Jobs"),
            vec![placed("Runnable", "Runnable.java")]
        );
        assert_eq!(
            above(&mut state, "p.Names"),
            vec![placed("ArrayList", "ArrayList.java")]
        );
        // `List` has no site, as a JAR type without sources has none, and
        // `RandomAccess` is the workspace's as well as the JDK's.
        assert_eq!(
            above(&mut state, "java.util.ArrayList"),
            vec![placed("AbstractList", "AbstractList.java")]
        );
        assert_eq!(below(&mut state, "java.lang.Runnable"), vec!["Jobs"]);
        assert_eq!(below(&mut state, "java.util.ArrayList"), vec!["Names"]);
    }

    /// Each is a supertype the tiers would read as `java.lang.Runnable`
    /// and Java may not.
    #[test]
    fn an_uncertain_supertype_is_listed_in_neither_direction() {
        let (_sources, mut state) = session("hierarchy-uncertain", Some(Default::default()));
        for ty in ["p.Outer.Inner", "p.Static", "p.Broken", "p.Bad", "p.Gen"] {
            assert_eq!(above(&mut state, ty), Vec::new(), "{ty}");
        }
        assert_eq!(below(&mut state, "java.lang.Runnable"), vec!["Jobs"]);
    }

    #[test]
    fn a_world_not_trusted_or_a_generated_name_lists_no_jdk_supertype() {
        let (_sources, mut state) = session("hierarchy-untrusted", None);
        assert_eq!(above(&mut state, "p.Jobs"), Vec::new());
        assert_eq!(
            below(&mut state, "java.lang.Runnable"),
            Vec::<String>::new()
        );
        let survey = javelin_index::Survey {
            generated: ["Runnable".to_owned()].into(),
            ..Default::default()
        };
        let (_sources, mut state) = session("hierarchy-generated", Some(survey));
        assert_eq!(above(&mut state, "p.Jobs"), Vec::new());
        assert_eq!(
            above(&mut state, "p.Names").len(),
            1,
            "only the generated name"
        );
    }

    #[test]
    fn prepare_on_a_jdk_type_name_answers_its_item() {
        let (sources, mut state) = session("hierarchy-prepare", Some(Default::default()));
        let uri = file_uri(&sources.path("p/Jobs.java")).expect("a uri");
        state.did_open(DocumentId::new(uri.as_str()), 1, JOBS.to_owned());
        let params = TypeHierarchyPrepareParams {
            text_document_position_params: at(uri.as_str(), JOBS, "Runnable"),
            work_done_progress_params: Default::default(),
        };
        let root = prepare_type_hierarchy(&mut state, &params, &CancellationToken::new())
            .expect("not cancelled")
            .expect("Runnable");
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].name, "Runnable");
        assert_eq!(root[0].kind, SymbolKind::INTERFACE);
        assert!(
            root[0].uri.as_str().ends_with("java/lang/Runnable.java"),
            "{:?}",
            root[0].uri
        );
        assert_eq!(type_data(&root[0]), Some("java.lang.Runnable"));
    }

    /// The name the tiers read as `java.lang.Runnable` means `Base.Runnable`.
    #[test]
    fn prepare_on_a_name_an_inherited_member_type_may_mean_answers_nothing() {
        let (sources, mut state) = session("hierarchy-prepare-shadowed", Some(Default::default()));
        let uri = file_uri(&sources.path("p/Outer.java")).expect("a uri");
        state.did_open(DocumentId::new(uri.as_str()), 1, OUTER.to_owned());
        let params = TypeHierarchyPrepareParams {
            text_document_position_params: at(uri.as_str(), OUTER, "Runnable"),
            work_done_progress_params: Default::default(),
        };
        let root = prepare_type_hierarchy(&mut state, &params, &CancellationToken::new())
            .expect("not cancelled");
        assert_eq!(root, None);
    }

    /// With the build's JARs not yet in — or never, with the build import
    /// off — a JAR in `p` may hold the `Runnable` the JDK answers for now.
    #[test]
    fn no_jdk_supertype_is_listed_before_every_dependency_is_in() {
        let incomplete = crate::classpath::DependencyReport {
            complete: false,
            ..whole(None)
        };
        for (name, report) in [
            ("hierarchy-no-deps", None),
            ("hierarchy-incomplete-deps", Some(incomplete)),
        ] {
            let (_sources, mut state) = session_over(name, Some(Default::default()), |dir, _| {
                jdk_world(dir, &[], Vec::new(), report)
            });
            assert_eq!(above(&mut state, "p.Jobs"), Vec::new(), "{name}");
            assert_eq!(
                below(&mut state, "java.lang.Runnable"),
                Vec::<String>::new(),
                "{name}"
            );
        }
    }

    /// Two JARs' versions of `ArrayList`: which one's supertypes, and which
    /// one's source, a file compiles against is not the first one's to say.
    #[test]
    fn a_type_two_jars_declare_differently_is_not_listed() {
        let (sources, mut state) =
            session_over("hierarchy-rivalled", Some(Default::default()), |dir, _| {
                jdk_world(dir, &["java.util.ArrayList"], Vec::new(), Some(whole(None)))
            });
        assert_eq!(above(&mut state, "p.Names"), Vec::new());
        // Named where nothing else doubts it: `Jobs`' closure is whole.
        let text = "package p;\nimport java.util.ArrayList;\npublic class Jobs implements Runnable {\n    ArrayList<String> xs;\n    public void run() {}\n}\n";
        let uri = file_uri(&sources.path("p/Jobs.java")).expect("a uri");
        state.did_open(DocumentId::new(uri.as_str()), 1, text.to_owned());
        let params = TypeHierarchyPrepareParams {
            text_document_position_params: at(uri.as_str(), text, "ArrayList<"),
            work_done_progress_params: Default::default(),
        };
        let root = prepare_type_hierarchy(&mut state, &params, &CancellationToken::new())
            .expect("not cancelled");
        assert_eq!(root, None);
        assert_eq!(above(&mut state, "java.util.ArrayList"), Vec::new());
        assert_eq!(
            above(&mut state, "p.Jobs"),
            vec![placed("Runnable", "Runnable.java")]
        );
    }

    /// `p.Runnable` is in a JAR only another module depends on, so for
    /// `Jobs`' module the package tier holds nothing and `Runnable` is
    /// `java.lang`'s; read over the union it would be `p.Runnable`. And a
    /// file no module claims is read for nothing.
    #[test]
    fn a_supertype_is_read_against_its_own_modules_jars() {
        let jar = crate::classpath::fact("p.Runnable");
        // `held`: whether the world holds the JAR's type at all.
        let modules = |claimed: &'static str, held: bool| {
            let jar = jar.clone();
            move |dir: &Path, sources: &Sources| {
                let module = |source: &str, jars: Vec<usize>| javelin_build::Module {
                    sources: vec![sources.path(source)],
                    jars: Some(jars),
                };
                let modules = std::sync::Arc::new(crate::modules::Modules::new(
                    [(0, std::slice::from_ref(&jar))],
                    vec![module(claimed, Vec::new()), module("other", vec![0])],
                ));
                let jars = if held { vec![jar] } else { Vec::new() };
                jdk_world(dir, &[], jars, Some(whole(Some(modules))))
            }
        };
        let (_sources, mut state) = session_over(
            "hierarchy-module",
            Some(Default::default()),
            modules("p", true),
        );
        assert_eq!(
            above(&mut state, "p.Jobs"),
            vec![placed("Runnable", "Runnable.java")]
        );
        assert!(below(&mut state, "java.lang.Runnable").contains(&"Jobs".to_owned()));
        let (_sources, mut state) = session_over(
            "hierarchy-unclaimed",
            Some(Default::default()),
            modules("q", false),
        );
        assert_eq!(above(&mut state, "p.Jobs"), Vec::new());
        assert_eq!(
            below(&mut state, "java.lang.Runnable"),
            Vec::<String>::new()
        );
    }
}
