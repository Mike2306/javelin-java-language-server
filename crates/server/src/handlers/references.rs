//! `textDocument/references` — every place a name the cursor is on is used.
//!
//! One rule decides membership, and it is go-to-definition's: **a token is a
//! reference to a target exactly when definition at that token lands on the
//! target.** So the two features cannot disagree about what a position names,
//! and references inherits every refusal definition makes — an overload call
//! that fits two candidates is not a reference to either.
//!
//! Two kinds of target come out of the cursor:
//!
//! - **A type**, by fully-qualified name — one the workspace declares or one
//!   the JDK does. Answered in the open buffer from its live text, and in every
//!   other indexed file from the occurrences the workspace walk recorded, each
//!   resolved now against its own file's package and imports.
//! - **Any other declaration the open file makes** — a local, a parameter, a
//!   field, a method, a type parameter, a local class. Answered in the open
//!   buffer only.
//!
//! No document other than the one the request named is opened: the index's
//! rows carry their own line and UTF-16 character, the way `workspace/symbol`'s
//! do.

use std::collections::HashMap;

use javelin_core::{CancellationToken, Cancelled};
use javelin_document::DocumentId;
use javelin_parser::{DefinitionKind, Span};
use javelin_semantic::Externals;
use lsp_types::{Location, ReferenceParams};

use crate::ServerState;
use crate::handlers::{definition_in, file_uri, to_lsp_range, to_position};
use crate::state::FileView;

/// What the cursor is on, as the question references answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    /// A declaration in the open file, by where its name is.
    Decl {
        name: String,
        name_span: Span,
        kind: DefinitionKind,
    },
    /// A type anything outside the file can name, by its fully-qualified name.
    Type(String),
    /// A member another workspace file declares, by its site: the declaring
    /// file and the byte offset of the member's name there — the key its
    /// recorded references are stored under.
    ///
    /// ponytail: the key is a byte offset, so within the quiet period after an
    /// edit above the declaration, before its file's re-index lands, the
    /// recorded uses are keyed at the old offset and are missing rather than
    /// wrong. The upgrade is keying by owner, name and signature.
    Member {
        name: String,
        source: String,
        start: u32,
        declaration: Option<Location>,
    },
    /// A constructor another workspace file declares, by its site, the key
    /// [`Target::Member`]'s is.
    Constructor {
        name: String,
        source: String,
        start: u32,
        declaration: Option<Location>,
    },
}

/// Every reference to the target under the cursor, sorted by file and then by
/// position, with no location twice.
///
/// An empty list, never an error, for every position with nothing to answer:
/// a document that is not open, whitespace, a keyword.
///
/// A field or a method is answered across files: the workspace worker
/// resolved every member selection of every file and recorded what each one
/// lands on, and the open file is answered live. See `member_refs.rs` for
/// what a recorded reference is and when it is re-resolved.
pub fn handle(
    state: &mut ServerState,
    params: &ReferenceParams,
    token: &CancellationToken,
) -> Result<Vec<Location>, Cancelled> {
    token.check()?;
    let at = &params.text_document_position;
    let uri = at.text_document.uri.clone();
    let id = DocumentId::new(uri.as_str());
    let include_declaration = params.context.include_declaration;
    // Both taken before the view, which borrows the state mutably.
    let externals = state.externals();
    let workspace = state.workspace();
    let survey = crate::handlers::trusted_survey(state);
    let survey = survey.as_deref();
    let Some(view) = state.view(&id) else {
        return Ok(Vec::new());
    };
    let offset = view.document.offset_at(to_position(at.position));
    let named = named_types(&view);
    let lombok = crate::handlers::inlay_hint::lombok_short(
        view.syntax,
        view.document.text(),
        &externals,
        &id,
    );
    // The constructor a `new` selects, as definition answers it; the type its
    // name names where it selects none the workspace declares.
    let Some(target) = constructor_target(&view, &id, &externals, survey, offset, lombok)
        .or_else(|| target_at(&view, &id, &externals, &named, offset))
    else {
        return Ok(Vec::new());
    };
    let current_path = crate::workspace::path_of(uri.as_str()).unwrap_or_default();
    let current_path = current_path.to_string_lossy();

    let mut found: Vec<Location> = Vec::new();
    match &target {
        Target::Decl {
            name,
            name_span,
            kind,
        } => {
            // A local, a parameter, a pattern binding, a type parameter or a
            // local class can only be named inside the scope that binds it, so
            // only that stretch of the file is searched. A field or a method
            // can be selected from anywhere in the file, `other.count`, and is
            // searched everywhere.
            let bound = match kind {
                DefinitionKind::Field | DefinitionKind::Method | DefinitionKind::Constructor => {
                    None
                }
                _ => view
                    .scopes
                    .resolve(name_span.start_byte, name)
                    .filter(|binding| binding.name_span.start_byte == name_span.start_byte)
                    .map(|binding| {
                        binding.visible_from.min(name_span.start_byte)..=binding.visible_to
                    }),
            };
            let candidates = match bound {
                Some(within) => javelin_parser::identifier_spans_in(
                    &view.document,
                    view.tree,
                    name,
                    within,
                    token,
                )?,
                None => javelin_parser::identifier_spans(&view.document, view.tree, name, token)?,
            };
            // A field Lombok generates accessors for is used through them: a
            // token spelled like one is a use when it lands on the field.
            let mut candidates = candidates;
            if *kind == DefinitionKind::Constructor {
                candidates.extend(explicit_calls_of(&view, name, token)?);
            }
            // Only while the index's row at this offset is still this field:
            // a quarter-second after an edit above it, another field's
            // accessors can stand there.
            if *kind == DefinitionKind::Field
                && let Some(current) = crate::workspace::path_of(uri.as_str())
                && workspace
                    .member_at(&current, name_span.start_byte)
                    .is_some_and(|indexed| indexed.name == *name)
            {
                let mut spellings: Vec<&str> = workspace
                    .generated_at(&current, name_span.start_byte)
                    .map(|member| member.name.as_str())
                    .filter(|spelling| spelling != name)
                    .collect();
                spellings.sort_unstable();
                spellings.dedup();
                for spelling in spellings {
                    candidates.extend(javelin_parser::identifier_spans(
                        &view.document,
                        view.tree,
                        spelling,
                        token,
                    )?);
                }
            }
            // A method's line through its overrides, when it has one: then
            // a token is a use when it lands anywhere on the line.
            let line = match (kind, crate::workspace::path_of(uri.as_str())) {
                (DefinitionKind::Method, Some(current)) => crate::hierarchy::override_line(
                    &externals,
                    externals.workspace(),
                    (&current.to_string_lossy(), name_span.start_byte, name),
                    token,
                )?,
                _ => Vec::new(),
            };
            if line.len() > 1 {
                let current = crate::workspace::path_of(uri.as_str()).unwrap_or_default();
                found.extend(on_the_line(
                    &view,
                    &id,
                    &uri,
                    &externals,
                    &line,
                    &current.to_string_lossy(),
                    name,
                    include_declaration,
                    token,
                )?);
                for (source, start) in &line {
                    found.extend(recorded_elsewhere(&workspace, source, *start, &current));
                }
                return Ok(sorted(found));
            }
            let files = javelin_semantic::Files::single(id.clone(), view.syntax, view.scopes);
            for span in candidates {
                token.check()?;
                if !include_declaration && span.start_byte == name_span.start_byte {
                    continue;
                }
                // A `new` lands on the constructor it selects, never its
                // name on the type.
                if *kind == DefinitionKind::Constructor && span.start_byte != name_span.start_byte {
                    if new_lands(
                        &view,
                        &id,
                        &externals,
                        survey,
                        &current_path,
                        span.start_byte,
                        lombok,
                    ) == Some((current_path.clone().into_owned(), name_span.start_byte))
                    {
                        found.push(Location::new(uri.clone(), to_lsp_range(span.range)));
                    }
                    continue;
                }
                let lands = match definition_in(&view, &files, &externals, span.start_byte) {
                    Some(found) => {
                        found.file == id && found.name_span.start_byte == name_span.start_byte
                    }
                    // A generated accessor is answered as a fact, sited at
                    // its field.
                    None => {
                        *kind == DefinitionKind::Field
                            && crate::workspace::path_of(uri.as_str()).is_some_and(|current| {
                                member_at(&view, &id, &externals, span.start_byte)
                                    == Some((
                                        current.to_string_lossy().into_owned(),
                                        name_span.start_byte,
                                    ))
                            })
                    }
                };
                if lands {
                    found.push(Location::new(uri.clone(), to_lsp_range(span.range)));
                }
            }
            // A field or a method of this file can be selected from any other
            // file: the worker recorded those selections under this file's
            // path and the name's offset.
            if matches!(
                kind,
                DefinitionKind::Field | DefinitionKind::Method | DefinitionKind::Constructor
            ) && let Some(current) = crate::workspace::path_of(uri.as_str())
            {
                found.extend(recorded_elsewhere(
                    &workspace,
                    &current.to_string_lossy(),
                    name_span.start_byte,
                    &current,
                ));
            }
        }
        Target::Member {
            name,
            source,
            start,
            declaration,
        } => {
            // The open file, live: every same-spelled selection that lands on
            // this member or on its line through its overrides.
            let line = crate::hierarchy::override_line(
                &externals,
                externals.workspace(),
                (source, *start, name),
                token,
            )?;
            let current = crate::workspace::path_of(uri.as_str()).unwrap_or_default();
            found.extend(on_the_line(
                &view,
                &id,
                &uri,
                &externals,
                &line,
                &current.to_string_lossy(),
                name,
                false,
                token,
            )?);
            for (source, start) in &line {
                found.extend(recorded_elsewhere(&workspace, source, *start, &current));
            }
            if include_declaration && let Some(declaration) = declaration {
                found.push(declaration.clone());
            }
        }
        Target::Constructor {
            name,
            source,
            start,
            declaration,
        } => {
            let target = (source.clone(), *start);
            let explicit = explicit_calls_of(&view, name, token)?;
            for span in javelin_parser::identifier_spans(&view.document, view.tree, name, token)?
                .into_iter()
                .chain(explicit)
            {
                token.check()?;
                if new_lands(
                    &view,
                    &id,
                    &externals,
                    survey,
                    &current_path,
                    span.start_byte,
                    lombok,
                )
                .as_ref()
                    == Some(&target)
                {
                    found.push(Location::new(uri.clone(), to_lsp_range(span.range)));
                }
            }
            let current = std::path::Path::new(current_path.as_ref());
            found.extend(recorded_elsewhere(&workspace, source, *start, current));
            if include_declaration && let Some(declaration) = declaration {
                found.push(declaration.clone());
            }
        }
        Target::Type(fqn) => {
            let simple = fqn.rsplit('.').next().unwrap_or(fqn);
            // The declaration's own name, wherever the index or the buffer
            // saw it, for `includeDeclaration: false` to leave out.
            // The open file's own declarations come from its buffer: the
            // index's record of them is as old as the last walk, and a stale
            // one would take a live use at the same position out of the list.
            let current = crate::workspace::path_of(uri.as_str());
            let mut declarations: Vec<Location> = workspace
                .with_fqn(fqn)
                .filter(|ty| current.as_deref() != Some(ty.file.as_path()))
                .filter_map(|ty| {
                    Some(Location::new(
                        file_uri(&ty.file)?,
                        to_lsp_range(ty.name_range),
                    ))
                })
                .collect();
            declarations.extend(
                named
                    .values()
                    .filter(|(declared, _)| declared == fqn)
                    .map(|(_, range)| Location::new(uri.clone(), to_lsp_range(*range))),
            );

            // The open buffer, from its live text.
            for span in javelin_parser::identifier_spans(&view.document, view.tree, simple, token)?
            {
                token.check()?;
                if target_at(&view, &id, &externals, &named, span.start_byte).as_ref()
                    == Some(&target)
                {
                    found.push(Location::new(uri.clone(), to_lsp_range(span.range)));
                }
            }

            // Every other file, from what the walk recorded. The open file's
            // own rows are skipped: the buffer is the authority for the file in
            // front of the user, and its rows may be a quiet period behind it.
            // Every other file's rows follow its own buffer or its disk, the
            // way `handlers::workspace_type_at`'s `ponytail:` records.
            for occurrence in workspace.occurrences_named(simple) {
                token.check()?;
                let file = workspace.file(occurrence.file);
                if current.as_deref() == Some(file.path.as_path()) {
                    continue;
                }
                let means = match &occurrence.fqn {
                    Some(declared) => declared == fqn,
                    None => {
                        externals
                            .resolve(&occurrence.name, file.package.as_deref(), &file.imports)
                            .as_deref()
                            == Some(fqn.as_str())
                    }
                };
                if means && let Some(uri) = file_uri(&file.path) {
                    found.push(Location::new(uri, to_lsp_range(occurrence.range)));
                }
            }
            if !include_declaration {
                found.retain(|location| !declarations.contains(location));
            }
        }
    }

    Ok(sorted(found))
}

/// The `this(…)`s and constants in a type named `type_name`, and the
/// `super(…)`s in a class writing it as its superclass: the explicit calls
/// that may select one of its constructors. Sifted by name before each is
/// selected, since a selection reads the whole of its type's body, and a
/// generated enum's thousands of constants would each read it for another
/// type's constructor.
///
/// ponytail: references on an enum's own constructor still select every
/// constant against the enum's body, quadratic in its constants — 51 at most
/// on the sample. The upgrade is choosing the enum's candidates once per
/// request.
fn explicit_calls_of(
    view: &FileView<'_>,
    type_name: &str,
    token: &CancellationToken,
) -> Result<Vec<Span>, Cancelled> {
    let text = view.document.text();
    let mut spans = javelin_parser::explicit_constructor_spans(&view.document, view.tree, token)?;
    spans.retain(|span| {
        let Some(owner) = view.syntax.innermost_type_at(span.start_byte) else {
            return false;
        };
        if text.get(span.start_byte as usize..span.end_byte as usize) == Some("super") {
            owner.extends.as_deref().is_some_and(|written| {
                let raw = written.split('<').next().unwrap_or(written);
                raw.rsplit('.').next().unwrap_or(raw).trim() == type_name
            })
        } else {
            owner.name == type_name
        }
    });
    Ok(spans)
}

/// Sorted by file and then by position, with no location twice.
fn sorted(mut found: Vec<Location>) -> Vec<Location> {
    found.sort_by(|a, b| {
        (a.uri.as_str(), a.range.start.line, a.range.start.character).cmp(&(
            b.uri.as_str(),
            b.range.start.line,
            b.range.start.character,
        ))
    });
    found.dedup();
    found
}

/// Every token in the open file spelled `name` that lands on a member of
/// `line` — by definition in the file, or on another file's member. A
/// token that *is* a declaration on the line is an override's, not a use,
/// and is skipped; the line's first entry, the target, is kept when
/// `include_declaration` says so.
#[allow(clippy::too_many_arguments)]
fn on_the_line(
    view: &FileView<'_>,
    id: &DocumentId,
    uri: &lsp_types::Uri,
    externals: &dyn Externals,
    line: &[(String, u32)],
    current: &str,
    name: &str,
    include_declaration: bool,
    token: &CancellationToken,
) -> Result<Vec<Location>, Cancelled> {
    let mut found = Vec::new();
    let files = javelin_semantic::Files::single(id.clone(), view.syntax, view.scopes);
    for span in javelin_parser::identifier_spans(&view.document, view.tree, name, token)? {
        token.check()?;
        let lands = match definition_in(view, &files, externals, span.start_byte) {
            Some(found) if found.file == *id => {
                Some((current.to_owned(), found.name_span.start_byte))
            }
            _ => member_at(view, id, externals, span.start_byte),
        };
        let Some(lands) = lands.filter(|lands| line.contains(lands)) else {
            continue;
        };
        if lands == (current.to_owned(), span.start_byte)
            && !(include_declaration && Some(&lands) == line.first())
        {
            continue;
        }
        found.push(Location::new(uri.clone(), to_lsp_range(span.range)));
    }
    Ok(found)
}

/// The fully-qualified name of every type the open file declares by a name
/// another file could write, and where that name is, keyed by where it starts.
pub(crate) fn named_types(view: &FileView<'_>) -> HashMap<u32, (String, javelin_document::Range)> {
    view.syntax
        .named_types()
        .into_iter()
        .map(|named| {
            let name = named.decl.name_span;
            (name.start_byte, (named.fqn, name.range))
        })
        .collect()
}

/// Every recorded use of the member at `start` in `source`, in any file but
/// `current` — which the caller answers from its live buffer.
fn recorded_elsewhere(
    workspace: &javelin_index::WorkspaceIndex,
    source: &str,
    start: u32,
    current: &std::path::Path,
) -> Vec<Location> {
    workspace
        .member_refs_to(source, start)
        .filter_map(|found| {
            let file = workspace.file(found.file);
            if file.path == current {
                return None;
            }
            Some(Location::new(
                file_uri(&file.path)?,
                to_lsp_range(found.range),
            ))
        })
        .collect()
}

/// The workspace member a selection at `offset` lands on, by its site.
fn member_at(
    view: &FileView<'_>,
    id: &DocumentId,
    externals: &dyn Externals,
    offset: u32,
) -> Option<(String, u32)> {
    let files = javelin_semantic::Files::single(id.clone(), view.syntax, view.scopes);
    let found =
        javelin_semantic::external_member_at(&view.document, view.tree, &files, externals, offset)?;
    Some(keyed(found))
}

/// A member another file declares, by its site — or, for a JDK or JAR member
/// with no workspace site, by the key the worker records it under.
fn keyed(found: javelin_semantic::ExternalSummary) -> (String, u32) {
    match found.site {
        Some(site) if site.name_range.is_some() => (site.source, site.name_start),
        _ => (
            crate::member_refs::library_key(&found.owner, &found.name, found.ordinal),
            0,
        ),
    }
}

/// The constructor the `new` at `offset` selects, by its site — `current`
/// for one the open file declares.
fn new_lands(
    view: &FileView<'_>,
    id: &DocumentId,
    externals: &crate::classpath::ClasspathExternals,
    survey: Option<&javelin_index::Survey>,
    current: &str,
    offset: u32,
    lombok: bool,
) -> Option<(String, u32)> {
    match crate::handlers::constructor_in(view, id, externals, survey, offset, true, lombok)?.target
    {
        javelin_semantic::ConstructorTarget::Local(found) => {
            Some((current.to_owned(), found.name_span.start_byte))
        }
        javelin_semantic::ConstructorTarget::External(found) => {
            let site = found.site.filter(|site| site.name_range.is_some())?;
            Some((site.source, site.name_start))
        }
    }
}

/// The constructor the `new` at `offset` selects, as a target: a
/// declaration the file makes, or another workspace file's by its site. A
/// JDK's is none, and the name answers as the type it names.
fn constructor_target(
    view: &FileView<'_>,
    id: &DocumentId,
    externals: &crate::classpath::ClasspathExternals,
    survey: Option<&javelin_index::Survey>,
    offset: u32,
    lombok: bool,
) -> Option<Target> {
    match crate::handlers::constructor_in(view, id, externals, survey, offset, false, lombok)?
        .target
    {
        javelin_semantic::ConstructorTarget::Local(found) => Some(Target::Decl {
            name: found.name,
            name_span: found.name_span,
            kind: found.kind,
        }),
        javelin_semantic::ConstructorTarget::External(found) => {
            let site = found.site.filter(|site| site.name_range.is_some())?;
            let declaration = site.name_range.and_then(|range| {
                Some(Location::new(
                    file_uri(std::path::Path::new(&site.source))?,
                    to_lsp_range(range),
                ))
            });
            Some(Target::Constructor {
                name: found.name,
                source: site.source,
                start: site.name_start,
                declaration,
            })
        }
    }
}

/// What go-to-definition at `offset` lands on, as a target.
///
/// A `new`'s class name is the type here, as every other use of the name
/// is: the handler asks [`constructor_target`] first, and a type's own
/// references keep listing every `new` of it.
///
/// A declaration the file makes is a [`Target::Decl`] — except a type that has
/// a name outside the file, which is the same [`Target::Type`] a use of it in
/// another file resolves to, so the two are one target. A name no reachable
/// file declares resolves through [`javelin_semantic::external_type_at`], the
/// call definition makes for it.
pub(crate) fn target_at(
    view: &FileView<'_>,
    id: &DocumentId,
    externals: &dyn Externals,
    named: &HashMap<u32, (String, javelin_document::Range)>,
    offset: u32,
) -> Option<Target> {
    let files = javelin_semantic::Files::single(id.clone(), view.syntax, view.scopes);
    if let Some(found) = definition_in(view, &files, externals, offset) {
        if found.file != *id {
            return None;
        }
        if found.kind == DefinitionKind::Type
            && let Some((fqn, _)) = named.get(&found.name_span.start_byte)
        {
            return Some(Target::Type(fqn.clone()));
        }
        return Some(Target::Decl {
            name: found.name,
            name_span: found.name_span,
            kind: found.kind,
        });
    }
    let files = javelin_semantic::Files::single(id.clone(), view.syntax, view.scopes);
    // A member another workspace file declares: definition's external path.
    // A JDK or JAR one only where the name means no type, as it did before
    // such a member was a target; it has no workspace location to list as
    // its declaration.
    let member =
        javelin_semantic::external_member_at(&view.document, view.tree, &files, externals, offset);
    let library = match member {
        Some(found)
            if found
                .site
                .as_ref()
                .is_some_and(|site| site.name_range.is_some()) =>
        {
            let declaration = found.site.as_ref().and_then(|site| {
                Some(Location::new(
                    file_uri(std::path::Path::new(&site.source))?,
                    to_lsp_range(site.name_range?),
                ))
            });
            let name = found.name.clone();
            let (source, start) = keyed(found);
            return Some(Target::Member {
                name,
                source,
                start,
                declaration,
            });
        }
        other => other,
    };
    javelin_semantic::external_type_at(&view.document, view.tree, &files, externals, offset)
        .map(|found| Target::Type(found.fqn))
        .or_else(|| {
            let found = library?;
            let name = found.name.clone();
            let (source, start) = keyed(found);
            Some(Target::Member {
                name,
                source,
                start,
                declaration: None,
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classpath::Classpath;
    use javelin_core::Config;
    use javelin_document::{Position, Range};
    use javelin_index::{WorkspaceFile, WorkspaceIndex, WorkspaceOccurrence, WorkspaceType};
    use javelin_parser::DeclKind;
    use lsp_types::GotoDefinitionResponse;
    use std::path::PathBuf;

    const URI: &str = "file:///w/com/example/A.java";

    fn opened(source: &str) -> ServerState {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        state.did_open(DocumentId::new(URI), 1, source.to_owned());
        state
    }

    fn params(line: u32, character: u32, include_declaration: bool) -> ReferenceParams {
        serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": URI },
            "position": { "line": line, "character": character },
            "context": { "includeDeclaration": include_declaration },
        }))
        .expect("well-formed params")
    }

    fn references(
        state: &mut ServerState,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Vec<(String, u32, u32)> {
        handle(
            state,
            &params(line, character, include_declaration),
            &CancellationToken::new(),
        )
        .expect("not cancelled")
        .into_iter()
        .map(|location| {
            (
                location.uri.as_str().to_owned(),
                location.range.start.line,
                location.range.start.character,
            )
        })
        .collect()
    }

    /// `(uri, line, character)` for the requested buffer.
    fn here(line: u32, character: u32) -> (String, u32, u32) {
        (URI.to_owned(), line, character)
    }

    fn range(line: u32, character: u32, length: u32) -> Range {
        Range::new(
            Position::new(line, character),
            Position::new(line, character + length),
        )
    }

    /// One indexed file that is not the open one, writing `name` at each of
    /// `at`, with no fqn — so every row is resolved at query time against the
    /// file's own package and imports.
    fn elsewhere(
        path: &str,
        package: &str,
        imports: &[&str],
        name: &str,
        at: &[(u32, u32)],
    ) -> (WorkspaceFile, Vec<(String, Range)>) {
        let file = WorkspaceFile {
            path: PathBuf::from(path),
            package: Some(package.to_owned()),
            imports: imports
                .iter()
                .map(|import| javelin_parser::ImportDecl {
                    name: (*import).to_owned(),
                    span: zero(),
                    name_span: zero(),
                    is_static: false,
                    is_on_demand: false,
                })
                .collect(),
            parsed_cleanly: true,
        };
        let rows = at
            .iter()
            .map(|&(line, character)| (name.to_owned(), range(line, character, name.len() as u32)))
            .collect();
        (file, rows)
    }

    fn zero() -> javelin_parser::Span {
        javelin_parser::Span {
            start_byte: 0,
            end_byte: 0,
            range: range(0, 0, 0),
        }
    }

    fn declared(fqn: &str, path: &str, name_range: Range) -> WorkspaceType {
        let simple = fqn.rsplit('.').next().unwrap_or(fqn).to_owned();
        WorkspaceType {
            is_public: true,
            doc_span: None,
            fqn: fqn.to_owned(),
            simple,
            kind: DeclKind::Class,
            file: PathBuf::from(path),
            name_start: 0,
            name_end: 0,
            decl_start: 0,
            decl_end: 0,
            name_range,
            supertypes: Vec::new(),
            synthetic: false,
            lombok: Default::default(),
            type_parameters: Vec::new(),
        }
    }

    /// An index of `types` and of `files`, each file's rows turned into
    /// unresolved occurrences of that file.
    fn install(
        state: &mut ServerState,
        types: Vec<WorkspaceType>,
        files: Vec<(WorkspaceFile, Vec<(String, Range)>)>,
    ) {
        let mut sources = Vec::new();
        let mut occurrences = Vec::new();
        for (position, (file, rows)) in files.into_iter().enumerate() {
            sources.push(file);
            for (name, range) in rows {
                occurrences.push(WorkspaceOccurrence {
                    file: position,
                    name,
                    start_byte: 0,
                    end_byte: 0,
                    range,
                    fqn: None,
                });
            }
        }
        state.install_workspace(crate::workspace::Workspace {
            index: std::sync::Arc::new(WorkspaceIndex::with_occurrences(
                types,
                sources,
                occurrences,
            )),
            elapsed: std::time::Duration::ZERO,
            refreshed: false,
            survey: None,
            absorbed: 0,
        });
    }

    #[test]
    fn a_local_lists_its_declaration_and_every_use() {
        let mut state = opened("class A {\n  void m() {\n    int x = 1;\n    use(x, x);\n  }\n}\n");
        assert_eq!(
            references(&mut state, 2, 8, true),
            vec![here(2, 8), here(3, 8), here(3, 11)]
        );
    }

    #[test]
    fn include_declaration_false_drops_the_declaration() {
        let mut state = opened("class A {\n  void m() {\n    int x = 1;\n    use(x, x);\n  }\n}\n");
        assert_eq!(
            references(&mut state, 3, 8, false),
            vec![here(3, 8), here(3, 11)]
        );
    }

    #[test]
    fn a_shadowed_local_is_a_different_target() {
        let mut state = opened(
            "class A {\n  int x;\n  void m() {\n    int x = 1;\n    use(x);\n  }\n  void n() { use(x); }\n}\n",
        );
        assert_eq!(
            references(&mut state, 3, 8, true),
            vec![here(3, 8), here(4, 8)]
        );
        assert_eq!(
            references(&mut state, 1, 6, true),
            vec![here(1, 6), here(6, 17)]
        );
    }

    #[test]
    fn a_field_selected_through_this_is_a_reference() {
        let mut state =
            opened("class A {\n  int count;\n  void m() { this.count = count + 1; }\n}\n");
        assert_eq!(
            references(&mut state, 1, 6, true),
            vec![here(1, 6), here(2, 18), here(2, 26)]
        );
    }

    #[test]
    fn a_methods_call_sites_are_its_references() {
        let mut state = opened(
            "class A {\n  void go() {}\n  void m(A other) { go(); other.go(); this.go(); }\n}\n",
        );
        assert_eq!(
            references(&mut state, 1, 7, true),
            vec![here(1, 7), here(2, 20), here(2, 32), here(2, 43)]
        );
    }

    #[test]
    fn only_the_overload_a_call_resolves_to_is_referenced() {
        let mut state = opened(
            "class A {\n  void f(String s) {}\n  void f(int i) {}\n  void go(String s) { f(s); }\n}\n",
        );
        state.install_classpath(Classpath::hand_built(&["java.lang.String"]));
        assert_eq!(
            references(&mut state, 2, 7, true),
            vec![here(2, 7)],
            "f(s) resolves to f(String), so f(int) has no call site"
        );
        assert_eq!(
            references(&mut state, 1, 7, true),
            vec![here(1, 7), here(3, 22)]
        );
    }

    #[test]
    fn a_type_the_file_declares_lists_its_uses_in_the_file() {
        let mut state = opened("package com.example;\nclass A { A self; }\nclass B { A a; }\n");
        assert_eq!(
            references(&mut state, 1, 6, true),
            vec![here(1, 6), here(1, 10), here(2, 10)]
        );
    }

    #[test]
    fn a_type_is_referenced_in_a_file_the_editor_never_opened() {
        let mut state = opened("package com.example;\npublic class Greeter {}\n");
        install(
            &mut state,
            vec![declared(
                "com.example.Greeter",
                "/w/com/example/A.java",
                range(1, 13, 7),
            )],
            vec![elsewhere(
                "/w/com/example/Reception.java",
                "com.example",
                &[],
                "Greeter",
                &[(4, 10)],
            )],
        );
        assert_eq!(
            references(&mut state, 1, 16, true),
            vec![
                here(1, 13),
                ("file:///w/com/example/Reception.java".to_owned(), 4, 10),
            ]
        );
    }

    #[test]
    fn a_same_named_type_in_another_package_is_not_a_reference() {
        let mut state = opened("package com.example;\npublic class Greeter {}\n");
        install(
            &mut state,
            vec![
                declared(
                    "com.example.Greeter",
                    "/w/com/example/A.java",
                    range(1, 13, 7),
                ),
                declared(
                    "org.other.Greeter",
                    "/w/org/other/Greeter.java",
                    range(1, 13, 7),
                ),
            ],
            vec![
                elsewhere(
                    "/w/org/other/User.java",
                    "org.other",
                    &[],
                    "Greeter",
                    &[(3, 4)],
                ),
                elsewhere(
                    "/w/net/Importer.java",
                    "net",
                    &["com.example.Greeter"],
                    "Greeter",
                    &[(5, 2)],
                ),
            ],
        );
        assert_eq!(
            references(&mut state, 1, 16, false),
            vec![("file:///w/net/Importer.java".to_owned(), 5, 2)],
            "org.other's own Greeter is a different type; the importer means this one"
        );
    }

    #[test]
    fn the_open_files_own_index_rows_are_not_listed() {
        // The index saw an older version of the open file, with a use on line
        // 7; the buffer has none there any more.
        let mut state = opened("package com.example;\npublic class Greeter {}\n");
        install(
            &mut state,
            vec![declared(
                "com.example.Greeter",
                "/w/com/example/A.java",
                range(1, 13, 7),
            )],
            vec![elsewhere(
                "/w/com/example/A.java",
                "com.example",
                &[],
                "Greeter",
                &[(1, 13), (7, 4)],
            )],
        );
        assert_eq!(references(&mut state, 1, 16, true), vec![here(1, 13)]);
    }

    #[test]
    fn a_jdk_type_is_referenced_wherever_a_file_means_it() {
        let mut state = opened("class A {\n  String s;\n}\n");
        state.install_classpath(Classpath::hand_built(&["java.lang.String"]));
        install(
            &mut state,
            Vec::new(),
            vec![elsewhere("/w/B.java", "p", &[], "String", &[(2, 2)])],
        );
        assert_eq!(
            references(&mut state, 1, 4, true),
            vec![("file:///w/B.java".to_owned(), 2, 2), here(1, 2)]
        );
    }

    #[test]
    fn definition_at_every_reference_lands_on_the_target() {
        let source = "class A {\n  int count;\n  void go() {}\n  void m(A other) {\n    \
                      this.count = count + other.count;\n    go(); other.go();\n  }\n}\n";
        let mut state = opened(source);
        for (line, character) in [(1, 6), (2, 7)] {
            let target = references(&mut state, line, character, true);
            assert!(target.len() > 2, "a target with uses: {target:?}");
            for (_, at_line, at_character) in &target {
                let answer = crate::handlers::definition::handle(
                    &mut state,
                    &serde_json::from_value(serde_json::json!({
                        "textDocument": { "uri": URI },
                        "position": { "line": at_line, "character": at_character },
                    }))
                    .expect("well-formed"),
                    &CancellationToken::new(),
                )
                .expect("not cancelled");
                let Some(GotoDefinitionResponse::Scalar(location)) = answer else {
                    panic!("no definition at {at_line}:{at_character}");
                };
                assert_eq!(
                    (location.range.start.line, location.range.start.character),
                    (line, character),
                    "definition at {at_line}:{at_character} must land on the target"
                );
            }
        }
    }

    /// The index said, at its last walk, that the open file declared
    /// `Greeter` at 1:10; the buffer now *uses* it there. The buffer is the
    /// authority for the open file, so the index's stale declaration must not
    /// take a live use out of an `includeDeclaration: false` answer.
    #[test]
    fn a_stale_declaration_row_for_the_open_file_does_not_hide_a_live_use() {
        let mut state = opened("package com.example;\nclass A { Greeter g; }\n");
        install(
            &mut state,
            vec![declared(
                "com.example.Greeter",
                "/w/com/example/A.java",
                range(1, 10, 7),
            )],
            Vec::new(),
        );
        assert_eq!(references(&mut state, 1, 12, false), vec![here(1, 10)]);
    }

    /// The walk and the buffer are two readers of one file, and the one rule
    /// has to hold across the seam: the references a file contributes cannot
    /// depend on whether it is the one open.
    #[test]
    fn a_file_answers_the_same_from_the_index_as_from_its_open_buffer() {
        let root = std::env::temp_dir().join(format!(
            "javelin-references-agreement-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let package = root.join("com").join("example");
        std::fs::create_dir_all(&package).expect("temp tree");
        let greeter = package.join("Greeter.java");
        let greeter_text = "package com.example;\n\
                            public class Greeter {\n\
                            \x20   Greeter() {}\n\
                            \x20   static Greeter make() { return new Greeter(); }\n\
                            \x20   void run(Greeter other) {}\n\
                            }\n";
        std::fs::write(&greeter, greeter_text).expect("write");
        let reception = package.join("Reception.java");
        let reception_text = "package com.example;\nclass Reception { Greeter g; }\n";
        std::fs::write(&reception, reception_text).expect("write");

        let found = javelin_index::java_sources(
            std::slice::from_ref(&root),
            1 << 20,
            &CancellationToken::new(),
        );
        let index = javelin_index::CacheDb::in_memory()
            .expect("db")
            .index_sources(&found, &CancellationToken::new())
            .expect("indexing");
        let index = std::sync::Arc::new(index);
        let greeter_uri = crate::handlers::file_uri(&greeter).expect("a uri");
        let reception_uri = crate::handlers::file_uri(&reception).expect("a uri");

        let in_greeter = |open: &lsp_types::Uri, text: &str, line: u32, character: u32| {
            let mut state = ServerState::new(Config::default()).expect("grammar loads");
            state.did_open(DocumentId::new(open.as_str()), 1, text.to_owned());
            state.install_workspace(crate::workspace::Workspace {
                index: std::sync::Arc::clone(&index),
                elapsed: std::time::Duration::ZERO,
                refreshed: false,
                survey: None,
                absorbed: 0,
            });
            let params: ReferenceParams = serde_json::from_value(serde_json::json!({
                "textDocument": { "uri": open.as_str() },
                "position": { "line": line, "character": character },
                "context": { "includeDeclaration": true },
            }))
            .expect("well-formed");
            let mut lines: Vec<(u32, u32)> = handle(&mut state, &params, &CancellationToken::new())
                .expect("not cancelled")
                .into_iter()
                .filter(|location| location.uri == greeter_uri)
                .map(|location| (location.range.start.line, location.range.start.character))
                .collect();
            lines.sort_unstable();
            lines
        };

        // From the file next door, Greeter.java is the index's; with
        // Greeter.java open, it is the buffer's.
        let from_index = in_greeter(&reception_uri, reception_text, 1, 18);
        let from_buffer = in_greeter(&greeter_uri, greeter_text, 1, 13);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(
            from_index, from_buffer,
            "the constructor's own name is not a use of the class in either reader"
        );
        assert_eq!(from_buffer, vec![(1, 13), (3, 11), (3, 39), (4, 13)]);
    }

    #[test]
    fn a_document_that_is_not_open_answers_an_empty_list() {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        assert_eq!(references(&mut state, 0, 0, true), Vec::new());
    }

    #[test]
    fn a_cursor_on_whitespace_answers_an_empty_list() {
        let mut state = opened("class A {\n\n  int x;\n}\n");
        assert_eq!(references(&mut state, 1, 0, true), Vec::new());
    }

    #[test]
    fn a_cancelled_token_returns_request_cancelled() {
        let mut state = opened("class A { int x; }");
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            handle(&mut state, &params(0, 14, true), &token).err(),
            Some(Cancelled)
        );
    }

    const GREETER_SOURCE: &str = "package com.example;\npublic class Greeter {\n    void greet() {}\n    void loud() { greet(); }\n}\n";
    const CALLER_SOURCE: &str = "package com.example;\nclass Caller {\n    void m(Greeter g) {\n        g.greet();\n    }\n}\n";

    fn open_in(
        sources: &crate::workspace::testing::Sources,
        relative: &str,
        text: &str,
    ) -> (ServerState, String) {
        let uri = crate::handlers::file_uri(&sources.path(relative)).expect("a uri");
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

    fn references_in(
        state: &mut ServerState,
        uri: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Vec<(String, u32, u32)> {
        let params: ReferenceParams = serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
            "context": { "includeDeclaration": include_declaration },
        }))
        .expect("well-formed");
        handle(state, &params, &CancellationToken::new())
            .expect("not cancelled")
            .into_iter()
            .map(|location| {
                let file = location
                    .uri
                    .as_str()
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                (
                    file,
                    location.range.start.line,
                    location.range.start.character,
                )
            })
            .collect()
    }

    /// Who calls a JAR method: the open file's calls, live, and another
    /// file's, from the rows the worker recorded under the member's key. A
    /// JDK method of a type nothing extends lists the open file's alone.
    #[test]
    fn a_library_methods_references_include_calls_in_other_files() {
        const CALLER: &str = "package com.example;\nimport org.lib.Repo;\n\
                              class Caller {\n    void m(Repo r, String s) {\n        \
                              r.save(s);\n        s.length();\n    }\n}\n";
        const OPEN: &str = "package com.example;\nimport org.lib.Repo;\n\
                            class Open {\n    void m(Repo r, String s) {\n        \
                            r.save(s);\n        r.find();\n        s.length();\n    }\n}\n";
        let sources = crate::workspace::testing::indexed_with_member_refs_over(
            "references-library",
            &[("com/example/Caller.java", CALLER)],
            crate::member_refs::library_world(),
        );
        let (mut state, uri) = open_in(&sources, "com/example/Open.java", OPEN);
        state.install_classpath(Classpath {
            world: crate::member_refs::library_world(),
            ..Classpath::hand_built(&[])
        });
        assert_eq!(
            references_in(&mut state, &uri, 4, 10, true),
            vec![
                ("Caller.java".to_owned(), 4, 10),
                ("Open.java".to_owned(), 4, 10),
            ]
        );
        assert_eq!(
            references_in(&mut state, &uri, 6, 10, true),
            vec![("Open.java".to_owned(), 6, 10)]
        );
    }

    /// Who calls `greet`, asked from the file that declares it: its own call
    /// in `loud`, live, and `Caller`'s, from the index.
    #[test]
    fn a_methods_references_include_calls_in_other_files() {
        let sources = crate::workspace::testing::indexed_with_member_refs(
            "references-member-declaring",
            &[
                ("com/example/Greeter.java", GREETER_SOURCE),
                ("com/example/Caller.java", CALLER_SOURCE),
            ],
        );
        let (mut state, uri) = open_in(&sources, "com/example/Greeter.java", GREETER_SOURCE);
        assert_eq!(
            references_in(&mut state, &uri, 2, 10, true),
            vec![
                ("Caller.java".to_owned(), 3, 10),
                ("Greeter.java".to_owned(), 2, 9),
                ("Greeter.java".to_owned(), 3, 18),
            ]
        );
    }

    /// A statically imported constant and method are used where they are
    /// named bare, in a file the editor never opened.
    #[test]
    fn a_static_members_references_include_statically_imported_uses() {
        const LIMITS: &str = "package p;\npublic class Limits {\n    \
                              public static final String MAX = \"m\";\n    \
                              public static int twice(int x) { return x; }\n}\n";
        const USE: &str = "package q;\nimport static p.Limits.MAX;\n\
                           import static p.Limits.twice;\nclass Use {\n    Object m() {\n        \
                           twice(1);\n        return MAX;\n    }\n}\n";
        let sources = crate::workspace::testing::indexed_with_member_refs(
            "references-static-imports",
            &[("p/Limits.java", LIMITS), ("q/Use.java", USE)],
        );
        let (mut state, uri) = open_in(&sources, "p/Limits.java", LIMITS);
        assert_eq!(
            references_in(&mut state, &uri, 2, 31, false),
            vec![("Use.java".to_owned(), 6, 15)]
        );
        assert_eq!(
            references_in(&mut state, &uri, 3, 22, false),
            vec![("Use.java".to_owned(), 5, 8)]
        );
    }

    const BOX: &str = "package com.example;\npublic class Box {\n    public Box(int size) {}\n    \
                       public Box(int a, int b) {}\n    private Box(String s) {}\n    \
                       Box twice() { return new Box(1, 2); }\n}\n";
    const USER: &str = "package com.example;\nclass User {\n    \
                        Object m() { Box b = new Box(1); return new Box(1, 2); }\n}\n";

    /// [`open_in`] with a walk's survey, which a claim read under a type
    /// hint's rules needs.
    fn surveyed(
        sources: &crate::workspace::testing::Sources,
        relative: &str,
        text: &str,
    ) -> (ServerState, String) {
        let (mut state, uri) = open_in(sources, relative, text);
        state.install_workspace(crate::workspace::Workspace {
            index: std::sync::Arc::clone(&sources.index),
            elapsed: std::time::Duration::ZERO,
            refreshed: false,
            survey: Some(std::sync::Arc::default()),
            absorbed: 0,
        });
        (state, uri)
    }

    /// A constructor is used where a `new` selects it — in its own file,
    /// live, and in one the editor never opened — and nowhere else its
    /// type's name is written.
    #[test]
    fn a_constructors_references_are_the_news_that_select_it() {
        let sources = crate::workspace::testing::indexed_with_member_refs(
            "references-constructor",
            &[
                ("com/example/Box.java", BOX),
                ("com/example/User.java", USER),
            ],
        );
        let both = vec![
            ("Box.java".to_owned(), 3, 11),
            ("Box.java".to_owned(), 5, 29),
            ("User.java".to_owned(), 2, 48),
        ];
        let (mut state, uri) = surveyed(&sources, "com/example/Box.java", BOX);
        assert_eq!(references_in(&mut state, &uri, 3, 12, true), both);
        assert_eq!(references_in(&mut state, &uri, 5, 30, true), both);
        assert_eq!(
            references_in(&mut state, &uri, 2, 12, true),
            vec![
                ("Box.java".to_owned(), 2, 11),
                ("User.java".to_owned(), 2, 29)
            ]
        );
        let (mut state, uri) = surveyed(&sources, "com/example/User.java", USER);
        assert_eq!(references_in(&mut state, &uri, 2, 49, true), both);
        assert_eq!(
            references_in(&mut state, &uri, 2, 30, false),
            vec![("User.java".to_owned(), 2, 29)]
        );
    }

    /// What Lombok writes into the file is read once per request and handed
    /// to every `new`: a model-covered `@Getter` still lets them answer, a
    /// `@Delegate` the model does not cover still refuses them.
    #[test]
    fn a_constructors_references_follow_the_files_one_lombok_reading() {
        let sources = crate::workspace::testing::indexed_with_member_refs(
            "references-constructor-lombok",
            &[
                ("com/example/Box.java", BOX),
                ("com/example/User.java", USER),
            ],
        );
        let user = |import: &str, field: &str| {
            USER.replace(
                "package com.example;\n",
                &format!("package com.example;\n{import}\n"),
            )
            .replace("class User {\n", &format!("class User {{\n    {field}\n"))
        };
        let getter = user("import lombok.Getter;", "@Getter int n;");
        let (mut state, uri) = surveyed(&sources, "com/example/User.java", &getter);
        assert_eq!(
            references_in(&mut state, &uri, 4, 49, true),
            vec![
                ("Box.java".to_owned(), 3, 11),
                ("Box.java".to_owned(), 5, 29),
                ("User.java".to_owned(), 4, 48),
            ]
        );
        let delegate = user(
            "import lombok.experimental.Delegate;",
            "@Delegate Object d;",
        );
        let (mut state, uri) = surveyed(&sources, "com/example/User.java", &delegate);
        assert!(!references_in(&mut state, &uri, 4, 49, true).contains(&(
            "Box.java".to_owned(),
            3,
            11
        )));
    }

    /// `super(…)` in a file the editor never opened, `this(…)` and an enum
    /// constant are uses of the constructor they select; the constant's own
    /// references are still the constant's.
    #[test]
    fn a_constructors_references_include_explicit_calls_and_enum_constants() {
        const CRATE: &str =
            "package com.example;\nclass Crate extends Box {\n    Crate() { super(1, 2); }\n}\n";
        const DAY: &str = "package com.example;\nenum Day {\n    MON(1), TUE;\n    \
                           Day(int n) {}\n    Day() { this(2); }\n}\n";
        let sources = crate::workspace::testing::indexed_with_member_refs(
            "references-explicit-constructor",
            &[
                ("com/example/Box.java", BOX),
                ("com/example/User.java", USER),
                ("com/example/Crate.java", CRATE),
                ("com/example/Day.java", DAY),
            ],
        );
        let two = vec![
            ("Box.java".to_owned(), 3, 11),
            ("Box.java".to_owned(), 5, 29),
            ("Crate.java".to_owned(), 2, 14),
            ("User.java".to_owned(), 2, 48),
        ];
        let (mut state, uri) = surveyed(&sources, "com/example/Box.java", BOX);
        assert_eq!(references_in(&mut state, &uri, 3, 12, true), two);
        let (mut state, uri) = surveyed(&sources, "com/example/Crate.java", CRATE);
        assert_eq!(references_in(&mut state, &uri, 2, 15, true), two);
        let (mut state, uri) = surveyed(&sources, "com/example/Day.java", DAY);
        assert_eq!(
            references_in(&mut state, &uri, 3, 5, false),
            vec![
                ("Day.java".to_owned(), 2, 4),
                ("Day.java".to_owned(), 4, 12)
            ]
        );
        assert_eq!(
            references_in(&mut state, &uri, 2, 5, true),
            vec![("Day.java".to_owned(), 2, 4)]
        );
    }

    /// The type's own references still list every `new` of it: its name
    /// there names the type, whichever constructor it selects.
    #[test]
    fn a_types_references_still_list_every_new_of_it() {
        let sources = crate::workspace::testing::indexed_with_member_refs(
            "references-constructor-type",
            &[
                ("com/example/Box.java", BOX),
                ("com/example/User.java", USER),
            ],
        );
        let (mut state, uri) = surveyed(&sources, "com/example/User.java", USER);
        let found = references_in(&mut state, &uri, 2, 18, false);
        for new in [
            ("User.java".to_owned(), 2, 29),
            ("User.java".to_owned(), 2, 48),
        ] {
            assert!(found.contains(&new), "{found:?}");
        }
        assert!(
            found.contains(&("User.java".to_owned(), 2, 17)),
            "{found:?}"
        );
    }

    const RUNNER: &str = "package p;\npublic interface Runner {\n    void run();\n}\n";
    const FAST: &str =
        "package p;\npublic class Fast implements Runner {\n    public void run() {}\n}\n";
    const SLOW: &str =
        "package p;\npublic class Slow implements Runner {\n    public void run() {}\n}\n";
    const USE: &str = "package p;\nclass Use {\n    void m(Runner r, Fast f, Slow s) {\n        \
                       r.run();\n        f.run();\n        s.run();\n    }\n}\n";

    fn runners(name: &str) -> crate::workspace::testing::Sources {
        crate::workspace::testing::indexed_with_member_refs(
            name,
            &[
                ("p/Runner.java", RUNNER),
                ("p/Fast.java", FAST),
                ("p/Slow.java", SLOW),
                ("p/Use.java", USE),
            ],
        )
    }

    /// References on an interface method list the calls resolved to every
    /// override of it too; an override's own declaration is not a use.
    #[test]
    fn an_interface_methods_references_include_calls_to_its_overrides() {
        let sources = runners("references-overrides-down");
        let (mut state, uri) = open_in(&sources, "p/Runner.java", RUNNER);
        assert_eq!(
            references_in(&mut state, &uri, 2, 10, false),
            vec![
                ("Use.java".to_owned(), 3, 10),
                ("Use.java".to_owned(), 4, 10),
                ("Use.java".to_owned(), 5, 10),
            ]
        );
    }

    /// References on an override list the calls resolved to what it
    /// overrides, and not those resolved to a sibling override.
    #[test]
    fn an_overrides_references_include_calls_to_what_it_overrides_and_not_its_siblings() {
        let sources = runners("references-overrides-up");
        let (mut state, uri) = open_in(&sources, "p/Fast.java", FAST);
        assert_eq!(
            references_in(&mut state, &uri, 2, 17, false),
            vec![
                ("Use.java".to_owned(), 3, 10),
                ("Use.java".to_owned(), 4, 10)
            ]
        );
        // Asked from a call site in another file, with the declaration: the
        // target's own declaration, and not the interface's.
        let (mut state, uri) = open_in(&sources, "p/Use.java", USE);
        assert_eq!(
            references_in(&mut state, &uri, 4, 11, true),
            vec![
                ("Fast.java".to_owned(), 2, 16),
                ("Use.java".to_owned(), 3, 10),
                ("Use.java".to_owned(), 4, 10),
            ]
        );
    }

    /// An override declared in the open file is on the line, and its own
    /// declaration is still not a use of the method it overrides.
    #[test]
    fn an_override_declared_in_the_same_file_is_followed_and_not_listed() {
        let both = "package p;\ninterface I {\n    void go();\n}\nclass Impl implements I {\n    \
                    public void go() {}\n    void use(I i, Impl x) {\n        i.go();\n        \
                    x.go();\n    }\n}\n";
        let sources = crate::workspace::testing::indexed_with_member_refs(
            "references-overrides-same-file",
            &[("p/Both.java", both)],
        );
        let (mut state, uri) = open_in(&sources, "p/Both.java", both);
        assert_eq!(
            references_in(&mut state, &uri, 2, 10, false),
            vec![
                ("Both.java".to_owned(), 7, 10),
                ("Both.java".to_owned(), 8, 10)
            ]
        );
        assert_eq!(
            references_in(&mut state, &uri, 2, 10, true),
            vec![
                ("Both.java".to_owned(), 2, 9),
                ("Both.java".to_owned(), 7, 10),
                ("Both.java".to_owned(), 8, 10)
            ]
        );
    }

    /// Each a pair `overrides` would call an override and Java does not, or
    /// one only a stale index would: none of them may put a call on the
    /// line. Asked on the subtype's method, each answers nothing.
    #[test]
    fn what_is_not_an_override_puts_no_call_on_the_line() {
        for (name, files, open, line, character) in [
            // A generic method's own `T` is no type a subtype pins.
            (
                "references-not-override-generic",
                vec![
                    (
                        "p/Base.java",
                        "package p;\npublic class Base {\n    public <T> void log(T value) {}\n}\n",
                    ),
                    (
                        "p/Sub.java",
                        "package p;\npublic class Sub extends Base {\n    public void log(String s) {}\n}\n",
                    ),
                    (
                        "p/Use.java",
                        "package p;\nclass Use {\n    void m(Base b) {\n        b.log(1);\n    }\n}\n",
                    ),
                ],
                "p/Sub.java",
                2,
                17,
            ),
            // An overload of a method whose parameter is the type's variable.
            (
                "references-not-override-overload",
                vec![
                    (
                        "p/Box.java",
                        "package p;\npublic interface Box<T> {\n    void put(T t);\n}\n",
                    ),
                    (
                        "p/IntBox.java",
                        "package p;\npublic abstract class IntBox implements Box<String> {\n    public void put(Integer i) {}\n}\n",
                    ),
                    (
                        "p/Use.java",
                        "package p;\nclass Use {\n    void m(Box<String> b) {\n        b.put(\"x\");\n    }\n}\n",
                    ),
                ],
                "p/IntBox.java",
                2,
                17,
            ),
            // A package-private method is not inherited by another package.
            (
                "references-not-override-package",
                vec![
                    (
                        "a/A.java",
                        "package a;\npublic class A {\n    void run() {}\n    public void go() { this.run(); }\n}\n",
                    ),
                    (
                        "b/B.java",
                        "package b;\nimport a.A;\npublic class B extends A {\n    public void run() {}\n}\n",
                    ),
                ],
                "b/B.java",
                3,
                17,
            ),
        ] {
            let sources = crate::workspace::testing::indexed_with_member_refs(name, &files);
            let text = files
                .iter()
                .find(|(path, _)| *path == open)
                .expect("open")
                .1;
            let (mut state, uri) = open_in(&sources, open, text);
            assert_eq!(
                references_in(&mut state, &uri, line, character, false),
                vec![],
                "{name}"
            );
        }
    }

    /// The index is a quiet period behind the buffer: a method typed where
    /// another one stood is not that one's line.
    #[test]
    fn a_stale_index_offset_does_not_lend_another_methods_line() {
        let base = "package p;\npublic class Base {\n    public void bar() {}\n    public void foo() {}\n}\n";
        let indexed = "package p;\npublic class Sub extends Base {\n    public void bar() {}\n}\n";
        let live = "package p;\npublic class Sub extends Base {\n    public void foo() {}\n}\n";
        let usage = "package p;\nclass Use {\n    void m(Base b) {\n        b.bar();\n    }\n}\n";
        let sources = crate::workspace::testing::indexed_with_member_refs(
            "references-stale-offset",
            &[
                ("p/Base.java", base),
                ("p/Sub.java", indexed),
                ("p/Use.java", usage),
            ],
        );
        let (mut state, uri) = open_in(&sources, "p/Sub.java", live);
        assert_eq!(references_in(&mut state, &uri, 2, 17, false), vec![]);
    }

    /// A generated visitor: sixty overloads, each implemented by every one of
    /// many types. The upward walk must not redo the downward one per
    /// overload.
    #[test]
    fn a_wide_visitor_hierarchy_answers_inside_the_references_budget() {
        let overloads = 60;
        let mut files: Vec<(String, String)> = Vec::new();
        let mut visitor = String::from("package v;\npublic interface Visitor {\n");
        for n in 0..overloads {
            visitor.push_str(&format!("    void visit(N{n} n);\n"));
            files.push((
                format!("v/N{n}.java"),
                format!("package v;\npublic class N{n} {{}}\n"),
            ));
        }
        visitor.push_str("}\n");
        files.push(("v/Visitor.java".into(), visitor));
        for i in 0..150 {
            let mut body = format!("package v;\npublic class Impl{i} implements Visitor {{\n");
            for n in 0..overloads {
                body.push_str(&format!("    public void visit(N{n} n) {{}}\n"));
            }
            body.push_str("}\n");
            files.push((format!("v/Impl{i}.java"), body));
        }
        let borrowed: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect();
        let sources = crate::workspace::testing::indexed_with_member_refs(
            "references-wide-visitor",
            &borrowed,
        );
        let open = &files
            .iter()
            .find(|(path, _)| path == "v/Impl0.java")
            .expect("Impl0")
            .1;
        let (mut state, uri) = open_in(&sources, "v/Impl0.java", open);
        let started = std::time::Instant::now();
        let _ = references_in(&mut state, &uri, 2, 17, false);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(400),
            "{:?}",
            started.elapsed()
        );
    }

    /// The same question asked from a third file's call site: the target is
    /// a member another file declares, found through the call under the
    /// cursor, and every file's uses of it are listed.
    #[test]
    fn a_cross_file_members_references_are_listed_from_any_call_site() {
        let third =
            "package com.example;\nclass Third {\n    void t(Greeter g) { g.greet(); }\n}\n";
        let sources = crate::workspace::testing::indexed_with_member_refs(
            "references-member-third",
            &[
                ("com/example/Greeter.java", GREETER_SOURCE),
                ("com/example/Caller.java", CALLER_SOURCE),
                ("com/example/Third.java", third),
            ],
        );
        let (mut state, uri) = open_in(&sources, "com/example/Third.java", third);
        assert_eq!(
            references_in(&mut state, &uri, 2, 27, true),
            vec![
                ("Caller.java".to_owned(), 3, 10),
                ("Greeter.java".to_owned(), 2, 9),
                // `loud`'s own call: the declaring file is not open, so this
                // one comes out of the index too.
                ("Greeter.java".to_owned(), 3, 18),
                ("Third.java".to_owned(), 2, 26),
            ]
        );
        assert_eq!(
            references_in(&mut state, &uri, 2, 27, false),
            vec![
                ("Caller.java".to_owned(), 3, 10),
                ("Greeter.java".to_owned(), 3, 18),
                ("Third.java".to_owned(), 2, 26),
            ],
            "without the declaration"
        );
    }
}
