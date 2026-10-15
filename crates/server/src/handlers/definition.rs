//! `textDocument/definition` — jump to a declaration this server can resolve.
//!
//! The answer is a location in the file the *declaration* is in, which is not
//! always the file the request named. A member of a classpath type is written
//! down in the JDK's own sources, and the jump goes there: the `src.zip` entry
//! is extracted and the location names the extracted file.
//!
//! So every URI here is built from what was resolved and never cloned off the
//! request. A name nothing resolves answers null rather than the nearest
//! same-named thing, and a declaration whose own file cannot be named answers
//! null rather than being rendered against the requested buffer — an editor
//! that jumps somewhere wrong is worse than one that does not jump.

use std::str::FromStr;

use javelin_core::{CancellationToken, Cancelled};
use javelin_document::{DocumentId, TextDocument};
use javelin_parser::Definition;
use javelin_protocol::Answer;
use lsp_types::{GotoDefinitionParams, GotoDefinitionResponse, Location, Uri};

use crate::ServerState;
use crate::handlers::{file_uri, resolve_at, to_lsp_range, to_position};

pub fn handle(
    state: &mut ServerState,
    params: &GotoDefinitionParams,
    token: &CancellationToken,
) -> Result<Option<GotoDefinitionResponse>, Cancelled> {
    Ok(answer(state, params, token)?.value().flatten())
}

/// The same answer, still carrying whether it is the best one there will be.
///
/// LSP has nowhere to put that for a definition — there is no `isIncomplete`
/// on this response — so nothing goes on the wire. It is on the seam anyway
/// because a client-side retry policy, and every test of one, needs to be able
/// to ask.
pub(crate) fn answer(
    state: &mut ServerState,
    params: &GotoDefinitionParams,
    token: &CancellationToken,
) -> Result<Answer<Option<GotoDefinitionResponse>>, Cancelled> {
    token.check()?;
    let at = &params.text_document_position_params;
    let id = DocumentId::new(at.text_document.uri.as_str());
    // `resolve_at` first and unchanged, exactly as hover does it, so every
    // answer that had one keeps it; only a position it cannot address at all
    // falls through. A classpath member is such a position — `resolve_at`
    // answers with a `Definition`, and a fact has no declaration span in any
    // file this server opened for one to be built from.
    // The class name in a `new` names the type, and the constructor its
    // arguments select is the nearer answer where there is one.
    let found = match constructor(state, &id, at.position) {
        Some(found) => Some(found),
        None => match resolve_at(state, &id, at.position) {
            Some(found) => located(&found),
            // Two positions the open buffer cannot address, tried in the order
            // they are cheap to refuse: a member of a classpath type, and then a
            // *name* that means a type declared somewhere else in the workspace.
            // They cannot both answer — one is the right-hand side of a `.` and
            // the other is a plain name — so the `or_else` is a fall-through
            // rather than a preference.
            // A JDK type's name is tried last, because it is the one name the
            // workspace can also declare and the workspace's copy wins.
            None => external(state, &id, at.position)
                .or_else(|| workspace_type(state, &id, at.position))
                .or_else(|| jdk_type(state, &id, at.position)),
        },
    }
    .map(GotoDefinitionResponse::Scalar);
    token.check()?;
    // A location found at all is final: the classpath adds resolutions where
    // there were none and never moves one, so only *nothing found* can still
    // improve.
    let complete = found.is_some() || state.is_complete();
    Ok(crate::handlers::answer(found, complete))
}

/// Where a declaration is, in the file that declares it.
///
/// The URI is built from [`Definition::file`] and never cloned off the
/// request. The two were the same string until a query could reach more than
/// one file, and a caller that still assumes so renders a foreign span against
/// the requested buffer — a jump into the wrong place, which is worse than no
/// jump at all.
///
/// A `DocumentId` that will not parse as a URI answers `None` for that same
/// reason. It cannot arise from a request, since one arrived as a `Uri`; and
/// substituting the requested URI in its place is precisely the wrong jump.
fn located(found: &Definition) -> Option<Location> {
    let uri = Uri::from_str(found.file.as_str())
        .inspect_err(|err| {
            tracing::debug!(
                file = found.file.as_str(),
                %err,
                "definition: the declaring file is not a URI, so there is nowhere to jump"
            );
        })
        .ok()?;
    // The name alone, not the whole declaration: an editor puts the caret where
    // the range starts, and that should be on the identifier.
    Some(Location::new(uri, to_lsp_range(found.name_span.range)))
}

/// The classpath member the cursor is on, as a location in the source entry
/// that declares it.
///
/// [`crate::handlers::hover`]'s `external` is the sibling of this, and the two
/// start from the same [`javelin_semantic::ExternalSummary`]: hover renders the
/// facts it carries, and this follows the site on it into the JDK's `src.zip`.
/// `None` at every step that cannot be taken, and never an error on the wire.
/// Most of those steps are silent on purpose, because they are not failures:
/// a document with no view, a position that selects no member, a member the
/// world cannot site, and no JDK behind the world at all are the ordinary
/// answers to a cursor that is not on one of these, and a `debug` line for
/// each would bury the ones that mean something. What is logged is a step that
/// got far enough to have gone wrong: the extraction — see
/// [`crate::jdk_sources::JdkSources::materialize`], which logs all four of its
/// own refusals — and an extracted path that will not make a URI.
fn external(
    state: &mut ServerState,
    id: &DocumentId,
    position: lsp_types::Position,
) -> Option<Location> {
    // Taken before the view, which borrows the state mutably — the same order,
    // and for the same reason, as `handlers::resolve_at`.
    let externals = state.externals();
    // The site is owned, so the view's borrow ends with this block and the
    // state is free for the `&mut` that extracting an entry needs.
    let (site, name) = {
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
        (found.site?, found.name)
    };
    sited(state, &site, &name)
}

/// The constructor the `new` under the cursor selects, in this file or where
/// the world sites it.
fn constructor(
    state: &mut ServerState,
    id: &DocumentId,
    position: lsp_types::Position,
) -> Option<Location> {
    let externals = state.externals();
    let survey = crate::handlers::trusted_survey(state);
    let (site, name) = {
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
            javelin_semantic::ConstructorTarget::Local(definition) => return located(&definition),
            javelin_semantic::ConstructorTarget::External(summary) => (summary.site?, summary.name),
        }
    };
    sited(state, &site, &name)
}

/// The classpath type the name under the cursor means, as a location on its
/// own name in the source entry that declares it.
fn jdk_type(
    state: &mut ServerState,
    id: &DocumentId,
    position: lsp_types::Position,
) -> Option<Location> {
    let (site, name) = crate::handlers::jdk_type_at(state, id, position)?;
    sited(state, &site, &name)
}

/// Where an [`javelin_semantic::ExternalSite`] points, extracting the
/// `src.zip` entry it names when it is not already a file on this machine.
///
/// An extracted entry's span must spell `name`: the offsets were read when
/// the facts were, and the archive is read now, so a JDK patched in place or
/// a sources JAR fetched again in between would otherwise land the caret on
/// whatever text now sits there.
pub(crate) fn sited(
    state: &mut ServerState,
    site: &javelin_semantic::ExternalSite,
    name: &str,
) -> Option<Location> {
    // A member the workspace walk recorded is a file on this machine, and the
    // walk wrote down where its name was: point there and open nothing. The
    // range is as fresh as the index, whose windows of staleness are
    // `ponytail:`-marked on `handlers::workspace_type_at`.
    if let Some(range) = site.name_range {
        return Some(Location::new(
            file_uri(std::path::Path::new(&site.source))?,
            to_lsp_range(range),
        ));
    }
    // Read before the `&mut` below rather than through it. The cap is the one
    // the bulk `src.zip` read uses: a single entry too large to be read for
    // its facts is too large to be extracted for a jump.
    let (path, text) = crate::handlers::site_text(state, site)?;
    // The one refusal here with nothing else to show for it: the entry was
    // found, decompressed and written, and the jump is lost at the last step.
    let Some(uri) = file_uri(&path) else {
        tracing::debug!(
            path = %path.display(),
            "definition: the extracted entry's path is not a URI, so there is nowhere to jump"
        );
        return None;
    };
    if text.get(site.name_start as usize..site.name_end as usize) != Some(name) {
        tracing::debug!(
            path = %path.display(),
            name,
            "definition: the extracted text no longer writes the name there"
        );
        return None;
    }
    // A document over the extracted text, so the byte offsets a fact carries
    // become UTF-16 positions through the same code every other range on the
    // wire goes through.
    let document = TextDocument::new(DocumentId::new(uri.as_str()), 0, text);
    let range = javelin_document::Range::new(
        document.position_at(site.name_start),
        document.position_at(site.name_end),
    );
    Some(Location::new(uri, to_lsp_range(range)))
}

/// The workspace type the name under the cursor means, as a location in the
/// file that declares it.
///
/// The first answer this server gives that is about a *sibling source file*
/// rather than about the open buffer or the JDK. No document is opened to
/// produce it: the index recorded the declaration's name as a line and a
/// UTF-16 character when it walked the file, so the whole answer is a lookup
/// and a path-to-URI. What that costs in freshness is on
/// [`crate::handlers::workspace_type_at`], where the `ponytail:` is.
fn workspace_type(
    state: &mut ServerState,
    id: &DocumentId,
    position: lsp_types::Position,
) -> Option<Location> {
    let (_, ty) = crate::handlers::workspace_type_at(state, id, position)?;
    // A path that is not a URI is dropped rather than rendered against the
    // requested buffer, for the reason `located` gives: the wrong jump is
    // worse than none. Not reachable from the walk, which takes UTF-8 paths.
    Some(Location::new(
        file_uri(&ty.file)?,
        to_lsp_range(ty.name_range),
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::classpath::Classpath;
    use javelin_classpath::{MemberFacts, MemberType, TypeFacts};
    use javelin_core::Config;
    use javelin_parser::{DeclKind, DefinitionKind, Span};
    use javelin_protocol::Readiness;
    use lsp_types::Position;
    use std::fs;
    use std::io::Write as _;
    use std::path::{Path, PathBuf};

    pub(crate) const URI: &str = "file:///A.java";

    pub(crate) fn opened(source: &str) -> ServerState {
        opened_with(Config::default(), source)
    }

    fn opened_with(config: Config, source: &str) -> ServerState {
        let mut state = ServerState::new(config).expect("grammar loads");
        state.did_open(DocumentId::new(URI), 1, source.to_owned());
        state
    }

    fn params(uri: &str, line: u32, character: u32) -> GotoDefinitionParams {
        serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
        }))
        .expect("well-formed params")
    }

    fn go_to(state: &mut ServerState, line: u32, character: u32) -> Option<Location> {
        let answer = handle(
            state,
            &params(URI, line, character),
            &CancellationToken::new(),
        );
        match answer.expect("not cancelled")? {
            GotoDefinitionResponse::Scalar(location) => Some(location),
            other => panic!("expected one location, got {other:?}"),
        }
    }

    /// A declaration in some file other than the one a request would name.
    ///
    /// Hand-built, because the server holds one document: the only way to have
    /// the declaring file and the asking file differ is to say so.
    fn declaration_in(file: &str) -> Definition {
        let span = |line, start, end| Span {
            start_byte: start,
            end_byte: end,
            range: javelin_document::Range::new(
                javelin_document::Position::new(line, start),
                javelin_document::Position::new(line, end),
            ),
        };
        Definition {
            file: DocumentId::new(file),
            reference: span(1, 0, 1),
            name: "x".to_owned(),
            kind: DefinitionKind::Field,
            span: span(3, 4, 11),
            name_span: span(3, 8, 9),
            signature: "int x;".to_owned(),
            doc: None,
        }
    }

    /// `s.length()` with the cursor in the name, on line 1 at character 24.
    const SELECTING: &str = "class A {\n  void m(String s) { s.length(); }\n}\n";

    /// The `src.zip` entry the world below says `length` is written in, and
    /// what it holds. Not the real `String.java`: what a jump has to land on
    /// is the offsets the facts carry, and a four-line file makes those
    /// readable in the assertion.
    const ENTRY: &str = "java.base/java/lang/String.java";
    const ENTRY_SOURCE: &str =
        "package java.lang;\npublic class String {\n    public int length() { return 0; }\n}\n";

    /// A world holding `java.lang.String` and its `length`, sited at
    /// `name_start` in `source`.
    ///
    /// Hand-built rather than read off the machine, for the reason
    /// `hover.rs`'s equivalent is: a test that read a real JDK would depend on
    /// which one is installed and cost eleven seconds.
    fn string_facts(name_start: u32, source: String) -> TypeFacts {
        TypeFacts {
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
            members: vec![MemberFacts {
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
                name_start,
                name_end: name_start + "length".len() as u32,
            }],
            source,
        }
    }

    pub(crate) fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("javelin-server-tests")
            .join("definition")
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// The path a `file:` URI names, undoing exactly what [`file_uri`] did.
    ///
    /// The production decoder rather than a second one written for the test:
    /// a temp directory under a profile with a space in its name is an
    /// ordinary Windows machine, and two decoders that disagreed would fail
    /// this test over the path rather than over the jump it is about.
    fn path_of(uri: &str) -> PathBuf {
        crate::workspace::path_of(uri).expect("a file URI")
    }

    /// A one-entry `src.zip` at `dir/src.zip`, stored uncompressed — nothing
    /// downstream cares which method wrote it.
    fn write_zip(dir: &Path, entry: &str, contents: &str) -> PathBuf {
        let path = dir.join("src.zip");
        let file = fs::File::create(&path).expect("zip file");
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file(entry, options).expect("start entry");
        writer
            .write_all(contents.as_bytes())
            .expect("write the entry");
        writer.finish().expect("finish zip");
        path
    }

    #[test]
    fn a_local_use_jumps_to_the_name_of_its_declaration() {
        let mut state = opened("class A {\n  int go(int n) {\n    return n;\n  }\n}\n");
        let location = go_to(&mut state, 2, 11).expect("a location");
        assert_eq!(location.uri.as_str(), URI);
        assert_eq!(location.range.start, Position::new(1, 13));
        assert_eq!(location.range.end, Position::new(1, 14));
    }

    #[test]
    fn a_name_this_file_does_not_declare_answers_null() {
        let mut state = opened("import java.util.List;\nclass A { List<String> items; }\n");
        assert!(go_to(&mut state, 1, 11).is_none());
    }

    #[test]
    fn whitespace_and_keywords_answer_null() {
        let mut state = opened("class A {\n  int x = 1;\n}\n");
        assert!(go_to(&mut state, 1, 2).is_none(), "on `int`");
        assert!(go_to(&mut state, 1, 10).is_none(), "on the `1`");
    }

    #[test]
    fn a_document_that_was_never_opened_answers_null_rather_than_failing() {
        let mut state = opened("class A {}");
        let params = params("file:///Absent.java", 0, 6);
        let answer = handle(&mut state, &params, &CancellationToken::new());
        assert_eq!(answer.expect("not cancelled"), None);
    }

    #[test]
    fn a_position_past_the_end_of_the_file_answers_null() {
        let mut state = opened("class A {}");
        assert!(go_to(&mut state, 900, 900).is_none());
    }

    /// Nothing found *and* no classpath is the one answer that can still
    /// improve: a name this buffer does not declare may be a JDK type.
    #[test]
    fn a_request_answered_before_the_world_arrives_is_partial() {
        let mut state = opened("import java.util.List;\nclass A { List<String> items; }\n");
        let answer = answer(&mut state, &params(URI, 1, 11), &CancellationToken::never())
            .expect("not cancelled");
        assert!(!answer.is_complete());
        assert_eq!(
            answer,
            Answer::Partial {
                value: None,
                complete_at: Readiness::Dependencies,
            }
        );
    }

    #[test]
    fn a_request_answered_after_the_world_arrives_is_complete() {
        let mut state = opened("import java.util.List;\nclass A { List<String> items; }\n");
        state.install_classpath(Classpath::hand_built(&["java.util.List"]));
        let answer = answer(&mut state, &params(URI, 1, 11), &CancellationToken::never())
            .expect("not cancelled");
        assert!(answer.is_complete(), "{answer:?}");
    }

    /// A location out of the open buffer is final whatever the readiness: the
    /// classpath adds resolutions where there were none and never moves one,
    /// so calling this partial would tell a client to re-ask forever.
    #[test]
    fn a_location_found_without_a_classpath_is_already_complete() {
        let mut state = opened("class A {\n  int go(int n) {\n    return n;\n  }\n}\n");
        let answer = answer(&mut state, &params(URI, 2, 11), &CancellationToken::never())
            .expect("not cancelled");
        assert!(!state.is_complete(), "no classpath arrived");
        assert!(answer.is_complete(), "{answer:?}");
    }

    /// The wiring itself. Swapping `&externals` back to `&NoExternals` in
    /// `resolve_at` leaves every other test in this crate green, because
    /// nothing else makes an answer depend on the world. This does: with two
    /// applicable overloads the invariant is to answer nothing, and `f(int)`
    /// is only excluded once `java.lang.String` is a type the world holds and
    /// so demonstrably unboxes to no primitive.
    #[test]
    fn a_handler_answer_changes_once_the_classpath_is_in() {
        let mut state = opened(
            "class A {\n  void f(String s) {}\n  void f(int i) {}\n  void go(String s) { f(s); }\n}\n",
        );
        assert!(
            go_to(&mut state, 3, 22).is_none(),
            "two candidates the open file cannot separate"
        );

        state.install_classpath(Classpath::hand_built(&["java.lang.String"]));
        let location = go_to(&mut state, 3, 22).expect("f(String)");
        assert_eq!(location.range.start, Position::new(1, 7));
    }

    /// The constraint the whole slice owes, at the one seam where it can
    /// regress unnoticed: a span out of one file rendered against another is a
    /// jump into the wrong place, and every other test here opens a single
    /// document, so none of them can tell the two URIs apart. Building the
    /// `Location` out of the request's URI again turns this red and nothing
    /// else.
    #[test]
    fn a_definition_is_located_in_the_file_its_declaration_names_rather_than_the_request() {
        let elsewhere = "file:///Elsewhere.java";
        let found = declaration_in(elsewhere);

        let location = located(&found).expect("a location");
        assert_eq!(location.uri.as_str(), elsewhere);
        assert_ne!(location.uri.as_str(), URI, "not the document that asked");
        assert_eq!(location.range.start, Position::new(3, 8));

        // And a file that cannot be named answers nothing rather than being
        // rendered somewhere that can be: no request can produce this, and
        // falling back to the asking document is the wrong jump above.
        assert_eq!(located(&declaration_in("not a uri at all")), None);
    }

    /// The ordinary case, restated now that the URI is built rather than
    /// cloned: a declaration in the open buffer names that buffer, so the
    /// location the client gets is the document it asked about.
    #[test]
    fn a_local_definition_still_answers_in_the_requested_document() {
        let mut state = opened("class A {\n  int go(int n) {\n    return n;\n  }\n}\n");
        assert_eq!(
            go_to(&mut state, 2, 11).expect("a location").uri.as_str(),
            URI
        );
    }

    /// A world that holds the member and cannot say where it is written: a
    /// hand-built one, and any fact read before sites were recorded at all.
    /// Not vacuous — the test below asks the same position of the same buffer
    /// with a site and an archive behind it and gets a location.
    #[test]
    fn an_external_member_without_a_classpath_source_answers_null() {
        let mut state = opened(SELECTING);
        state.install_classpath(Classpath::holding(vec![string_facts(0, String::new())]));

        assert_eq!(go_to(&mut state, 1, 24), None);
    }

    #[test]
    fn an_external_member_answers_a_location_in_the_extracted_entry() {
        let dir = scratch("external_definition");
        let zip_path = write_zip(&dir, ENTRY, ENTRY_SOURCE);
        let mut config = Config::default();
        config.index.cache_dir = Some(dir.join("cache"));
        let mut state = opened_with(config, SELECTING);
        let name_start = ENTRY_SOURCE.find("length").expect("the member's name") as u32;
        let mut classpath = Classpath::holding(vec![string_facts(name_start, ENTRY.to_owned())]);
        classpath.sources = Some((zip_path, "21".to_owned()));
        state.install_classpath(classpath);

        let location = go_to(&mut state, 1, 24).expect("a location");

        assert!(
            location.uri.as_str().ends_with(ENTRY),
            "the extracted entry, not the buffer: {}",
            location.uri.as_str()
        );
        assert_ne!(location.uri.as_str(), URI);
        // `public int length()` on the third line of the entry, which is the
        // name and not the declaration.
        assert_eq!(
            location.range,
            lsp_types::Range::new(Position::new(2, 15), Position::new(2, 21))
        );
        // And the client has something to open there.
        assert_eq!(
            fs::read_to_string(path_of(location.uri.as_str())).expect("the extracted file"),
            ENTRY_SOURCE
        );
    }

    /// A path is not a URI, and the two things that make it one are the two
    /// that would otherwise cost the jump silently: a byte the RFC 3986 parse
    /// refuses, and Windows' separator. Driven as strings rather than through
    /// `Path::is_absolute`, so both shapes are checked on every platform.
    #[test]
    fn a_path_with_a_space_or_a_drive_letter_still_makes_a_parseable_uri() {
        let unix = file_uri(Path::new("/home/a b/Größe.java")).expect("a URI");
        assert_eq!(unix.as_str(), "file:///home/a%20b/Gr%C3%B6%C3%9Fe.java");

        let windows = file_uri(Path::new(r"C:\Users\a b\String.java")).expect("a URI");
        assert_eq!(windows.as_str(), "file:///C:/Users/a%20b/String.java");

        // And [`path_of`] undoes it, which is what lets the test above it read
        // the extracted file back on a machine whose temp path has a space in
        // it — a Windows profile named after a person with two names.
        assert_eq!(path_of(unix.as_str()), Path::new("/home/a b/Größe.java"));
        assert_eq!(
            path_of(windows.as_str()),
            Path::new(r"C:/Users/a b/String.java")
        );
    }

    /// A relative path would become a URI naming it from the root — another
    /// file — so it makes none, in either platform's shape.
    #[test]
    fn a_relative_path_makes_no_uri_rather_than_one_from_the_root() {
        assert_eq!(file_uri(Path::new("cache/jdk/String.java")), None);
        assert_eq!(file_uri(Path::new(r"cache\jdk\String.java")), None);
        assert_eq!(file_uri(Path::new("C:String.java")), None);
        assert!(file_uri(Path::new(r"\\server\share\String.java")).is_some());
    }

    /// A workspace index holding one type per `(fqn, file, line)`, as the
    /// walk would have left it.
    ///
    /// The byte spans are zero and only `name_range` is filled in, because
    /// only `name_range` is read: the index stores an editor range beside the
    /// byte offsets precisely so that a reader which has never opened the file
    /// does not have to convert one.
    pub(crate) fn indexed(types: &[(&str, &str, u32)]) -> javelin_index::WorkspaceIndex {
        javelin_index::WorkspaceIndex::new(
            types
                .iter()
                .map(|(fqn, file, line)| {
                    let simple = fqn.rsplit('.').next().unwrap_or(fqn).to_owned();
                    let end = simple.len() as u32;
                    javelin_index::WorkspaceType {
                        is_public: true,
                        doc_span: None,
                        fqn: (*fqn).to_owned(),
                        simple,
                        kind: DeclKind::Class,
                        file: PathBuf::from(file),
                        name_start: 0,
                        name_end: 0,
                        decl_start: 0,
                        decl_end: 0,
                        name_range: javelin_document::Range::new(
                            javelin_document::Position::new(*line, 13),
                            javelin_document::Position::new(*line, 13 + end),
                        ),
                        supertypes: Vec::new(),
                        synthetic: false,
                        lombok: Default::default(),
                        type_parameters: Vec::new(),
                    }
                })
                .collect(),
        )
    }

    fn install(state: &mut ServerState, index: javelin_index::WorkspaceIndex) {
        state.install_workspace(crate::workspace::Workspace {
            index: std::sync::Arc::new(index),
            elapsed: std::time::Duration::ZERO,
            refreshed: false,
            survey: Some(std::sync::Arc::default()),
            absorbed: 0,
        });
    }

    /// The first jump in the project's life that is neither inside the open
    /// buffer nor inside the JDK.
    #[test]
    fn a_name_that_means_a_sibling_source_file_jumps_into_it() {
        let mut state = opened("package com.example;\nclass A { Greeter g; }\n");
        assert_eq!(
            go_to(&mut state, 1, 12),
            None,
            "the premise: with no index the name means nothing"
        );

        install(
            &mut state,
            indexed(&[("com.example.Greeter", "/w/com/example/Greeter.java", 4)]),
        );
        let location = go_to(&mut state, 1, 12).expect("the sibling's declaration");
        assert_eq!(location.uri.as_str(), "file:///w/com/example/Greeter.java");
        assert_ne!(location.uri.as_str(), URI, "not the document that asked");
        assert_eq!(
            location.range,
            lsp_types::Range::new(Position::new(4, 13), Position::new(4, 20)),
            "the type's own name, as the index recorded it"
        );
    }

    /// A stray copy or a half-finished move. Ambiguity answers nothing, here
    /// as everywhere: picking whichever the walk saw first is a jump into a
    /// file the user may not even have meant to keep.
    #[test]
    fn two_files_declaring_one_type_answer_null() {
        let mut state = opened("package com.example;\nclass A { Greeter g; }\n");
        install(
            &mut state,
            indexed(&[
                ("com.example.Greeter", "/w/a/Greeter.java", 4),
                ("com.example.Greeter", "/w/b/Greeter.java", 9),
            ]),
        );
        assert_eq!(go_to(&mut state, 1, 12), None);
    }

    /// `String` in `String s` on line 0 at character 12, with the JDK's
    /// `String` sited at its own name in [`ENTRY_SOURCE`].
    pub(crate) fn jdk_string(dir: &Path, sited: bool) -> ServerState {
        jdk_string_from(dir, if sited { ENTRY } else { "" }, sited)
    }

    fn jdk_string_from(dir: &Path, source: &str, sited: bool) -> ServerState {
        let zip_path = write_zip(dir, ENTRY, ENTRY_SOURCE);
        let mut config = Config::default();
        config.index.cache_dir = Some(dir.join("cache"));
        let mut state = opened_with(config, "class A { String s; }\n");
        let mut facts = string_facts(0, source.to_owned());
        if sited {
            facts.name_start = ENTRY_SOURCE.find("String {").expect("the name") as u32;
            facts.name_end = facts.name_start + "String".len() as u32;
        }
        let mut classpath = Classpath::holding(vec![facts]);
        classpath.sources = Some((zip_path, "21".to_owned()));
        state.install_classpath(classpath);
        state
    }

    #[test]
    fn an_external_type_name_jumps_to_its_own_name_in_the_extracted_entry() {
        let mut state = jdk_string(&scratch("external_type_definition"), true);
        install(&mut state, indexed(&[]));

        let location = go_to(&mut state, 0, 12).expect("a location");

        assert!(
            location.uri.as_str().ends_with(ENTRY),
            "{}",
            location.uri.as_str()
        );
        // `public class String {` on the second line of the entry.
        assert_eq!(
            location.range,
            lsp_types::Range::new(Position::new(1, 13), Position::new(1, 19))
        );
    }

    /// A JAR's type is read out of a class file, which has no name to point
    /// at, so the jump is refused rather than landing anywhere else.
    #[test]
    fn an_external_type_with_no_source_answers_null() {
        let mut state = jdk_string(&scratch("external_type_no_source"), false);
        assert_eq!(go_to(&mut state, 0, 12), None);
    }

    /// A source with no recorded name is not the top of the file: the jump
    /// would land on `package`.
    #[test]
    fn an_external_type_with_a_source_and_no_name_answers_null() {
        let mut state = jdk_string_from(&scratch("external_type_no_name"), ENTRY, false);
        assert_eq!(go_to(&mut state, 0, 12), None);
    }

    /// The workspace declaring the name at all is the workspace's to answer:
    /// two copies are an ambiguity, and the JDK's copy is not a tie-break.
    #[test]
    fn a_type_the_workspace_declares_twice_does_not_fall_back_to_the_jdk() {
        let mut state = jdk_string(&scratch("external_type_shadowed"), true);
        install(
            &mut state,
            indexed(&[
                ("java.lang.String", "/w/a/String.java", 1),
                ("java.lang.String", "/w/b/String.java", 1),
            ]),
        );
        assert_eq!(go_to(&mut state, 0, 12), None);
    }

    const ERROR_ENTRY: &str = "java.base/java/lang/Error.java";
    const ERROR_SOURCE: &str = "package java.lang;\npublic class Error {}\n";

    /// A buffer naming `Error` on its last line at character 4, a JDK holding
    /// `java.lang.Error` sited in [`ERROR_SOURCE`] and `java.lang.Object`,
    /// and a workspace of `files` — so the name resolves through the tiers
    /// to the JDK's type unless something Java reads first means another.
    fn error_state(name: &str, buffer: &str, files: &[(&str, &str)]) -> ServerState {
        let mut state = error_state_unwalked(name, buffer);
        let sources = crate::workspace::testing::indexed(name, files);
        install_arc(&mut state, std::sync::Arc::clone(&sources.index));
        state
    }

    fn error_state_unwalked(name: &str, buffer: &str) -> ServerState {
        let dir = scratch(name);
        let zip_path = write_zip(&dir, ERROR_ENTRY, ERROR_SOURCE);
        let mut config = Config::default();
        config.index.cache_dir = Some(dir.join("cache"));
        let mut state = opened_with(config, buffer);
        let mut error = string_facts(0, ERROR_ENTRY.to_owned());
        error.fqn = "java.lang.Error".to_owned();
        error.simple = "Error".to_owned();
        error.members.clear();
        error.supertypes = vec!["java.lang.Object".to_owned()];
        error.name_start = ERROR_SOURCE.find("Error").expect("the name") as u32;
        error.name_end = error.name_start + "Error".len() as u32;
        let mut object = string_facts(0, String::new());
        object.fqn = "java.lang.Object".to_owned();
        object.simple = "Object".to_owned();
        object.members.clear();
        let mut classpath = Classpath::holding(vec![error, object]);
        classpath.sources = Some((zip_path, "21".to_owned()));
        state.install_classpath(classpath);
        state
    }

    fn error_line(buffer: &str) -> u32 {
        buffer.lines().count() as u32 - 1
    }

    #[test]
    fn a_jdk_type_name_jumps_when_nothing_shadows_it() {
        let buffer = "package p;\nclass Sub {\n    Error e;\n}\n";
        let mut state = error_state("error-plain", buffer, &[]);
        let location = go_to(&mut state, 2, 6).expect("java.lang.Error");
        assert!(location.uri.as_str().ends_with(ERROR_ENTRY));
    }

    /// `Base.Error` is in scope in `Sub` by inheritance, and shadows
    /// `java.lang.Error`; the resolver's tiers do not know that.
    #[test]
    fn a_member_type_a_supertype_declares_shadows_a_jdk_type() {
        let buffer = "package p;\nclass Sub extends Base {\n    Error e;\n}\n";
        let mut state = error_state(
            "error-inherited",
            buffer,
            &[(
                "p/Base.java",
                "package p;\npublic class Base {\n    public static class Error {}\n}\n",
            )],
        );
        assert_eq!(go_to(&mut state, error_line(buffer) - 1, 6), None);
        assert_eq!(hover(&mut state, 2, 6), None);
    }

    #[test]
    fn a_statically_imported_member_type_shadows_a_jdk_type() {
        for import in ["import static q.Outer.Error;", "import static q.Outer.*;"] {
            let buffer = format!("package p;\n{import}\nclass Sub {{\n    Error e;\n}}\n");
            let mut state = error_state(
                "error-static",
                &buffer,
                &[(
                    "q/Outer.java",
                    "package q;\npublic class Outer {\n    public static class Error {}\n}\n",
                )],
            );
            assert_eq!(go_to(&mut state, 3, 6), None, "{import}");
        }
    }

    /// A type a generator wrote where the walk does not read may be the
    /// sibling `Error` means.
    #[test]
    fn a_generated_name_or_an_unwalked_workspace_shadows_a_jdk_type() {
        let buffer = "package p;\nclass Sub {\n    Error e;\n}\n";
        let mut state = error_state("error-generated", buffer, &[]);
        let mut survey = javelin_index::Survey::default();
        survey.generated.insert("Error".to_owned());
        state.install_workspace(crate::workspace::Workspace {
            index: state.workspace(),
            elapsed: std::time::Duration::ZERO,
            refreshed: false,
            survey: Some(std::sync::Arc::new(survey)),
            absorbed: 0,
        });
        assert_eq!(go_to(&mut state, 2, 6), None, "a generated name");

        let mut unwalked = error_state_unwalked("error-unwalked", buffer);
        assert_eq!(go_to(&mut unwalked, 2, 6), None, "no walk yet");
    }

    fn hover(state: &mut ServerState, line: u32, character: u32) -> Option<lsp_types::Hover> {
        let params: lsp_types::HoverParams = serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": URI },
            "position": { "line": line, "character": character },
        }))
        .expect("well-formed params");
        crate::handlers::hover::handle(state, &params, &CancellationToken::new())
            .expect("not cancelled")
    }

    const GREETER: &str = "package com.example;\npublic class Greeter {\n    void greet() {}\n}\n";

    fn with_sources(source: &str, sources: &crate::workspace::testing::Sources) -> ServerState {
        let mut state = opened(source);
        install_arc(&mut state, std::sync::Arc::clone(&sources.index));
        state
    }

    fn install_arc(state: &mut ServerState, index: std::sync::Arc<javelin_index::WorkspaceIndex>) {
        state.install_workspace(crate::workspace::Workspace {
            index,
            elapsed: std::time::Duration::ZERO,
            refreshed: false,
            survey: Some(std::sync::Arc::default()),
            absorbed: 0,
        });
    }

    const BOX: &str = "package com.example;\npublic class Box {\n    public Box(int size) {}\n    \
                       public Box(int a, int b) {}\n}\n";

    /// `new Box(1, 2)` names the constructor its arguments select, in the
    /// file and in the one next door; before the walk"s survey arrives no
    /// constructor is claimed and the name is the type"s, as it always was.
    #[test]
    fn a_new_jumps_to_the_constructor_its_arguments_select() {
        let local = "class Box {\n  Box(int a) {}\n  Box(int a, int b) {}\n  Box m() { return new Box(1, 2); }\n}\n";
        let mut state = opened(local);
        assert_eq!(
            go_to(&mut state, 3, 23).map(|l| l.range.start),
            Some(Position::new(0, 6)),
            "no survey: the type"
        );
        install_arc(&mut state, std::sync::Arc::new(indexed(&[])));
        assert_eq!(
            go_to(&mut state, 3, 23).map(|l| l.range.start),
            Some(Position::new(2, 2))
        );

        let sources = crate::workspace::testing::indexed(
            "constructor-jump",
            &[("com/example/Box.java", BOX)],
        );
        let mut state = with_sources(
            "package com.example;\nclass A { Object m() { return new Box(1, 2); } }\n",
            &sources,
        );
        let location = go_to(&mut state, 1, 34).expect("Box(int, int)");
        assert_eq!(
            Some(location.uri),
            file_uri(&sources.path("com/example/Box.java"))
        );
        assert_eq!(location.range.start, Position::new(3, 11));
        let hovered = format!("{:?}", hover(&mut state, 1, 34).expect("a hover"));
        assert!(hovered.contains("Box(int a, int b)"), "{hovered}");
    }

    /// `this(…)` and `super(…)` jump from their keyword to the constructor
    /// they select, here and in the superclass's file; an enum constant's
    /// name is still the constant.
    #[test]
    fn an_explicit_constructor_call_jumps_to_the_constructor_it_selects() {
        let local = "enum Day {\n  MON(1), TUE;\n  Day(int n) {}\n  Day() { this(2); }\n}\n";
        let mut state = opened(local);
        install_arc(&mut state, std::sync::Arc::new(indexed(&[])));
        assert_eq!(
            go_to(&mut state, 3, 11).map(|l| l.range.start),
            Some(Position::new(2, 2)),
            "this(2)"
        );
        assert_eq!(
            go_to(&mut state, 1, 3).map(|l| l.range.start),
            Some(Position::new(1, 2)),
            "the constant itself"
        );

        let sources = crate::workspace::testing::indexed(
            "constructor-super-jump",
            &[("com/example/Box.java", BOX)],
        );
        let mut state = with_sources(
            "package com.example;\nclass Crate extends Box { Crate() { super(1, 2); } }\n",
            &sources,
        );
        let location = go_to(&mut state, 1, 38).expect("Box(int, int)");
        assert_eq!(
            Some(location.uri),
            file_uri(&sources.path("com/example/Box.java"))
        );
        assert_eq!(location.range.start, Position::new(3, 11));
        let hovered = format!("{:?}", hover(&mut state, 1, 38).expect("a hover"));
        assert!(hovered.contains("Box(int a, int b)"), "{hovered}");
    }

    /// An anonymous class's own `new T(…)` is written outside its body, so it
    /// selects as a plain `new` does; one written inside the body does not.
    #[test]
    fn an_anonymous_classs_own_new_selects_its_constructor() {
        let source = "class Box {\n  Box(int a) {}\n  Box(int a, int b) {}\n  Object m() { return new Box(1, 2) {\n    Object n() { return new Box(1, 2); }\n  }; }\n}\n";
        let mut state = opened(source);
        install_arc(&mut state, std::sync::Arc::new(indexed(&[])));
        assert_eq!(
            go_to(&mut state, 3, 26).map(|l| l.range.start),
            Some(Position::new(2, 2))
        );
        assert_eq!(
            go_to(&mut state, 4, 28).map(|l| l.range.start),
            Some(Position::new(0, 6)),
            "inside the body: the type"
        );
    }

    /// The workspace's constructors of a type are all it has only where its
    /// file parsed, a processor did not write it and no Lombok annotation
    /// the walk does not model sits on it; a `private` one is no other
    /// file's to select.
    #[test]
    fn a_workspace_types_constructors_are_read_only_where_they_are_whole() {
        let user = "package com.example;\nclass A { Object m() { return new Box(1, 2); } }\n";
        let jump = |name: &str, path: &str, source: &str| {
            let sources = crate::workspace::testing::indexed(name, &[(path, source)]);
            let mut state = with_sources(user, &sources);
            go_to(&mut state, 1, 34).map(|location| location.range.start)
        };
        let at = |line| Some(Position::new(line, 11));
        assert_eq!(jump("whole", "com/example/Box.java", BOX), at(3));
        let broken = BOX.replace("}\n}\n", "}\n    void broken( {}\n}\n");
        assert_ne!(jump("broken", "com/example/Box.java", &broken), at(3));
        let delegated = BOX
            .replace(
                "public class",
                "import lombok.experimental.Delegate;\npublic class",
            )
            .replace(
                "    public Box(int size)",
                "    @Delegate Runnable r;\n    public Box(int size)",
            );
        assert_ne!(jump("delegated", "com/example/Box.java", &delegated), at(5));
        assert_eq!(
            jump(
                "delegated-plain",
                "com/example/Box.java",
                &delegated.replace("@Delegate ", "")
            ),
            at(5)
        );
        assert_ne!(
            jump(
                "generated",
                "build/generated/sources/annotationProcessor/java/main/com/example/Box.java",
                BOX
            ),
            at(3)
        );
        let private = BOX.replace("public Box(int a", "private Box(int a");
        assert_ne!(jump("private", "com/example/Box.java", &private), at(3));
    }

    /// protobuf's Gradle output is walked as a processor's is: a message and
    /// its accessor jump into it, and a hand-written copy wins.
    #[test]
    fn a_protobuf_message_is_a_jump_target_unless_written_by_hand() {
        const PROTO: &str = "build/generated/source/proto/main/java/com/acme/Order.java";
        const ORDER: &str = "package com.acme;\npublic final class Order {\n    \
                             public String getId() { return null; }\n}\n";
        let user = "package com.acme;\nclass A { Order o; Object m() { return o.getId(); } }\n";
        let sources = crate::workspace::testing::indexed("protobuf-output", &[(PROTO, ORDER)]);
        let mut state = with_sources(user, &sources);
        for column in [10, 41] {
            let location = go_to(&mut state, 1, column).expect("into the output");
            assert_eq!(
                Some(location.uri),
                file_uri(&sources.path(PROTO)),
                "{column}"
            );
        }
        let sources = crate::workspace::testing::indexed(
            "protobuf-output-written",
            &[(PROTO, ORDER), ("com/acme/Order.java", ORDER)],
        );
        let mut state = with_sources(user, &sources);
        let location = go_to(&mut state, 1, 10).expect("the hand-written copy");
        assert_eq!(
            Some(location.uri),
            file_uri(&sources.path("com/acme/Order.java"))
        );
    }

    /// Read through the index, the open file's own type has lost the
    /// `private` constructors its own code may call, so a qualified `new` of
    /// it inside it selects nothing: Java picks `Box(int)` here.
    #[test]
    fn a_qualified_new_of_this_files_own_type_is_not_read_through_the_index() {
        let own = "package com.example;\npublic class Box {\n    private Box(int n) {}\n    \
                   public Box(Object o) {}\n    \
                   static Box of() { return new com.example.Box(1); }\n}\n";
        let sources =
            crate::workspace::testing::indexed("constructor-own", &[("com/example/Box.java", own)]);
        let uri = file_uri(&sources.path("com/example/Box.java")).expect("a uri");
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        state.did_open(DocumentId::new(uri.as_str()), 1, own.to_owned());
        install_arc(&mut state, std::sync::Arc::clone(&sources.index));
        let column = own
            .lines()
            .nth(4)
            .and_then(|line| line.rfind("Box"))
            .expect("the new") as u32;
        let params: lsp_types::GotoDefinitionParams = serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 4, "character": column + 1 },
        }))
        .expect("well-formed params");
        let found =
            match handle(&mut state, &params, &CancellationToken::new()).expect("not cancelled") {
                Some(GotoDefinitionResponse::Scalar(location)) => Some(location.range.start),
                _ => None,
            };
        assert_ne!(found, Some(Position::new(3, 11)), "not Box(Object)");
    }

    /// The jump the eighth slice exists for: a member of a type declared in
    /// the file next door, followed into that file without opening it.
    #[test]
    fn definition_on_a_cross_file_member_lands_on_its_name() {
        let sources = crate::workspace::testing::indexed(
            "cross-file-member",
            &[("com/example/Greeter.java", GREETER)],
        );
        let mut state = with_sources(
            "package com.example;\nclass A { void m(Greeter g) { g.greet(); } }\n",
            &sources,
        );
        let location = go_to(&mut state, 1, 34).expect("Greeter.greet");
        assert_eq!(
            Some(location.uri),
            file_uri(&sources.path("com/example/Greeter.java"))
        );
        assert_eq!(
            location.range,
            lsp_types::Range::new(Position::new(2, 9), Position::new(2, 14))
        );
    }

    #[test]
    fn definition_on_an_inherited_unqualified_call_lands_in_the_supertypes_file() {
        let sources = crate::workspace::testing::indexed(
            "inherited-call",
            &[("com/example/Greeter.java", GREETER)],
        );
        let mut state = with_sources(
            "package com.example;\nclass Loud extends Greeter { void m() { greet(); } }\n",
            &sources,
        );
        let location = go_to(&mut state, 1, 41).expect("the inherited method");
        assert_eq!(
            Some(location.uri),
            file_uri(&sources.path("com/example/Greeter.java"))
        );
        assert_eq!(location.range.start, Position::new(2, 9));
    }

    /// The wrong pick the reviewer found: `Error` is `Result`'s own type
    /// parameter, so on a raw `r` the call `r.error()` types as nothing and is
    /// refused. Read through the tiers it typed as `java.lang.Error`, and
    /// `apply.rs` excluded `log(String)` on that JDK fact — a refusal turned
    /// into a guess. With arguments it is what they bind it to.
    #[test]
    fn a_workspace_members_type_parameter_does_not_become_a_jdk_type_and_pick_an_overload() {
        let sources = crate::workspace::testing::indexed(
            "type-parameter-pick",
            &[(
                "p/Result.java",
                "package p;\npublic class Result<Value, Error> {\n    public Error error() { return null; }\n}\n",
            )],
        );
        let mut state = with_sources(
            "package p;\nclass A {\n  void log(String s) {}\n  void log(Throwable t) {}\n  \
             void m(Result r) { log(r.error()); }\n  \
             void n(Result<String, String> r) { log(r.error()); }\n}\n",
            &sources,
        );
        state.install_classpath(Classpath::holding(vec![
            javelin_classpath::TypeFacts {
                is_public: true,
                constructors: Vec::new(),
                doc_span: None,
                name_start: 0,
                name_end: 0,
                fqn: "java.lang.Object".to_owned(),
                simple: "Object".to_owned(),
                kind: DeclKind::Class,
                type_parameters: Vec::new(),
                supertype_arguments: Vec::new(),
                supertypes: Vec::new(),
                supertypes_complete: true,
                members_complete: true,
                members: Vec::new(),
                source: String::new(),
            },
            javelin_classpath::TypeFacts {
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
                supertypes: vec!["java.lang.Object".to_owned()],
                supertypes_complete: true,
                members_complete: true,
                members: Vec::new(),
                source: String::new(),
            },
            javelin_classpath::TypeFacts {
                is_public: true,
                constructors: Vec::new(),
                doc_span: None,
                name_start: 0,
                name_end: 0,
                fqn: "java.lang.Throwable".to_owned(),
                simple: "Throwable".to_owned(),
                kind: DeclKind::Class,
                type_parameters: Vec::new(),
                supertype_arguments: Vec::new(),
                supertypes: vec!["java.lang.Object".to_owned()],
                supertypes_complete: true,
                members_complete: true,
                members: Vec::new(),
                source: String::new(),
            },
            javelin_classpath::TypeFacts {
                is_public: true,
                constructors: Vec::new(),
                doc_span: None,
                name_start: 0,
                name_end: 0,
                fqn: "java.lang.Error".to_owned(),
                simple: "Error".to_owned(),
                kind: DeclKind::Class,
                type_parameters: Vec::new(),
                supertype_arguments: Vec::new(),
                supertypes: vec!["java.lang.Throwable".to_owned()],
                supertypes_complete: true,
                members_complete: true,
                members: Vec::new(),
                source: String::new(),
            },
        ]));
        assert_eq!(
            go_to(&mut state, 4, 22),
            None,
            "two overloads nothing separates"
        );
        let location = go_to(&mut state, 5, 38).expect("bound to `String`");
        assert_eq!(location.range.start, Position::new(2, 7));
    }

    #[test]
    fn a_cancelled_definition_stops_instead_of_answering() {
        let mut state = opened("class A { int x = 1; }");
        let token = CancellationToken::new();
        token.cancel();
        let answer = handle(&mut state, &params(URI, 0, 14), &token);
        assert_eq!(answer.err(), Some(Cancelled));
    }
}
