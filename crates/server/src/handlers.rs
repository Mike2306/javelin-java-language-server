//! One module per LSP request.
//!
//! A handler translates: it takes LSP-shaped parameters, asks the engine a
//! question in plain Rust, and turns the plain Rust answer back into
//! `lsp-types`. The engine crates below this one stay unaware that LSP exists,
//! which is what keeps them usable from the CLI and the benchmarks.
//!
//! ponytail: `ServerState::view` warms a `FileSyntax` and a `ScopeTree`
//! whichever handler asks for it, and [`selection_range`] reads neither — only
//! the tree and the document. A reparse arrives from the pool already warm, so
//! the ceiling is one unused build of each on the version `didOpen` parsed,
//! when a selection is the first thing asked of it. The upgrade is a
//! tree-and-document accessor on `ServerState` keeping `view`'s coherency —
//! both from one version, or the answer is a mix. [`folding_range`] reads the
//! tree alone, which `ServerState::tree` already hands out.

pub mod code_action;
pub mod completion;
pub mod definition;
pub mod document_highlight;
pub mod document_symbol;
pub mod folding_range;
pub mod formatting;
pub mod hierarchy;
pub mod hover;
pub mod inlay_hint;
pub(crate) mod javadoc;
#[cfg(test)]
mod lombok_tests;
pub mod references;
pub mod rename;
pub mod selection_range;
pub mod semantic_tokens;
pub mod type_definition;
pub mod workspace_symbol;

use std::path::Path;
use std::str::FromStr as _;

use javelin_document::DocumentId;
use javelin_parser::Definition;
use javelin_protocol::{Answer, Readiness};
use lsp_types::Uri;

use crate::ServerState;

/// Where an answer from these four features stops improving.
///
/// The classpath is the last input any of them takes that this ladder
/// describes, so a partial answer is one given before it arrived. The
/// workspace index is an input too and is not on the ladder — see the
/// `ponytail:` on `ServerState::install_workspace` for what that overclaims
/// and why nothing reads it.
const COMPLETE_AT: Readiness = Readiness::Dependencies;

/// Wrap what a handler found in the vocabulary [`javelin_protocol`] exists for.
///
/// `complete` is the caller's judgement rather than this function's, and the
/// three callers judge differently. Completion is complete once the classpath
/// is in. Definition and hover are complete the moment they land on anything
/// at all — the classpath can add a resolution where there was none and never
/// moves one, so only *nothing found* can still improve. "Anything at all" is
/// no longer the same as "something the open file declares": both of them
/// answer an external member — hover from the facts, definition from the
/// `src.zip` entry those facts name — which is an answer the classpath
/// supplied rather than one it could improve.
///
/// This is not [`completion::handle`]'s `isIncomplete`, which is the strictly
/// stronger claim that a list is the whole of what a position offers. The
/// classpath's arrival is one conjunct of that predicate and the whole of this
/// one.
pub(crate) fn answer<T>(value: T, complete: bool) -> Answer<T> {
    if complete {
        Answer::Complete(value)
    } else {
        Answer::Partial {
            value,
            complete_at: COMPLETE_AT,
        }
    }
}

/// What the identifier at an LSP position refers to, if this file declares it.
///
/// Shared by definition and hover: they ask exactly the same question and
/// differ only in what they render from the answer.
pub(crate) fn resolve_at(
    state: &mut ServerState,
    id: &DocumentId,
    position: lsp_types::Position,
) -> Option<Definition> {
    // Taken before the view, which borrows the state mutably: the world is an
    // `Arc` clone, so holding both costs a refcount.
    let externals = state.externals();
    let view = state.view(id)?;
    let offset = view.document.offset_at(to_position(position));
    // ponytail: `definition_in`'s semantic queries answer `None` both off
    // their shape and on it unresolved, so an unresolved `a.b` or `go()`
    // falls through to the file-local query. No wrong jump is reachable only
    // because `definition_at`'s `is_simple_name` refuses a field access's
    // right-hand name and every call's name; a step added to the chain gets
    // no such guard. The upgrade is a three-way answer — not this shape,
    // refused, found — so a refusal ends the chain.
    let files = javelin_semantic::Files::single(id.clone(), view.syntax, view.scopes);
    definition_in(&view, &files, &externals, offset)
}

/// [`resolve_at`]'s question at a byte offset into a view already in hand.
///
/// The chain definition answers a position with, in the order it asks: a
/// member selection by its receiver's type, an unqualified call by its
/// enclosing type, then a plain name against the file. One function so that
/// a caller asking it at many offsets — references, which keeps a token
/// exactly when this lands where the cursor's did — asks the identical
/// question definition does, rather than a copy that could drift from it.
/// Such a caller passes one `files` for every offset, which memoises what
/// each enclosing type's walk says about bare names.
pub(crate) fn definition_in(
    view: &crate::state::FileView<'_>,
    files: &javelin_semantic::Files<'_>,
    externals: &dyn javelin_semantic::Externals,
    offset: u32,
) -> Option<Definition> {
    if let Some(found) =
        javelin_semantic::member_definition_at(&view.document, view.tree, files, externals, offset)
    {
        return Some(found);
    }
    if let Some(found) =
        javelin_semantic::call_definition_at(&view.document, view.tree, files, externals, offset)
    {
        return Some(found);
    }
    // Under an enum selector a `case` name is the enum's constant (JLS 14.11.1)
    // whatever the scope binds: one another file declares is
    // `external_member_at`'s answer, and an unknown selector may be an enum.
    if !javelin_semantic::scope_answers_case_label(
        &view.document,
        view.tree,
        files,
        externals,
        offset,
    ) {
        return None;
    }
    javelin_parser::definition_at(&view.document, view.tree, view.scopes, view.syntax, offset)
}

/// The survey a strict claim reads, or `None` where it may not be trusted:
/// one that skipped a file for its size, hid one or found a foreign source.
pub(crate) fn trusted_survey(state: &ServerState) -> Option<std::sync::Arc<javelin_index::Survey>> {
    state
        .survey()
        .filter(|survey| !survey.skipped_large && !survey.hidden && !survey.foreign)
}

/// Whether a claim at `offset`, read under a type hint's rules, is refused
/// for where it is — inside a local or an anonymous class body, or in a file
/// Lombok writes past the model in — or for a name it read that a generated
/// source, a static import or an inherited member type may mean. Every name
/// is refused before the workspace's first walk.
///
/// `lombok_short` is [`inlay_hint::lombok_short`] for the view's file, asked
/// once per request by the caller: it reads the whole file.
pub(crate) fn unclaimable(
    view: &crate::state::FileView<'_>,
    lombok_short: bool,
    externals: &crate::classpath::ClasspathExternals,
    survey: Option<&javelin_index::Survey>,
    offset: u32,
    written: &[String],
) -> bool {
    let bodies = crate::diagnostics::local_and_anonymous_bodies(
        &view.syntax.declarations,
        &(offset..offset + 1),
    );
    bodies
        .iter()
        .any(|body| body.start_byte <= offset && offset < body.end_byte)
        || lombok_short
        || survey.map_or(!written.is_empty(), |survey| {
            written.iter().any(|name| {
                survey.generated.contains(name) || shadowed(name, view.syntax, externals)
            })
        })
}

/// The constructor the `new` whose class name is at `offset` selects, or the
/// `this(…)` or `super(…)` whose keyword is — and, with `constants`, the enum
/// constant whose name is — where it may be claimed: definition, hover and
/// references ask it ahead of the type the name means. `lombok_short` is
/// [`unclaimable`]'s.
pub(crate) fn constructor_in(
    view: &crate::state::FileView<'_>,
    id: &DocumentId,
    externals: &crate::classpath::ClasspathExternals,
    survey: Option<&javelin_index::Survey>,
    offset: u32,
    constants: bool,
    lombok_short: bool,
) -> Option<javelin_semantic::SelectedConstructor> {
    let files = javelin_semantic::Files::single(id.clone(), view.syntax, view.scopes);
    let found = javelin_semantic::constructor_at(
        &view.document,
        view.tree,
        &files,
        externals,
        offset,
        constants,
    )?;
    // The file's own type read through the index has lost the `private`
    // constructors this file may call — `new com.example.Box(1)` inside `Box`.
    if let javelin_semantic::ConstructorTarget::External(summary) = &found.target
        && let Some(site) = &summary.site
        && crate::workspace::path_of(id.as_str())
            .is_some_and(|own| std::path::Path::new(&site.source) == own.as_path())
    {
        return None;
    }
    // An anonymous class's own `new T(…)` is outside its body (JLS 15.9.5),
    // but its decl's span starts at `new`: ask the body gate just before it.
    fn header(decls: &[javelin_parser::Decl], offset: u32) -> Option<u32> {
        decls.iter().find_map(|decl| {
            let name = decl.name_span;
            if decl.kind == javelin_parser::DeclKind::AnonymousClass
                && name.start_byte <= offset
                && offset < name.end_byte
            {
                return Some(decl.span.start_byte.saturating_sub(1));
            }
            header(&decl.children, offset)
        })
    }
    let at = header(&view.syntax.declarations, offset).unwrap_or(offset);
    (!unclaimable(view, lombok_short, externals, survey, at, &found.written)).then_some(found)
}

/// The fully-qualified type the plain name under the cursor means, when the
/// open file does not declare it, and the name's span.
///
/// Shared by definition, hover and the two placings below for the reason
/// [`resolve_at`] is shared: one question, several renderings.
/// `javelin-semantic` resolves the name against the open file's package and
/// imports through the one table that holds the workspace's types and the
/// world's; turning the answer into a *place* is this side's.
///
/// The resolver reads a name through the file's scopes and declarations and
/// then JLS 7.5's tiers, and Java has three more ways for a plain name to
/// mean something else, which it does not model: a member type a supertype
/// declares is in scope by inheritance, a static import can bring one in,
/// and a type the index does not hold — a generated one it never read, one
/// in a file it skipped, any before its first walk — can be a sibling that
/// outranks an import. Each would turn `Error` into `java.lang.Error` where
/// Java means another, so each answers `None` here: the gates the type
/// diagnostic puts in front of the same resolver.
pub(crate) fn type_name_at(
    state: &mut ServerState,
    id: &DocumentId,
    position: lsp_types::Position,
) -> Option<(javelin_parser::Span, String)> {
    // Taken before the view, which borrows the state mutably — the same
    // order, and for the same reason, as [`resolve_at`].
    let externals = state.externals();
    let survey = state
        .survey()
        .filter(|survey| !survey.skipped_large && !survey.hidden)?;
    let view = state.view(id)?;
    let offset = view.document.offset_at(to_position(position));
    let files = javelin_semantic::Files::single(id.clone(), view.syntax, view.scopes);
    let found =
        javelin_semantic::external_type_at(&view.document, view.tree, &files, &externals, offset)?;
    // A qualified name is judged by its segments: any of them a generated
    // name, or its first one shadowed, is enough. An import's is canonical
    // (JLS 7.5.1), so nothing inherited or imported shadows it.
    let written = found.written.as_str();
    let first = written.split('.').next().unwrap_or(written);
    let in_import = view.syntax.imports.iter().any(|import| {
        import.name_span.start_byte <= found.reference.start_byte
            && found.reference.end_byte <= import.name_span.end_byte
    });
    // A generated name is one the index never read; a processor's output is
    // read, so its names are not among them.
    //
    // ponytail: a stale processor type left in `build/` by a build that no
    // longer generates it still outranks an on-demand import of the same
    // simple name, so a jump can land on a type javac would not see. The
    // upgrade is dropping output older than the last build's start, which
    // needs the build tool's view of what it ran.
    if written
        .split('.')
        .any(|segment| survey.generated.contains(segment))
        || (!in_import && shadowed(first, view.syntax, &externals))
    {
        return None;
    }
    Some((found.reference, found.fqn))
}

/// Whether `name` may mean a member type this file inherits or imports
/// statically, rather than what the tiers resolved it to.
pub(crate) fn shadowed(
    name: &str,
    syntax: &javelin_parser::FileSyntax,
    externals: &crate::classpath::ClasspathExternals,
) -> bool {
    let static_import = syntax
        .imports
        .iter()
        .filter(|import| import.is_static)
        .any(|import| {
            if import.is_on_demand {
                externals.knows_type(&format!("{}.{name}", import.name))
            } else {
                import.name.rsplit('.').next() == Some(name)
            }
        });
    if static_import {
        return true;
    }
    let package = syntax.package.as_ref().map(|package| package.name.as_str());
    // A closure that cannot be shown whole may inherit the name from where
    // it stops.
    crate::diagnostics::frames(syntax, package, externals).is_none_or(|frames| {
        frames
            .iter()
            .any(|frame| externals.knows_type(&format!("{frame}.{name}")))
    })
}

/// The workspace type the name under the cursor means, and where the index
/// last saw it declared.
///
/// `None` at every step that cannot be taken, and in particular for a name
/// that resolved to a type the workspace index does not hold — a JDK or a JAR
/// type, which [`jdk_type_at`] places instead.
///
/// Two files declaring one fully-qualified name answer `None` as well. A
/// stray copy or a half-finished move is an ordinary broken workspace, and
/// picking whichever the walk happened to see first is the guess this codebase
/// does not make.
///
/// ponytail: the range comes out of the index, which follows saves, watched
/// changes and open buffers after a quiet period — so it can be a
/// quarter-second old, and older for a change on disk a client does not
/// report. The same windows `handlers/workspace_symbol.rs` records.
pub(crate) fn workspace_type_at(
    state: &mut ServerState,
    id: &DocumentId,
    position: lsp_types::Position,
) -> Option<(javelin_parser::Span, javelin_index::WorkspaceType)> {
    let (reference, fqn) = type_name_at(state, id, position)?;
    let workspace = state.workspace();
    let mut hits = workspace.with_fqn(&fqn);
    let ty = hits.next()?;
    hits.next().is_none().then(|| (reference, ty.clone()))
}

/// The classpath type the name under the cursor means, its simple name, and
/// where its source writes that name: an entry of the JDK's `src.zip` or of a
/// dependency's `-sources.jar`, still to be extracted.
///
/// `None` for a name the workspace declares at all — once or twice, since
/// [`workspace_type_at`] places the one and refuses the two, and a vendored
/// copy of a JDK class is the workspace's to place — and for a fact with no
/// source: a JAR type whose sources are not on disk.
pub(crate) fn jdk_type_at(
    state: &mut ServerState,
    id: &DocumentId,
    position: lsp_types::Position,
) -> Option<(javelin_semantic::ExternalSite, String)> {
    let (_, fqn) = type_name_at(state, id, position)?;
    let externals = state.externals();
    if externals.workspace().with_fqn(&fqn).next().is_some() {
        return None;
    }
    let simple = fqn.rsplit('.').next().unwrap_or(&fqn).to_owned();
    Some((externals.type_site(&fqn)?, simple))
}

/// The text a site is in, and the file holding it: an open buffer from the
/// document store, any other workspace file from disk, or the JDK's or a
/// sources JAR's entry, extracted under the cache root.
///
/// ponytail: a file is read whole on the request loop — once per hover for a
/// workspace file, once per entry for one the extractor's few cached texts
/// do not hold — and the first hover into a JDK or JAR source opens its
/// archive, as the first jump does. The upgrade is the index keeping the
/// comments it needs, which costs the store their text.
pub(crate) fn site_text(
    state: &mut ServerState,
    site: &javelin_semantic::ExternalSite,
) -> Option<(std::path::PathBuf, String)> {
    let max_bytes = state.config.index.max_file_size_bytes;
    if site.name_range.is_some() {
        let path = std::path::PathBuf::from(&site.source);
        // An open buffer is what the index's rows for it were read from.
        if let Some(open) =
            file_uri(&path).and_then(|uri| state.documents.get(&DocumentId::new(uri.as_str())))
        {
            return Some((path, open.text().to_owned()));
        }
        if std::fs::metadata(&path).ok()?.len() > max_bytes {
            return None;
        }
        let bytes = std::fs::read(&path).ok()?;
        return Some((path, javelin_index::decode_source(bytes)));
    }
    // A dependency's site names its sources JAR before the entry; a JDK
    // entry name never holds the separator.
    match site.source.rsplit_once(javelin_classpath::IN_ARCHIVE) {
        Some((archive, entry)) => state
            .jar_sources(std::path::Path::new(archive))?
            .materialize(entry, max_bytes),
        None => state.jdk_sources()?.materialize(&site.source, max_bytes),
    }
}

/// The `/** … */` at `doc` in `text`, when the declaration is still where
/// the site says — `name` at `name_start..name_end`, or the field a Lombok
/// accessor of that name is generated from — and the comment is right before
/// it: offsets read earlier than the text is are refused rather than trusted.
pub(crate) fn doc_at<'t>(
    text: &'t str,
    name: &str,
    (name_start, name_end): (u32, u32),
    (doc_start, doc_end): (u32, u32),
) -> Option<&'t str> {
    let written = text.get(name_start as usize..name_end as usize)?;
    let mut chars = written.chars();
    let accessor_of = chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect::<String>())
        .is_some_and(|field| name.len() > field.len() && name.ends_with(&field));
    if !(written == name || accessor_of) || doc_end > name_start {
        return None;
    }
    // What is between the comment and the name is that declaration's
    // modifiers, annotations and type: never the end of another one.
    let between = text.get(doc_end as usize..name_start as usize)?;
    if between.contains([';', '{', '}']) {
        return None;
    }
    let comment = text.get(doc_start as usize..doc_end as usize)?;
    // `/**/` is an empty block comment, not a doc comment.
    (comment.len() > 4 && comment.starts_with("/**") && comment.ends_with("*/")).then_some(comment)
}

/// Translate a document range onto the wire.
///
/// `javelin-document` computes UTF-16 positions already — the two types differ
/// only in which crate they come from.
pub(crate) fn to_lsp_range(range: javelin_document::Range) -> lsp_types::Range {
    lsp_types::Range::new(to_lsp_position(range.start), to_lsp_position(range.end))
}

pub(crate) fn to_lsp_position(position: javelin_document::Position) -> lsp_types::Position {
    lsp_types::Position::new(position.line, position.character)
}

/// Translate a position off the wire. The inverse of [`to_lsp_position`]; both
/// live here so the two directions cannot drift apart.
pub(crate) fn to_position(position: lsp_types::Position) -> javelin_document::Position {
    javelin_document::Position::new(position.line, position.character)
}

/// A `file:` URI naming a path on this machine.
///
/// Built rather than parsed, because a path is not a URI. [`Uri`] is an
/// RFC 3986 parse: a cache directory under a home with a space or a non-ASCII
/// character in it would not parse at all, and on Windows the separator is a
/// backslash, which a URI path never is. Percent-encoding everything outside
/// the unreserved set on the way in makes the result parse by construction.
///
/// `None` for a path that is not UTF-8, which has no percent-encoding
/// without knowing the platform's encoding and which the cache root cannot
/// produce on its own, and for a relative one — a relative `index.cacheDir` —
/// which a URI would name from the root instead. Rooted is read as a string
/// rather than through `Path::is_absolute`, so the Windows shape is
/// recognised, and tested, on every platform.
pub(crate) fn file_uri(path: &Path) -> Option<Uri> {
    let path = path.to_str()?;
    let drive =
        matches!(path.as_bytes(), [letter, b':', b'/' | b'\\', ..] if letter.is_ascii_alphabetic());
    if !(drive || path.starts_with(['/', '\\'])) {
        return None;
    }
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            // The unreserved set (RFC 3986 2.3), plus the two delimiters a
            // path segment is allowed to carry: `/` between segments and `:`
            // for a Windows drive letter.
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                encoded.push(char::from(byte));
            }
            b'\\' => encoded.push('/'),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    // `file://` names an empty authority, so what follows has to start at the
    // path's own root. A Unix path already does; a Windows one starts at a
    // drive letter and needs the slash put in front of it.
    let root = if encoded.starts_with('/') { "" } else { "/" };
    Uri::from_str(&format!("file://{root}{encoded}")).ok()
}
